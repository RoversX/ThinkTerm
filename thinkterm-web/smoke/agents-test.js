// The Agents panel end to end, against a throwaway server: the settings
// switch puts it on the right and the terminal gives up the room for it,
// its edge drags without resizing the terminal on every frame, turning it
// off gives the room back, and the tab row's button is the same switch.
//   node agents-test.js <url> <out.png>
const { spawn } = require("child_process");
const http = require("http"); const fs = require("fs"); const os = require("os");
const WebSocket = require("ws");
const [url, outPng] = process.argv.slice(2);
const chromePath = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 9333 + Math.floor(Math.random() * 500);
setTimeout(() => { console.error("watchdog"); try { chrome.kill(); } catch {} process.exit(4); }, 150000).unref();
const profile = fs.mkdtempSync(`${os.tmpdir()}/tt-agents-`);
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
  const until = async (what, fn, ms = 15000) => { const t = Date.now(); while (Date.now() - t < ms) { const v = await fn(); if (v) return v; await sleep(120); } throw new Error("timeout waiting for " + what); };
  const click = (sel) => ev(`(() => { const el = document.querySelector(${JSON.stringify(sel)}); if (!el) return false; el.click(); return true; })()`);
  const mouse = (type, x, y, extra = {}) => send("Input.dispatchMouseEvent", { type, x, y, button: "left", clickCount: 1, ...extra }, s);
  const fail = (what) => { throw new Error(what); };
  const attached = async () => /this browser has|mirroring|following/.test(await ev("document.getElementById('status').dataset.summary || ''"));
  const canvasWidth = () => ev("document.querySelector('canvas#term').getBoundingClientRect().width");
  const agentsW = () => ev("getComputedStyle(document.documentElement).getPropertyValue('--agents-w').trim()");
  // The switch lives in the settings panel; the gear opens it, and the
  // backdrop is over the page while it is up, so it is closed again before
  // anything is dragged.
  const toggleFromSettings = async () => {
    if (!(await click("#side [data-action=settings]"))) fail("no gear in the sidebar footer");
    await until("the settings panel", () => ev("!!document.getElementById('settings')"));
    await click("#settings [data-section=agents]");
    if (!(await click("#settings input[data-setting=agents-panel]"))) fail("no agents-panel switch");
    if (!(await click("#settings .sc"))) fail("no close button");
    await until("the settings panel to close", () => ev("!document.getElementById('settings')"));
  };
  const out = {};

  await send("Page.navigate", { url }, s);
  await until("attach", attached, 20000);
  await sleep(900);
  if (await ev("!!document.getElementById('agents')")) fail("the panel is on before anything asked for it");
  const wide = await canvasWidth();
  out.canvas_without = wide;

  // --- (a) the switch puts the panel up and the terminal gives up the room.
  await toggleFromSettings();
  await until("the panel", () => ev("!!document.getElementById('agents')"));
  if (!(await ev("document.body.hasAttribute('data-agents')"))) fail("the body carries no data-agents mark");
  if (!(await ev("document.querySelector('#tabs [data-action=agents]').classList.contains('on')"))) fail("the tab row's button does not show the panel is on");
  out.width = await agentsW();
  if (out.width !== "260px") fail("the panel did not open at its default width: " + out.width);
  out.summary = await ev("document.querySelector('#agents .hd .sum').textContent");
  if (out.summary !== "No agents detected") fail("the header does not carry the wasm's summary: " + JSON.stringify(out.summary));
  out.title = await ev("document.querySelector('#agents .hd .ti').textContent");
  if (out.title !== "Agents") fail("the header is not titled: " + JSON.stringify(out.title));
  await until("the terminal to give up the room", async () => Math.abs(wide - (await canvasWidth()) - 260) < 1);
  out.canvas_with = await canvasWidth();
  out.panel = "ok";

  // --- (b) the edge drags without resizing the terminal on every frame.
  // The panel clips what overflows it, so only the part of the 4px handle
  // inside its padding box can be pressed: the first two columns, the 1px
  // border taking the one before them.
  const grab = await ev(`(() => { const aside = document.getElementById('agents');
    const x = Math.floor(aside.getBoundingClientRect().right - aside.clientWidth) + 1;
    const el = document.elementFromPoint(x, 400);
    return { x, on: el ? (el.className || el.id || el.tagName) : null }; })()`);
  if (grab.on !== "handle") fail(`${grab.x},400 is not the handle but ` + JSON.stringify(grab.on));
  // 40 px further left is 40 px more panel; the pointer's distance from the
  // right edge is the width, so this is where it has to end up.
  const target = (await ev("window.innerWidth")) - 300;
  // Every change of the canvas's backing store, which is what one terminal
  // resize costs; the count is read before the release and after it.
  await ev(`(() => { const c = document.querySelector('canvas#term'); window.__resizes = 0; window.__last = c.width;
    new MutationObserver(() => { if (c.width !== window.__last) { window.__last = c.width; window.__resizes++; } }).observe(c, { attributes: true, attributeFilter: ['width'] });
    return c.width; })()`);
  await mouse("mousePressed", grab.x, 400, { buttons: 1 });
  for (let x = grab.x - 8; x > target; x -= 8) { await mouse("mouseMoved", x, 400, { buttons: 1 }); await sleep(70); }
  await mouse("mouseMoved", target, 400, { buttons: 1 });
  await sleep(300);
  out.resizes_during_drag = await ev("window.__resizes");
  const midWidth = await agentsW();
  await mouse("mouseReleased", target, 400, { buttons: 0 });
  await sleep(900);
  out.resizes_after_release = (await ev("window.__resizes")) - out.resizes_during_drag;
  out.agents_w = await agentsW();
  out.stored = await ev("localStorage.getItem('thinkterm.agents-width')");
  if (midWidth !== "300px") fail("the panel did not follow the pointer: " + midWidth);
  if (out.resizes_during_drag !== 0) fail("the terminal was resized " + out.resizes_during_drag + " times during the drag");
  if (out.resizes_after_release !== 1) fail("the release should resize the terminal exactly once, not " + out.resizes_after_release);
  if (out.agents_w !== "300px") fail("the width ended at " + out.agents_w);
  if (out.stored !== "300") fail("the width stored is " + JSON.stringify(out.stored));
  out.drag = "ok";

  const shot = await send("Page.captureScreenshot", { format: "png" }, s); fs.writeFileSync(outPng, Buffer.from(shot.data, "base64"));

  // --- (c) turning it off gives the room back.
  await toggleFromSettings();
  await until("the panel to go", () => ev("!document.getElementById('agents')"));
  if (await ev("document.body.hasAttribute('data-agents')")) fail("the body kept its data-agents mark");
  await until("the terminal to take the room back", async () => Math.abs((await canvasWidth()) - wide) < 1);
  out.canvas_after = await canvasWidth();
  out.off = "ok";

  // --- (d) the tab row's button is the same switch.
  if (!(await click("#tabs [data-action=agents]"))) fail("no agents button in the tab row");
  await until("the panel again", () => ev("!!document.getElementById('agents')"));
  if ((await agentsW()) !== "300px") fail("the panel did not come back at the width it was dragged to");
  if (!(await ev("JSON.parse(window.thinkterm.client.settings())['agents-panel']")))
    fail("the button did not put the preference on");
  await click("#tabs [data-action=agents]");
  await until("the panel to go again", () => ev("!document.getElementById('agents')"));
  out.button = "ok";

  out.logs = logs.filter((l) => !/^\s*$/.test(l) && !/INFO/.test(l)).slice(-6);
  console.log(JSON.stringify(out));
  ws.close(); chrome.kill(); process.exit(0);
})().catch((e) => { console.error(e); chrome.kill(); process.exit(1); });
