// The sidebar's page-side behaviours, which the end-to-end sidebar test
// cannot see: the inline name field survives the re-listings that arrive
// while it is being typed into, dragging the panel's edge does not resize
// the terminal on every frame, and a put-away panel comes back on hover as
// an overlay that resizes nothing.
//   node sidebar-drag-test.js <url> <out.png>
const { spawn } = require("child_process");
const http = require("http"); const fs = require("fs"); const os = require("os");
const WebSocket = require("ws");
const [url, outPng] = process.argv.slice(2);
const chromePath = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 9333 + Math.floor(Math.random() * 500);
setTimeout(() => { console.error("watchdog"); try { chrome.kill(); } catch {} process.exit(4); }, 120000).unref();
const profile = fs.mkdtempSync(`${os.tmpdir()}/tt-drag-`);
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
  const rows = async () => JSON.parse(await ev("document.getElementById('side')?.dataset.rows || '[]'"));
  const threads = async () => (await rows()).filter((r) => r.kind === "thread");
  const until = async (what, fn, ms = 15000) => { const t = Date.now(); while (Date.now() - t < ms) { const v = await fn(); if (v) return v; await sleep(120); } throw new Error("timeout waiting for " + what + ": rows=" + JSON.stringify(await rows())); };
  const click = (sel) => ev(`(() => { const el = document.querySelector(${JSON.stringify(sel)}); if (!el) return false; el.click(); return true; })()`);
  const dblclick = (sel) => ev(`(() => { const el = document.querySelector(${JSON.stringify(sel)}); if (!el) return false; el.dispatchEvent(new MouseEvent('click', { bubbles: true, detail: 2 })); return true; })()`);
  const key = async (k, code, vk) => { await send("Input.dispatchKeyEvent", { type: "rawKeyDown", key: k, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk }, s); await send("Input.dispatchKeyEvent", { type: "keyUp", key: k, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk }, s); };
  const type = async (text) => { for (const c of text) { await send("Input.dispatchKeyEvent", { type: "keyDown", text: c, key: c, code: "Key" + c.toUpperCase(), windowsVirtualKeyCode: c.toUpperCase().charCodeAt(0) }, s); await send("Input.dispatchKeyEvent", { type: "keyUp", key: c, code: "Key" + c.toUpperCase(), windowsVirtualKeyCode: c.toUpperCase().charCodeAt(0) }, s); await sleep(60); } };
  const mouse = (type, x, y, extra = {}) => send("Input.dispatchMouseEvent", { type, x, y, button: "left", clickCount: 1, ...extra }, s);
  const fail = (what) => { throw new Error(what); };
  const out = {};

  await send("Page.navigate", { url }, s);
  await until("attach", async () => /this browser has/.test(await ev("document.getElementById('status').dataset.summary || ''")), 20000);
  await sleep(800);

  // --- (a) the field being typed into survives the panel's re-listings.
  await click("[data-action=new-thread]");
  const thread = await until("landing thread on show", async () => (await threads()).find((t) => t.selected));
  await dblclick(`[data-action=rename-thread][data-thread="${thread.id}"]`);
  await until("rename field", () => ev("!!document.querySelector('#side input.rename')"));
  // A mark on the element itself: a field that was thrown away and made
  // again during the typing comes back without it.
  await ev("document.querySelector('#side input.rename').dataset.probe = 'same'");
  if (!(await ev("document.activeElement === document.querySelector('#side input.rename')"))) fail("the rename field did not take focus");
  await type("abc");
  out.typing = await ev(`(() => { const a = document.activeElement; const i = document.querySelector('#side input.rename');
    return { same_element: a === i, kept_mark: a?.dataset?.probe === 'same', value: a?.value, listings: JSON.parse(document.getElementById('side').dataset.rows).length }; })()`);
  // select() ran when the field appeared, so the first character replaced
  // the whole name rather than being appended to it.
  if (!out.typing.same_element || !out.typing.kept_mark) fail("the rename field was re-mounted while it was typed into: " + JSON.stringify(out.typing));
  if (out.typing.value !== "abc") fail("what was typed did not survive: " + JSON.stringify(out.typing));
  out.typing = "ok";

  // Escape drops it: the name is the one the server has.
  await key("Escape", "Escape", 27);
  await until("field gone", () => ev("!document.querySelector('#side input.rename')"));
  if ((await threads()).find((t) => t.id === thread.id)?.name !== thread.name) fail("Escape renamed the thread anyway");
  out.escape = "ok";

  // --- (b) the edge drags without resizing the terminal on every frame.
  if ((await ev("getComputedStyle(document.documentElement).getPropertyValue('--side-w').trim()")) !== "220px") fail("the panel did not start at its default width");
  // The panel clips what overflows it, so only the part of the 4px handle
  // inside the panel's padding box can be pressed: at a width of 220 that
  // is 217..219, the 1px border taking the last pixel. The press lands on
  // the last column of what is left.
  const grab = await ev(`(() => { const h = document.querySelector('#side .handle'); const side = document.getElementById('side');
    const clip = side.getBoundingClientRect().left + side.clientWidth;
    const x = Math.floor(Math.min(h.getBoundingClientRect().right, clip)) - 1;
    const el = document.elementFromPoint(x, 400);
    return { x, on: el ? (el.className || el.id || el.tagName) : null }; })()`);
  if (grab.on !== "handle") fail(`${grab.x},400 is not the handle but ` + JSON.stringify(grab.on));
  // Every change of the canvas's backing store, which is what one terminal
  // resize costs; the count is read before the release and after it.
  await ev(`(() => { const c = document.querySelector('canvas#term'); window.__resizes = 0; window.__last = c.width;
    new MutationObserver(() => { if (c.width !== window.__last) { window.__last = c.width; window.__resizes++; } }).observe(c, { attributes: true, attributeFilter: ['width'] });
    return c.width; })()`);
  await mouse("mousePressed", grab.x, 400, { buttons: 1 });
  for (let x = 215; x >= 180; x -= 5) { await mouse("mouseMoved", x, 400, { buttons: 1 }); await sleep(70); }
  await sleep(300);
  out.resizes_during_drag = await ev("window.__resizes");
  const midWidth = await ev("getComputedStyle(document.documentElement).getPropertyValue('--side-w').trim()");
  await mouse("mouseReleased", 180, 400, { buttons: 0 });
  await sleep(900);
  out.resizes_after_release = (await ev("window.__resizes")) - out.resizes_during_drag;
  out.side_w = await ev("getComputedStyle(document.documentElement).getPropertyValue('--side-w').trim()");
  out.stored = await ev("localStorage.getItem('thinkterm.sidebar')");
  if (midWidth !== "180px") fail("the panel did not follow the pointer: " + midWidth);
  if (out.resizes_during_drag < 1) fail("the terminal did not follow the drag");
  if (out.resizes_after_release > 1) fail("the release resized the terminal " + out.resizes_after_release + " times");
  if (out.side_w !== "180px") fail("the width ended at " + out.side_w);
  if (out.stored !== "180") fail("the width stored is " + JSON.stringify(out.stored));
  out.drag = "ok";

  // --- (c) the collapsed panel comes back on hover, over the canvas.
  if (!(await click("#tabs [data-action=sidebar]"))) fail("no sidebar toggle in the tab row");
  await until("the panel put away", () => ev("document.body.dataset.side === 'off'"));
  await sleep(600);
  const bare = await ev("document.querySelector('canvas#term').getBoundingClientRect().width");
  const revealed = () => ev("!!document.querySelector('#side.revealed')");
  const canvas = () => ev("document.querySelector('canvas#term').getBoundingClientRect().width");
  const centre = await ev("Math.round(window.innerWidth / 2)");

  // The window's left edge: 150 ms of the pointer resting there.
  await mouse("mouseMoved", 2, 400, { button: "none", buttons: 0 });
  await sleep(300);
  if (!(await revealed())) fail("the left edge did not bring the panel back");
  if (Math.abs((await canvas()) - bare) > 0.5) fail("the reveal resized the terminal: " + (await canvas()) + " vs " + bare);
  // The layout's width is the one the body carries while the panel is put
  // away; the root keeps the width the reveal paints at.
  if ((await ev("getComputedStyle(document.body).getPropertyValue('--side-w').trim()")) !== "0px") fail("the reveal moved the layout's width");
  out.reveal_width = await ev("document.getElementById('side').getBoundingClientRect().width");
  if (out.reveal_width <= 0) fail("the revealed panel has no width");

  // Away from it, and it retreats 250 ms later.
  await mouse("mouseMoved", centre, 400, { button: "none", buttons: 0 });
  await sleep(400);
  if (await revealed()) fail("the panel stayed out after the pointer left it");

  // The toggle itself is the other trigger.
  const toggle = await ev(`(() => { const r = document.querySelector('#tabs [data-action=sidebar]').getBoundingClientRect();
    return { x: Math.round(r.left + r.width / 2), y: Math.round(r.top + r.height / 2) }; })()`);
  await mouse("mouseMoved", toggle.x, toggle.y, { button: "none", buttons: 0 });
  await sleep(300);
  if (!(await revealed())) fail("hovering the toggle did not bring the panel back");

  // Pressing it pins the panel open the ordinary way, and the reveal is over.
  await click("#tabs [data-action=sidebar]");
  await until("the panel pinned open", () => ev("document.body.dataset.side !== 'off'"));
  if (await revealed()) fail("the reveal outlived the press that pinned the panel open");
  out.hover = "ok";

  const shot = await send("Page.captureScreenshot", { format: "png" }, s); fs.writeFileSync(outPng, Buffer.from(shot.data, "base64"));
  out.logs = logs.filter((l) => !/^\s*$/.test(l) && !/INFO/.test(l)).slice(-6);
  console.log(JSON.stringify(out));
  ws.close(); chrome.kill(); process.exit(0);
})().catch((e) => { console.error(e); chrome.kill(); process.exit(1); });
