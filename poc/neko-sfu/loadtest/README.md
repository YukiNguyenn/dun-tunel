# neko-sfu load-test harnesses

Two Playwright-driven harnesses live here:

| File | Purpose | Channel | Ack model |
|---|---|---|---|
| `loadtest.ts` | **Phase 0 PoC** — drives the standalone PoC viewer web app (`../viewer`), measures first-frame latency + RTP throughput + per-event input acks. | `neko-poc-input/v1` (ordered) | per-event `InputAck` |
| `loadtest-input.ts` | **Promoted (neko-v3-migration task 11.1)** — drives the **production** viewer wire protocol directly, asserts 100% input ack + unchanged video bitrate/drop-rate vs the PoC baseline. | `neko-input/v1` (`ordered:false maxRetransmits:0`) | one `InputProduced` channel-open ack |

```bash
npm install                 # playwright + tsx + typescript
npx playwright install chromium

npm run typecheck           # tsc --noEmit — type-checks BOTH harnesses (CI-safe, no edge needed)
npm run test                # PoC harness (loadtest.ts)
npm run test:input          # promoted harness (loadtest-input.ts)
```

## `loadtest-input.ts` — the promoted harness

This is the production promotion of the PoC harness called out in
`design.md` (Testing Strategy → Integration testing) and tasks.md **task
11.1**. It drives **5 concurrent viewers** (configurable) each completing
the full production handshake against `useSfuViewer.ts` ↔ `sfu_ws.rs`:

```
Init → recv transport → Consume video (+audio) → ConsumerResume
     → ConnectInputTransport → ProduceInput → open `neko-input` channel
     → burst of InputEnvelope JSON frames (Data Model M1)
```

### Pass criteria (gates CI — process exits non-zero on failure)

1. **100% input ack** (Requirements 3.1, 3.2) — every viewer's
   `neko-input` channel opens and is acked by the server's
   `InputProduced`. The production channel is `maxRetransmits:0` unordered
   (no per-frame ack), so "100% ack" means **all N viewers reach
   `inputReady`** and the channel stays open while the burst is sent.
2. **Video bitrate within baseline tolerance** — aggregate inbound bitrate
   within `±BITRATE_TOL` (default ±20%) of the PoC baseline **7.34 Mbps**
   (`../RESULTS.md`, Task 1.5 / 1.6), proving input traffic did not degrade
   the video path.
3. **Drop rate within ceiling** — per-viewer packet drop rate
   ≤ `DROP_RATE_MAX` (default 1%, R19; PoC measured 0%).

### Runtime requirement

This is an **integration** harness. A live, GREEN run needs a running edge
stack: a mediasoup worker (`edge-sfu`) + `edge-control` + a Neko v3
container emitting VP8/Opus RTP through the GStreamer bridge. **That stack
does not build on the Windows dev box** (`mediasoup-sys` won't compile
there — documented in the spec), so run this on **Linux / CI** or against a
**deployed edge**. The harness itself type-checks anywhere
(`npm run typecheck`).

### Run against a deployed edge

```bash
# Production edge (cookie-gated /v1/sfu/viewer/ws):
WS_BASE="wss://<share-sub>.<edge-host>" \
SESSION_ID="<share-session-id>" \
VIEWER_COOKIE="viewer-cookie=<jwt>" \
PAGE_ORIGIN="https://<share-sub>.<edge-host>" \
npm run test:input

# PoC server (no cookie; global producer on the PoC /ws endpoint):
WS_BASE="ws://localhost:4443" WS_PATH="/ws" SESSION_ID="" \
PAGE_ORIGIN="http://localhost:8090" \
npm run test:input
```

### Environment variables

| Var | Default | Meaning |
|---|---|---|
| `WS_BASE` | `ws://localhost:4443` | ws(s):// origin of the edge |
| `WS_PATH` | `/v1/sfu/viewer/ws?session={session}` | WS path template; `{session}` is substituted |
| `SESSION_ID` | `` | share-session id (required for the production path) |
| `VIEWER_COOKIE` | `` | `name=value` cookie set on `PAGE_ORIGIN` so Caddy `forward_auth` allows the upgrade |
| `PAGE_ORIGIN` | `http://localhost:8090` | http(s) origin Playwright navigates to (same-origin with the WS + cookie) |
| `MEDIASOUP_MODULE` | `https://esm.sh/mediasoup-client@3.20.0` | ESM URL for mediasoup-client (pinned to match the production viewer); CI can point this at a locally-served bundle |
| `VIEWER_COUNT` | `5` | concurrent viewers |
| `INPUT_EVENTS` | `150` | input frames per viewer (→ 750 total, the PoC pattern) |
| `INPUT_RATE_HZ` | `30` | per-viewer send rate (stays under the edge's 60/s/session clamp) |
| `RUN_DURATION_MS` | `60000` | steady-state hold for RTP stats |
| `FIRST_FRAME_TIMEOUT_MS` | `15000` | per-viewer first-frame / inputReady budget |
| `BITRATE_TOL` | `0.20` | fractional bitrate tolerance vs baseline |
| `DROP_RATE_MAX` | `0.01` | max acceptable per-viewer drop rate |
| `OUT_FILE` | `loadtest-input-results.json` | results JSON path |

### Output

A JSON summary (also written to `OUT_FILE`) plus a `PASS`/`FAIL` block listing
each check and its detail. The process exits non-zero on any failed check so
CI can gate on it.
