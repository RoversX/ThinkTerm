// Performance baseline for the browser client, over the DevTools protocol
// (not part of the served bundle). Run it through bench.sh.
//
//   node bench.js <url> <send-text command> [--timeout <ms>]
//
// `send-text command` is a shell command that reads a line on stdin and
// sends it to the pane the page shows (a `thinkterm cli send-text` pinned
// to the same server). Prints one JSON line: bundle sizes and fetch times,
// time to attach, and for each scenario the frames the page asked for
// (requestAnimationFrame is wrapped before the page loads, so this counts
// the page's own paints, not the display), the gaps between them, long
// tasks (>50 ms), JS heap and the renderer's RSS. Two scenarios carry a
// CPU profile with the top self-time entries. Read the numbers with
// docs/thinkterm/web-baseline.md.
//
// Needs the `ws` npm package on NODE_PATH and Google Chrome installed.
const { spawn, execSync } = require("child_process");
const http = require("http");
const fs = require("fs");
const os = require("os");
const WebSocket = require("ws");

const args = process.argv.slice(2);
const url = args.shift();
const sendPrefix = args.shift(); // shell prefix that sends text to pane 0
const opt = (name, def) => { const i = args.indexOf(name); return i >= 0 ? args[i + 1] : def; };
const timeoutMs = Number(opt("--timeout", 60000));
const chromePath = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 9333 + Math.floor(Math.random() * 500);
setTimeout(() => { console.error("bench: watchdog"); try { chrome.kill(); } catch {} process.exit(4); }, timeoutMs + 30000).unref();
const profile = fs.mkdtempSync(`${os.tmpdir()}/tt-bench-`);
const chrome = spawn(chromePath, [
  "--headless=new", "--no-first-run", `--remote-debugging-port=${port}`,
  `--user-data-dir=${profile}`, "--window-size=1240,720", "--hide-scrollbars",
  "--enable-unsafe-webgpu", "--enable-precise-memory-info", "about:blank",
], { stdio: "ignore" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function browserWs() {
  for (let i = 0; i < 50; i++) {
    try {
      return await new Promise((res, rej) => http.get(`http://127.0.0.1:${port}/json/version`, (r) => {
        let b = ""; r.on("data", (d) => b += d); r.on("end", () => res(JSON.parse(b).webSocketDebuggerUrl));
      }).on("error", rej));
    } catch { await sleep(200); }
  }
  throw new Error("chrome did not come up");
}

const INSTRUMENT = `
  window.__tt = { rafAsked: 0, paints: [], long: [], t0: performance.now() };
  const raf = window.requestAnimationFrame.bind(window);
  window.requestAnimationFrame = (cb) => { window.__tt.rafAsked++; return raf((t) => { window.__tt.paints.push(performance.now()); return cb(t); }); };
  try { new PerformanceObserver((l) => { for (const e of l.getEntries()) window.__tt.long.push([e.startTime, e.duration]); }).observe({ type: 'longtask', buffered: true }); } catch {}
`;

(async () => {
  const ws = new WebSocket(await browserWs());
  await new Promise((r) => ws.on("open", r));
  let id = 0; const waiting = new Map(); const logs = [];
  ws.on("message", (m) => {
    const msg = JSON.parse(m);
    if (msg.id && waiting.has(msg.id)) { waiting.get(msg.id)(msg.result || msg.error); waiting.delete(msg.id); }
    if (msg.method === "Runtime.exceptionThrown") logs.push("EXCEPTION " + JSON.stringify(msg.params.exceptionDetails.exception?.description || msg.params.exceptionDetails.text));
  });
  const send = (method, params = {}, sessionId) => new Promise((r) => { const i = ++id; waiting.set(i, r); ws.send(JSON.stringify({ id: i, method, params, sessionId })); });
  const { targetId } = await send("Target.createTarget", { url: "about:blank" });
  const s = (await send("Target.attachToTarget", { targetId, flatten: true })).sessionId;
  await send("Runtime.enable", {}, s);
  await send("Page.enable", {}, s);
  await send("Performance.enable", {}, s);
  await send("Page.addScriptToEvaluateOnNewDocument", { source: INSTRUMENT }, s);
  const ev = async (expr) => (await send("Runtime.evaluate", { expression: expr, returnByValue: true }, s)).result?.value;
  const status = () => ev("document.getElementById('status')?.textContent || ''");
  const snap = async () => ({ ...(await ev("JSON.stringify(window.__tt)").then(JSON.parse)), heap: await ev("performance.memory ? performance.memory.usedJSHeapSize : null") });
  const metrics = async () => Object.fromEntries((await send("Performance.getMetrics", {}, s)).metrics.map((m) => [m.name, m.value]));
  const rendererRss = () => {
    try {
      const out = execSync(`ps -o rss=,command= -p $(pgrep -P ${chrome.pid} | tr '\\n' ',' | sed 's/,$//')`).toString();
      const rows = out.split("\n").filter((l) => /--type=renderer/.test(l)).map((l) => Number(l.trim().split(/\s+/)[0]));
      return rows.length ? +(Math.max(...rows) / 1024).toFixed(1) : null;
    } catch { return null; }
  };
  const profiled = async (fn) => {
    await send("Profiler.enable", {}, s);
    await send("Profiler.setSamplingInterval", { interval: 200 }, s);
    await send("Profiler.start", {}, s);
    const r = await fn();
    const { profile } = await send("Profiler.stop", {}, s);
    await send("Profiler.disable", {}, s);
    // self time per function, from sample counts
    const byId = new Map(profile.nodes.map((n) => [n.id, n]));
    const counts = new Map();
    for (const id of profile.samples) counts.set(id, (counts.get(id) || 0) + 1);
    const total = profile.samples.length;
    const dt = (profile.endTime - profile.startTime) / 1000 / total; // ms per sample
    const agg = new Map();
    for (const [id, c] of counts) {
      const n = byId.get(id); const f = n.callFrame;
      const key = (f.functionName || "(anonymous)") + (f.url ? " @" + f.url.split("/").slice(-1)[0] : "");
      agg.set(key, (agg.get(key) || 0) + c);
    }
    const top = [...agg].sort((a, b) => b[1] - a[1]).slice(0, 12).map(([k, c]) => [k, +(c * dt).toFixed(1)]);
    r.profile_top_self_ms = top;
    r.profile_total_ms = +(total * dt).toFixed(0);
    return r;
  };

  const tNav = Date.now();
  await send("Page.navigate", { url }, s);
  let text = ""; let tAttached = null; let tFirst = null;
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    text = await status();
    if (text && tFirst === null) tFirst = Date.now() - tNav;
    if (/this browser has|following/.test(text)) { tAttached = Date.now() - tNav; break; }
    if (/^(disconnected|no token|WebGPU|error)/i.test(text)) break;
    await sleep(50);
  }
  if (tAttached === null) { console.log(JSON.stringify({ error: "not attached", status: text, logs })); chrome.kill(); process.exit(3); }
  const nav = await ev("JSON.stringify(performance.getEntriesByType('navigation')[0])").then(JSON.parse);
  const wasmRes = await ev("JSON.stringify(performance.getEntriesByType('resource').filter(r => /wasm|\\.js|fonts/.test(r.name)).map(r => [r.name.split('/').slice(-1)[0], Math.round(r.duration), r.transferSize, r.decodedBodySize]))").then(JSON.parse);

  const scenario = async (name, action, settleMs) => {
    const before = await snap();
    const t0 = Date.now();
    if (action) execSync(action, { stdio: "ignore" });
    await sleep(settleMs);
    const after = await snap();
    const paints = after.paints.slice(before.paints.length);
    const gaps = paints.slice(1).map((t, i) => t - paints[i]);
    const long = after.long.slice(before.long.length);
    return {
      name, window_ms: Date.now() - t0,
      frames: paints.length, raf_asked: after.rafAsked - before.rafAsked,
      gap_ms: gaps.length ? { median: med(gaps), p95: pct(gaps, 0.95), max: Math.round(Math.max(...gaps)) } : null,
      long_tasks: long.length, long_max_ms: long.length ? Math.round(Math.max(...long.map((l) => l[1]))) : 0,
      heap_mb: after.heap ? +(after.heap / 1048576).toFixed(1) : null,
      renderer_rss_mb: rendererRss(),
    };
  };
  const med = (a) => { const b = [...a].sort((x, y) => x - y); return Math.round(b[Math.floor(b.length / 2)]); };
  const pct = (a, p) => { const b = [...a].sort((x, y) => x - y); return Math.round(b[Math.min(b.length - 1, Math.floor(b.length * p))]); };

  const line = (text) => `printf '%s\\n' '${text}' | ${sendPrefix}`;
  const CJK = 'python3 -c "print((\\"中文测试汉字渲染性能基线 한국어 테스트 日本語のテスト ★☆♥ \\" * 2 + chr(10)) * 30, end=\\"\\")"';
  const cjk = line(CJK);
  const cjk2 = line("clear; " + CJK);
  const seq = line("seq 1 20000");
  const CJK2 = 'python3 -c "print((\\"東京都渋谷区神宮前 대한민국 서울특별시 강남구 北京市朝阳区 ♠♣♦ \\" * 2 + chr(10)) * 30, end=\\"\\")"';
  const cjk_other = line("clear; " + CJK2);
  const trickle = line('python3 -c "import time\nfor i in range(300): print(i, flush=True); time.sleep(0.01)"');
  const results = {
    bundle: wasmRes, load: { dom_content_loaded_ms: Math.round(nav.domContentLoadedEventEnd), first_status_ms: tFirst, attached_ms: tAttached },
    idle_after_attach: await scenario("idle 3 s", null, 3000),
    cjk_cold: await profiled(() => scenario("screen of fresh CJK", cjk, 3000)),
    cjk_warm: await scenario("same CJK again (cached)", cjk2, 3000),
    cjk_cold_other: await profiled(() => scenario("a second screen of different fresh CJK", cjk_other, 3000)),
    trickle: await scenario("300 lines, one every 10 ms", trickle, 5000),
    ascii_scroll: await scenario("seq 1 20000", seq, 6000),
    idle_after: await scenario("idle 3 s after", null, 3000),
    metrics: await metrics().then((m) => ({ js_heap_mb: +(m.JSHeapUsedSize / 1048576).toFixed(1), documents: m.Documents, nodes: m.Nodes })),
    logs,
  };
  console.log(JSON.stringify(results));
  ws.close(); chrome.kill();
  process.exit(0);
})().catch((e) => { console.error(e); chrome.kill(); process.exit(1); });
