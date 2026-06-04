# Edge_Server deployment (Phase 1)

Per-region VPS stack for the public dun-browser tunnel feature.
Phase 1 deploys 1 region (SIN); Phase 3 adds IAD + FRA.

## Stack

| Component       | Process              | Port (host)        |
|-----------------|----------------------|--------------------|
| edge-control    | systemd / Docker     | 8443/tcp           |
| Caddy           | systemd / Docker     | 443/tcp + 2019/tcp (admin, localhost-only) |
| rathole server  | systemd / Docker     | 2333/tcp           |
| mediasoup RTP   | edge-control inproc  | 50000-60000/udp    |

## First-time setup

1. Provision a Debian 12 / Ubuntu 22.04 VPS, ≥ 4 GB RAM, ≥ 2 vCPU.
2. SSH in as root or a sudoer.
3. Clone this repo, copy `deploy/` to `/opt/dun-tunel/`.
4. Run `REGION_ID=sin /opt/dun-tunel/scripts/bootstrap.sh`.
5. Populate `/etc/dun-tunel/edge-control.env`:

   ```env
   REGION_ID=sin
   EDGE_BIND_PORT=8443
   DUN_API_ENDPOINT=https://api.dun-studio.xyz/api
   DUN_API_KEY=<24+ char shared secret>
   CADDY_ADMIN_URL=http://127.0.0.1:2019
   RATHOLE_CONFIG_PATH=/etc/rathole/server.toml
   RATHOLE_PID_FILE=/run/rathole/rathole.pid
   PERSISTENT_QUEUE_DIR=/var/lib/dun-tunel/queue
   TUNNEL_JWT_SECRET_V1=<base64-32-bytes>
   TUNNEL_JWT_SECRET_V2=<base64-32-bytes>
   RUST_LOG=info,edge_control=debug
   ```

6. Populate `/etc/dun-tunel/caddy.env`:

   ```env
   CLOUDFLARE_API_TOKEN=<scoped DNS:Edit token for dun-studio.xyz zone>
   ```

7. Render `Caddyfile.tpl` → `/etc/caddy/Caddyfile`:

   ```bash
   REGION=sin envsubst < /opt/dun-tunel/Caddyfile.tpl \
       > /etc/caddy/Caddyfile
   ```

8. Render `rathole.tpl.toml` → `/etc/rathole/server.toml`. Replace
   the `pkcs12` password placeholder with a real value generated via
   `openssl pkcs12 -export …` (cert chain for `tunnel.<region>.dun-studio.xyz`).

9. Install systemd units:

   ```bash
   sudo cp deploy/systemd/*.service /etc/systemd/system/
   sudo systemctl daemon-reload
   sudo systemctl enable --now caddy rathole edge-control
   ```

10. Verify the stack:

    ```bash
    DUN_API_KEY=… deploy/scripts/smoke-test.sh
    ```

## Cloudflare DNS

For a region `sin`:

| Record                          | Type | Value                |
|---------------------------------|------|----------------------|
| `api.dun-studio.xyz`            | A    | `<edge-vps-ip>`      |
| `edge.sin.dun-studio.xyz`       | A    | `<edge-vps-ip>`      |
| `*.sin.dun-studio.xyz`          | A    | `<edge-vps-ip>`      |
| `tunnel.sin.dun-studio.xyz`     | A    | `<edge-vps-ip>`      |

The wildcard A record covers viewer subdomains. The non-wildcard
`tunnel.<region>` host is what dun-app rathole clients dial; its
cert is provisioned via the same DNS-01 challenge.

## Update workflow

`edge-control` writes the rathole TOML atomically and triggers
SIGHUP — there is no need to restart the service for new tunnels.
For binary updates:

```bash
sudo systemctl restart edge-control
```

If the update is breaking, drain first:

```bash
# stop accepting new tunnels (admin)
curl -X POST -H "x-edge-api-key:$DUN_API_KEY" \
    https://api.dun-studio.xyz/admin/region/sin/drain
# wait until activeSessions == 0 (visible via /healthz)
sudo systemctl restart edge-control
```

## Smoke test

`deploy/scripts/smoke-test.sh` exercises the happy-path provision +
deprovision flow. Use it after every deploy and as a rolling health
check from the central monitoring host.

## Logs

All services emit JSON to stdout. Recommended journald query:

```bash
journalctl -u edge-control -u caddy -u rathole -f
```

Ship to Loki / Datadog / Honeycomb via vector.dev — see
`deploy/observability/` (Phase 3).
