# dun-tunel

Edge tunnel server cho Dun Studio share-profile feature. Một workspace Rust deploy lên mỗi Edge_Region (`sin`, `iad`, `fra`).

Tham chiếu spec: [`dun-app/.kiro/specs/browser-profile-public-tunnel/`](../dun-app/.kiro/specs/browser-profile-public-tunnel/)

## Crates

| Crate | Type | Mô tả |
|---|---|---|
| `edge-control` | bin | Axum HTTP server :9443 — entrypoint nhận lệnh từ dun-api |
| `edge-sfu` | lib | Wrapper mediasoup-rust quản lý Router/Transport/Producer/Consumer per session |
| `edge-rathole-bridge` | lib | Manage rathole config + spawn process |
| `edge-caddy-bridge` | lib | Caddy admin API client cho route động |
| `edge-bandwidth` | lib | Bandwidth metering với sequence counter (idempotent callback) |
| `edge-callback-client` | lib | HTTP client tới dun-api với retry + persistent queue |
| `edge-shared` | lib | Types + JWT verify + revocation cache shared giữa các crate |

## Deploy lên Ubuntu VPS (Phase 1)

> Yêu cầu: VPS Ubuntu 22.04+ với Docker + Compose plugin đã cài, ≥ 4 GB RAM / 2 vCPU.

### 1. DNS records (Cloudflare)

4 record A trỏ cùng IP VPS, **DNS only / xám** (KHÔNG bật proxy):

```
api.dun-studio.xyz          A   <vps-ip>
edge.sin.dun-studio.xyz     A   <vps-ip>
tunnel.sin.dun-studio.xyz   A   <vps-ip>
*.sin.dun-studio.xyz        A   <vps-ip>
```

Tạo Cloudflare API token scope `Zone:DNS:Edit` cho zone `dun-studio.xyz` ONLY (Caddy dùng cho DNS-01 challenge).

### 2. Mở firewall

```bash
sudo ufw allow 22/tcp                 # SSH
sudo ufw allow 8443/tcp               # Caddy public
sudo ufw allow 2333/tcp               # rathole control
sudo ufw allow 50000:60000/udp        # mediasoup ICE
sudo ufw enable
```

### 3. Clone repo + bootstrap dirs

```bash
git clone <repo-url> ~/dun-tunel
cd ~/dun-tunel

# Persistent state cho bandwidth sequence + rathole config
mkdir -p ~/dun-tunel/state/rathole ~/dun-tunel/state/queue
```

### 4. Sinh secret

```bash
# Lưu lại 2 giá trị này — phải khớp với dun-api .env tương ứng
JWT_SECRET=$(openssl rand -base64 48)
EDGE_API_KEY=$(openssl rand -base64 32)
echo "JWT_SECRET=$JWT_SECRET"
echo "EDGE_API_KEY=$EDGE_API_KEY"
```

### 5. Cấu hình env

```bash
cd deploy
cp .env.example .env
nano .env
```

Điền tối thiểu:

```env
REGION_ID=sin
SHARE_TUNNEL_DOMAIN=dun-studio.xyz
DUN_API_ENDPOINT=http://localhost:3010
DUN_API_KEY=<EDGE_API_KEY ở bước 4>
CLOUDFLARE_API_TOKEN=<Cloudflare API token>
TUNNEL_JWT_SECRET_V1=<JWT_SECRET ở bước 4>
```

### 6. Render Caddyfile + rathole.toml từ template

```bash
# Caddy config
sed 's/{{REGION}}/sin/g; s/share\.dun\.app/dun-studio.xyz/g' \
    deploy/Caddyfile.tpl > deploy/Caddyfile

# Rathole config
sed 's/{{REGION}}/sin/g; s/share\.dun\.app/dun-studio.xyz/g' \
    deploy/rathole.tpl.toml | sudo tee /etc/rathole/server.toml
```

### 7. Build + start stack

```bash
cd /opt/dun-tunel/deploy
docker compose build edge-control       # ~8-12 phút lần đầu
docker compose up -d
```

Build incremental sau đó (đổi code Rust) chỉ ~2 phút nhờ BuildKit cache mounts.

### 8. Verify

```bash
# edge-control health
curl http://localhost:9443/healthz

# Caddy đã bind 8443
curl -k https://localhost:8443/healthz   # qua reverse proxy

# Logs realtime
docker compose logs -f edge-control caddy rathole

# Smoke test full pipeline
DUN_API_KEY=$EDGE_API_KEY ./scripts/smoke-test.sh
```

### 9. Cấu hình dun-api tương ứng

Trên server chạy dun-api, set env (`.env.docker`):

```env
SHARE_TUNNEL_DOMAIN=dun-studio.xyz
TUNNEL_JWT_KEYS=v1:<JWT_SECRET ở bước 4>
TUNNEL_JWT_CURRENT_KID=v1
EDGE_CALLBACK_API_KEY=<EDGE_API_KEY ở bước 4>
EDGE_ENDPOINTS=sin:http://host.docker.internal:9443
```

Nếu dun-api chạy cùng VPS, đảm bảo `docker-compose.yml` của dun-api có `extra_hosts: host.docker.internal:host-gateway` cho service `app` (đã có sẵn).

### 10. Mediasoup announced IP

Mặc định mediasoup auto-detect IP. Nếu VPS sau NAT (cloud có private IP):

```env
# deploy/.env
MEDIASOUP_ANNOUNCED_IP=<public IP của VPS>
```

VPS Hetzner/DO/Vultr public IP trực tiếp thì bỏ qua.

## Update / rollback

```bash
cd /opt/dun-tunel
git pull
cd deploy
docker compose build edge-control
docker compose up -d edge-control       # rolling restart, Caddy + rathole giữ nguyên
```

Drain trước nếu update breaking:

```bash
# Mark region unhealthy (admin endpoint dun-api)
curl -X POST -H "x-edge-api-key:$DUN_API_KEY" \
     https://api.dun-studio.xyz:8443/admin/region/sin/drain

# Đợi activeSessions == 0
watch -n 5 'curl -s http://localhost:9443/healthz | jq .activeSessions'

# Update
docker compose build edge-control && docker compose up -d edge-control
```

## Build local (dev không deploy)

Ubuntu/macOS/WSL2 với Python + Meson + C++ toolchain:

```bash
cargo check --workspace
cargo test --workspace
cargo clippy --all-targets -- -D warnings
cargo build --release -p edge-control
```

Windows host không có toolchain mediasoup → build qua Docker:

```bash
docker build -f deploy/Dockerfile -t dun-tunel-edge:dev .
```

Hoặc exclude SFU crate khi check thuần Windows:

```bash
cargo check --workspace --exclude edge-sfu --exclude edge-bandwidth --exclude edge-control
```

## Architecture

Đây là **data plane** của hệ thống share-profile. **Control plane** ở `dun-api`. Edge KHÔNG có quyền truy cập MongoDB/Redis của dun-api — mọi communication qua HTTP endpoints.

```
                dun-api (control plane)
                       │ HTTP + X-Edge-Api-Key
                       ▼
        ┌──────────────────────────────────┐
        │  edge-control :9443              │
        │  ├─ edge-sfu (mediasoup pool)    │
        │  ├─ edge-rathole-bridge          │
        │  ├─ edge-caddy-bridge            │
        │  ├─ edge-bandwidth (60s tick)    │
        │  └─ edge-callback-client         │
        └──────────────────────────────────┘
            │           │           │
            ▼           ▼           ▼
        mediasoup    rathole       Caddy
        workers      :2333         :8443
        UDP 50k-60k    (TCP)       (HTTPS)
            ▲           ▲           ▲
            │           │           │
            │           │           ▼
            │           │     Viewer browsers
            │           │
            │           └── Tunnel_Client (rathole client trong dun-app)
            │
            └── WebRTC Producer (Neko_Server trong Profile_Container qua tunnel)
```

## Troubleshooting

**Build fail `mediasoup-sys`**: thiếu Python 3 / Meson / Ninja. Linux installer sẵn trong Dockerfile, dev local cần `apt install python3 python3-pip python3-invoke meson ninja-build build-essential cmake pkg-config`.

**Viewer kết nối được nhưng video đen**: Mediasoup advertise sai IP → set `MEDIASOUP_ANNOUNCED_IP` (bước 10).

**Cert renewal fail**: kiểm tra Cloudflare token còn valid + DNS records vẫn DNS-only (xám). `docker compose logs caddy | grep -i error`.

**Rathole disconnect liên tục**: kiểm tra firewall mở `2333/tcp`, log `docker compose logs rathole`.

Chi tiết deploy reference: [`deploy/README.md`](deploy/README.md).
