/**
 * Phase 0 PoC task 1.5 — concurrent viewer load test.
 *
 * Spawns N headless Chromium tabs that connect to the PoC viewer at
 * http://localhost:8090, click the "connect & consume" button, and measure
 * first-frame latency + per-viewer inbound RTP throughput over a fixed window.
 *
 * Outputs JSON results to stdout and (optionally) a file. Pretty summary at the
 * end shows P50/P95 first-frame latency + drop rate, which gates the Phase 0
 * GO/NO-GO checkpoint (R19.4 target: P95 < 250ms).
 *
 * Why Playwright (not puppeteer): mediasoup-client needs a real Chromium with
 * H/W codec stack; both work but Playwright bundles its own Chromium pinned
 * version which avoids host-Chrome version drift when running in CI.
 */

import { chromium, type Browser, type Page } from "playwright";
import { writeFileSync } from "node:fs";
import { argv } from "node:process";

interface ViewerResult {
  viewerId: number;
  status: "ok" | "timeout" | "error";
  /** ms from page navigation to consumer.track unmute event. */
  firstFrameMs: number | null;
  totalBytes: number;
  totalPackets: number;
  /** Number of input events the viewer sent over its DataChannel. */
  inputSent: number;
  /** Number of input events the SFU acked back. */
  inputAcked: number;
  durationMs: number;
  errorMessage: string | null;
}

interface LoadTestSummary {
  viewersRequested: number;
  viewersOk: number;
  viewersTimeout: number;
  viewersError: number;
  firstFrameLatencyMs: { p50: number; p95: number; min: number; max: number } | null;
  aggregateBitrateMbps: number;
  /** Sum of input events sent by every viewer's DataChannel. */
  totalInputSent: number;
  /** Sum of input events the SFU acked back. */
  totalInputAcked: number;
  durationSec: number;
  perViewer: ViewerResult[];
}

const VIEWER_URL = process.env.VIEWER_URL ?? "http://localhost:8090";
const NEKO_URL = process.env.NEKO_URL ?? "http://localhost:8080";
const NEKO_USER = process.env.NEKO_USER ?? "neko";
const NEKO_PASS = process.env.NEKO_PASS ?? "neko";
const VIEWER_COUNT = Number(process.env.VIEWER_COUNT ?? argv[2] ?? 5);
const RUN_DURATION_MS = Number(process.env.RUN_DURATION_MS ?? 60_000);
const FIRST_FRAME_TIMEOUT_MS = Number(process.env.FIRST_FRAME_TIMEOUT_MS ?? 15_000);

const log = (msg: string): void => console.log(`[loadtest] ${msg}`);

/**
 * Open Neko UI in a tab, log in, and dispatch a synthetic click into the page
 * to (a) bypass autoplay policy and (b) trigger the GStreamer pipeline so it
 * starts emitting RTP. We hold this tab open in the background while the
 * viewer fleet streams.
 */
async function spawnNekoOperator(browser: Browser): Promise<{ close: () => Promise<void> }> {
  log(`logging into Neko at ${NEKO_URL} as ${NEKO_USER}`);
  const ctx = await browser.newContext();
  const page = await ctx.newPage();
  await page.goto(NEKO_URL, { waitUntil: "domcontentloaded", timeout: 15_000 });

  // Neko v3's legacy frontend uses a query-string token-style login. The
  // existing PoC config has cookie auth disabled and accepts password=neko in
  // the URL via the websocket. The visible UI flow: there's a password input,
  // we type and press Enter.
  // The username field has placeholder "Username" and password has type=password.
  // We also force a click into the body to satisfy autoplay user-gesture rule.
  try {
    await page.fill("input[placeholder='Username']", NEKO_USER, { timeout: 3_000 });
    await page.fill("input[type='password']", NEKO_PASS, { timeout: 3_000 });
    await page.keyboard.press("Enter");
  } catch {
    // Some Neko builds bypass the login form when password is in URL — try
    // appending it. We refresh with credentials in the query string.
    log("login form not found, retrying with URL-embedded credentials");
    const urlWithCreds = `${NEKO_URL}/?password=${NEKO_PASS}&username=${NEKO_USER}`;
    await page.goto(urlWithCreds, { waitUntil: "domcontentloaded", timeout: 15_000 });
  }

  // Click into the page so autoplay policy unblocks the <video> element and
  // the WebRTC peer fully comes up — this is what triggers the GStreamer
  // pipeline to start emitting RTP toward the SFU.
  await page.waitForTimeout(2_000);
  await page.mouse.click(640, 360);
  log("Neko operator session warm — pipeline should be streaming");

  return {
    close: async () => {
      await ctx.close().catch(() => undefined);
    },
  };
}

async function spawnViewer(
  browser: Browser,
  viewerId: number,
): Promise<ViewerResult> {
  const result: ViewerResult = {
    viewerId,
    status: "error",
    firstFrameMs: null,
    totalBytes: 0,
    totalPackets: 0,
    inputSent: 0,
    inputAcked: 0,
    durationMs: 0,
    errorMessage: null,
  };

  const context = await browser.newContext();
  const page: Page = await context.newPage();

  // Capture browser console for debugging.
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.text().includes("err")) {
      log(`viewer ${viewerId} console.${msg.type()}: ${msg.text()}`);
    }
  });

  const startMs = Date.now();
  try {
    // Append `?auto=1` so the viewer fires connect-and-consume on its own
    // without requiring a button click — avoids brittle click selectors.
    const url = VIEWER_URL.includes("?") ? `${VIEWER_URL}&auto=1` : `${VIEWER_URL}?auto=1`;
    await page.goto(url, { waitUntil: "domcontentloaded", timeout: 10_000 });

    // Wait until the <video> element gets a track that emits "unmute".
    // We expose it via window.__firstFrameMs in the page context, set by main.ts
    // when consumer.track receives 'unmute'. We poll for it.
    const firstFrameMs = (await page.waitForFunction(
      () => {
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const w = window as unknown as { __firstFrameMs?: number };
        return typeof w.__firstFrameMs === "number" ? w.__firstFrameMs : false;
      },
      { timeout: FIRST_FRAME_TIMEOUT_MS, polling: 100 },
    ).then((handle) => handle.jsonValue())) as number;
    result.firstFrameMs = firstFrameMs;
    log(`viewer ${viewerId} first frame at ${firstFrameMs}ms`);

    // Hold the connection for the configured duration to gather throughput.
    await page.waitForTimeout(RUN_DURATION_MS);

    // Read final RTP stats injected by main.ts.
    const stats = await page.evaluate(() => {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const w = window as unknown as {
        __rtpTotalBytes?: number;
        __rtpTotalPackets?: number;
        __inputSent?: number;
        __inputAcked?: number;
      };
      return {
        bytes: w.__rtpTotalBytes ?? 0,
        packets: w.__rtpTotalPackets ?? 0,
        inputSent: w.__inputSent ?? 0,
        inputAcked: w.__inputAcked ?? 0,
      };
    });
    result.totalBytes = stats.bytes;
    result.totalPackets = stats.packets;
    result.inputSent = stats.inputSent;
    result.inputAcked = stats.inputAcked;
    result.status = "ok";
  } catch (e) {
    if (e instanceof Error && e.message.includes("Timeout")) {
      result.status = "timeout";
    } else {
      result.status = "error";
    }
    result.errorMessage = e instanceof Error ? e.message : String(e);
    log(`viewer ${viewerId} ${result.status}: ${result.errorMessage}`);
  } finally {
    result.durationMs = Date.now() - startMs;
    await context.close().catch(() => undefined);
  }

  return result;
}

function percentile(values: number[], p: number): number {
  const sorted = [...values].sort((a, b) => a - b);
  const idx = Math.min(sorted.length - 1, Math.floor((p / 100) * sorted.length));
  return sorted[idx] ?? 0;
}

async function main(): Promise<void> {
  log(`launching ${VIEWER_COUNT} viewers against ${VIEWER_URL}`);
  log(`first-frame timeout=${FIRST_FRAME_TIMEOUT_MS}ms, hold duration=${RUN_DURATION_MS}ms`);

  const browser = await chromium.launch({
    headless: true,
    args: [
      "--no-sandbox",
      "--disable-dev-shm-usage",
      "--use-fake-ui-for-media-stream",
      "--autoplay-policy=no-user-gesture-required",
    ],
  });

  try {
    const startedAt = Date.now();

    // 1. Establish Neko streaming session first so the pipeline is producing
    //    RTP before any viewer subscribes. Otherwise the first viewer pays a
    //    cold-start penalty (~3s) that pollutes the latency measurement.
    const operator = await spawnNekoOperator(browser);

    // 2. Wait for the SFU to actually see RTP from Neko. We don't have a way
    //    to query SFU stats from here, so just sleep a small amount.
    await new Promise((r) => setTimeout(r, 3_000));

    // 3. Spawn N viewer tabs in parallel, lightly staggered.
    const tasks: Promise<ViewerResult>[] = [];
    for (let i = 0; i < VIEWER_COUNT; i++) {
      tasks.push(spawnViewer(browser, i));
      await new Promise((r) => setTimeout(r, 100));
    }
    const results = await Promise.all(tasks);

    await operator.close();
    const elapsedSec = (Date.now() - startedAt) / 1000;

    const okResults = results.filter((r) => r.status === "ok");
    const firstFrameLatencies = okResults
      .map((r) => r.firstFrameMs)
      .filter((v): v is number => v !== null);

    const totalBytes = okResults.reduce((acc, r) => acc + r.totalBytes, 0);
    const aggregateMbps = (totalBytes * 8) / 1_000_000 / Math.max(1, RUN_DURATION_MS / 1000);

    const totalInputSent = okResults.reduce((acc, r) => acc + r.inputSent, 0);
    const totalInputAcked = okResults.reduce((acc, r) => acc + r.inputAcked, 0);

    const summary: LoadTestSummary = {
      viewersRequested: VIEWER_COUNT,
      viewersOk: okResults.length,
      viewersTimeout: results.filter((r) => r.status === "timeout").length,
      viewersError: results.filter((r) => r.status === "error").length,
      firstFrameLatencyMs:
        firstFrameLatencies.length > 0
          ? {
              p50: percentile(firstFrameLatencies, 50),
              p95: percentile(firstFrameLatencies, 95),
              min: Math.min(...firstFrameLatencies),
              max: Math.max(...firstFrameLatencies),
            }
          : null,
      aggregateBitrateMbps: Math.round(aggregateMbps * 100) / 100,
      totalInputSent,
      totalInputAcked,
      durationSec: Math.round(elapsedSec * 10) / 10,
      perViewer: results,
    };

    console.log("\n=== LOAD TEST SUMMARY ===");
    console.log(JSON.stringify(summary, null, 2));

    const outFile = process.env.OUT_FILE ?? "loadtest-results.json";
    writeFileSync(outFile, JSON.stringify(summary, null, 2));
    log(`wrote ${outFile}`);

    if (summary.viewersOk < VIEWER_COUNT) {
      log(`WARNING: ${VIEWER_COUNT - summary.viewersOk}/${VIEWER_COUNT} viewers did not reach first frame`);
      process.exitCode = 1;
    }
    const p95 = summary.firstFrameLatencyMs?.p95 ?? Infinity;
    if (p95 > 250) {
      log(`WARNING: P95 first-frame latency ${p95}ms exceeds R19.4 target of 250ms`);
    }
  } finally {
    await browser.close();
  }
}

main().catch((e) => {
  log(`fatal: ${String(e)}`);
  process.exitCode = 1;
});
