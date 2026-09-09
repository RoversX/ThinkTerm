// The phone shape end to end, against a throwaway server, driven with a
// finger: the page comes up in its mobile shape with the key bar along the
// bottom and the canvas sized around it, a tap takes the terminal over and
// asks for the soft keyboard, the bar's keys and its sticky Ctrl reach the
// pane, a drag scrolls instead of selecting, a swipe from the left edge
// brings the drawer out and the backdrop puts it away, and a long press
// opens the pane's menu.
//   node mobile-test.js <url> <cli prefix> <out.png>
const { spawn, execSync } = require("child_process");
const http = require("http"); const fs = require("fs"); const os = require("os");
const WebSocket = require("ws");
const [url, cli, outPng] = process.argv.slice(2);
const chromePath = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 9333 + Math.floor(Math.random() * 500);
setTimeout(() => { console.error("watchdog"); try { chrome.kill(); } catch {} process.exit(4); }, 150000).unref();
const profile = fs.mkdtempSync(`${os.tmpdir()}/tt-mobile-`);
const chrome = spawn(chromePath, ["--headless=new", "--no-first-run", `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, "--window-size=430,920", "--hide-scrollbars", "--enable-unsafe-webgpu", "about:blank"], { stdio: "ignore" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function browserWs() { for (let i = 0; i < 50; i++) { try { return await new Promise((res, rej) => http.get(`http://127.0.0.1:${port}/json/version`, (r) => { let b = ""; r.on("data", (d) => b += d); r.on("end", () => res(JSON.parse(b).webSocketDebuggerUrl)); }).on("error", rej)); } catch { await sleep(200); } } throw new Error("chrome did not come up"); }
const sh = (cmd) => execSync(cmd, { encoding: "utf8" });
/** The phone the page is emulated on. */
const PHONE = { width: 390, height: 844, deviceScaleFactor: 3, mobile: true };
/** The key bar's height, as tokens.css and mobile.svelte.ts have it. */
const BAR = 44;

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
  const summary = () => ev("document.getElementById('status')?.dataset.summary || ''");
  const until = async (what, fn, ms = 15000) => { const t = Date.now(); while (Date.now() - t < ms) { const v = await fn(); if (v) return v; await sleep(120); } throw new Error("timeout waiting for " + what + ": status=" + (await ev("document.getElementById('status')?.textContent")) + " summary=" + (await summary())); };
  const fail = (what) => { throw new Error(what); };
  // A finger. touchEnd releases every point, so it carries none.
  const touch = (type, points) => send("Input.dispatchTouchEvent", { type, touchPoints: points }, s);
  const at = (x, y, i = 0) => ({ x: Math.round(x), y: Math.round(y), id: i });
  const tap = async (x, y) => { await touch("touchStart", [at(x, y)]); await sleep(60); await touch("touchEnd", []); };
  // The key bar scrolls sideways: a key past the right edge of a 390 px
  // phone has to be brought into view before a finger can reach it.
  const tapOn = async (sel) => {
    const box = await ev(`(() => { const el = document.querySelector(${JSON.stringify(sel)}); if (!el) return null;
      el.scrollIntoView({ block: 'nearest', inline: 'center' }); const r = el.getBoundingClientRect();
      return [r.left + r.width / 2, r.top + r.height / 2, window.innerWidth, window.innerHeight]; })()`);
    if (!box) fail("nothing to tap at " + sel);
    if (box[0] < 0 || box[0] > box[2] || box[1] < 0 || box[1] > box[3]) fail(sel + " is off the screen at " + box[0] + "," + box[1]);
    await tap(box[0], box[1]);
  };
  // A key the way a hardware or soft keyboard sends one, into #kbd.
  const typeKey = async (ch) => {
    const code = /^[a-z]$/.test(ch) ? "Key" + ch.toUpperCase() : "";
    const vk = ch.toUpperCase().charCodeAt(0);
    await send("Input.dispatchKeyEvent", { type: "keyDown", text: ch, key: ch, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk }, s);
    await send("Input.dispatchKeyEvent", { type: "keyUp", key: ch, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk }, s);
  };
  const text = (pane) => sh(`${cli} get-text --pane-id ${pane}`);
  const lastLine = (pane) => text(pane).split("\n").map((l) => l.replace(/\s+$/, "")).filter((l) => l !== "").pop() ?? "";
  const out = {};

  // The phone, and its touchscreen, before anything is loaded: the page
  // decides its shape as it mounts and never sees the desktop's window.
  await send("Emulation.setDeviceMetricsOverride", PHONE, s);
  await send("Emulation.setTouchEmulationEnabled", { enabled: true }, s);
  await send("Page.navigate", { url }, s);
  await until("attach", async () => /this browser has|mirroring|following/.test(await summary()), 25000);
  await sleep(900);

  // --- (a) the shape: the mark on the body, the bar at the foot of the
  // window, and a canvas that stops above it.
  if (!(await ev("document.body.dataset.mobile !== undefined"))) fail("the page is not in its mobile shape");
  out.mode = "ok";

  const bar = await ev(`(() => { const el = document.getElementById('keybar'); if (!el) return null; const r = el.getBoundingClientRect();
    return { bottom: r.bottom, height: r.height, keys: document.querySelectorAll('#keybar .k').length, inner: window.innerHeight }; })()`);
  if (!bar) fail("no key bar");
  if (bar.inner !== PHONE.height) fail("the window is " + bar.inner + " tall, not the phone's " + PHONE.height);
  if (Math.abs(bar.bottom - bar.inner) > 1) fail("the key bar ends at " + bar.bottom + ", not at " + bar.inner);
  if (Math.abs(bar.height - BAR) > 1) fail("the key bar is " + bar.height + " tall");
  if (bar.keys !== 17) fail("the key bar has " + bar.keys + " keys");
  const box = await ev(`(() => { const r = document.getElementById('term').getBoundingClientRect();
    const tabs = parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--tabs'));
    return { height: r.height, want: window.innerHeight - tabs - ${BAR}, width: r.width, inner: window.innerWidth }; })()`);
  if (Math.abs(box.height - box.want) > 2) fail("the canvas is " + box.height + " tall, not " + box.want);
  if (Math.abs(box.width - box.inner) > 1) fail("the canvas is " + box.width + " wide, not the window's " + box.inner);
  out.keybar = "ok";

  // --- (b) a tap in the terminal: the wasm's own press takes it over, and
  // the page asks for the soft keyboard.
  const l = await until("a laid-out tab", async () => { const v = await layout(); return v.panes && v.panes.length === 1 ? v : null; });
  const pane = l.panes[0].id;
  // The grid starts a cell in from the left and half a cell down (App::pad);
  // a few rows below the pane's own bar, mid-pane.
  const point = await ev(`(() => { const r = document.getElementById('term').getBoundingClientRect();
    const l = JSON.parse(document.getElementById('term').dataset.layout); const p = l.panes[0];
    return [r.left + l.cell[0] * (1 + p.left + Math.floor(p.cols / 2)), r.top + l.cell[1] * (0.5 + p.top + 5)]; })()`);
  await ev("document.getElementById('kbd').blur()");
  await tap(point[0], point[1]);
  await sleep(500);
  if (!/this browser has/.test(await summary())) fail("the tap did not take the terminal over: " + (await summary()));
  if (!(await ev("document.activeElement?.id === 'kbd'"))) fail("the tap did not ask for the soft keyboard: " + (await ev("document.activeElement?.id || document.activeElement?.tagName")));
  out.tap = "ok";

  // --- (c) a key from the bar reaches the pane.
  const before = lastLine(pane);
  await tapOn('#keybar .k[data-key="-"]');
  await until("the dash in the pane", async () => lastLine(pane).endsWith("-"), 3000);
  out.key = "ok";

  // A soft keyboard's own keys still go through the wasm's field.
  await typeKey("l"); await typeKey("s");
  await until("what was typed", async () => /-ls/.test(text(pane)), 3000);
  out.typed = "ok";

  // --- (d) Ctrl is sticky: one tap arms it, the next key on #kbd spends it,
  // and the pane gets Ctrl+C rather than a plain c.
  await tapOn('#keybar .k[data-key="Control"]');
  await sleep(120);
  if (!(await ev(`!!document.querySelector('#keybar .k[data-key="Control"].armed')`))) fail("Ctrl did not arm");
  await typeKey("c");
  await sleep(200);
  if (await ev(`!!document.querySelector('#keybar .k[data-key="Control"].armed')`)) fail("Ctrl stayed armed after the key that spent it");
  const after = await until("the interrupt", async () => { const line = lastLine(pane); return line !== before + "-ls" || /\^C/.test(text(pane)) ? line : null; }, 4000);
  if (after.endsWith("c")) fail("a plain c was typed: " + JSON.stringify(after));
  if (/-ls$/.test(after)) fail("the interrupt never reached the pane: " + JSON.stringify(after));
  out.ctrl = "ok";

  // --- (e) a drag scrolls. The wasm publishes no viewport offset, so what
  // is checked here is the page's side of it: the wheel notches it hands
  // over, and that the wasm never saw the drag as a selection or a click in
  // another cell (its focus, and the pane's text, are where they were).
  sh(`printf 'seq 1 200\\n' | ${cli} send-text --pane-id ${pane} --no-paste`);
  await sleep(1200);
  const settled = text(pane);
  const focused = (await layout()).focused;
  await ev("window.__wheels = 0; document.getElementById('term').addEventListener('wheel', () => { window.__wheels++; }, true);");
  await touch("touchStart", [at(point[0], point[1])]);
  for (let dy = 10; dy <= 200; dy += 10) { await touch("touchMove", [at(point[0], point[1] - dy)]); await sleep(16); }
  await touch("touchEnd", []);
  await sleep(400);
  out.wheels = await ev("window.__wheels");
  if ((await layout()).focused !== focused) fail("the drag moved the focus");
  if (text(pane) !== settled) fail("the drag typed into the pane");
  // The offset the wasm scrolled to is not in the layout it publishes, so
  // the scroll itself cannot be read back from the page.
  out.scroll = out.wheels > 0 ? "skipped" : "no wheels";
  if (!(out.wheels > 0)) fail("the drag handed over no wheel notches");

  // --- (f) the drawer: a swipe in from the left edge brings it out, the
  // backdrop puts it away.
  await touch("touchStart", [at(4, 420)]);
  for (const x of [30, 70, 110, 160, 210]) { await touch("touchMove", [at(x, 420)]); await sleep(24); }
  await touch("touchEnd", []);
  await until("the drawer out", () => ev("!!document.querySelector('#side.open')"), 3000);
  const drawer = await ev("(() => { const r = document.getElementById('side').getBoundingClientRect(); return { w: r.width, top: r.top, bottom: r.bottom }; })()");
  if (!(drawer.w > 0 && drawer.w <= 300)) fail("the drawer is " + drawer.w + " wide");
  if (!(await ev("!!document.getElementById('scrim')"))) fail("no backdrop behind the drawer");
  await tap(360, 420);
  await until("the drawer away", () => ev("!document.querySelector('#side.open')"), 3000);
  if (await ev("!!document.getElementById('scrim')")) fail("the backdrop outlived the drawer");
  out.drawer = "ok";

  // --- (g) a long press on the canvas is the pane's menu.
  await touch("touchStart", [at(point[0], point[1])]);
  await sleep(700);
  await touch("touchEnd", []);
  await until("the pane's menu", () => ev("!!document.getElementById('menu')"), 3000);
  const labels = await ev("Array.from(document.querySelectorAll('.menu .mi .lb')).map((e) => e.textContent)");
  if (labels[0] !== "Copy") fail("the long press opened " + JSON.stringify(labels));
  const row = await ev("(() => { const r = document.querySelector('.menu .mi').getBoundingClientRect(); return r.height; })()");
  if (Math.abs(row - 40) > 1) fail("a menu row is " + row + " tall, not the 40 a finger needs");
  out.longpress = "ok";

  const shot = await send("Page.captureScreenshot", { format: "png" }, s); fs.writeFileSync(outPng, Buffer.from(shot.data, "base64"));
  out.logs = logs.filter((x) => !/^\s*$/.test(x) && !/INFO/.test(x)).slice(-6);
  console.log(JSON.stringify(out));
  ws.close(); chrome.kill(); process.exit(0);
})().catch((e) => { console.error(e); chrome.kill(); process.exit(1); });
