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

# ─── edge-control admin + wildcard cert ─────────────────────────
#
# We DO NOT declare an `edge.{{REGION}}.{{DOMAIN}}:8443 { ... }` site
# block here, even though we still need that hostname to reverse-
# proxy to 127.0.0.1:9443. Reason: any Caddyfile-generated `tls {}`
# block produces an automation policy whose subject is the explicit
# host (e.g. `edge.sin.dun-studio.xyz`). The wildcard policy we
# install via the admin API at edge-control startup uses subject
# `*.{{REGION}}.{{DOMAIN}}`, which also matches `edge.<region>` —
# Caddy refuses that overlap with `cannot apply more than one
# automation policy to host` and the entire config fails to load.
#
# So instead, edge-control posts BOTH the policy and the
# `edge.<region>.<domain>` HTTP route via the admin API at startup
# (`AdminClient::ensure_edge_admin_route` and
# `ensure_wildcard_tls_policy`). This gives us a single policy
# covering every `*.<region>.<domain>` cert (edge admin + every
# per-session viewer subdomain) with no overlap conflicts.
