# Custom Caddy image với module cloudflare DNS cho DNS-01 challenge.
# Stock `caddy:2-alpine` không có plugin này.
#
# Build: docker compose build caddy

FROM caddy:2-builder-alpine AS builder
RUN xcaddy build \
    --with github.com/caddy-dns/cloudflare

FROM caddy:2-alpine
COPY --from=builder /usr/bin/caddy /usr/bin/caddy
