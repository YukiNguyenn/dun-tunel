//! `NekoInputBridge` (design C5) — one per active session.
//!
//! Holds a Neko v3 WebSocket **admin** client and translates decoded
//! [`InputEnvelope`] frames (received over a viewer's `neko-input` SCTP
//! DataChannel) into Neko v3 admin events (`control/mouse`,
//! `control/scroll`, `control/keyboard`, `control/clipboard`).
//!
//! Lifecycle (this module, task 4.1):
//!  - [`NekoInputBridge::connect`] — open the admin WS with a Bearer token,
//!    spawn the read loop, and register the bridge so the call is
//!    **idempotent per session** (a second `connect` for the same session
//!    returns the already-live bridge instead of opening a second socket).
//!  - [`NekoInputBridge::shutdown`] — abort the read loop and close the WS.
//!
//! Forwarding ([`NekoInputBridge::forward`], the single `control/request`
//! claim and per-envelope event mapping) is implemented in task 4.2, and the
//! per-session rate limiter in task 4.3. This file deliberately leaves a
//! minimal `forward` stub plus the `control_held` flag and the private
//! [`NekoInputBridge::send_event`] helper so those tasks can extend it
//! without restructuring.

use crate::input_envelope::InputEnvelope;
use anyhow::{Context, Result};
use dashmap::DashMap;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<WsStream, Message>;

/// Process-global registry of live bridges keyed by `session_id`. Backs the
/// "idempotent per session" contract of [`NekoInputBridge::connect`]: a
/// second connect for a session already present returns the existing
/// `Arc<NekoInputBridge>` rather than opening a duplicate admin socket.
///
/// Entries are removed by [`NekoInputBridge::shutdown`] so a later connect
/// re-establishes the session cleanly.
fn registry() -> &'static DashMap<String, Arc<NekoInputBridge>> {
    static REG: OnceLock<DashMap<String, Arc<NekoInputBridge>>> = OnceLock::new();
    REG.get_or_init(DashMap::new)
}

/// One Neko v3 admin WebSocket client per session.
pub struct NekoInputBridge {
    /// Neko v3 session id this bridge drives. Also the registry key.
    session_id: String,
    /// Write half of the admin WS. Behind a `Mutex` because `forward` takes
    /// `&self` (it is shared via `Arc`) yet needs `&mut` access to send.
    sink: Mutex<WsSink>,
    /// Monotonic `false → true`. Set the first time control is claimed so at
    /// most one `control/request` is ever sent per session (design Property
    /// 3). Wired up by the `forward` implementation in task 4.2.
    control_held: AtomicBool,
    /// Per-session sliding-window rate limiter (design Property 7). Gates
    /// `forward` so at most [`MAX_EVENTS_PER_SECOND`] envelopes are forwarded
    /// within any rolling 1-second window; excess is dropped (best-effort,
    /// the input channel is `ordered:false maxRetransmits:0` so loss is
    /// acceptable). Behind a `Mutex` because `forward` takes `&self`.
    rate_limiter: Mutex<RateLimiter>,
    /// Handle to the spawned read loop, aborted on [`Self::shutdown`].
    read_task: Mutex<Option<JoinHandle<()>>>,
}

impl NekoInputBridge {
    /// Connect to the session's Neko admin WS (Bearer token) and spawn the
    /// read loop. **Idempotent per session**: if a live bridge already exists
    /// for `session_id`, the existing handle is returned and no new socket is
    /// opened.
    ///
    /// `neko_ws_url` is the Neko v3 admin WebSocket URL (`ws://` or `wss://`);
    /// `token` is attached as an `Authorization: Bearer <token>` header on the
    /// upgrade request.
    pub async fn connect(session_id: &str, neko_ws_url: &str, token: &str) -> Result<Arc<Self>> {
        // Fast path: an already-connected bridge for this session.
        if let Some(existing) = registry().get(session_id) {
            tracing::debug!(
                session_id = %session_id,
                "neko input bridge already connected; reusing"
            );
            return Ok(existing.clone());
        }

        let mut request = neko_ws_url
            .into_client_request()
            .with_context(|| format!("invalid neko ws url: {neko_ws_url}"))?;
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {token}")
                .parse()
                .context("invalid bearer token header value")?,
        );

        let (ws, _response) = connect_async(request)
            .await
            .with_context(|| format!("neko admin ws connect failed: {neko_ws_url}"))?;

        let (sink, mut stream) = ws.split();

        let bridge = Arc::new(Self {
            session_id: session_id.to_string(),
            sink: Mutex::new(sink),
            control_held: AtomicBool::new(false),
            rate_limiter: Mutex::new(RateLimiter::new()),
            read_task: Mutex::new(None),
        });

        // Read loop: Neko admin emits acks / events we don't act on yet
        // (event handling lands with later tasks). We still must drain the
        // stream so the connection stays healthy and so a server-side close
        // is observed and logged.
        let sid = session_id.to_string();
        let task = tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(Message::Close(_)) => {
                        tracing::info!(session_id = %sid, "neko admin ws closed by server");
                        break;
                    }
                    Ok(_) => {
                        // Drain. Inbound event handling is added by later tasks.
                    }
                    Err(e) => {
                        tracing::warn!(
                            session_id = %sid,
                            error = %e,
                            "neko admin ws read error; ending read loop"
                        );
                        break;
                    }
                }
            }
        });
        *bridge.read_task.lock().await = Some(task);

        registry().insert(session_id.to_string(), bridge.clone());
        tracing::info!(session_id = %session_id, "neko input bridge connected");
        Ok(bridge)
    }

    /// Translate one input envelope into Neko v3 admin events (pseudocode A3).
    ///
    /// Sends `control/request` exactly once per session lifetime — the first
    /// envelope flips [`Self::control_held`] `false → true` and claims control,
    /// matching the host hook's `hostingRef` gate so the Neko server log is not
    /// spammed with `ErrIsAlreadyTheHost`. After the claim, the envelope maps
    /// to **exactly one** Neko v3 admin event:
    ///
    /// | envelope            | Neko admin event   | payload                   |
    /// |---------------------|--------------------|---------------------------|
    /// | `Move { x, y }`     | `control/mouse`    | `{ x, y }`                |
    /// | `Scroll { dx, dy }` | `control/scroll`   | `{ x: dx, y: dy }`        |
    /// | `KeyDown { key }`   | `control/keyboard` | `{ key, pressed: true }`  |
    /// | `KeyUp { key }`     | `control/keyboard` | `{ key, pressed: false }` |
    /// | `Clipboard { t }`   | `control/clipboard`| `{ text: truncate(t) }`   |
    ///
    /// The per-session rate limiter (design Property 7) gates at the top of
    /// this method: if the rolling 1-second window is already full, the
    /// envelope is dropped (best-effort) and `Ok(())` is returned **without**
    /// claiming control or sending any event.
    pub async fn forward(&self, ev: InputEnvelope) -> Result<()> {
        // Per-session rate limiter gate (pseudocode A3:
        // `IF NOT rateLimiter.allow(now()) THEN RETURN`). Drop excess before
        // any control claim or event send — input is best-effort and unordered.
        if !self.rate_limiter.lock().await.allow(Instant::now()) {
            return Ok(());
        }

        self.claim_control().await?;

        let event = envelope_to_event(&ev);
        self.send_event(&event).await
    }

    /// Send `control/request` at most once per session lifetime.
    ///
    /// Uses a `compare_exchange` on [`Self::control_held`] so the claim is
    /// monotonic `false → true` and races between concurrently-forwarded
    /// envelopes still send a single request (design Property 3:
    /// `count(control/request) ≤ 1`).
    async fn claim_control(&self) -> Result<()> {
        // Delegate the monotonic false→true decision to the pure helper so the
        // claim logic is testable without a live admin socket (design Property
        // 3). Only the caller that wins the transition sends `control/request`.
        if should_send_control_request(&self.control_held) {
            self.send_event(&json!({ "event": "control/request" }))
                .await
                .context("send control/request")?;
        }
        Ok(())
    }

    /// Close the admin WS. Called on session teardown. Removes the bridge from
    /// the registry, aborts the read loop, and sends a close frame.
    pub async fn shutdown(&self) {
        // Drop from the registry first so any concurrent `connect` re-creates
        // rather than handing back a socket we're tearing down.
        registry().remove(&self.session_id);

        if let Some(task) = self.read_task.lock().await.take() {
            task.abort();
        }

        let mut sink = self.sink.lock().await;
        let _ = sink.send(Message::Close(None)).await;
        let _ = sink.close().await;

        tracing::info!(session_id = %self.session_id, "neko input bridge shut down");
    }

    /// Send one Neko v3 admin event as a WS JSON text frame. Private helper
    /// used by [`Self::forward`] so the event-mapping code stays terse and
    /// consistent.
    async fn send_event(&self, payload: &serde_json::Value) -> Result<()> {
        let text = serde_json::to_string(payload).context("serialize neko admin event")?;
        let mut sink = self.sink.lock().await;
        sink.send(Message::Text(text.into()))
            .await
            .context("send neko admin event")
    }
}

/// Maximum input events forwarded per rolling 1-second window per session
/// (design Property 7: `forwarded ≤ 60`). Excess is dropped best-effort.
const MAX_EVENTS_PER_SECOND: usize = 60;

/// Length of the rolling window the limiter enforces.
const RATE_WINDOW: Duration = Duration::from_secs(1);

/// Per-session sliding-window rate limiter.
///
/// Keeps the [`Instant`]s of the events admitted in the last [`RATE_WINDOW`]
/// in a FIFO deque. On each [`RateLimiter::allow`] call, timestamps older than
/// one window are pruned from the front; the event is admitted only when fewer
/// than [`MAX_EVENTS_PER_SECOND`] remain. This guarantees that across **any**
/// rolling 1-second window at most [`MAX_EVENTS_PER_SECOND`] events are
/// admitted (design Property 7).
///
/// `allow` takes the current [`Instant`] as a parameter (rather than reading
/// the clock internally) so the window logic is deterministic and unit-testable
/// with injected timestamps.
struct RateLimiter {
    /// Admission timestamps within the current window, oldest at the front.
    window: VecDeque<Instant>,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(MAX_EVENTS_PER_SECOND),
        }
    }

    /// Returns `true` and records `now` if admitting an event at `now` keeps
    /// the rolling-window count at or below [`MAX_EVENTS_PER_SECOND`];
    /// otherwise returns `false` and records nothing (the event is dropped).
    fn allow(&mut self, now: Instant) -> bool {
        // Prune everything that fell out of the rolling window.
        // `saturating_duration_since` guards against `now` predating a recorded
        // stamp (only possible with non-monotonic injected test inputs).
        while let Some(&front) = self.window.front() {
            if now.saturating_duration_since(front) >= RATE_WINDOW {
                self.window.pop_front();
            } else {
                break;
            }
        }

        if self.window.len() < MAX_EVENTS_PER_SECOND {
            self.window.push_back(now);
            true
        } else {
            false
        }
    }
}

/// Decide whether a `control/request` should be sent for this claim, flipping
/// `control_held` from `false → true` exactly once.
///
/// Extracted as a free function (taking the [`AtomicBool`] by reference) so the
/// single-claim guarantee (design Property 3: `count(control/request) ≤ 1`) is
/// testable without a live Neko admin socket. Uses `compare_exchange` so the
/// transition is monotonic and concurrent callers race to a single winner:
/// only the task that observes `false` and installs `true` returns `true`; all
/// others observe `true` and return `false`.
///
/// AcqRel on success / Acquire on failure mirrors the original inline guard so
/// `claim_control` keeps identical runtime behavior.
fn should_send_control_request(control_held: &AtomicBool) -> bool {
    control_held
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// Maximum clipboard payload (bytes) forwarded to Neko. Longer text is
/// truncated on a UTF-8 char boundary (design M1 / Error Handling: "Clipboard
/// too large → truncate + warn").
const MAX_CLIPBOARD_BYTES: usize = 8192;

/// Map one [`InputEnvelope`] to its single Neko v3 admin event (pseudocode A3).
///
/// Pure and side-effect free (aside from the truncation warning) so the
/// envelope→event contract is unit-testable without a live Neko socket. Every
/// envelope yields **exactly one** event (design Property 4).
fn envelope_to_event(ev: &InputEnvelope) -> serde_json::Value {
    match ev {
        InputEnvelope::Move { x, y, .. } => {
            json!({ "event": "control/mouse", "x": x, "y": y })
        }
        InputEnvelope::Scroll { dx, dy, .. } => {
            // A3 maps the scroll delta onto Neko's {x, y} scroll payload.
            json!({ "event": "control/scroll", "x": dx, "y": dy })
        }
        InputEnvelope::KeyDown { key, .. } => {
            json!({ "event": "control/keyboard", "key": key, "pressed": true })
        }
        InputEnvelope::KeyUp { key, .. } => {
            json!({ "event": "control/keyboard", "key": key, "pressed": false })
        }
        InputEnvelope::Clipboard { text, .. } => {
            json!({ "event": "control/clipboard", "text": truncate_clipboard(text) })
        }
    }
}

/// Truncate clipboard `text` to at most [`MAX_CLIPBOARD_BYTES`] bytes without
/// splitting a multi-byte UTF-8 character. Returns the original string
/// unchanged when it already fits.
fn truncate_clipboard(text: &str) -> &str {
    if text.len() <= MAX_CLIPBOARD_BYTES {
        return text;
    }
    // Walk back from the byte cap to the nearest char boundary so we never
    // emit invalid UTF-8.
    let mut end = MAX_CLIPBOARD_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    tracing::warn!(
        original_bytes = text.len(),
        truncated_bytes = end,
        "clipboard text exceeded 8192 bytes; truncating"
    );
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_rejects_non_ws_url() {
        // A plain http URL is not a valid WebSocket upgrade target, so the
        // request build fails fast without needing a live Neko server.
        let res = NekoInputBridge::connect("sess-1", "http://example.com", "tok").await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn connect_rejects_garbage_url() {
        let res = NekoInputBridge::connect("sess-2", "not a url", "tok").await;
        assert!(res.is_err());
    }

    #[test]
    fn maps_move_to_control_mouse() {
        let ev = envelope_to_event(&InputEnvelope::Move {
            x: 100,
            y: 200,
            ts: 1,
        });
        assert_eq!(ev, json!({ "event": "control/mouse", "x": 100, "y": 200 }));
    }

    #[test]
    fn maps_scroll_delta_to_control_scroll_xy() {
        let ev = envelope_to_event(&InputEnvelope::Scroll {
            dx: -5,
            dy: 10,
            ts: 1,
        });
        // A3: dx → x, dy → y.
        assert_eq!(ev, json!({ "event": "control/scroll", "x": -5, "y": 10 }));
    }

    #[test]
    fn maps_key_down_and_up_with_pressed_flag() {
        let down = envelope_to_event(&InputEnvelope::KeyDown { key: 65307, ts: 1 });
        assert_eq!(
            down,
            json!({ "event": "control/keyboard", "key": 65307, "pressed": true })
        );

        let up = envelope_to_event(&InputEnvelope::KeyUp { key: 65307, ts: 2 });
        assert_eq!(
            up,
            json!({ "event": "control/keyboard", "key": 65307, "pressed": false })
        );
    }

    #[test]
    fn maps_clipboard_to_control_clipboard() {
        let ev = envelope_to_event(&InputEnvelope::Clipboard {
            text: "hello".to_string(),
            ts: 1,
        });
        assert_eq!(ev, json!({ "event": "control/clipboard", "text": "hello" }));
    }

    #[test]
    fn truncate_clipboard_leaves_short_text_unchanged() {
        assert_eq!(truncate_clipboard("hello"), "hello");
        // Exactly at the cap is left intact.
        let exact = "a".repeat(MAX_CLIPBOARD_BYTES);
        assert_eq!(truncate_clipboard(&exact).len(), MAX_CLIPBOARD_BYTES);
    }

    #[test]
    fn truncate_clipboard_clamps_oversized_ascii() {
        let big = "a".repeat(MAX_CLIPBOARD_BYTES + 500);
        let out = truncate_clipboard(&big);
        assert_eq!(out.len(), MAX_CLIPBOARD_BYTES);
    }

    #[test]
    fn truncate_clipboard_never_splits_utf8_char() {
        // '€' is 3 bytes. Build a string whose byte length crosses the cap in
        // the middle of a multi-byte char, then assert the result is valid
        // UTF-8 and within the cap.
        let euro = "€"; // 3 bytes
        let count = (MAX_CLIPBOARD_BYTES / euro.len()) + 10;
        let big = euro.repeat(count);
        let out = truncate_clipboard(&big);
        assert!(out.len() <= MAX_CLIPBOARD_BYTES);
        // Round-trips as valid UTF-8 and is a whole number of '€' chars.
        assert!(out.chars().all(|c| c == '€'));
    }

    #[test]
    fn rate_limiter_admits_up_to_cap_at_same_instant() {
        let mut rl = RateLimiter::new();
        let t = Instant::now();
        // The first 60 events in one instant are admitted; the 61st is dropped.
        for i in 0..MAX_EVENTS_PER_SECOND {
            assert!(rl.allow(t), "event {i} within cap should be admitted");
        }
        assert!(
            !rl.allow(t),
            "event {} exceeds the per-second cap and must be dropped",
            MAX_EVENTS_PER_SECOND + 1
        );
    }

    #[test]
    fn rate_limiter_never_exceeds_cap_in_any_rolling_window() {
        // Drive a dense burst across several seconds at fine-grained ticks and
        // assert that for every admitted event, the number admitted in the
        // preceding rolling 1s window never exceeds the cap (design Property 7).
        let mut rl = RateLimiter::new();
        let start = Instant::now();
        let mut admitted: Vec<Instant> = Vec::new();

        // 5000 attempts at 1ms spacing => 5s of attempts at 1000/s offered load.
        for ms in 0..5000u64 {
            let now = start + Duration::from_millis(ms);
            if rl.allow(now) {
                admitted.push(now);
                // Count admissions strictly within the last rolling second:
                // every t with (now - t) < 1s, i.e. the window (now - 1s, now].
                let count = admitted
                    .iter()
                    .filter(|&&t| now.saturating_duration_since(t) < RATE_WINDOW)
                    .count();
                assert!(
                    count <= MAX_EVENTS_PER_SECOND,
                    "rolling-window admissions {count} exceeded cap at {ms}ms"
                );
            }
        }

        // Sanity: the limiter actually admitted a bounded, non-trivial number.
        assert!(!admitted.is_empty());
        assert!(admitted.len() <= 6 * MAX_EVENTS_PER_SECOND);
    }

    #[test]
    fn rate_limiter_refills_after_window_elapses() {
        let mut rl = RateLimiter::new();
        let t0 = Instant::now();
        // Saturate the window.
        for _ in 0..MAX_EVENTS_PER_SECOND {
            assert!(rl.allow(t0));
        }
        assert!(!rl.allow(t0), "window is full");

        // One full second later every prior stamp has aged out, so the limiter
        // admits a fresh full window of events.
        let t1 = t0 + RATE_WINDOW;
        for i in 0..MAX_EVENTS_PER_SECOND {
            assert!(rl.allow(t1), "post-window event {i} should be admitted");
        }
        assert!(!rl.allow(t1), "the refilled window is full again");
    }

    // ---------------------------------------------------------------------
    // Property-based tests (proptest). These validate the design's
    // Correctness Properties 3, 4 and 7 over generated input spaces.
    // ---------------------------------------------------------------------

    use proptest::prelude::*;

    /// Strategy producing arbitrary [`InputEnvelope`] values across every
    /// variant, spanning the full documented field bounds (M1):
    /// `x,y ∈ u16`, `dx,dy ∈ i16`, `key,ts ∈ u64`, clipboard text up to a few
    /// KB (including past the 8192-byte clamp) with arbitrary unicode.
    fn arb_envelope() -> impl Strategy<Value = InputEnvelope> {
        prop_oneof![
            (any::<u16>(), any::<u16>(), any::<u64>()).prop_map(|(x, y, ts)| InputEnvelope::Move {
                x,
                y,
                ts
            }),
            (any::<i16>(), any::<i16>(), any::<u64>())
                .prop_map(|(dx, dy, ts)| InputEnvelope::Scroll { dx, dy, ts }),
            (any::<u64>(), any::<u64>()).prop_map(|(key, ts)| InputEnvelope::KeyDown { key, ts }),
            (any::<u64>(), any::<u64>()).prop_map(|(key, ts)| InputEnvelope::KeyUp { key, ts }),
            (".{0,12000}", any::<u64>())
                .prop_map(|(text, ts)| InputEnvelope::Clipboard { text, ts }),
        ]
    }

    proptest! {
        /// **Property 3: Single control claim** — across any number of forwarded
        /// envelopes, `count(control/request) ≤ 1`.
        ///
        /// Drives the extracted [`should_send_control_request`] decision over a
        /// sequence of N claims (the pure core of `claim_control`, exercised
        /// once per forwarded envelope). Exactly one claim — the first — may
        /// return `true`; every subsequent claim must observe the monotonic
        /// `control_held` flag as already set and return `false`.
        ///
        /// **Validates: Requirements 3.2, 3.5**
        #[test]
        fn prop_single_control_claim(n in 0usize..512) {
            let control_held = AtomicBool::new(false);
            let granted = (0..n)
                .filter(|_| should_send_control_request(&control_held))
                .count();
            prop_assert!(granted <= 1, "control/request sent {granted} times, expected ≤ 1");
            // A claim was attempted at least once ⟹ control is now held.
            if n > 0 {
                prop_assert!(control_held.load(Ordering::Acquire));
                prop_assert_eq!(granted, 1);
            } else {
                prop_assert_eq!(granted, 0);
            }
        }

        /// **Property 3 (concurrent):** even when many claims race across
        /// threads, exactly one wins the `false → true` transition.
        ///
        /// **Validates: Requirements 3.2, 3.5**
        #[test]
        fn prop_single_control_claim_concurrent(threads in 2usize..16, per_thread in 1usize..64) {
            use std::sync::Arc as StdArc;
            let control_held = StdArc::new(AtomicBool::new(false));
            let granted = StdArc::new(std::sync::atomic::AtomicUsize::new(0));

            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    let ch = control_held.clone();
                    let g = granted.clone();
                    std::thread::spawn(move || {
                        for _ in 0..per_thread {
                            if should_send_control_request(&ch) {
                                g.fetch_add(1, Ordering::AcqRel);
                            }
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }

            let total = granted.load(Ordering::Acquire);
            prop_assert_eq!(total, 1, "exactly one thread should win the control claim");
            prop_assert!(control_held.load(Ordering::Acquire));
        }

        /// **Property 4: Envelope-to-event injectivity** — each accepted
        /// `InputEnvelope` maps to exactly one Neko admin event of the matching
        /// kind. The mapping is total (never panics) and produces exactly one
        /// JSON object whose `event` field matches the variant.
        ///
        /// **Validates: Requirements 3.2**
        #[test]
        fn prop_envelope_to_event_injectivity(env in arb_envelope()) {
            let event = envelope_to_event(&env);

            // Exactly one event object with a single `event` discriminator.
            let obj = event.as_object().expect("event must be a JSON object");
            let kind = obj
                .get("event")
                .and_then(|v| v.as_str())
                .expect("event object must carry a string `event` field");

            match &env {
                InputEnvelope::Move { x, y, .. } => {
                    prop_assert_eq!(kind, "control/mouse");
                    prop_assert_eq!(obj.get("x").and_then(|v| v.as_u64()), Some(*x as u64));
                    prop_assert_eq!(obj.get("y").and_then(|v| v.as_u64()), Some(*y as u64));
                }
                InputEnvelope::Scroll { dx, dy, .. } => {
                    prop_assert_eq!(kind, "control/scroll");
                    prop_assert_eq!(obj.get("x").and_then(|v| v.as_i64()), Some(*dx as i64));
                    prop_assert_eq!(obj.get("y").and_then(|v| v.as_i64()), Some(*dy as i64));
                }
                InputEnvelope::KeyDown { key, .. } => {
                    prop_assert_eq!(kind, "control/keyboard");
                    prop_assert_eq!(obj.get("key").and_then(|v| v.as_u64()), Some(*key));
                    prop_assert_eq!(obj.get("pressed").and_then(|v| v.as_bool()), Some(true));
                }
                InputEnvelope::KeyUp { key, .. } => {
                    prop_assert_eq!(kind, "control/keyboard");
                    prop_assert_eq!(obj.get("key").and_then(|v| v.as_u64()), Some(*key));
                    prop_assert_eq!(obj.get("pressed").and_then(|v| v.as_bool()), Some(false));
                }
                InputEnvelope::Clipboard { .. } => {
                    prop_assert_eq!(kind, "control/clipboard");
                    let text = obj.get("text").and_then(|v| v.as_str())
                        .expect("clipboard event must carry text");
                    // Clamp bound holds for every accepted clipboard envelope.
                    prop_assert!(text.len() <= MAX_CLIPBOARD_BYTES);
                }
            }
        }

        /// **Property 7: Rate clamp bound** — for any burst of timestamps, the
        /// limiter admits ≤ [`MAX_EVENTS_PER_SECOND`] events within any trailing
        /// rolling 1-second window. Generates bursts of arbitrary millisecond
        /// offsets from a base instant, feeds them in chronological order, and
        /// checks the rolling-window invariant against every admitted event.
        ///
        /// **Validates: Requirements 3.3**
        #[test]
        fn prop_rate_clamp_bound(mut offsets in prop::collection::vec(0u64..4000, 0..2000)) {
            offsets.sort_unstable();
            let base = Instant::now();
            let mut rl = RateLimiter::new();
            let mut admitted: Vec<Instant> = Vec::new();

            for off in offsets {
                let now = base + Duration::from_millis(off);
                if rl.allow(now) {
                    admitted.push(now);
                    // Count admissions within the trailing window (now-1s, now].
                    let count = admitted
                        .iter()
                        .filter(|&&t| now.saturating_duration_since(t) < RATE_WINDOW)
                        .count();
                    prop_assert!(
                        count <= MAX_EVENTS_PER_SECOND,
                        "rolling-window admissions {count} exceeded cap {MAX_EVENTS_PER_SECOND}"
                    );
                }
            }
        }
    }
}
