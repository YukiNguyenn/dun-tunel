# Cross-NAT PoC Runbook — Direct UDP từ Owner Neko → Edge mediasoup

> Bằng chứng pixel video từ owner Neko đến browser viewer khác mạng,
> qua public internet, không qua rathole tunnel.

## Topology

```
┌──────────────────────┐   public internet (UDP)   ┌──────────────────────┐
│  Owner máy remote    │                           │  Edge VPS — SIN      │
│  (sau NAT 192.168.x) │                           │  (public IPv4)       │
│                      │                           │                      │
│  poc-owner-neko      │── direct UDP RTP :50004 ─▶│  poc-edge-sfu        │
│  Neko firefox        │                           │  PlainTransport      │
│  GStreamer pipeline  │                           │  comedia=true        │
│  udpsink host=<vps>  │                           │  bind 0.0.0.0:50004  │
│                      │                           │                      │
└──────────────────────┘                           │  poc-edge-viewer     │
                                                   │  http :8091          │
        Browser viewer cá nhân ─────HTTP/WS────────▶  http :4443          │
        (mạng khác bất kỳ)         WebRTC ICE/SRTP   (UDP 50100-60000)    │
                                                   └──────────────────────┘
```

---

## Bước 1 — Edge VPS (SIN) firewall

SSH vào VPS SIN. Verify ports đã mở. Chỉ cần 2 TCP port mới —
**dải UDP 50000-60000 đã được mở sẵn trên VPS**, plain RTP/RTCP
+ WebRtcTransport range đều nằm gọn trong dải đó.

```bash
# TCP — signalling + viewer page
sudo ufw allow 4443/tcp comment 'PoC SFU signalling'
sudo ufw allow 8091/tcp comment 'PoC viewer page'

# Verify dải UDP 50000-60000 đã mở (đã setup từ trước):
sudo ufw status numbered | grep -E '50000|60000|udp'

sudo ufw status numbered
```

> Nếu provider có firewall ngoài UFW (DigitalOcean cloud firewall,
> Hetzner cloud, AWS SG…), cũng phải mở những port này tương đương.

> **Lưu ý về `network_mode: host`**: SFU container chạy host
> network nên bind thẳng lên 4443/tcp và toàn bộ dải UDP 50100-60000
> + 50004/50005 trên host. Lý do: bridge mode + iptables NAT bind
> fail ngẫu nhiên khi range UDP overlap với Linux ephemeral port
> range mặc định 32768-60999 (process khác trên host vô tình chiếm
> port → Docker proxy bind fail). Production stack `dun-tunel/deploy`
> cũng dùng cùng pattern này, đã document trong `.env.example`.

Verify từ máy ngoài có thể chạm port:

```bash
nc -zv <vps_public_ip> 4443     # phải success
nc -zuv <vps_public_ip> 50004   # UDP success (vẫn báo open vì OS chưa drop)
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
curl -I http://localhost:8091
# 200 OK
```

Test signalling từ browser cá nhân của bạn:

```
mở trình duyệt → http://<vps_public_ip>:8091
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
# Phải thấy: "udpsink host=<vps_public_ip> port=50004 ..."

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
plain transport listening rtp=:50004 rtcp=:50005
plain producer id=<uuid> kind=video
plain producer stats: RtpStreamRecv { ssrc: 22222222, kind: Video, ... }
plain producer score update: ssrc=22222222 score=10
```

Nếu **không** thấy stats:

| Symptom | Likely cause | Fix |
|---|---|---|
| Stats luôn `(no streams seen yet)` | Owner Neko chưa fire pipeline | Mở browser :8080 trên máy owner và click vào page |
| Stats vẫn rỗng sau 30s click | UDP packet không tới VPS | `tcpdump -i any -nn 'udp port 50004'` trên VPS |
| `tcpdump` thấy packet nhưng score=0 | RTP format sai | Verify `ssrc=22222222 pt=96` trong log GStreamer |

`tcpdump` sniff trên VPS để confirm UDP đến từ public internet:

```bash
sudo tcpdump -i any -nn 'udp port 50004' -c 30
# Phải thấy nguồn IP = NAT công cộng của owner network
# (KHÔNG phải private 192.168.x.x — NAT đã rewrite source)
```

---

## Bước 5 — Test viewer cross-NAT

Trên **một máy khác hoàn toàn** (laptop bạn ở mạng khác, hoặc điện thoại 4G):

1. Mở `http://<vps_public_ip>:8091`
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

### A. Owner và edge cùng public IP (hairpin NAT issue)

Một loop xảy ra khi máy owner và VPS edge có cùng public IP (ví dụ
chung 1 cloud provider, chung 1 NAT pool, hoặc đang VPN). Symptom:
- `curl ifconfig.me` trên owner và VPS trả cùng IP
- Owner gửi UDP đến `<edge_public_ip>:50004` nhưng tcpdump VPS không
  thấy packet (kernel hoặc router chặn route loop)
- Nhưng TCP 4443 vẫn work bình thường

Test:
```bash
# Trên cả 2 máy
curl -s https://api.ipify.org && echo
```
Nếu output trùng → owner machine không phải cross-NAT thật. Plan:
- Dùng máy KHÁC mạng làm owner (4G phone, laptop ở quán cà phê, máy nhà
  bạn bè).
- Hoặc nếu VPS hỗ trợ private networking, test loopback qua private IP
  thay vì public.

### B. Provider firewall (UFW disabled, có lớp ngoài)

Nếu UFW inactive nhưng owner ngoài mạng vẫn fail UDP:
- Liên hệ provider mở thêm dải UDP cho VPS này (50000-60000/udp).
- Provider VN nhỏ thường yêu cầu ticket; cloud lớn (DigitalOcean,
  Hetzner) có cloud panel mở rule.

### C. ISP block UDP outbound

Hiếm nhưng tồn tại — fallback sang TURN relay (coturn deploy + force
ICE qua TURN). Đó là Phase 3+.

### D. Pipeline emit OK nhưng mediasoup không nhận

Verify trong container Neko:
```bash
docker exec poc-owner-neko apt-get install -y tcpdump
docker exec poc-owner-neko sh -c "timeout 5 tcpdump -i any -nn 'udp port 50004' -c 10"
```
Nếu thấy packet egress từ container `eth0 Out IP 172.x.x.x.PORT > <vps>:50004`
→ pipeline OK, vấn đề ở network giữa owner và VPS.

---

## Cleanup

VPS:

```bash
docker compose -f docker-compose.edge.yml down -v
sudo ufw delete allow 4443/tcp
sudo ufw delete allow 8091/tcp
# Dải UDP 50000-60000 KHÔNG xoá — đó là rule global đã có sẵn.
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
  - Mở firewall `udp/50000-60000` ở edge production (đã sẵn — match dải
    PoC này dùng).

- **FAIL** → diagnose UDP path cụ thể, có thể cần TURN relay từ ngày
  một thay vì defer Phase 3.
