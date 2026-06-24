# Dun Tunel — Agent Guide

Rust edge data-plane for Dun Studio's "share-profile" (browser-profile-public-tunnel) feature. Deployed one instance per edge region (`sin`, `iad`, `fra`). The control plane lives in a separate `dun-api` service — edge has no direct DB/Redis access; all communication is over HTTP with a shared API key (`X-Edge-Api-Key`).

## Architecture

```
dun-api (control plane)
   │ HTTP + X-Edge-Api-Key
   ▼
edge-control :9443  ──┬── edge-sfu (mediasoup pool)
                      ├── edge-rathole-bridge (rathole config + process)
                      ├── edge-caddy-bridge (Caddy admin API dynamic routes)
                      ├── edge-bandwidth (60s metering tick)
                      └── edge-callback-client (HTTP → dun-api, retry + queue)
   │
   ▼
mediasoup (UDP 50k-60k)  rathole :2333 (TCP)  Caddy :8443 (HTTPS)
```

- **Caddy** terminates TLS (wildcard cert via Cloudflare DNS-01) and reverse-proxies per-session viewer subdomains (`*.sin.dun-studio.xyz`) into rathole tunnels.
- **rathole** carries the TCP control plane (HTTP/WS) from the owner's container to the edge.
- **mediasoup** handles WebRTC media (direct UDP from owner's Neko `udpsink` → PlainTransport; viewer browsers connect via WebRtcTransport).
- **edge-viewer-gate** is a stateless EdDSA cookie-auth sidecar invoked by Caddy `forward_auth` before proxying viewer traffic.

## Build & Dev Commands

| Command | Purpose |
|---------|---------|
| `cargo check --workspace` | Type-check all crates |
| `cargo test --workspace` | Run tests (currently only a placeholder smoke test) |
| `cargo clippy --all-targets -- -D warnings` | Lint |
| `cargo build --release -p edge-control` | Build the main binary |
| `cargo build --release -p edge-viewer-gate` | Build the auth sidecar |

**Windows caveat:** `mediasoup-sys` requires Python 3 + Meson + Ninja + C++ toolchain, unavailable on a bare Windows host. Options:
- Build via Docker: `docker build -f deploy/Dockerfile -t dun-tunel-edge:dev .`
- Exclude SFU-dependent crates: `cargo check --workspace --exclude edge-sfu --exclude edge-bandwidth --exclude edge-control`

**Docker dev stack** (Windows/WSL2-friendly):
```bash
docker compose -f docker-compose.yml -f docker-compose.dev.yml up -d
```
Uses bridge networking + named volumes instead of `network_mode: host`.

**Smoke test** (against a running stack): `deploy/scripts/smoke-test.sh`

## Crates

| Crate | Type | Purpose |
|-------|------|---------|
| `edge-control` | bin | Axum HTTP server (`:9443`). Entrypoint receiving commands from dun-api. Routes: `POST /v1/tunnels`, `DELETE /v1/tunnels/:id`, `POST /v1/tunnels/:id/sfu/router`, `DELETE /v1/tunnels/:id/sfu/viewer/:viewer_id`, `GET /v1/sfu/viewer/ws`, `POST /v1/tunnel/verify`, `GET /v1/state/snapshot`, `GET /healthz`. |
| `edge-sfu` | lib | mediasoup-rust wrapper. Manages `Router`/`Transport`/`Producer`/`Consumer` per session. Viewer cap: `VIEWER_CAP_PER_SESSION = 30`. |
| `edge-rathole-bridge` | lib | Manages rathole config file (atomic writes + SIGHUP) + port allocation + service registry. |
| `edge-caddy-bridge` | lib | Caddy admin API client for dynamic route management (add/remove per-session routes, wildcard TLS policy, SFU split-routes, session-ended 410 fallback). |
| `edge-bandwidth` | lib | Sequence-based idempotent bandwidth reporter (60s tick). Monotonic sequence counter persisted to disk for restart-safety. |
| `edge-callback-client` | lib | HTTP client to dun-api `/tunnels/edge-callback` with exponential backoff retry + persistent on-disk queue. |
| `edge-shared` | lib | Shared types, JWT verification (`JwtVerifier` with `kid` rotation + `RevocationOracle` trait), revocation, error types. **Dependency leaf — no cycles.** |
| `edge-viewer-gate` | bin | Stateless EdDSA cookie-auth sidecar (`:9444`). Caddy `forward_auth` calls `/check` on every viewer request. Verifies cookie JWT via JWKS (24h refresh) + revocation list polling (5s). |

## Conventions

- **Naming:** Crates prefixed `edge-`. Modules use snake_case. HTTP types use `#[serde(rename_all = "camelCase")]` for JSON wire format.
- **Architecture:** `edge-shared` is the dependency leaf. All other edge crates depend on it; none depend on each other cyclically. `edge-control` orchestrates and wires all sub-crates into `AppState`.
- **Error handling:** `anyhow` for app-level errors, `thiserror` (`EdgeError`/`EdgeResult`) in `edge-shared` for typed errors.
- **Logging:** `tracing` + `tracing-subscriber` with JSON output to stdout. Default filter: `info,edge_control=debug`.
- **Config:** All crates load config from env vars (`Config::from_env()`), fail-loud on required vars.
- **Idempotency:** Bandwidth callbacks use monotonic sequence counters (persisted). Deprovision is idempotent (second DELETE is a no-op).
- **Restart safety:** `session_id → subdomain` mapping persisted to disk (`SubdomainStore`) so Caddy routes can be cleaned after a restart. Bandwidth sequence state also persisted.
- **Fail-CLOSED:** JWT revocation oracle errors → token rejected. `edge-viewer-gate` returns 503 (not 401) when revocation feed is stale.
- **Security:** mTLS optional (`EDGE_MTLS_*` env vars). Viewer gate is loopback-only (`127.0.0.1:9444`). Rathole uses raw JWT bytes as shared secret (not the hash) because rathole compares wire bytes.
- **Release profile:** `lto = "fat"`, `codegen-units = 1`, `strip = true`, `panic = "abort"`, `opt-level = 3`.
- **Toolchain:** stable Rust with `rustfmt`, `clippy`, `rust-analyzer` (per `rust-toolchain.toml`).

## Deployment

### Docker Compose (primary)

4 services, all `network_mode: host`:
- **edge-control** — multi-stage build (`rust:1.88-bookworm` builder → `debian:bookworm-slim` runtime, UID/GID 1500)
- **caddy** — custom build via xcaddy with `caddy-dns/cloudflare` module for DNS-01 wildcard certs
- **rathole** — stock `rapiz1/rathole:latest`
- **edge-viewer-gate** — separate Dockerfile, pure-Rust, UID/GID 1501

State layout: `~/dun-tunel/state/{rathole,queue,caddy_data,caddy_config}/`.

### Systemd (alternative)

3 units in `deploy/systemd/`: `caddy.service`, `rathole.service`, `edge-control.service`. `edge-control` runs as user `edge`, with `ProtectSystem=strict`, `ReadWritePaths=/var/lib/dun-tunel`.

### Config templates

- `deploy/Caddyfile.tpl` — template with `{{REGION}}`/`{{DOMAIN}}` placeholders. Rendered via `scripts/render-caddyfile.sh` (sed, not envsubst). Caddy listens `:8443`, demuxes by Host.
- `deploy/rathole.tpl.toml` — baseline rathole server config. TCP transport (Phase 1, no TLS). `[server.services]` populated dynamically by `edge-rathole-bridge`.
- `deploy/.env.example` — comprehensive env var reference.

## Critical Pitfalls

1. **mediasoup-sys build on Windows:** Requires Python 3, Meson, Ninja, C++ toolchain. Not available on bare Windows. Use Docker or `--exclude edge-sfu --exclude edge-bandwidth --exclude edge-control`.

2. **`MEDIASOUP_ANNOUNCED_IP` / `SFU_ANNOUNCED_IP`:** Must be the VPS **public** IPv4, not private NAT IP. If wrong → viewers connect but video is black (mediasoup advertises wrong ICE candidate).

3. **Caddy TLS policy overlap:** The `Caddyfile.tpl` deliberately does NOT declare an `edge.<region>.<domain>` site block because it would conflict with the wildcard automation policy installed via admin API. edge-control bootstraps both the wildcard TLS policy and the edge admin route programmatically.

4. **Rathole shared secret = raw JWT, not hash:** `tunnel_token_hash` is for audit/storage; `tunnel_token` (raw JWT bytes) is the rathole shared secret. Storing the hash would cause silent handshake failures.

5. **`network_mode: host` required for production:** mediasoup UDP performance needs host networking. Docker Desktop on Windows doesn't support this — use `docker-compose.dev.yml` override (bridge + port forwarding).

6. **Cloudflare DNS records must be DNS-only (grey cloud):** Proxying (orange cloud) breaks Caddy DNS-01 challenge and rathole TCP connections.

7. **Firewall ports:** `22/tcp`, `8443/tcp` (Caddy), `2333/tcp` (rathole), `50000-60000/udp` (mediasoup ICE).

8. **`SFU_WORKERS` defaults to 1:** Each worker binds one UDP mux port. Previously defaulted to `num_cpus` — now explicitly defaults to 1 to minimize NAT port-forwarding requirements.

9. **`edge-viewer-gate` strict mode:** `REVOCATION_REQUIRED=true` without `REVOCATION_URL` is ineffective — the gate logs an error at boot but doesn't fail to start. Operators must verify both are set together.

10. **Subdomain store persistence:** If `subdomain_store` disk writes fail, the mapping is in-memory only. A restart while sessions are active would lose entries and subsequent DELETEs would leak Caddy routes.

11. **`DUN_API_ENDPOINT` parsing:** edge-control parses `host:port` from this to configure Caddy split-routes. If unparseable, viewer cookie endpoints will 404. A warning is logged but the process continues.

## Documentation

- `README.md` — Top-level: crate table, deploy steps, architecture diagram, troubleshooting, local build instructions
- `deploy/README.md` — Deployment details: stack table, systemd setup, Cloudflare DNS records, update/rollback workflow, smoke test, logging
- `deploy/.env.example` — Full env var reference with comments
- `deploy/Caddyfile.tpl` — Annotated Caddy config template
- `deploy/rathole.tpl.toml` — Annotated rathole config template