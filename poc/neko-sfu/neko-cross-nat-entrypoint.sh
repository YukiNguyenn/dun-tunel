#!/bin/sh
# entrypoint cho `docker-compose.owner.yml` — render placeholder
# `__SFU_PUBLIC_IP__` trong neko-config.cross-nat.yaml.tpl thành IP thật,
# sau đó exec sang neko binary của upstream image.
#
# Tại sao cần script này: Docker compose `${VAR}` substitution chỉ
# áp dụng ở compose file, KHÔNG render placeholders trong volume-mount
# YAML. Neko đọc gst_pipeline literal nên ta cần substitute trước.

set -eu

if [ -z "${SFU_PUBLIC_IP:-}" ]; then
  echo "FATAL: SFU_PUBLIC_IP env not set — cross-NAT PoC needs the edge VPS public IPv4." >&2
  exit 1
fi

# Validate IPv4 format minimum để tránh đẩy junk vào pipeline.
case "$SFU_PUBLIC_IP" in
  *.*.*.*) : ;;
  *) echo "FATAL: SFU_PUBLIC_IP='$SFU_PUBLIC_IP' không phải IPv4 hợp lệ." >&2; exit 1 ;;
esac

TPL=/etc/neko/neko.yaml.tpl
OUT=/etc/neko/neko.yaml

if [ ! -f "$TPL" ]; then
  echo "FATAL: template $TPL không tồn tại trong container." >&2
  exit 1
fi

# `sed` đơn giản, không leo escape vì IPv4 không chứa ký tự đặc biệt.
sed "s/__SFU_PUBLIC_IP__/${SFU_PUBLIC_IP}/g" "$TPL" > "$OUT"

echo "[entrypoint] rendered neko config with SFU_PUBLIC_IP=${SFU_PUBLIC_IP}"
echo "[entrypoint] gst pipeline udpsink target line:"
grep "udpsink" "$OUT" || true

# Hand off cho neko binary của upstream Docker image.
# Upstream m1k1o/neko entrypoint là /usr/bin/neko với args lấy từ
# config qua --config flag được set qua NEKO_CONFIG env (= $OUT).
exec /usr/bin/neko serve --config "$OUT"
