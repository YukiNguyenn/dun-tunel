#!/usr/bin/env bash
#
# Phase 1 smoke test (browser-profile-public-tunnel task 9.3).
#
# Walks the happy path on a freshly-deployed Edge_Server:
#   1. Health check `/healthz` returns region + active sessions.
#   2. Create a synthetic tunnel via `POST /v1/tunnels`.
#   3. Verify Caddy admin tree picked up the route.
#   4. Verify rathole config rendered the service entry.
#   5. Delete the tunnel and assert reverse cleanup.
#
# Inputs (env):
#   EDGE_HOST       — host to probe (default 127.0.0.1)
#   EDGE_PORT       — default 9443 (edge-control direct, NOT Caddy 8443).
#                     Smoke runs on the host loopback so we bypass Caddy
#                     and hit edge-control's plain-HTTP admin port. Caddy
#                     terminates TLS for external traffic.
#   DUN_API_KEY     — same key edge-control reads from env
#   CADDY_ADMIN_URL — default http://127.0.0.1:2019
#   REGION_ID       — default sin
#   SHARE_TUNNEL_DOMAIN — default dun-studio.xyz (must match Caddy +
#                          DNS + Cloudflare zone)
#
# All assertions print PASS/FAIL on stdout and exit non-zero on the
# first failure so the script is CI-friendly.

set -euo pipefail

EDGE_HOST="${EDGE_HOST:-127.0.0.1}"
EDGE_PORT="${EDGE_PORT:-9443}"
EDGE_BASE="http://${EDGE_HOST}:${EDGE_PORT}"
CADDY_ADMIN_URL="${CADDY_ADMIN_URL:-http://127.0.0.1:2019}"
REGION_ID="${REGION_ID:-sin}"
SHARE_TUNNEL_DOMAIN="${SHARE_TUNNEL_DOMAIN:-dun-studio.xyz}"
: "${DUN_API_KEY:?DUN_API_KEY env var required}"

SESSION_ID="smoke-$(date +%s)-$$"
SUBDOMAIN="${SESSION_ID}.${REGION_ID}.${SHARE_TUNNEL_DOMAIN}"
ROUTE_ID="dun-tunel-$(echo "$SUBDOMAIN" | tr '.' '_')"
TOKEN_HASH="0000000000000000000000000000000000000000000000000000000000000000"

cleanup() {
    # best-effort teardown so a failed run doesn't leak state.
    curl -fs -X DELETE \
        -H "x-edge-api-key: ${DUN_API_KEY}" \
        "${EDGE_BASE}/v1/tunnels/${SESSION_ID}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

pass() { printf '  \033[32mPASS\033[0m %s\n' "$1"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; exit 1; }
step() { printf '\n>> %s\n' "$1"; }

# ── 1. Health ───────────────────────────────────────────────────────
step "1. healthz"
HEALTH=$(curl -fsS "${EDGE_BASE}/healthz" || true)
[[ -n "$HEALTH" ]] || fail "healthz did not respond"
echo "$HEALTH" | grep -q '"region"' || fail "healthz missing 'region' field: $HEALTH"
pass "healthz responding ($HEALTH)"

# ── 2. Provision ────────────────────────────────────────────────────
step "2. POST /v1/tunnels (provision $SESSION_ID)"
PROVISION_BODY=$(cat <<JSON
{
    "sessionId": "${SESSION_ID}",
    "subdomain": "${SUBDOMAIN}",
    "tunnelTokenHash": "${TOKEN_HASH}",
    "viewerTokenHash": "${TOKEN_HASH}",
    "codecs": [
        { "kind": "video", "mimeType": "video/VP8", "clockRate": 90000 },
        { "kind": "audio", "mimeType": "audio/opus", "clockRate": 48000, "channels": 2 }
    ],
    "expiresAt": "$(date -u -d '+1 hour' +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -v+1H +%Y-%m-%dT%H:%M:%SZ)"
}
JSON
)
PROVISION=$(curl -fsS -X POST \
    -H "Content-Type: application/json" \
    -H "x-edge-api-key: ${DUN_API_KEY}" \
    -d "$PROVISION_BODY" \
    "${EDGE_BASE}/v1/tunnels")
echo "$PROVISION" | grep -q '"localUpstreamPort"' || fail "missing localUpstreamPort in $PROVISION"
pass "provisioned ($PROVISION)"

# ── 3. Caddy admin ──────────────────────────────────────────────────
step "3. Caddy admin /id/${ROUTE_ID}"
CADDY_ROUTE=$(curl -fsS "${CADDY_ADMIN_URL}/id/${ROUTE_ID}" || true)
[[ -n "$CADDY_ROUTE" ]] || fail "Caddy did not register route for ${ROUTE_ID}"
echo "$CADDY_ROUTE" | grep -q "$SUBDOMAIN" || fail "Caddy route doesn't mention subdomain"
pass "Caddy route present"

# ── 4. Rathole config ───────────────────────────────────────────────
step "4. rathole config contains service.${SESSION_ID}"
if [[ -r /etc/rathole/server.toml ]]; then
    grep -q "services.\"\?${SESSION_ID}\"\?" /etc/rathole/server.toml \
        && pass "rathole service entry present" \
        || fail "rathole config missing service.${SESSION_ID}"
else
    echo "  SKIP rathole config not readable from this host"
fi

# ── 5. Deprovision ──────────────────────────────────────────────────
step "5. DELETE /v1/tunnels/${SESSION_ID}"
curl -fsS -X DELETE \
    -H "x-edge-api-key: ${DUN_API_KEY}" \
    -o /dev/null -w '%{http_code}' \
    "${EDGE_BASE}/v1/tunnels/${SESSION_ID}" | grep -q '^\(200\|204\)$' \
    || fail "deprovision returned non-2xx"
pass "deprovision returned 2xx"

# Caddy route should be gone (404). Test this two ways to defend
# against a regression that surfaced earlier: the deprovision handler
# previously called `caddy.remove_route(session_id)` instead of
# `remove_route(subdomain)`, which made Caddy answer 404 for the
# WRONG @id while the real route entry stayed in the config tree.
#   - Probe 1: official @id derived from subdomain (must be 404).
#   - Probe 2: scan Caddy's automation domain list — the subdomain
#     should NOT appear after delete. Catches stuck routes regardless
#     of @id name.
sleep 1
CADDY_AFTER=$(curl -s -o /dev/null -w '%{http_code}' "${CADDY_ADMIN_URL}/id/${ROUTE_ID}")
[[ "$CADDY_AFTER" == "404" ]] || fail "Caddy still has route after delete (status=$CADDY_AFTER, id=${ROUTE_ID})"

DOMAIN_LIST=$(curl -fsS "${CADDY_ADMIN_URL}/config/apps/tls/automation/policies" 2>/dev/null || echo "")
if echo "$DOMAIN_LIST" | grep -q "\"${SUBDOMAIN}\""; then
    fail "Caddy still tracks ${SUBDOMAIN} in tls.automation.policies after delete"
fi
pass "Caddy route cleaned (id 404 + subdomain not in automation list)"

echo
echo "All smoke tests passed."
