# Per-region wildcard cert via Cloudflare DNS-01.
# Replace {{REGION}} at deploy time (sin / iad / fra).

{
    admin localhost:2019
    log {
        output stdout
        format json
    }
    # Phase 4 task 22.2 — per-IP rate limiter for the reverse proxy
    # path. Caddy's `caddy-ratelimit` module (third-party) implements a
    # token-bucket refill against an in-memory key derived from
    # `client_ip`. 60 requests / minute / IP is the spec ceiling
    # (R13.2); above that Caddy returns 429 directly without ever
    # touching the upstream rathole connection.
    #
    # Module install:
    #   xcaddy build --with github.com/mholt/caddy-ratelimit
    # then point the `caddy.service` ExecStart at the new binary.
    order rate_limit before reverse_proxy
}

(rate_limit_per_ip) {
    rate_limit {
        zone share_tunnel_per_ip {
            key {client_ip}
            events 60
            window 60s
        }
    }
}

*.{{REGION}}.share.dun.app {
    tls {
        dns cloudflare {env.CLOUDFLARE_API_TOKEN}
    }

    # Apply the rate-limit snippet to every request. Caddy evaluates
    # the snippet inline; the `rate_limit` directive populates the
    # response with `Retry-After` headers when the bucket empties.
    import rate_limit_per_ip

    @ws {
        path /api/ws /webrtc
    }

    # Routes are added dynamically by edge-caddy-bridge via admin API.
    # No static @id-matched route here — defer to admin PATCH.

    # Default catch-all: 404 if no dynamic route matches the host.
    respond 404
}
