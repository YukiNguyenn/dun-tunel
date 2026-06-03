// Phase 0 PoC viewer — Consumer-only.
//
// Flow on click "connect & consume":
//   1. POST /v1/plain-producer  → SFU lazily creates PlainTransport for Neko's
//                                 RTP stream and a Producer, returns producer id.
//   2. WS /ws                   → exchange Init, create Consumer transport,
//                                 send Consume(producerId), play stream.

import { Device } from "mediasoup-client";
import type {
  AppData,
  Consumer,
  DtlsParameters,
  SctpStreamParameters,
  Transport,
} from "mediasoup-client/types";

import type {
  ClientMessage,
  PlainProducerInfo,
  ServerMessage,
} from "./protocol";

type LogClass = "" | "ok" | "err" | "warn";

const $ = <T extends HTMLElement>(id: string): T => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`element #${id} not found`);
  return el as T;
};

const logEl = $<HTMLPreElement>("log");
const log = (msg: string, cls: LogClass = ""): void => {
  const span = document.createElement("span");
  if (cls) span.className = cls;
  span.textContent = `[${new Date().toISOString().slice(11, 23)}] ${msg}\n`;
  logEl.appendChild(span);
  logEl.scrollTop = logEl.scrollHeight;
  // eslint-disable-next-line no-console
  console.log(msg);
};

class Spike {
  private ws?: WebSocket;
  private consumerTransport?: Transport;
  /** SendTransport used only for the neko-input DataChannel. */
  private inputTransport?: Transport;
  private consumers = new Map<string, Consumer>();
  private producerId?: string;
  /** Last input event sequence acked by SFU. */
  private inputAcked = 0;
  /** Last input event sequence sent. */
  private inputSent = 0;

  start(): void {
    $<HTMLButtonElement>("connect").onclick = (): void => {
      void this.connectAndConsume().catch((e: unknown) => {
        log(`fatal: ${String(e)}`, "err");
      });
    };
    log("ready. click connect.");

    // Auto-connect mode for headless load tests: append `?auto=1` to the URL
    // and the spike fires the same flow as a manual button click.
    if (new URLSearchParams(window.location.search).has("auto")) {
      log("auto-connect enabled via ?auto=1 — firing connect immediately");
      void this.connectAndConsume().catch((e: unknown) => {
        log(`fatal: ${String(e)}`, "err");
      });
    }
  }

  private async connectAndConsume(): Promise<void> {
    const sfuBase = $<HTMLInputElement>("sfu-url").value.replace(/\/$/, "");
    log(`fetching plain producer from ${sfuBase}/v1/plain-producer`);
    const r = await fetch(`${sfuBase}/v1/plain-producer`, { method: "POST" });
    if (!r.ok) {
      log(`plain-producer ${r.status}: ${await r.text()}`, "err");
      return;
    }
    const info = (await r.json()) as PlainProducerInfo;
    this.producerId = info.producerId;
    log(`plain producer id=${info.producerId} (Neko → ${info.rtpListenIp}:${info.rtpListenPort})`, "ok");

    const wsUrl = sfuBase.replace(/^http/, "ws") + "/ws";
    log(`ws connecting ${wsUrl}`);
    const ws = new WebSocket(wsUrl);
    ws.onopen = (): void => log("ws open", "ok");
    ws.onclose = (e: CloseEvent): void =>
      log(`ws close code=${e.code} reason=${e.reason || "(none)"}`, "warn");
    ws.onerror = (): void => log("ws error", "err");
    ws.onmessage = (ev: MessageEvent): void => {
      void this.onServerMessage(ev);
    };
    this.ws = ws;
  }

  private send(msg: ClientMessage): void {
    if (!this.ws) {
      log("ws not connected", "err");
      return;
    }
    this.ws.send(JSON.stringify(msg));
  }

  private async onServerMessage(ev: MessageEvent): Promise<void> {
    let msg: ServerMessage;
    try {
      msg = JSON.parse(String(ev.data)) as ServerMessage;
    } catch (e) {
      log(`bad json: ${String(e)}`, "err");
      return;
    }

    switch (msg.action) {
      case "Init":
        await this.onInit(msg);
        break;
      case "ConnectedConsumerTransport":
        log("consumer transport connected", "ok");
        break;
      case "ConnectedInputTransport":
        log("input transport connected", "ok");
        break;
      case "Consumed":
        await this.onConsumed(msg);
        break;
      case "InputAck":
        this.inputAcked = msg.sequence;
        // Publish acked count for the loadtest harness.
        (window as unknown as { __inputAcked?: number }).__inputAcked = msg.sequence;
        if (msg.sequence % 10 === 0 || msg.sequence < 5) {
          log(
            `input ack seq=${msg.sequence} bytes=${msg.receivedBytes} (sent=${this.inputSent}, acked=${this.inputAcked})`,
          );
        }
        break;
      default: {
        const exhaustive: never = msg;
        log(`unhandled: ${JSON.stringify(exhaustive)}`, "warn");
      }
    }
  }

  private async onInit(msg: Extract<ServerMessage, { action: "Init" }>): Promise<void> {
    log(`server Init: routerCaps codecs=${msg.routerRtpCapabilities.codecs?.length ?? 0}`);
    if (msg.plainProducerId && msg.plainProducerId !== this.producerId) {
      log(
        `producer id mismatch: server=${msg.plainProducerId} client=${this.producerId ?? "(none)"}`,
        "warn",
      );
      this.producerId = msg.plainProducerId;
    }

    const device = new Device();
    await device.load({ routerRtpCapabilities: msg.routerRtpCapabilities });
    log("device loaded", "ok");

    // RecvTransport for video Consumer.
    const recvTransport = device.createRecvTransport({
      id: msg.consumerTransportOptions.id,
      iceParameters: msg.consumerTransportOptions.iceParameters,
      iceCandidates: msg.consumerTransportOptions.iceCandidates,
      dtlsParameters: msg.consumerTransportOptions.dtlsParameters,
      sctpParameters: msg.sctpParameters ?? undefined,
    });
    recvTransport.on(
      "connect",
      (
        { dtlsParameters }: { dtlsParameters: DtlsParameters },
        callback: () => void,
        errback: (err: Error) => void,
      ): void => {
        try {
          this.send({ action: "ConnectConsumerTransport", dtlsParameters });
          callback();
        } catch (e) {
          errback(e as Error);
        }
      },
    );
    this.consumerTransport = recvTransport;

    // Separate SendTransport for the neko-input DataChannel.
    // mediasoup-client only allows produceData() on a SendTransport.
    const sendTransport = device.createSendTransport({
      id: msg.inputTransportOptions.id,
      iceParameters: msg.inputTransportOptions.iceParameters,
      iceCandidates: msg.inputTransportOptions.iceCandidates,
      dtlsParameters: msg.inputTransportOptions.dtlsParameters,
      sctpParameters: msg.inputSctpParameters ?? undefined,
    });
    sendTransport.on(
      "connect",
      (
        { dtlsParameters }: { dtlsParameters: DtlsParameters },
        callback: () => void,
        errback: (err: Error) => void,
      ): void => {
        try {
          this.send({ action: "ConnectInputTransport", dtlsParameters });
          callback();
        } catch (e) {
          errback(e as Error);
        }
      },
    );
    sendTransport.on(
      "producedata",
      (
        args: {
          sctpStreamParameters: SctpStreamParameters;
          label?: string;
          protocol?: string;
          appData: AppData;
        },
        callback: ({ id }: { id: string }) => void,
        errback: (err: Error) => void,
      ): void => {
        try {
          this.send({
            action: "ProduceInput",
            sctpStreamParameters: args.sctpStreamParameters,
            label: args.label ?? "",
            protocol: args.protocol ?? "",
          });
          // Server doesn't echo the producer id back; mediasoup-client uses
          // this id locally only.
          callback({ id: `client-${Math.random().toString(36).slice(2, 10)}` });
        } catch (e) {
          errback(e as Error);
        }
      },
    );
    this.inputTransport = sendTransport;

    this.send({ action: "Init", rtpCapabilities: device.rtpCapabilities });
    log("sent Init with rtpCapabilities", "ok");

    // Trigger Consume immediately. The transport.on("connect") handler will
    // be invoked by mediasoup-client when the consumer first needs DTLS.
    if (this.producerId) {
      log(`requesting Consume(producerId=${this.producerId})`);
      this.send({ action: "Consume", producerId: this.producerId });
    } else {
      log("no producer id available — cannot consume", "err");
    }
  }

  private async onConsumed(msg: Extract<ServerMessage, { action: "Consumed" }>): Promise<void> {
    if (!this.consumerTransport) {
      log("consumer transport missing", "err");
      return;
    }
    const consumer = await this.consumerTransport.consume({
      id: msg.id,
      producerId: msg.producerId,
      kind: msg.kind,
      rtpParameters: msg.rtpParameters,
    });
    this.consumers.set(msg.id, consumer);
    log(`consumed ${msg.kind} id=${msg.id}`, "ok");

    const videoEl = $<HTMLVideoElement>("remote");
    let stream = videoEl.srcObject as MediaStream | null;
    if (!stream) {
      stream = new MediaStream();
      videoEl.srcObject = stream;
    }
    stream.addTrack(consumer.track);

    // Try to play (may need user gesture). Browser autoplay policy permits
    // playback for muted videos, but log any failure regardless.
    void videoEl.play().then(
      () => log("video.play() ok", "ok"),
      (e: unknown) => log(`video.play() blocked: ${String(e)}`, "warn"),
    );

    // Trace lifecycle of underlying MediaStreamTrack. Headless Chromium
    // sometimes never fires `unmute` reliably (esp. without H/W decode), so
    // we ALSO publish first-frame timestamp when the inbound-rtp counter
    // first crosses zero — that's a more robust signal of "RTP arrived".
    consumer.track.addEventListener("ended", () => log(`track ${msg.id} ended`, "warn"));
    consumer.track.addEventListener("mute", () => log(`track ${msg.id} muted`, "warn"));
    consumer.track.addEventListener("unmute", () => {
      log(`track ${msg.id} unmuted`, "ok");
      const w = window as unknown as { __firstFrameMs?: number };
      if (typeof w.__firstFrameMs !== "number") {
        w.__firstFrameMs = Math.round(performance.now());
      }
    });

    this.send({ action: "ConsumerResume", id: msg.id });

    // Open the input DataChannel and start emitting synthetic mouse/key
    // events. Phase 0 task 1.6 spec: viewer publishes a `neko-input`
    // DataChannel, SFU receives + acks (Phase 1+ will forward to Neko).
    void this.openInputChannel().catch((e: unknown) =>
      log(`input channel: ${String(e)}`, "warn"),
    );

    // Poll inbound RTP stats every 2s — answers "are bytes actually flowing?".
    // Also publishes running totals on `window` so load tests can read them.
    const pc = (this.consumerTransport as unknown as { _handler?: { _pc?: RTCPeerConnection } })
      ._handler?._pc;
    if (pc) {
      log(`pc connectionState=${pc.connectionState} ice=${pc.iceConnectionState}`);
      pc.addEventListener("iceconnectionstatechange", () =>
        log(`pc ice → ${pc.iceConnectionState}`, "ok"),
      );
      pc.addEventListener("connectionstatechange", () =>
        log(`pc conn → ${pc.connectionState}`, "ok"),
      );
      let lastBytes = 0;
      let lastPackets = 0;
      // Poll at 500ms so the load-test fallback "first packet arrived"
      // detector has reasonable resolution.
      setInterval(() => {
        void pc.getStats(consumer.track).then((stats: RTCStatsReport) => {
          stats.forEach((report) => {
            if (report.type === "inbound-rtp") {
              const r = report as { bytesReceived?: number; packetsReceived?: number };
              const bytes = r.bytesReceived ?? 0;
              const packets = r.packetsReceived ?? 0;
              const dBytes = bytes - lastBytes;
              const dPackets = packets - lastPackets;
              lastBytes = bytes;
              lastPackets = packets;
              log(`inbound-rtp Δbytes=${dBytes} Δpackets=${dPackets} total=${bytes}B/${packets}p`);
              const w = window as unknown as {
                __rtpTotalBytes?: number;
                __rtpTotalPackets?: number;
                __firstFrameMs?: number;
              };
              w.__rtpTotalBytes = bytes;
              w.__rtpTotalPackets = packets;
              // Fallback first-frame signal: first time we see > 0 packets.
              if (packets > 0 && typeof w.__firstFrameMs !== "number") {
                w.__firstFrameMs = Math.round(performance.now());
              }
            }
          });
        });
      }, 500);
    } else {
      log("could not access underlying RTCPeerConnection for stats", "warn");
    }
  }

  /**
   * Open `neko-input` DataChannel via mediasoup-client.produceData() and
   * start sending synthetic input events at ~30Hz so the loadtest can verify
   * end-to-end ack from the SFU. Each message is a small JSON envelope; in
   * Phase 1+ the same envelope shape will be forwarded to Neko's WebSocket
   * admin interface (signal/keyboard, signal/mouse).
   */
  private async openInputChannel(): Promise<void> {
    if (!this.inputTransport) {
      log("input channel skipped — no input transport", "warn");
      return;
    }
    const dataProducer = await this.inputTransport.produceData({
      ordered: true,
      label: "neko-input",
      protocol: "neko-poc-input/v1",
    });
    log(`input data producer ready id=${dataProducer.id}`, "ok");

    dataProducer.on("transportclose", () => log("input dp transportclose", "warn"));
    dataProducer.on("close", () => log("input dp closed", "warn"));

    // Synthetic mouse-move / keypress events at 30Hz for 5s, then 1Hz keepalive.
    // We send 150 events fast for the loadtest to count, then switch to slow
    // mode to keep the channel alive.
    let seq = 0;
    const fast = setInterval(() => {
      if (dataProducer.closed) {
        clearInterval(fast);
        return;
      }
      seq += 1;
      const ev = {
        type: seq % 10 === 0 ? "key" : "mousemove",
        seq,
        ts: Math.round(performance.now()),
        x: Math.floor(Math.random() * 1280),
        y: Math.floor(Math.random() * 720),
      };
      try {
        dataProducer.send(JSON.stringify(ev));
        this.inputSent = seq;
        (window as unknown as { __inputSent?: number }).__inputSent = seq;
      } catch (e) {
        log(`input send: ${String(e)}`, "warn");
        clearInterval(fast);
      }
      if (seq >= 150) {
        clearInterval(fast);
        log(`input fast burst done — sent ${seq} events`, "ok");
      }
    }, 33);
  }
}

new Spike().start();
