#!/usr/bin/env bash
# Render Caddyfile.tpl → Caddyfile bằng cách thay {{REGION}} +
# {{DOMAIN}} placeholders. Tpl không dùng cú pháp `${VAR}` nên
# `envsubst` không phù hợp; dùng `sed` để khớp `{{...}}` literal.
#
# Usage:
#   REGION_ID=sin SHARE_TUNNEL_DOMAIN=dun-studio.xyz \
#     ./render-caddyfile.sh
#
# Hoặc đọc từ deploy/.env:
#   set -a; source .env; set +a; ./render-caddyfile.sh
#
# Output: ghi đè ./Caddyfile cùng thư mục với script.

set -euo pipefail

: "${REGION_ID:?REGION_ID env var required (e.g. sin)}"
: "${SHARE_TUNNEL_DOMAIN:?SHARE_TUNNEL_DOMAIN env var required (e.g. dun-studio.xyz)}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEPLOY_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TPL="$DEPLOY_DIR/Caddyfile.tpl"
OUT="$DEPLOY_DIR/Caddyfile"

if [[ ! -f "$TPL" ]]; then
    echo "ERR: template missing at $TPL" >&2
    exit 1
fi

# Escape `/` and `&` and `\` trong giá trị thay thế để sed không bị
# nhầm là delimiter.
escape_sed() {
    printf '%s' "$1" | sed -e 's/[\/&]/\\&/g'
}

REGION_ESC=$(escape_sed "$REGION_ID")
DOMAIN_ESC=$(escape_sed "$SHARE_TUNNEL_DOMAIN")

sed \
    -e "s/{{REGION}}/$REGION_ESC/g" \
    -e "s/{{DOMAIN}}/$DOMAIN_ESC/g" \
    "$TPL" > "$OUT"

# Sanity check — tpl không còn `{{` placeholder nào.
if grep -q '{{' "$OUT"; then
    echo "ERR: rendered Caddyfile still contains {{...}} placeholders" >&2
    grep -n '{{' "$OUT" >&2 || true
    exit 1
fi

echo "Wrote $OUT (REGION=$REGION_ID DOMAIN=$SHARE_TUNNEL_DOMAIN)"
