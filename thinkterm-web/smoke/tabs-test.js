// End-to-end check of the page's tabs and panes, driven through tabs.sh
// (not part of the served bundle). With two tabs on the server the page
// lists both, a click switches, typing lands in the new pane, and with
// following turned back on a focus change on the server side moves the
// page back. Then the strip's buttons: a split appears in the page's
// layout (read from `canvas.dataset.layout`) with the divider one cell
// past the first pane, a real mouse click into the second pane focuses it
// and makes the server's active pane follow, typing lands there and not
// beside it, the panes' sizes on the server are unchanged by any of it,
// zoom and close do what they say, and a new tab takes the page with it.
// Last, the listener is switched off and on: the split comes back.
//
//   node tabs-test.js <url> <cli prefix> <out.png>
//
// `cli prefix` is a `thinkterm cli` pinned to the same server. Needs the
// `ws` npm package on NODE_PATH and Google Chrome installed.
const { spawn, execSync } = require("child_process");
const http = require("http"); const fs = require("fs"); const os = require("os");
const WebSocket = require("ws");
const [url, cli, outPng] = process.argv.slice(2);
const chromePath = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 9333 + Math.floor(Math.random() * 500);
setTimeout(() => { console.error("watchdog"); try { chrome.kill(); } catch {} process.exit(4); }, 90000).unref();
const profile = fs.mkdtempSync(`${os.tmpdir()}/tt-strip-`);
const chrome = spawn(chromePath, ["--headless=new", "--no-first-run", `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, "--window-size=1240,720", "--hide-scrollbars", "--enable-unsafe-webgpu", "about:blank"], { stdio: "ignore" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function browserWs() { for (let i = 0; i < 50; i++) { try { return await new Promise((res, rej) => http.get(`http://127.0.0.1:${port}/json/version`, (r) => { let b = ""; r.on("data", (d) => b += d); r.on("end", () => res(JSON.parse(b).webSocketDebuggerUrl)); }).on("error", rej)); } catch { await sleep(200); } } throw new Error("chrome did not come up"); }
const sh = (cmd) => execSync(cmd, { encoding: "utf8" });
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
  const status = () => ev("document.getElementById('status')?.textContent || ''");
  const until = async (what, expr, ms = 10000) => { const t = Date.now(); while (Date.now() - t < ms) { const v = await ev(expr); if (v) return v; await sleep(100); } throw new Error("timeout waiting for " + what + ": " + JSON.stringify({ status: await status(), strip: await ev("document.getElementById('tabs').innerText") })); };
  const out = {};
  const layout = async () => JSON.parse(await ev("document.getElementById('term').dataset.layout || '{}'"));
  const click = (action) => ev(`document.querySelector('[data-action=${JSON.stringify(action)}]').click(); 1`);
  const type = async (text) => { await ev(`(() => { const kbd = document.getElementById('kbd'); kbd.focus(); const fire = (key, code) => kbd.dispatchEvent(new KeyboardEvent('keydown', { key, code, bubbles: true, cancelable: true })); for (const ch of ${JSON.stringify(text)}) fire(ch, ch === ' ' ? 'Space' : 'Key' + ch.toUpperCase()); fire('Enter', 'Enter'); return 1; })()`); await sleep(1200); };
  const has = (pane, text) => new RegExp(text).test(sh(`${cli} get-text --pane-id ${pane}`));
  const list = () => JSON.parse(sh(`${cli} list --format json`));
  const sizes = () => list().map((p) => [p.pane_id, p.size.cols + "x" + p.size.rows]);
  const fail = (what) => { throw new Error(what); };

  await send("Page.navigate", { url }, s);
  await until("attach", "/this browser has|mirroring|following/.test(document.getElementById('status').textContent)", 20000);

  // --- tabs: two on the server, click the other, type, follow back
  out.strip_at_start = await until("two tabs", "document.querySelectorAll('#tabs .tab').length === 2 && document.getElementById('tabs').innerText");
  const first = await ev("document.querySelector('#tabs .tab.current').dataset.pane");
  await ev("document.querySelector('#tabs .tab:not(.current)').click()");
  await until("switch", `document.querySelector('#tabs .tab.current').dataset.pane !== ${JSON.stringify(first)}`);
  await sleep(400);
  const second = await ev("document.querySelector('#tabs .tab.current').dataset.pane");
  out.follow_after_click = await ev("document.querySelector('#tabs .follow').textContent");
  await type("echo TAB2-OK");
  if (!has(second, "TAB2-OK") || has(first, "TAB2-OK")) fail("typing after a tab click went to the wrong pane");
  await ev("document.querySelector('#tabs .follow').click()");
  sh(`${cli} activate-pane --pane-id ${first}`);
  await until("follow back", `document.querySelector('#tabs .tab.current').dataset.pane === ${JSON.stringify(first)}`);
  out.tabs = "ok";

  // --- split: the layout has two placements and one divider
  const before = sizes();
  await click("split-right");
  await until("two panes", "document.querySelectorAll('#tabs .pane').length === 2", 8000);
  await sleep(400);
  let l = await layout();
  if (l.panes.length !== 2) fail("expected two placements: " + JSON.stringify(l));
  const [a, b] = l.panes[0].left < l.panes[1].left ? [l.panes[0], l.panes[1]] : [l.panes[1], l.panes[0]];
  if (b.left !== a.left + a.cols + 1) fail(`second pane at ${b.left}, expected ${a.left + a.cols + 1}`);
  if (l.dividers.length !== 1 || l.dividers[0].col !== a.left + a.cols) fail("divider not in the gap cell: " + JSON.stringify(l.dividers));
  if (l.focused !== b.id) fail("the new pane is not focused");
  out.split = { a: [a.id, a.cols, a.rows], b: [b.id, b.cols, b.rows], divider: l.dividers[0] };

  // --- a real click into the left pane focuses it, on the server too
  const rect = await ev("(r => [r.left, r.top])(document.getElementById('term').getBoundingClientRect())");
  const px = rect[0] + (a.left + Math.floor(a.cols / 2)) * l.cell[0] + 2;
  const py = rect[1] + (a.top + 3) * l.cell[1] + 2;
  await send("Input.dispatchMouseEvent", { type: "mousePressed", x: px, y: py, button: "left", clickCount: 1 }, s);
  await send("Input.dispatchMouseEvent", { type: "mouseReleased", x: px, y: py, button: "left", clickCount: 1 }, s);
  await until("focus moved", `document.querySelector('#tabs .pane.current').dataset.pane === ${JSON.stringify(String(a.id))}`);
  await sleep(300);
  const active = list().find((p) => p.is_active);
  if (!active || active.pane_id !== a.id) fail("the server's active pane did not follow the click: " + JSON.stringify(list().map((p) => [p.pane_id, p.is_active])));
  await type("echo CLICKED");
  if (!has(a.id, "CLICKED") || has(b.id, "CLICKED")) fail("typing after a click went to the wrong pane");
  const after = sizes();
  if (JSON.stringify(after) !== JSON.stringify(sizes())) fail("sizes changed while the page typed");
  out.click = { focused: a.id, sizes_after: after };

  // --- zoom, unzoom, close (twice), new tab
  await click("zoom");
  await until("zoomed", "document.querySelector('[data-action=zoom]').classList.contains('on')", 8000);
  l = await layout();
  if (l.zoomed !== a.id || l.panes.length !== 1 || l.dividers.length !== 0) fail("zoom not reflected: " + JSON.stringify(l));
  await click("zoom");
  await until("unzoomed", "!document.querySelector('[data-action=zoom]').classList.contains('on')", 8000);
  await click("close");
  if ((await ev("document.querySelector('[data-action=close]').textContent")) !== "close pane?") fail("close did not ask");
  await click("close");
  await until("one pane", "document.querySelectorAll('#tabs .pane').length === 0", 8000);
  if (list().some((p) => p.pane_id === a.id)) fail("the pane was not closed");
  out.zoom_close = "ok";
  await click("new-tab");
  await until("three tabs", "document.querySelectorAll('#tabs .tab').length === 3", 8000);
  const newest = Math.max(...list().map((p) => p.pane_id));
  await until("moved to the new tab", `document.querySelector('#tabs .tab.current').dataset.pane === ${JSON.stringify(String(newest))}`);
  out.new_tab = newest;

  // --- reconnect keeps a split
  await click("split-below");
  await until("two panes again", "document.querySelectorAll('#tabs .pane').length === 2", 8000);
  sh(`${cli} web-server off`);
  await until("disconnected", "/reconnect/.test(document.getElementById('status').textContent)", 10000);
  await sleep(1200);
  sh(`${cli} web-server on`);
  await until("back", "/this browser has|mirroring/.test(document.getElementById('status').textContent)", 25000);
  await sleep(600);
  l = await layout();
  if (l.panes.length !== 2) fail("the split did not survive the reconnect: " + JSON.stringify(l));
  await type("echo BACK");
  if (!has(l.focused, "BACK")) fail("typing after the reconnect went nowhere");
  out.reconnect = "ok";

  const shot = await send("Page.captureScreenshot", { format: "png" }, s); fs.writeFileSync(outPng, Buffer.from(shot.data, "base64"));
  out.logs = logs.filter((l) => !/^\s*$/.test(l) && !/INFO/.test(l)).slice(-6);
  console.log(JSON.stringify(out));
  ws.close(); chrome.kill(); process.exit(0);
})().catch((e) => { console.error(e); chrome.kill(); process.exit(1); });
