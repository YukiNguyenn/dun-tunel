# Phase 1 Caddyfile — single-VPS topology (dun-api + edge-control +
# Caddy + rathole cùng host).
#
# Replace placeholders at deploy time:
#   {{REGION}}  →  sin / iad / fra
#   {{DOMAIN}}  →  dun-studio.xyz
#
# Caddy listens on :8443 (provider không mở 443) và demux 3 hostname
# về 3 upstream nội bộ:
#   api.<domain>            → 127.0.0.1:3010 (dun-api)
#   edge.<region>.<domain>  → 127.0.0.1:9443 (edge-control admin)
#   *.<region>.<domain>     → dynamic routes (rathole upstream do
#                              edge-caddy-bridge add qua admin API)
#
# Phase 4 sẽ rebuild Caddy với `mholt/caddy-ratelimit` module để
# enforce R13.2 (60 req/IP/min). Phase 1 dùng image stock — không có
# rate_limit directive.

{
    admin 127.0.0.1:2019
    log {
        output stdout
        format json
    }
}

# ─── dun-api public ──────────────────────────────────────────────
api.{{DOMAIN}}:8443 {
    tls {
        dns cloudflare {env.CLOUDFLARE_API_TOKEN}
    }
    reverse_proxy 127.0.0.1:3010 {
        header_up X-Real-IP {remote_host}
        header_up X-Forwarded-For {remote_host}
        header_up X-Forwarded-Proto {scheme}
    }
}

# ─── edge-control admin (chỉ dun-api → edge calls) ──────────────
# KHÔNG khai báo `tls { dns }` riêng — Caddy tự match wildcard cert
# `*.sin.{{DOMAIN}}` đã obtain ở site block dưới. Tránh obtain cert
# riêng cho edge.sin (thừa + tốn ACME quota).
edge.{{REGION}}.{{DOMAIN}}:8443 {
    tls {
        dns cloudflare {env.CLOUDFLARE_API_TOKEN}
    }
    reverse_proxy 127.0.0.1:9443
}

# ─── viewer wildcard — dynamic routes injected by edge-caddy-bridge ─
*.{{REGION}}.{{DOMAIN}}:8443 {
    tls {
        dns cloudflare {env.CLOUDFLARE_API_TOKEN}
    }

    # edge-caddy-bridge POST các route entry vào array `routes` của
    # server `srv0` qua admin API (path `/config/apps/http/servers/srv0/routes/...`).
    # Routes match theo Host (subdomain) → reverse_proxy upstream
    # `127.0.0.1:<dynamic-port>` mà rathole gán per session.
    #
    # Catch-all dưới đây chạy SAU dynamic routes (Caddy match theo
    # thứ tự routes array; static block trong Caddyfile được expand
    # thành route entries cuối array). Tunnel chưa có session →
    # match catch-all → 404.
    respond 404
}
