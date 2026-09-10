// The settings panel end to end, against a throwaway server: the gear opens
// it, a fixed size really rasterises the grid at that size, "Follow the
// desktop" comes back to the size it was, the light theme is painted, and a
// reload opens on what was chosen.
//   node settings-test.js <url> <cli prefix> <out.png>
const { spawn } = require("child_process");
const http = require("http"); const fs = require("fs"); const os = require("os");
const WebSocket = require("ws");
const [url, , outPng] = process.argv.slice(2);
const chromePath = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 9333 + Math.floor(Math.random() * 500);
setTimeout(() => { console.error("watchdog"); try { chrome.kill(); } catch {} process.exit(4); }, 150000).unref();
const profile = fs.mkdtempSync(`${os.tmpdir()}/tt-settings-`);
const chrome = spawn(chromePath, ["--headless=new", "--no-first-run", `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, "--window-size=1400,800", "--hide-scrollbars", "--enable-unsafe-webgpu", "about:blank"], { stdio: "ignore" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function browserWs() { for (let i = 0; i < 50; i++) { try { return await new Promise((res, rej) => http.get(`http://127.0.0.1:${port}/json/version`, (r) => { let b = ""; r.on("data", (d) => b += d); r.on("end", () => res(JSON.parse(b).webSocketDebuggerUrl)); }).on("error", rej)); } catch { await sleep(200); } } throw new Error("chrome did not come up"); }
(async () => {
  const ws = new WebSocket(await browserWs()); await new Promise((r) => ws.on("open", r));
  let id = 0; const waiting = new Map(); const logs = [];
  ws.on("message", (m) => { const msg = JSON.parse(m); if (msg.id && waiting.has(msg.id)) { waiting.get(msg.id)(msg.result || msg.error); waiting.delete(msg.id); }
    if (msg.method === "Runtime.consoleAPICalled") logs.push(msg.params.args.map((a) => a.value ?? a.description).join(" "));
    if (msg.method === "Runtime.exceptionThrown") logs.push("EXCEPTION " + JSON.stringify(msg.params.exceptionDetails.exception?.description || msg.params.exceptionDetails.text)); });
  const send = (method, params = {}, sessionId) => new Promise((r) => { const i = ++id; waiting.set(i, r); ws.send(JSON.stringify({ id: i, method, params, sessionId })); });
  const { targetId } = await send("Target.createTarget", { url: "about:blank" });
  const s = (await send("Target.attachToTarget", { targetId, flatten: true })).sessionId;
  await send("Runtime.enable", {}, s); await send("Page.enable", {}, s);
  const ev = async (expr) => (await send("Runtime.evaluate", { expression: expr, returnByValue: true }, s)).result?.value;
  const layout = async () => JSON.parse(await ev("document.getElementById('term').dataset.layout || '{}'"));
  const settings = async () => JSON.parse(await ev("window.thinkterm && window.thinkterm.client ? window.thinkterm.client.settings() : '{}'"));
  const until = async (what, fn, ms = 12000) => { const t = Date.now(); while (Date.now() - t < ms) { const v = await fn(); if (v) return v; await sleep(120); } throw new Error("timeout waiting for " + what + ": settings=" + JSON.stringify(await settings()) + " layout=" + JSON.stringify(await layout())); };
  const click = (sel) => ev(`(() => { const el = document.querySelector(${JSON.stringify(sel)}); if (!el) return false; el.click(); return true; })()`);
  const fail = (what) => { throw new Error(what); };
  const attached = async () => /this browser has/.test(await ev("document.getElementById('status').dataset.summary || ''"));
  const fontPt = async () => (await layout()).font_pt;
  // The number field commits on change, as it does when the pointer leaves it.
  const setSize = (pt) => ev(`(() => { const n = document.querySelector('#settings .num'); if (!n || n.disabled) return false;
    n.value = ${JSON.stringify(String(pt))}; n.dispatchEvent(new Event('change', { bubbles: true })); return true; })()`);
  const openPanel = async () => {
    if (!(await click("#side [data-action=settings]"))) fail("no gear in the sidebar footer");
    await until("the settings panel", () => ev("!!document.getElementById('settings')"));
  };
  const out = {};

  await send("Page.navigate", { url }, s);
  await until("attach", attached, 20000);
  await sleep(900);
  const before = await until("a laid-out tab", async () => (await fontPt()) || null);
  out.font_at_start = before;

  // --- (a) a fixed size really rasterises the grid at that size.
  await openPanel();
  await click("#settings [data-section=appearance]");
  if (!(await click("#settings input[data-font=pinned]"))) fail("no fixed-size choice");
  if (!(await setSize(14))) fail("the size field is not there or still disabled");
  await until("the grid at 14 pt", async () => (await fontPt()) === 14);
  out.pin = "ok";

  // --- (b) "Follow the desktop" comes back to the size it was.
  if (!(await click("#settings input[data-font=follow]"))) fail("no follow choice");
  await until(`the grid back at ${before} pt`, async () => (await fontPt()) === before);
  out.follow = "ok";

  // --- (c) the light theme is the page's to paint.
  if (!(await ev("(() => { const sel = document.querySelector('#settings select[data-setting=theme]'); if (!sel) return false; sel.value = 'light'; sel.dispatchEvent(new Event('change', { bubbles: true })); return true; })()"))) fail("no theme choice");
  await until("the light theme", () => ev("document.documentElement.dataset.theme === 'light'"));
  out.theme = "ok";

  // --- (d) the server's colour scheme arrives on its own, and a scheme
  // picked here overrides it. The cleared ground is behind WebGPU, so the
  // canvas publishes it as data-bg instead.
  const bg = () => ev("document.getElementById('term').dataset.bg || ''");
  await until("the server's scheme", async () => (await bg()) === "#1d2021");
  out.server_scheme = "ok";
  await click("#settings [data-section=appearance]");
  if (!(await click("#settings [data-setting=terminal-scheme]"))) fail("no colour scheme row");
  await until("the scheme picker", () => ev("!!document.getElementById('schemes')"));
  if (!(await ev("(() => { const q = document.querySelector('#schemes .q'); if (!q) return false; q.value = 'Dracula'; q.dispatchEvent(new Event('input', { bubbles: true })); return true; })()"))) fail("no scheme search field");
  await until("Dracula in the list", () => ev("!!document.querySelector('#schemes .pe[data-scheme=\"Dracula\"]')"), 20000);
  if (!(await click('#schemes .pe[data-scheme="Dracula"]'))) fail("Dracula did not click");
  await until("the terminal in Dracula", async () => (await bg()) === "#1e1f29");
  out.scheme = await ev("(document.querySelector('#settings [data-setting=terminal-scheme]') || {}).dataset?.scheme");

  // --- (e) what was chosen survives a reload.
  await click("#settings [data-section=appearance]");
  if (!(await click("#settings input[data-font=pinned]"))) fail("no fixed-size choice");
  if (!(await setSize(14))) fail("the size field is not there or still disabled");
  await until("the grid at 14 pt again", async () => (await fontPt()) === 14);
  const shot = await send("Page.captureScreenshot", { format: "png" }, s); fs.writeFileSync(outPng, Buffer.from(shot.data, "base64"));

  // A real reload, not a jump to the same address with the token back in
  // the fragment, which the browser would not reload for at all. The token
  // is in this tab's session storage by now, so the address is enough.
  await ev("window.__before = 1");
  await send("Page.reload", { ignoreCache: true }, s);
  await until("a fresh document", () => ev("window.__before === undefined"), 20000);
  await until("attach again", attached, 20000);
  await until("the stored theme", () => ev("document.documentElement.dataset.theme === 'light'"));
  await until("the stored size", async () => (await fontPt()) === 14);
  await until("the stored scheme", async () => (await bg()) === "#1e1f29");
  if (await ev("!!document.getElementById('settings')")) fail("the panel came back up on its own");
  out.stored = await settings();
  out.persist = "ok";

  out.logs = logs.filter((l) => !/^\s*$/.test(l) && !/INFO/.test(l)).slice(-6);
  console.log(JSON.stringify(out));
  ws.close(); chrome.kill(); process.exit(0);
})().catch((e) => { console.error(e); chrome.kill(); process.exit(1); });
