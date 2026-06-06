# Hướng dẫn cài đặt máy Owner (Ubuntu 22.04)

> Mục đích: chạy `docker-compose.owner.yml` trên máy cá nhân của bạn
> để verify cross-NAT direct UDP đến edge VPS SIN.

## Yêu cầu

- Ubuntu 22.04 LTS
- Có sudo
- Public IP **khác** với edge VPS (`58.187.17.128`). Verify trước:
  ```bash
  curl -s https://api.ipify.org && echo
  ```
  Nếu output ≠ `58.187.17.128` → OK, nếu trùng → máy này cùng NAT
  với VPS, không phải cross-NAT thật.

---

## Bước 1 — Cài Docker Engine + Compose

Chạy nguyên block sau (copy-paste):

```bash
sudo apt-get update
sudo apt-get install -y ca-certificates curl gnupg lsb-release rsync

# Docker apt repo (official)
sudo install -m 0755 -d /etc/apt/keyrings
curl -fsSL https://download.docker.com/linux/ubuntu/gpg | \
  sudo gpg --dearmor -o /etc/apt/keyrings/docker.gpg
sudo chmod a+r /etc/apt/keyrings/docker.gpg

echo \
  "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] \
  https://download.docker.com/linux/ubuntu \
  $(. /etc/os-release && echo "$VERSION_CODENAME") stable" | \
  sudo tee /etc/apt/sources.list.d/docker.list > /dev/null

sudo apt-get update
sudo apt-get install -y docker-ce docker-ce-cli containerd.io \
  docker-buildx-plugin docker-compose-plugin

# Cho user hiện tại chạy docker không cần sudo
sudo usermod -aG docker "$USER"

# Verify
sudo docker --version
sudo docker compose version
```

**Logout/login lại** (hoặc reboot) để group `docker` có hiệu lực, rồi
verify không cần sudo:

```bash
docker ps
```

---

## Bước 2 — Sync folder PoC từ máy local Windows sang Ubuntu

Trên **máy local Windows** (máy đang code), chạy PowerShell:

```powershell
# Thay <ubuntu_user>@<ubuntu_ip> bằng credential thật của Ubuntu owner
$dest = "<ubuntu_user>@<ubuntu_ip>:~/poc-neko-sfu/"

# Cần có rsync hoặc scp trên Windows (Git Bash, WSL hoặc OpenSSH client)
# Nếu chưa có rsync:
#   - Cài Git for Windows → có rsync trong git-bash
#   - Hoặc dùng scp -r

scp -r `
  c:\Users\Admin\Documents\dun-studio\dun-tunel\poc\neko-sfu\* `
  ${dest}
```

Hoặc nếu có rsync (Git Bash / WSL):

```bash
rsync -avz --exclude='target/' --exclude='node_modules/' \
  /c/Users/Admin/Documents/dun-studio/dun-tunel/poc/neko-sfu/ \
  <ubuntu_user>@<ubuntu_ip>:~/poc-neko-sfu/
```

Verify trên máy Ubuntu:

```bash
ls ~/poc-neko-sfu/
# Phải thấy: docker-compose.owner.yml, neko-config.cross-nat.yaml,
# neko-cross-nat-entrypoint.sh, ...
```

---

## Bước 3 — Chmod + fix line ending

Trên máy Ubuntu owner (file gửi từ Windows có thể có CRLF):

```bash
cd ~/poc-neko-sfu

sudo apt-get install -y dos2unix
chmod +x start-owner.sh neko-cross-nat-entrypoint.sh
dos2unix start-owner.sh neko-cross-nat-entrypoint.sh neko-config.cross-nat.yaml
```

---

## Bước 4 — Up Neko owner stack

Dùng script wrapper render config trước khi up (tránh race Xorg):

```bash
cd ~/poc-neko-sfu
chmod +x start-owner.sh
dos2unix start-owner.sh

# IP của EDGE VPS, không phải IP máy này
./start-owner.sh 58.187.17.128
```

Nếu script báo OK, verify:

```bash
docker ps --filter name=poc-owner-neko
# Phải thấy: Up X seconds (healthy)

docker logs poc-owner-neko 2>&1 | grep -E "udpsink|panic|unable to open display" | head -10
# Phải thấy: rtpvp8pay ... udpsink host=58.187.17.128 port=50004
# KHÔNG được thấy "unable to open display"
```

---

## Bước 5 — Trigger Neko emit RTP

Neko v3 lazy-start GStreamer pipeline — chỉ phát RTP khi có ít nhất 1
WebRTC peer connect vào port 8080 native UI.

Trên **chính máy Ubuntu owner**:

```bash
# Kiểm tra port 8080 listen
ss -ltnp | grep 8080
# Phải có: LISTEN 0.0.0.0:8080
```

Mở browser local của Ubuntu (Firefox / Chrome) truy cập:

```
http://localhost:8080
```

- Login: user `neko`, password `neko`
- **Click vào page Firefox** trong Neko (mở 1 tab, nhập URL bất kỳ).
  Đây là bước trigger pipeline `creating pipeline` → emit RTP UDP.

---

## Bước 6 — Verify trên VPS edge

SSH vào VPS, chạy 2 lệnh sau:

```bash
# A) tcpdump nhận UDP port 50004 trong 10s
sudo timeout 10 tcpdump -i any -nn 'udp port 50004' -c 30

# B) Check mediasoup producer stats
docker logs poc-edge-sfu --tail 30 2>&1 | grep -E "plain|score"
```

**Pass criteria** trong 5-10 giây:
- tcpdump phải thấy nhiều dòng dạng:
  ```
  IP <ubuntu_owner_public_ip>.<random> > 192.168.20.6.50004: UDP, length 1200
  ```
- mediasoup log phải có `plain producer score update: ssrc=22222222 score=10`
  thay vì `(no streams seen yet)`

Nếu producer score > 0 → **PoC PASS** — direct UDP cross-NAT đã chứng minh.

---

## Bước 7 — Test viewer cross-NAT

Trên **máy thứ 3** (laptop ở mạng khác, hoặc 4G phone):

1. Browser → `http://58.187.17.128:8091`
2. Đổi value textbox SFU URL: `http://58.187.17.128:4443`
3. Click `connect & consume`
4. Quan sát log page:
   ```
   ws open
   server Init: routerCaps codecs=2
   pc ice → connected
   inbound-rtp Δbytes=... Δpackets=...
   ```
5. Video element phải hiện desktop Firefox của owner Ubuntu

---

## Cleanup khi xong

Trên Ubuntu owner:

```bash
cd ~/poc-neko-sfu
docker compose -f docker-compose.owner.yml down -v
```

Trên VPS edge giữ nguyên cho lần test sau, hoặc:

```bash
cd ~/dun-tunel/poc/neko-sfu
docker compose -f docker-compose.edge.yml down -v
```

---

## Troubleshoot nhanh

| Triệu chứng | Cause | Fix |
|---|---|---|
| `docker compose` báo command not found | docker-compose-plugin chưa cài | `sudo apt-get install -y docker-compose-plugin` |
| `permission denied` khi chạy `docker ps` | User chưa vào group docker | logout/login lại sau khi `usermod -aG docker` |
| Container restart loop | CRLF trong entrypoint script | `dos2unix neko-cross-nat-entrypoint.sh` |
| Container log `panic: unable to open display` | Override entrypoint sai | Pull lại file `docker-compose.owner.yml` mới nhất (entrypoint là `["sh", "/neko-cross-nat-entrypoint.sh"]`) |
| `:8080` không load | Neko health check chưa pass, đợi 30-60s | `docker ps` xem trạng thái healthy |
| tcpdump VPS không thấy gì | Public IP owner trùng VPS, hairpin NAT block | `curl ifconfig.me` so sánh; nếu trùng đổi máy khác |
