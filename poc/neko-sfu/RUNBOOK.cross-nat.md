# Cross-NAT PoC Runbook — Direct UDP từ Owner Neko → Edge mediasoup

> Bằng chứng pixel video từ owner Neko đến browser viewer khác mạng,
> qua public internet, không qua rathole tunnel.

## Topology

```
┌──────────────────────┐   public internet (UDP)   ┌──────────────────────┐
│  Owner máy remote    │                           │  Edge VPS — SIN      │
│  (sau NAT 192.168.x) │                           │  (public IPv4)       │
│                      │                           │                      │
│  poc-owner-neko      │── direct UDP RTP :5004 ──▶│  poc-edge-sfu        │
│  Neko firefox        │                           │  PlainTransport      │
│  GStreamer pipeline  │                           │  comedia=true        │
│  udpsink host=<vps>  │                           │  bind 0.0.0.0:5004   │
│                      │                           │                      │
└──────────────────────┘                           │  poc-edge-viewer     │
                                                   │  http :8090          │
        Browser viewer cá nhân ─────HTTP/WS────────▶  http :4443          │
        (mạng khác bất kỳ)         WebRTC ICE/SRTP   (UDP 40000-40100)    │
                                                   └──────────────────────┘
```

---

## Bước 1 — Edge VPS (SIN) firewall

SSH vào VPS SIN. Verify ports đã mở. Chạy:

```bash
# Public ports cần thiết cho PoC
sudo ufw allow 4443/tcp comment 'PoC SFU signalling'
sudo ufw allow 8090/tcp comment 'PoC viewer page'
sudo ufw allow 5004/udp comment 'PoC plain RTP from owner'
sudo ufw allow 5005/udp comment 'PoC plain RTCP'
sudo ufw allow 40000:40100/udp comment 'PoC viewer WebRtcTransport'
sudo ufw status numbered
```

> Nếu provider có firewall ngoài UFW (DigitalOcean cloud firewall,
> Hetzner cloud, AWS SG…), cũng phải mở những port này tương đương.

Verify từ máy ngoài có thể chạm port:

```bash
nc -zv <vps_public_ip> 4443     # phải success
nc -zuv <vps_public_ip> 5004    # UDP success (vẫn báo open vì OS chưa drop)
```

---

## Bước 2 — Edge VPS deploy SFU + viewer

Trên VPS, cd vào repo dun-tunel. Lần đầu cần copy folder PoC sang VPS:

```bash
# Trên máy local (máy bạn đang code) — sync folder PoC sang VPS:
rsync -avz --exclude='target/' --exclude='node_modules/' \
  ./dun-tunel/poc/neko-sfu/ \
  user@<vps_ip>:/home/user/poc-neko-sfu/
```

SSH vào VPS rồi:

```bash
cd /home/user/poc-neko-sfu

# Set SFU_PUBLIC_IP = chính IPv4 public của VPS này.
# Lấy IP với: curl -4 ifconfig.me
export SFU_PUBLIC_IP=<vps_public_ip>

# Build + start. Lần đầu mất ~3-5 phút build mediasoup-sys.
docker compose -f docker-compose.edge.yml up -d --build

# Verify SFU listening:
docker logs poc-edge-sfu 2>&1 | grep "PoC SFU listening"
# Phải thấy: "PoC SFU listening :4443 | announced_ip=<vps_public_ip> | ..."

# Verify viewer page reach được:
curl -I http://localhost:8090
# 200 OK
```

Test signalling từ browser cá nhân của bạn:

```
mở trình duyệt → http://<vps_public_ip>:8090
trang load OK, có ô input + button "connect & consume"
```

---

## Bước 3 — Máy owner remote (sau NAT) — chạy Neko

> Owner có thể là chính máy local của bạn HOẶC máy remote khác. Quan
> trọng nhất: máy đó **không phải** edge VPS. Mạng khác hoàn toàn.

Sync folder PoC sang máy owner (nếu chưa có):

```bash
# từ máy local
rsync -avz --exclude='target/' --exclude='node_modules/' \
  ./dun-tunel/poc/neko-sfu/ \
  user@<owner_ip>:/home/user/poc-neko-sfu/
```

SSH vào máy owner, chmod entrypoint + start:

```bash
cd /home/user/poc-neko-sfu
chmod +x neko-cross-nat-entrypoint.sh

# CRITICAL: phải là IP của EDGE VPS, không phải IP của máy owner.
export SFU_PUBLIC_IP=<vps_public_ip>

docker compose -f docker-compose.owner.yml up -d --build

# Verify pipeline được render đúng:
docker logs poc-owner-neko 2>&1 | grep "udpsink target line"
# Phải thấy: "udpsink host=<vps_public_ip> port=5004 ..."

# Verify Neko bind localhost (cho :8080 UI). Mở browser owner:
#   http://localhost:8080  → login neko/neko
# IMPORTANT: phải mở browser :8080 và CLICK VÀO PAGE để Neko start
# pipeline. Neko v3 không emit RTP cho đến khi có ít nhất 1 native
# WebRTC peer. Đây là quirk của Neko, đã note trong RESULTS.md PoC.
```

---

## Bước 4 — Verify RTP có chạm edge SFU không

Trên VPS edge, theo dõi log producer stats:

```bash
docker logs -f poc-edge-sfu 2>&1 | grep -E "plain producer (stats|score|id)"
```

Trong vòng ~3-5 giây sau khi click vào Neko UI :8080 ở máy owner,
phải thấy log dạng:

```
plain transport listening rtp=:5004 rtcp=:5005
plain producer id=<uuid> kind=video
plain producer stats: RtpStreamRecv { ssrc: 22222222, kind: Video, ... }
plain producer score update: ssrc=22222222 score=10
```

Nếu **không** thấy stats:

| Symptom | Likely cause | Fix |
|---|---|---|
| Stats luôn `(no streams seen yet)` | Owner Neko chưa fire pipeline | Mở browser :8080 trên máy owner và click vào page |
| Stats vẫn rỗng sau 30s click | UDP packet không tới VPS | `tcpdump -i any -nn 'udp port 5004'` trên VPS |
| `tcpdump` thấy packet nhưng score=0 | RTP format sai | Verify `ssrc=22222222 pt=96` trong log GStreamer |

`tcpdump` sniff trên VPS để confirm UDP đến từ public internet:

```bash
sudo tcpdump -i any -nn 'udp port 5004' -c 30
# Phải thấy nguồn IP = NAT công cộng của owner network
# (KHÔNG phải private 192.168.x.x — NAT đã rewrite source)
```

---

## Bước 5 — Test viewer cross-NAT

Trên **một máy khác hoàn toàn** (laptop bạn ở mạng khác, hoặc điện thoại 4G):

1. Mở `http://<vps_public_ip>:8090`
2. Click `connect & consume`.
3. Quan sát log trên page — phải thấy chuỗi:
   ```
   fetching plain producer from http://127.0.0.1:4443/v1/plain-producer
   ```
   Đây là **bug cosmetic** — input default là 127.0.0.1. Sửa bằng cách
   thay value trong textbox thành `http://<vps_public_ip>:4443` rồi
   reload + click lại.
4. Sau khi WS connect:
   ```
   ws open
   server Init: routerCaps codecs=2
   device loaded
   sent Init with rtpCapabilities
   requesting Consume(producerId=...)
   consumed video id=...
   pc ice → checking
   pc ice → connected
   inbound-rtp Δbytes=... Δpackets=... total=...
   ```
5. Video element phải chạy stream của Neko desktop owner.

---

## PASS criteria

- ✅ Producer stats trên edge SFU thấy `total packets > 0` trong 5s sau khi
  Neko UI :8080 được click ở owner.
- ✅ `tcpdump` xác nhận UDP packet vào port 5004 từ public internet.
- ✅ Browser viewer cross-NAT thấy video Neko desktop.
- ✅ `inbound-rtp` counter trong viewer page tăng đều, drop rate < 1%.

## FAIL fallback

Nếu owner gửi UDP nhưng edge không nhận được (firewall, ISP block, NAT
quá khắt khe), thử trong order:

1. UDP probe ngược chiều: trên VPS chạy `nc -ul 5004`, máy owner
   `echo hello | nc -u <vps_ip> 5004` → nếu nhận được, vấn đề ở
   Neko pipeline, không phải network.
2. Đổi port 5004 → 5006: một số ISP residential block port chẵn dưới
   1024 + một vài port magic.
3. Nếu fail tất cả: ISP block UDP outbound — fallback sang TURN
   relay (coturn deploy + force ICE qua TURN). Đó là Phase 3+.

---

## Cleanup

VPS:

```bash
docker compose -f docker-compose.edge.yml down -v
sudo ufw delete allow 4443/tcp
sudo ufw delete allow 8090/tcp
sudo ufw delete allow 5004/udp
sudo ufw delete allow 5005/udp
sudo ufw delete allow 40000:40100/udp
```

Owner:

```bash
docker compose -f docker-compose.owner.yml down -v
```

---

## Sau khi PASS

Báo lại tôi kết quả. Tùy kết quả, tôi sẽ:

- **PASS** → wire `mediaEndpoint{host, port, payloadType, ssrc}` vào
  - `edge-shared::CreateSessionResp`
  - `dun-api EdgeClient` propagate
  - dun-app `ShareTunnelService` cache + truyền vào dun-browser
  - dun-browser: tách neko-config thành template per-share-session,
    udpsink host = edge public IP của region đã pick.
  - Mở firewall `udp/5000-9999` + `udp/40000-60000` ở edge production.

- **FAIL** → diagnose UDP path cụ thể, có thể cần TURN relay từ ngày
  một thay vì defer Phase 3.
