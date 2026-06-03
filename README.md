# dun-tunel

Edge tunnel server cho Dun Studio share-profile feature. Một workspace Rust deploy lên mỗi Edge_Region (`sin`, `iad`, `fra`).

Tham chiếu spec: [`dun-app/.kiro/specs/browser-profile-public-tunnel/`](../dun-app/.kiro/specs/browser-profile-public-tunnel/)

## Crates

| Crate | Type | Mô tả |
|---|---|---|
| `edge-control` | bin | Axum HTTP/mTLS server :8443 — entrypoint nhận lệnh từ dun-api |
| `edge-sfu` | lib | Wrapper mediasoup-rust quản lý Router/Transport/Producer/Consumer per session |
| `edge-rathole-bridge` | lib | Manage rathole config + spawn process |
| `edge-caddy-bridge` | lib | Caddy admin API client cho route động |
| `edge-bandwidth` | lib | Bandwidth metering với sequence counter (idempotent callback) |
| `edge-callback-client` | lib | HTTP client tới dun-api với retry + persistent queue |
| `edge-shared` | lib | Types + JWT verify + revocation cache shared giữa các crate |

## Build

```bash
# Full workspace (requires Linux/macOS/WSL2 with Python+Meson+C++ for mediasoup)
cargo check --workspace
cargo test --workspace
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo build --release -p edge-control

# On Windows dev without toolchain, exclude SFU-dependent crates:
cargo check --workspace --exclude edge-sfu --exclude edge-bandwidth --exclude edge-control
cargo test  --workspace --exclude edge-sfu --exclude edge-bandwidth --exclude edge-control --lib
```

> Note: `edge-bandwidth` và `edge-control` đều depend trên `edge-sfu`, nên phải exclude cùng nhau khi không build mediasoup.

Binary output: `target/release/edge-control` (statically linked với musl trên Linux).

## Required env vars (edge-control)

| Var | Description |
|---|---|
| `REGION_ID` | `sin`, `iad`, `fra` |
| `DUN_API_ENDPOINT` | URL dun-api (vd `https://api.dun.app`) |
| `DUN_API_KEY` | API key shared với dun-api cho callback (Phase 1-3) |
| `EDGE_MTLS_CERT_PATH` | Server cert cho :8443 mTLS |
| `EDGE_MTLS_KEY_PATH` | Server key |
| `EDGE_MTLS_CA_PATH` | CA cert để verify dun-api client cert |
| `CLOUDFLARE_API_TOKEN` | DNS-01 challenge cho Caddy (Caddy đọc từ env này) |
| `CADDY_ADMIN_URL` | Default `http://127.0.0.1:2019` |
| `RATHOLE_CONFIG_PATH` | Path tới `rathole.toml` để bridge rewrite |
| `RATHOLE_RELOAD_SIGNAL` | Default `SIGHUP` |
| `PERSISTENT_QUEUE_DIR` | Default `/var/lib/dun-tunel/queue` |
| `RUST_LOG` | `info,edge_control=debug` |

## Deploy

Xem `deploy/` — Dockerfile, docker-compose, systemd units, bootstrap script.

## Architecture

Đây là **data plane** của hệ thống share-profile. **Control plane** ở `dun-api`. Đây KHÔNG có quyền truy cập MongoDB/Redis của dun-api — mọi communication qua HTTP/mTLS endpoints.

```
            dun-api (control)
                 │ mTLS
                 ▼
   ┌─────────────────────────────┐
   │  edge-control :8443         │
   │  ├─ edge-sfu (mediasoup)    │
   │  ├─ edge-rathole-bridge     │
   │  ├─ edge-caddy-bridge       │
   │  ├─ edge-bandwidth (60s)    │
   │  └─ edge-callback-client    │
   └─────────────────────────────┘
       │           │           │
       ▼           ▼           ▼
   mediasoup    rathole       Caddy
   workers      :2333         :443

       ▲           ▲           ▲
       │           │           │
       │           │           ▼
       │           │      Viewer browsers
       │           │
       │           └── Tunnel_Client (rathole client trong dun-app)
       │
       └─── WebRTC Producer (Neko_Server trong Profile_Container qua tunnel)
```

# 3. Clone repo
git clone <your-repo> dun-tunel
cd dun-tunel

# 4. Edit .env
cp deploy/.env.example deploy/.env
nano deploy/.env   # điền secret + cloudflare token

# 5. Render Caddyfile từ tpl
sed 's/{{REGION}}/sin/g' deploy/Caddyfile.tpl > deploy/Caddyfile

# 6. Tạo dirs cho rathole + sequence persistence
sudo mkdir -p /etc/rathole /var/lib/dun-tunel/queue
sudo chown -R $USER:$USER /var/lib/dun-tunel

# 7. Render rathole config
sed 's/{{REGION}}/sin/g' deploy/rathole.tpl.toml | sudo tee /etc/rathole/server.toml

# 8. Open firewall (UFW)
sudo ufw allow 22/tcp     # SSH
sudo ufw allow 8443/tcp   # Caddy
sudo ufw allow 2333/tcp   # rathole
sudo ufw allow 50000:60000/udp  # mediasoup ICE
sudo ufw enable

# 9. Build + start (DOCKER_BUILDKIT=1 đã default Linux 23+)
cd deploy
docker compose build
docker compose up -d

# 10. Smoke test
curl -k https://localhost:8443/healthz   # qua Caddy → edge-control
curl http://localhost:9443/healthz       # direct edge-control
docker compose logs -f edge-control