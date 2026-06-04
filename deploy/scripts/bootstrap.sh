#!/usr/bin/env bash
# Bootstrap script for a fresh Edge_Server VPS (Debian/Ubuntu).
# Usage: REGION_ID=sin ./bootstrap.sh

set -euo pipefail

: "${REGION_ID:?REGION_ID env var required}"

echo "==> Updating apt"
sudo apt-get update -qq
sudo apt-get install -y --no-install-recommends \
    ca-certificates curl gnupg lsb-release ufw

echo "==> Installing Docker (official)"
if ! command -v docker >/dev/null; then
    curl -fsSL https://get.docker.com | sudo sh
fi

echo "==> Creating directories"
sudo mkdir -p /etc/dun-tunel /etc/rathole /var/lib/dun-tunel/queue
sudo chown -R "$USER:$USER" /var/lib/dun-tunel

echo "==> Configuring firewall"
sudo ufw allow 22/tcp                # SSH
sudo ufw allow 443/tcp               # Caddy HTTPS
sudo ufw allow 2333/tcp              # Rathole control
sudo ufw allow 5000:9999/udp         # mediasoup PlainTransport (Neko ingest, per-session)
sudo ufw allow 50000:60000/udp       # mediasoup RTP (viewer Consumer transports)
sudo ufw --force enable

echo "==> Copy Caddyfile and rathole config templates"
envsubst < /etc/dun-tunel/Caddyfile.tpl > /etc/dun-tunel/Caddyfile

echo "==> Done. Set env in /etc/dun-tunel/edge-control.env then:"
echo "    docker compose -f /etc/dun-tunel/docker-compose.yml up -d"
