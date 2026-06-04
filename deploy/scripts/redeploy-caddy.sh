#!/usr/bin/env bash
# Apply Caddyfile.tpl + edge-control TLS/route bootstrap end-to-end.
#
# Run sau khi sửa Caddyfile.tpl hoặc env vars (REGION_ID,
# SHARE_TUNNEL_DOMAIN, CLOUDFLARE_API_TOKEN). Tự động:
#   1. Render Caddyfile từ tpl với placeholders thật.
#   2. Reload Caddy để load Caddyfile mới (wipes admin-API-only state).
#   3. Restart edge-control để re-bootstrap wildcard TLS policy +
#      edge.<region>.<domain> route qua Caddy admin API.
#   4. Verify route + policy đã có.
#
# Usage (từ deploy/):
#   ./scripts/redeploy-caddy.sh
#
# Yêu cầu: file `.env` cạnh `docker-compose.yml` chứa REGION_ID +
# SHARE_TUNNEL_DOMAIN + CLOUDFLARE_API_TOKEN.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEPLOY_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ENV_FILE="$DEPLOY_DIR/.env"

if [[ ! -f "$ENV_FILE" ]]; then
    echo "ERR: $ENV_FILE not found" >&2
    exit 1
fi

# Load .env vào current shell.
set -a
# shellcheck disable=SC1090
source "$ENV_FILE"
set +a

echo "==> [1/4] Rendering Caddyfile from template"
"$SCRIPT_DIR/render-caddyfile.sh"

echo "==> [2/4] Reloading Caddy"
docker exec caddy caddy reload --config /etc/caddy/Caddyfile

echo "==> [3/4] Restarting edge-control (re-bootstraps TLS policy + edge route)"
docker compose -f "$DEPLOY_DIR/docker-compose.yml" --env-file "$ENV_FILE" \
    restart edge-control

# Wait a few seconds for edge-control to finish bootstrap.
sleep 4

echo "==> [4/4] Verifying Caddy admin state"

ROUTES=$(docker exec caddy curl -s http://127.0.0.1:2019/config/apps/http/servers/srv0/routes)
echo "Routes:"
echo "$ROUTES" | python3 -c "
import sys, json
try:
    arr = json.load(sys.stdin)
except Exception as e:
    print('  (parse error:', e, ')')
    sys.exit(1)
for i, r in enumerate(arr):
    if not r:
        continue
    host = r.get('match', [{}])[0].get('host', ['?'])
    print(f'  {i}: {host}')
"

POLICIES=$(docker exec caddy curl -s http://127.0.0.1:2019/config/apps/tls/automation/policies)
echo "TLS automation policies:"
echo "$POLICIES" | python3 -c "
import sys, json
try:
    arr = json.load(sys.stdin)
except Exception as e:
    print('  (parse error:', e, ')')
    sys.exit(1)
for i, p in enumerate(arr):
    print(f'  {i}: subjects={p.get(\"subjects\")}')
"

echo
echo "Done. Tạo share session mới để test viewer URL trên port"
echo "\${CADDY_PUBLIC_PORT:-8443}. Wildcard cert sẽ được issue qua DNS-01"
echo "challenge (Cloudflare) khi request đầu tiên cho subdomain mới đến."
