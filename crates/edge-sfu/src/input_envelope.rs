//! `InputEnvelope` — the `neko-input` DataChannel contract (design C5 / M1).
//!
//! Viewers send mouse/keyboard/clipboard input as JSON envelopes over a
//! mediasoup SCTP DataChannel (`label: "neko-input"`,
//! `protocol: "neko-input/v1"`). The edge SFU decodes each frame into one of
//! these variants and the `NekoInputBridge` (task 4) forwards it to Neko v3
//! admin events.
//!
//! This enum is the Rust mirror of the TypeScript `InputEnvelope` union in
//! `viewer-ui-react`. Field bounds match the documented Data Model M1:
//!
//! | variant     | fields                          | bounds                       |
//! |-------------|---------------------------------|------------------------------|
//! | `move`      | `x`, `y` (`u16`), `ts` (`u64`)  | `[0, 65535]`                 |
//! | `scroll`    | `dx`, `dy` (`i16`), `ts` (`u64`)| `[-32768, 32767]`            |
//! | `key_down`  | `key` (`u64`), `ts` (`u64`)     | X11 keysym                   |
//! | `key_up`    | `key` (`u64`), `ts` (`u64`)     | X11 keysym                   |
//! | `clipboard` | `text` (`String`), `ts` (`u64`) | ≤ 8192 bytes (bridge clamps) |
//!
//! `ts` is a client monotonic ms timestamp used only for drop-old ordering.

use serde::Deserialize;

/// One decoded viewer input event. `#[serde(tag = "type")]` matches the
/// TS discriminated union: each JSON object carries a snake_case `type`
/// field (`move` / `scroll` / `key_down` / `key_up` / `clipboard`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputEnvelope {
    /// Absolute pointer position in Neko_Server screen space.
    Move { x: u16, y: u16, ts: u64 },
    /// Scroll wheel delta.
    Scroll { dx: i16, dy: i16, ts: u64 },
    /// Key press (X11 keysym).
    KeyDown { key: u64, ts: u64 },
    /// Key release (X11 keysym).
    KeyUp { key: u64, ts: u64 },
    /// Clipboard text to push to the remote (truncated to 8192 bytes by the bridge).
    Clipboard { text: String, ts: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_move() {
        let env: InputEnvelope =
            serde_json::from_str(r#"{"type":"move","x":100,"y":200,"ts":1234}"#).unwrap();
        assert_eq!(
            env,
            InputEnvelope::Move {
                x: 100,
                y: 200,
                ts: 1234
            }
        );
    }

    #[test]
    fn deserializes_scroll_with_negative_delta() {
        let env: InputEnvelope =
            serde_json::from_str(r#"{"type":"scroll","dx":-5,"dy":10,"ts":42}"#).unwrap();
        assert_eq!(
            env,
            InputEnvelope::Scroll {
                dx: -5,
                dy: 10,
                ts: 42
            }
        );
    }

    #[test]
    fn deserializes_key_down_and_up() {
        let down: InputEnvelope =
            serde_json::from_str(r#"{"type":"key_down","key":65307,"ts":1}"#).unwrap();
        assert_eq!(down, InputEnvelope::KeyDown { key: 65307, ts: 1 });

        let up: InputEnvelope =
            serde_json::from_str(r#"{"type":"key_up","key":65307,"ts":2}"#).unwrap();
        assert_eq!(up, InputEnvelope::KeyUp { key: 65307, ts: 2 });
    }

    #[test]
    fn deserializes_clipboard() {
        let env: InputEnvelope =
            serde_json::from_str(r#"{"type":"clipboard","text":"hello","ts":99}"#).unwrap();
        assert_eq!(
            env,
            InputEnvelope::Clipboard {
                text: "hello".to_string(),
                ts: 99
            }
        );
    }

    #[test]
    fn rejects_unknown_variant() {
        let res: Result<InputEnvelope, _> =
            serde_json::from_str(r#"{"type":"teleport","x":1,"y":2,"ts":3}"#);
        assert!(res.is_err());
    }

    #[test]
    fn rejects_out_of_range_u16_coordinate() {
        // 70000 overflows u16 → serde must reject.
        let res: Result<InputEnvelope, _> =
            serde_json::from_str(r#"{"type":"move","x":70000,"y":0,"ts":0}"#);
        assert!(res.is_err());
    }
}
