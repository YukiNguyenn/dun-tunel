# Neko Config — Option A (force ICE qua mediasoup TURN)

> Task 1.3. Config nằm trong `docker-compose.yml`. File này document why + how + verify.

## Goal

Neko Producer publish stream lên mediasoup SFU thay vì peer trực tiếp với viewer. Để mediasoup là điểm fan-out duy nhất → 1×bitrate upload từ container, N×bitrate fan-out tại edge.

## Knobs (xem `docker-compose.yml` service `neko`)

| Env | Value | Why |
|---|---|---|
| `NEKO_NAT1TO1` | `${SFU_PUBLIC_IP}` | Override host candidates → IP mediasoup TURN. Neko gửi candidate này trong SDP, ICE chọn relay path qua SFU thay vì direct. |
| `NEKO_EPR` | `59000-59100` | Port range Neko bind. Cho phép firewall/UFW rule hẹp. |
| `NEKO_ICELITE` | `false` | Neko phải full ICE để negotiate qua TURN. ICE-lite không relay được. |
| `NEKO_ICESERVERS` | JSON array `turn:` | TURN URL mediasoup expose. Username + credential từ HMAC shared secret (TURN REST API style). |

## Verify checklist

Khi `docker compose up -d`, kiểm tra:

1. `docker logs poc-neko 2>&1 | grep -i ice` — phải thấy candidate gathered từ TURN.
2. Mở `http://localhost:8080` Neko UI từ browser, F12 Network → WebRTC stats: `selectedCandidatePair.remote.candidateType` = `relay` (không phải `host` hoặc `srflx`).
3. `tcpdump -i any -nn 'udp port 3478 or portrange 40000-40100' | head -50` — thấy traffic qua SFU TURN port.

## Failure modes & next step

| Symptom | Likely cause | Action |
|---|---|---|
| Neko candidate type = `host` | NAT1TO1 không được honor | Check Neko version, có thể cần patch hoặc dùng env khác. |
| Neko candidate type = `srflx` không qua TURN | ICESERVERS không parse | Check JSON format, escape, log Neko boot. |
| ICE connect fail | TURN credential sai | Verify HMAC secret match giữa Neko + mediasoup TURN listener. |
| All looks correct nhưng SFU không nhận RTP | Producer side không publish | Vấn đề ở phía SFU receiving Producer (task 1.4 sẽ làm rõ). |

Nếu Neko không respect NAT1TO1 hoặc force-relay không khả thi → fallback Option B (xem RESULTS.md task 1.7) hoặc patch Neko upstream.
