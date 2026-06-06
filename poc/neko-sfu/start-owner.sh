#!/bin/sh
# Owner-side helper: render neko config với SFU public IP rồi up stack.
#
# Tại sao có script này: trước đó tôi override entrypoint container để
# render placeholder runtime, nhưng cách đó race với Xorg/supervisord
# init → "unable to open display :99" panic. Render trên host trước
# khi container start là cách sạch — supervisord upstream chạy
# nguyên bản, không cần custom entrypoint.

set -eu

if [ $# -lt 1 ]; then
  echo "Usage: $0 <edge_public_ip>" >&2
  echo "Example: $0 58.187.17.128" >&2
  exit 1
fi

SFU_PUBLIC_IP="$1"

# Validate IPv4 format minimum
case "$SFU_PUBLIC_IP" in
  *.*.*.*) : ;;
  *) echo "FATAL: '$SFU_PUBLIC_IP' không phải IPv4 hợp lệ." >&2; exit 1 ;;
esac

# CD vào folder chứa script (hỗ trợ chạy từ đường dẫn khác)
cd "$(dirname "$0")"

TPL=neko-config.cross-nat.yaml
OUT=.neko-rendered.yaml

if [ ! -f "$TPL" ]; then
  echo "FATAL: template $TPL không tồn tại trong cwd." >&2
  exit 1
fi

sed "s/__SFU_PUBLIC_IP__/${SFU_PUBLIC_IP}/g" "$TPL" > "$OUT"

echo "[start-owner] rendered $OUT with SFU_PUBLIC_IP=${SFU_PUBLIC_IP}"
echo "[start-owner] gst pipeline udpsink target line:"
grep "udpsink" "$OUT" || true
echo

# Down stack cũ nếu còn (sạch state)
docker compose -f docker-compose.owner.yml down -v --remove-orphans 2>/dev/null || true

# Up
SFU_PUBLIC_IP="$SFU_PUBLIC_IP" docker compose -f docker-compose.owner.yml up -d --build

echo
echo "[start-owner] container started. Verify:"
echo "  docker ps --filter name=poc-owner-neko"
echo "  docker logs -f poc-owner-neko"
echo
echo "[start-owner] then open browser → http://localhost:8080"
echo "  login neko/neko, click vào page để Neko start GStreamer pipeline."
