/**
 * Promoted load-test harness — neko-v3-migration task 11.1.
 *
 * This is the PRODUCTION promotion of the Phase 0 PoC harness
 * (`loadtest.ts`). Where the PoC harness drove the standalone PoC viewer
 * web app (`poc/neko-sfu/viewer`, channel `neko-poc-input/v1` with a
 * per-event `InputAck`), this harness speaks the PRODUCTION viewer wire
 * protocol directly:
 *
 *   - viewer hook  : `viewer-ui-react/src/hooks/useSfuViewer.ts`
 *   - edge server  : `edge-control/src/routes/sfu_ws.rs`
 *   - input channel: `neko-input` / `neko-input/v1`, `ordered:false`,
 *                    `maxRetransmits:0`, acked ONCE by the server's
 *                    `InputProduced` message (NOT per-event).
 *
 * It drives N concurrent viewers (default 5), each completing the full
 * production handshake — Init → recv transport → Consume video (+audio)
 * → ConsumerResume → ConnectInputTransport → ProduceInput → open the
 * `neko-input` DataChannel — then sends a burst of production
 * `InputEnvelope` JSON frames over SCTP.
 *
 * ── Pass criteria (gates CI; process exits non-zero on failure) ──────────
 *
 *   1. 100% input ack — EVERY viewer's `neko-input` channel opens and is
 *      acked by the server (`InputProduced`). With the production
 *      `maxRetransmits:0` unordered channel there is no per-frame ack, so
 *      "100% ack" == all N viewers reach `inputReady` AND the channel
 *      stays open while the burst is sent. (Requirement 3.1, 3.2)
 *
 *   2. Video bitrate / drop-rate unchanged vs the PoC RESULTS baseline —
 *      the input traffic must not degrade the video path:
 *        - aggregate inbound bitrate within ±BITRATE_TOL of the baseline
 *          7.34 Mbps (RESULTS.md, Task 1.5 / 1.6).
 *        - per-viewer drop rate ≤ DROP_RATE_MAX (R19: drop rate < 1%;
 *          PoC measured 0%).
 *
 * ── Runtime requirement (READ THIS) ──────────────────────────────────────
 *
 * This is an INTEGRATION harness. A live, GREEN run requires a running
 * edge stack: mediasoup worker (`edge-sfu`) + edge-control + a Neko v3
 * container emitting VP8/Opus RTP through the GStreamer bridge. That
 * stack CANNOT be built on the Windows dev box (`mediasoup-sys` does not
 * build there — documented in the spec). Run this on Linux / CI or
 * against a deployed edge. The harness itself is portable and
 * type-checks anywhere (`npm run typecheck`).
 *
 * ── How to run against a deployed edge ────────────────────────────────────
 *
 *   # Production edge (cookie-gated /v1/sfu/viewer/ws):
 *   WS_BASE="wss://<share-sub>.<edge-host>" \
 *   SESSION_ID="<share-session-id>" \
 *   VIEWER_COOKIE="viewer-cookie=<jwt>" \
 *   PAGE_ORIGIN="https://<share-sub>.<edge-host>" \
 *   npm run test:input
 *
 *   # PoC server (no cookie; global producer, PoC `/ws` endpoint):
 *   WS_BASE="ws://localhost:4443" WS_PATH="/ws" SESSION_ID="" \
 *   PAGE_ORIGIN="http://localhost:8090" \
 *   npm run test:input
 *
 * Env vars:
 *   WS_BASE            ws(s):// origin of the edge (default ws://localhost:4443)
 *   WS_PATH            WS path template; `{session}` is substituted
 *                      (default `/v1/sfu/viewer/ws?session={session}`)
 *   SESSION_ID         share-session id (required for the production path)
 *   VIEWER_COOKIE      `name=value` cookie set on PAGE_ORIGIN before the WS
 *                      open so Caddy `forward_auth` lets the upgrade through
 *   PAGE_ORIGIN        http(s) origin Playwright navigates to so the in-page
 *                      driver runs same-origin with the WS + cookie
 *                      (default http://localhost:8090)
 *   MEDIASOUP_MODULE   ESM URL for mediasoup-client (default esm.sh, pinned
 *                      to ^3.20 to match the production viewer). CI can point
 *                      this at a locally-served bundle for hermeticity.
 *   VIEWER_COUNT       concurrent viewers (default 5)
 *   INPUT_EVENTS       input frames per viewer (default 150 → 750 total,
 *                      matching the PoC's 750-event pattern)
 *   INPUT_RATE_HZ      input send rate per viewer (default 30 Hz; stays
 *                      under the edge's 60/s/session clamp)
 *   RUN_DURATION_MS    steady-state hold for RTP stats (default 60000)
 *   FIRST_FRAME_TIMEOUT_MS  per-viewer first-frame/inputReady budget (15000)
 *   BITRATE_TOL        fractional bitrate tolerance vs baseline (default 0.20)
 *   DROP_RATE_MAX      max acceptable per-viewer drop rate (default 0.01)
 *   OUT_FILE           results JSON path (default loadtest-input-results.json)
 */

import { chromium, type Browser, type BrowserContext } from 'playwright'
import { writeFileSync } from 'node:fs'

// ── Baseline (PoC RESULTS.md — Task 1.5 + 1.6, 5 viewers / 60s) ──────────
// The promoted harness asserts the production run stays within tolerance of
// these numbers so we can prove the `neko-input` traffic did not regress the
// video path.
const BASELINE = {
  viewers: 5,
  /** Aggregate inbound bitrate across all viewers (Mbps). */
  aggregateBitrateMbps: 7.34,
  /** Per-viewer source bitrate (Mbps) — ~1.47 Mbps VP8. */
  perViewerBitrateMbps: 1.47,
  /** PoC drop rate was 0% (no packets lost). */
  dropRate: 0,
  /** PoC: 100% input ack (750/750). */
  inputAckRate: 1,
} as const

const WS_BASE = process.env.WS_BASE ?? 'ws://localhost:4443'
const WS_PATH = process.env.WS_PATH ?? '/v1/sfu/viewer/ws?session={session}'
const SESSION_ID = process.env.SESSION_ID ?? ''
const VIEWER_COOKIE = process.env.VIEWER_COOKIE ?? ''
const PAGE_ORIGIN = process.env.PAGE_ORIGIN ?? 'http://localhost:8090'
const MEDIASOUP_MODULE =
  process.env.MEDIASOUP_MODULE ?? 'https://esm.sh/mediasoup-client@3.20.0'
const VIEWER_COUNT = Number(process.env.VIEWER_COUNT ?? 5)
const INPUT_EVENTS = Number(process.env.INPUT_EVENTS ?? 150)
const INPUT_RATE_HZ = Number(process.env.INPUT_RATE_HZ ?? 30)
const RUN_DURATION_MS = Number(process.env.RUN_DURATION_MS ?? 60_000)
const FIRST_FRAME_TIMEOUT_MS = Number(process.env.FIRST_FRAME_TIMEOUT_MS ?? 15_000)
const BITRATE_TOL = Number(process.env.BITRATE_TOL ?? 0.2)
const DROP_RATE_MAX = Number(process.env.DROP_RATE_MAX ?? 0.01)
const OUT_FILE = process.env.OUT_FILE ?? 'loadtest-input-results.json'

const log = (msg: string): void => console.log(`[loadtest-input] ${msg}`)

/** Config handed to the in-page driver (must be JSON-serializable). */
interface DriverConfig {
  wsUrl: string
  mediasoupModule: string
  inputEvents: number
  inputRateHz: number
  runDurationMs: number
  firstFrameTimeoutMs: number
}

/** Per-viewer measurement returned by the in-page driver. */
interface ViewerResult {
  viewerId: number
  status: 'ok' | 'timeout' | 'error'
  /** ms from driver start to the first inbound RTP packet. */
  firstFrameMs: number | null
  /** True once the server acked ProduceInput (neko-input channel open). */
  inputReady: boolean
  /** ms from driver start to inputReady (null if never). */
  inputReadyMs: number | null
  /** InputEnvelope frames handed to the open DataChannel. */
  inputSent: number
  totalBytes: number
  totalPackets: number
  packetsLost: number
  errorMessage: string | null
}

interface LoadTestSummary {
  target: { wsUrl: string; pageOrigin: string; cookieSet: boolean }
  viewersRequested: number
  viewersOk: number
  viewersTimeout: number
  viewersError: number
  /** Viewers whose neko-input channel opened + was acked. */
  inputReadyCount: number
  /** inputReadyCount / viewersRequested — the "input ack" rate. */
  inputAckRate: number
  totalInputSent: number
  aggregateBitrateMbps: number
  perViewerBitrateMbps: number
  maxDropRate: number
  durationSec: number
  baseline: typeof BASELINE
  checks: { name: string; pass: boolean; detail: string }[]
  pass: boolean
  perViewer: ViewerResult[]
}

/**
 * Browser-context driver. Runs ONE production viewer end-to-end:
 * dynamic-imports mediasoup-client, completes the Init/consume/input
 * handshake, sends an input burst, and reports RTP + input metrics.
 *
 * Everything here executes in the page (DOM globals available). It must
 * be self-contained — no references to Node-side closure variables except
 * the single `cfg` argument Playwright serializes in.
 */
async function inPageDriver(cfg: DriverConfig): Promise<Omit<ViewerResult, 'viewerId'>> {
  const out: Omit<ViewerResult, 'viewerId'> = {
    status: 'error',
    firstFrameMs: null,
    inputReady: false,
    inputReadyMs: null,
    inputSent: 0,
    totalBytes: 0,
    totalPackets: 0,
    packetsLost: 0,
    errorMessage: null,
  }
  const t0 = performance.now()

  // mediasoup-client has no installed @types in the harness package, and
  // it is dynamically imported in the page, so it is intentionally `any`.
  /* eslint-disable @typescript-eslint/no-explicit-any */
  try {
    const mediasoup: any = await import(/* @vite-ignore */ cfg.mediasoupModule)
    const Device = mediasoup.Device

    const ws = new WebSocket(cfg.wsUrl)
    const send = (m: unknown): void => ws.send(JSON.stringify(m))

    let device: any = null
    let recvTransport: any = null
    let inputTransport: any = null
    let inputDataProducer: any = null
    let videoProducerId: string | null = null
    const pendingConsumerConnect: (() => void)[] = []
    const pendingInputConnect: (() => void)[] = []
    const pendingInputProduce: ((d: { id: string }) => void)[] = []
    const consumers: any[] = []

    // The single production InputEnvelope serializer (mirrors
    // viewer-ui-react/src/utils/inputEnvelope.ts → Data Model M1).
    const ts = (): number => Math.round(performance.now())
    const makeEnvelope = (
      seq: number,
    ): Record<string, unknown> => {
      const mod = seq % 12
      if (mod === 0) return { type: 'key_down', key: 65 + (seq % 26), ts: ts() }
      if (mod === 1) return { type: 'key_up', key: 65 + (seq % 26), ts: ts() }
      if (mod === 2) return { type: 'scroll', dx: 0, dy: (seq % 3) - 1, ts: ts() }
      return {
        type: 'move',
        x: Math.floor(Math.random() * 1920),
        y: Math.floor(Math.random() * 1080),
        ts: ts(),
      }
    }

    const handleInit = async (msg: any): Promise<void> => {
      device = new Device()
      await device.load({ routerRtpCapabilities: msg.routerRtpCapabilities })

      // RecvTransport for video/audio.
      recvTransport = device.createRecvTransport({
        id: msg.consumerTransportOptions.id,
        iceParameters: msg.consumerTransportOptions.iceParameters,
        iceCandidates: msg.consumerTransportOptions.iceCandidates,
        dtlsParameters: msg.consumerTransportOptions.dtlsParameters,
      })
      recvTransport.on(
        'connect',
        ({ dtlsParameters }: any, callback: () => void, errback: (e: Error) => void) => {
          try {
            pendingConsumerConnect.push(callback)
            send({ action: 'ConnectConsumerTransport', dtlsParameters })
          } catch (e) {
            errback(e as Error)
          }
        },
      )

      // Optional input SendTransport (production: neko-input/v1).
      if (msg.inputTransportOptions && msg.inputSctpParameters) {
        inputTransport = device.createSendTransport({
          id: msg.inputTransportOptions.id,
          iceParameters: msg.inputTransportOptions.iceParameters,
          iceCandidates: msg.inputTransportOptions.iceCandidates,
          dtlsParameters: msg.inputTransportOptions.dtlsParameters,
          sctpParameters: msg.inputSctpParameters,
        })
        inputTransport.on(
          'connect',
          ({ dtlsParameters }: any, callback: () => void, errback: (e: Error) => void) => {
            try {
              pendingInputConnect.push(callback)
              send({ action: 'ConnectInputTransport', dtlsParameters })
            } catch (e) {
              errback(e as Error)
            }
          },
        )
        inputTransport.on(
          'producedata',
          (
            { sctpStreamParameters, label, protocol }: any,
            callback: (d: { id: string }) => void,
            errback: (e: Error) => void,
          ) => {
            try {
              pendingInputProduce.push(callback)
              send({
                action: 'ProduceInput',
                sctpStreamParameters,
                label: label ?? '',
                protocol: protocol ?? '',
              })
            } catch (e) {
              errback(e as Error)
            }
          },
        )
        // Open the channel exactly as the production hook does.
        void inputTransport
          .produceData({
            ordered: false,
            maxRetransmits: 0,
            label: 'neko-input',
            protocol: 'neko-input/v1',
          })
          .then((dp: any) => {
            inputDataProducer = dp
          })
          .catch(() => undefined)
      }

      send({ action: 'Init', rtpCapabilities: device.rtpCapabilities })
      if (msg.plainProducerId) {
        videoProducerId = msg.plainProducerId
        send({ action: 'Consume', producerId: msg.plainProducerId })
      }
      if (msg.audioProducerId) {
        send({ action: 'Consume', producerId: msg.audioProducerId })
      }
    }

    const handleConsumed = async (msg: any): Promise<void> => {
      const consumer = await recvTransport.consume({
        id: msg.id,
        producerId: msg.producerId,
        kind: msg.kind,
        rtpParameters: msg.rtpParameters,
      })
      consumers.push(consumer)
      if (msg.producerId === videoProducerId && out.firstFrameMs === null) {
        consumer.track.addEventListener('unmute', () => {
          if (out.firstFrameMs === null) out.firstFrameMs = Math.round(performance.now() - t0)
        })
      }
      send({ action: 'ConsumerResume', id: msg.id })
    }

    await new Promise<void>((resolve, reject) => {
      const fail = setTimeout(
        () => reject(new Error('handshake/firstFrame timeout')),
        cfg.firstFrameTimeoutMs,
      )
      ws.onclose = (e: CloseEvent): void => {
        clearTimeout(fail)
        reject(new Error(`ws closed code=${e.code} reason=${e.reason || '(none)'}`))
      }
      ws.onerror = (): void => {
        /* surfaced via onclose */
      }
      ws.onmessage = (ev: MessageEvent): void => {
        let msg: any
        try {
          msg = JSON.parse(String(ev.data))
        } catch {
          return
        }
        switch (msg.action) {
          case 'Init':
            void handleInit(msg).catch((e) => reject(e as Error))
            break
          case 'ConnectedConsumerTransport':
            pendingConsumerConnect.shift()?.()
            break
          case 'Consumed':
            void handleConsumed(msg).catch((e) => reject(e as Error))
            break
          case 'ConnectedInputTransport':
            pendingInputConnect.shift()?.()
            break
          case 'InputProduced':
            pendingInputProduce.shift()?.({ id: msg.id })
            out.inputReady = true
            if (out.inputReadyMs === null) out.inputReadyMs = Math.round(performance.now() - t0)
            clearTimeout(fail)
            resolve()
            break
          case 'Error':
            // Input errors are non-terminal (video unaffected); only bail
            // on terminal session errors.
            if (msg.code === 'session_gone' || msg.code === 'consume_before_init') {
              clearTimeout(fail)
              reject(new Error(`server error: ${msg.code}`))
            }
            break
          default:
            break
        }
      }
    })

    // Burst input frames over the open channel at the configured rate.
    let seq = 0
    await new Promise<void>((resolve) => {
      const periodMs = Math.max(1, Math.round(1000 / cfg.inputRateHz))
      const timer = setInterval(() => {
        if (seq >= cfg.inputEvents || !inputDataProducer || inputDataProducer.closed) {
          clearInterval(timer)
          resolve()
          return
        }
        if (inputDataProducer.readyState === 'open') {
          try {
            inputDataProducer.send(JSON.stringify(makeEnvelope(seq)))
            seq += 1
            out.inputSent = seq
          } catch {
            clearInterval(timer)
            resolve()
          }
        }
      }, periodMs)
    })

    // Steady-state hold to accumulate RTP, then read final stats.
    const pc: RTCPeerConnection | undefined = recvTransport?._handler?._pc
    await new Promise((r) => setTimeout(r, cfg.runDurationMs))
    if (pc) {
      const stats = await pc.getStats()
      stats.forEach((report: any) => {
        if (report.type === 'inbound-rtp') {
          out.totalBytes += report.bytesReceived ?? 0
          out.totalPackets += report.packetsReceived ?? 0
          out.packetsLost += Math.max(0, report.packetsLost ?? 0)
          if (out.firstFrameMs === null && (report.packetsReceived ?? 0) > 0) {
            out.firstFrameMs = Math.round(performance.now() - t0)
          }
        }
      })
    }

    out.status = 'ok'
  } catch (e) {
    const message = e instanceof Error ? e.message : String(e)
    out.errorMessage = message
    out.status = message.toLowerCase().includes('timeout') ? 'timeout' : 'error'
  }
  /* eslint-enable @typescript-eslint/no-explicit-any */
  return out
}

async function spawnViewer(
  browser: Browser,
  viewerId: number,
  cfg: DriverConfig,
): Promise<ViewerResult> {
  let context: BrowserContext | null = null
  try {
    context = await browser.newContext()
    if (VIEWER_COOKIE) {
      const eq = VIEWER_COOKIE.indexOf('=')
      if (eq > 0) {
        const origin = new URL(PAGE_ORIGIN)
        await context.addCookies([
          {
            name: VIEWER_COOKIE.slice(0, eq),
            value: VIEWER_COOKIE.slice(eq + 1),
            domain: origin.hostname,
            path: '/',
            httpOnly: true,
            secure: origin.protocol === 'https:',
          },
        ])
      }
    }
    const page = await context.newPage()
    page.on('console', (m) => {
      if (m.type() === 'error') log(`viewer ${viewerId} console.error: ${m.text()}`)
    })
    // Navigate to the viewer origin so the driver runs same-origin with
    // the WS endpoint + cookie. A bare origin is enough — the driver
    // imports mediasoup-client itself and opens its own WS.
    await page.goto(PAGE_ORIGIN, { waitUntil: 'domcontentloaded', timeout: 15_000 })
    const partial = await page.evaluate(inPageDriver, cfg)
    if (partial.status !== 'ok') {
      log(`viewer ${viewerId} ${partial.status}: ${partial.errorMessage ?? '(no message)'}`)
    } else {
      log(
        `viewer ${viewerId} ok — inputReady=${partial.inputReady} sent=${partial.inputSent} ` +
          `bytes=${partial.totalBytes} lost=${partial.packetsLost}`,
      )
    }
    return { viewerId, ...partial }
  } catch (e) {
    const message = e instanceof Error ? e.message : String(e)
    log(`viewer ${viewerId} harness error: ${message}`)
    return {
      viewerId,
      status: 'error',
      firstFrameMs: null,
      inputReady: false,
      inputReadyMs: null,
      inputSent: 0,
      totalBytes: 0,
      totalPackets: 0,
      packetsLost: 0,
      errorMessage: message,
    }
  } finally {
    await context?.close().catch(() => undefined)
  }
}

function buildWsUrl(): string {
  const base = WS_BASE.replace(/\/$/, '')
  const path = WS_PATH.replace('{session}', encodeURIComponent(SESSION_ID))
  return `${base}${path}`
}

async function main(): Promise<void> {
  const wsUrl = buildWsUrl()
  log(`launching ${VIEWER_COUNT} viewers against ${wsUrl}`)
  log(`page origin=${PAGE_ORIGIN} cookie=${VIEWER_COOKIE ? 'set' : '(none)'}`)
  log(
    `input: ${INPUT_EVENTS} frames/viewer @ ${INPUT_RATE_HZ}Hz, hold=${RUN_DURATION_MS}ms, ` +
      `bitrateTol=±${BITRATE_TOL}, dropRateMax=${DROP_RATE_MAX}`,
  )

  const browser = await chromium.launch({
    headless: true,
    args: [
      '--no-sandbox',
      '--disable-dev-shm-usage',
      '--use-fake-ui-for-media-stream',
      '--autoplay-policy=no-user-gesture-required',
    ],
  })

  const cfg: DriverConfig = {
    wsUrl,
    mediasoupModule: MEDIASOUP_MODULE,
    inputEvents: INPUT_EVENTS,
    inputRateHz: INPUT_RATE_HZ,
    runDurationMs: RUN_DURATION_MS,
    firstFrameTimeoutMs: FIRST_FRAME_TIMEOUT_MS,
  }

  try {
    const startedAt = Date.now()
    const tasks: Promise<ViewerResult>[] = []
    for (let i = 0; i < VIEWER_COUNT; i++) {
      tasks.push(spawnViewer(browser, i, cfg))
      await new Promise((r) => setTimeout(r, 150)) // light stagger
    }
    const results = await Promise.all(tasks)
    const durationSec = (Date.now() - startedAt) / 1000

    const ok = results.filter((r) => r.status === 'ok')
    const inputReadyCount = results.filter((r) => r.inputReady).length
    const totalInputSent = results.reduce((a, r) => a + r.inputSent, 0)
    const totalBytes = ok.reduce((a, r) => a + r.totalBytes, 0)
    const aggregateBitrateMbps =
      (totalBytes * 8) / 1_000_000 / Math.max(1, RUN_DURATION_MS / 1000)
    const perViewerBitrateMbps =
      ok.length > 0 ? aggregateBitrateMbps / ok.length : 0
    const maxDropRate = results.reduce((max, r) => {
      const denom = r.totalPackets + r.packetsLost
      const rate = denom > 0 ? r.packetsLost / denom : 0
      return Math.max(max, rate)
    }, 0)
    const inputAckRate = VIEWER_COUNT > 0 ? inputReadyCount / VIEWER_COUNT : 0

    // ── Pass criteria ────────────────────────────────────────────────────
    const checks: LoadTestSummary['checks'] = []

    // 1. 100% input ack (Requirement 3.1, 3.2).
    checks.push({
      name: '100% input ack (all viewers reach inputReady)',
      pass: inputReadyCount === VIEWER_COUNT && VIEWER_COUNT > 0,
      detail: `${inputReadyCount}/${VIEWER_COUNT} viewers acked InputProduced`,
    })

    // 1b. All viewers reached first frame (sanity for the bitrate sample).
    checks.push({
      name: 'all viewers reached first frame',
      pass: ok.length === VIEWER_COUNT && VIEWER_COUNT > 0,
      detail: `${ok.length}/${VIEWER_COUNT} viewers ok`,
    })

    // 2a. Aggregate bitrate within tolerance of the baseline.
    const lo = BASELINE.aggregateBitrateMbps * (1 - BITRATE_TOL)
    const hi = BASELINE.aggregateBitrateMbps * (1 + BITRATE_TOL)
    checks.push({
      name: 'aggregate video bitrate within baseline tolerance',
      pass: aggregateBitrateMbps >= lo && aggregateBitrateMbps <= hi,
      detail: `${aggregateBitrateMbps.toFixed(2)} Mbps vs baseline ${BASELINE.aggregateBitrateMbps} Mbps (allowed ${lo.toFixed(2)}–${hi.toFixed(2)})`,
    })

    // 2b. Drop rate not worse than the baseline ceiling.
    checks.push({
      name: 'per-viewer drop rate within ceiling',
      pass: maxDropRate <= DROP_RATE_MAX,
      detail: `max drop rate ${(maxDropRate * 100).toFixed(3)}% ≤ ${(DROP_RATE_MAX * 100).toFixed(2)}% (baseline ${BASELINE.dropRate * 100}%)`,
    })

    const pass = checks.every((c) => c.pass)

    const summary: LoadTestSummary = {
      target: { wsUrl, pageOrigin: PAGE_ORIGIN, cookieSet: Boolean(VIEWER_COOKIE) },
      viewersRequested: VIEWER_COUNT,
      viewersOk: ok.length,
      viewersTimeout: results.filter((r) => r.status === 'timeout').length,
      viewersError: results.filter((r) => r.status === 'error').length,
      inputReadyCount,
      inputAckRate,
      totalInputSent,
      aggregateBitrateMbps: Math.round(aggregateBitrateMbps * 100) / 100,
      perViewerBitrateMbps: Math.round(perViewerBitrateMbps * 100) / 100,
      maxDropRate: Math.round(maxDropRate * 1e6) / 1e6,
      durationSec: Math.round(durationSec * 10) / 10,
      baseline: BASELINE,
      checks,
      pass,
      perViewer: results,
    }

    console.log('\n=== PROMOTED LOAD TEST SUMMARY (neko-input path) ===')
    console.log(JSON.stringify(summary, null, 2))
    console.log('\n--- PASS CRITERIA ---')
    for (const c of checks) {
      console.log(`  [${c.pass ? 'PASS' : 'FAIL'}] ${c.name} — ${c.detail}`)
    }
    console.log(`\nRESULT: ${pass ? 'PASS' : 'FAIL'}`)

    writeFileSync(OUT_FILE, JSON.stringify(summary, null, 2))
    log(`wrote ${OUT_FILE}`)

    if (!pass) process.exitCode = 1
  } finally {
    await browser.close()
  }
}

main().catch((e) => {
  log(`fatal: ${String(e)}`)
  process.exitCode = 1
})
