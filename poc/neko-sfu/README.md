# Phase 0 PoC — Neko ↔ mediasoup Integration

> **BLOCKING gate** cho `browser-profile-public-tunnel` spec. Phase 1 KHÔNG được bắt đầu nếu PoC này fail.

## Mục đích

Verify rằng Neko_Server có thể publish video/audio/data stream lên mediasoup-rust SFU và viewer có thể subscribe stream + gửi input qua DataChannel với latency chấp nhận được.

Toàn bộ Requirement 8 (`SFU Media Routing`) phụ thuộc kết quả của PoC này (R8.12). Nếu Option A fail → fallback sang Option B (sidecar `webrtc-rs` publisher) hoặc Option C (thay Neko).

## Vị trí trong repo

PoC sống **ngoài** Cargo workspace chính của `dun-tunel` (xem `dun-tunel/Cargo.toml` — `members = ["crates/*"]` chỉ gồm các crate sản phẩm).

```
dun-tunel/
├── Cargo.toml         # workspace chính, KHÔNG include poc/
├── crates/...
└── poc/
    └── neko-sfu/      # <— bạn đang ở đây, standalone
        ├── README.md
        ├── RESULTS.md
        ├── .gitignore
        ├── docker-compose.yml   (task 1.2)
        ├── Cargo.toml           (task 1.4 — standalone, không link workspace)
        └── src/bin/...          (verify_publish, viewer_latency, ...)
```

Lý do tách workspace:
- PoC dùng dependency / version có thể khác main workspace (đặc biệt mediasoup feature flags, webrtc-rs).
- Tránh `cargo check --workspace` ở main workspace pull thêm build time của PoC.
- PoC scripts là throw-away — kết quả chính là `RESULTS.md`, không phải code.

## Stack

- **Neko**: image official `m1k1o/neko:*` (chrome flavour) chạy qua docker-compose.
- **mediasoup**: Rust binary build từ `edge-sfu` crate (stub) hoặc node-mediasoup throw-away cho spike.
- **Spike scripts**: Rust standalone binary trong `poc/neko-sfu/src/bin/` (Cargo.toml riêng, không link main workspace).
- **Viewer load test**: headless puppeteer hoặc `webrtc-rs` client.

## Cách chạy

> **Option 1**: Standalone mediasoup verification (Phase 0 hiện tại). Neko đã được tách ra
> khỏi compose stack — verify mediasoup-rust + browser client trước, integrate Neko sau.

```bash
cp .env.example .env
docker compose up -d --build

# Open viewer page:
#   http://localhost:8090   → spike viewer (Producer + Consumer)
#
# In viewer:
#   1. WS URL defaults to ws://127.0.0.1:4443/ws
#   2. Click "connect" → wait for "Init" log
#   3. Click "produce camera" → grants webcam, sends video+audio
#   4. Click "consume self (echo)" → server forwards back, second <video> plays
#
# Logs:
#   docker compose logs -f sfu
```

### Verify checklist (task 1.4)

- SFU container alive, log shows `PoC SFU listening on 0.0.0.0:4443`
- `ws://localhost:4443/ws` upgrades successfully (browser DevTools → Network → WS)
- Browser receives `Init` with `routerRtpCapabilities` containing Opus + VP8
- Producing camera yields server response `Produced` with valid producer id
- Consuming back: remote `<video>` element shows your own camera feed (echo)

## Success Criteria (GO threshold)

PoC PASS khi tất cả các điều kiện sau đồng thời thoả mãn:

| # | Criterion | Threshold | Source |
|---|---|---|---|
| 1 | Neko publish Producer thành công | `kind=video` + `kind=audio` xuất hiện trong < 10s sau khi container start | R8.12 |
| 2 | Viewer first-frame latency | < 4s từ khi gọi `consume()` | R19.2 |
| 3 | SFU forwarding latency p95 | ≤ 250ms (network RTT loopback localhost test) | R19.4 |
| 4 | Drop rate trong 60s steady-state | < 1% packet loss | R19.4 |
| 5 | 5 viewer concurrent subscribe | All 5 nhận stream stable, không crash SFU | R8.12 |
| 6 | DataChannel input forwarding | Mouse/keyboard event từ viewer → Neko execute trong container | R8.10, R8.11 |

## NO-GO Criteria (Fail threshold)

PoC FAIL nếu **bất kỳ** điều dưới đây xảy ra:

- Neko không thể publish stream qua mediasoup TURN (Option A fail) → fallback Option B.
- p95 latency > 250ms ở localhost test → SFU pipeline không khả thi cho production target.
- DataChannel input không forward được và không có patch path cho Neko → rewrite Requirement 8.
- 5 viewer concurrent crash SFU hoặc Neko → architecture không scale tới viewer cap = 30.

Trong trường hợp NO-GO, `RESULTS.md` PHẢI list rõ:
- Failure mode quan sát được
- Bench numbers cụ thể
- Recommended next path (Option B sidecar / Option C thay Neko / patch upstream)

## GO/NO-GO Decision Workflow

1. Hoàn thành tasks 1.1–1.6 trong `tasks.md`.
2. Điền số liệu thực vào `RESULTS.md` (task 1.7).
3. Owner review `RESULTS.md` + benchmark logs.
4. Owner xác nhận GO trước khi unblock Phase 1 (task 1.9 checkpoint).

## References

- Spec: `dun-app/.kiro/specs/browser-profile-public-tunnel/`
  - `requirements.md` — R8 (SFU Media Routing), R19 (Performance), R8.12 (PoC dependency)
  - `design.md` — §3.4 Neko_Server Integration (Option A vs B)
  - `tasks.md` — Phase 0, tasks 1.1–1.9
- Neko: <https://github.com/m1k1o/neko>
- mediasoup-rust: <https://github.com/versatica/mediasoup>
