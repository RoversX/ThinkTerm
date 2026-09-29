// A plugin's panel in a browser, against a throwaway server with the diff
// plugin installed and its terminal in a repository made for the test: the
// right panel's selector offers the plugin's panel; the page paints the
// repository's changed files; the pointer tints a file's row, rounded; a
// click on a file reaches the plugin, which asks for its extended view,
// and the page opens it left of the panel, the terminal making room, and
// paints the file's lines there; the wheel scrolls its ten thousand
// changed lines at once and the rows that come into view are asked for and
// painted, and a finger drags them too; a wide line scrolls sideways under
// still line numbers; the extended view's edge sizes it, kept for the
// plugin; ThinkTerm's close button at its top left closes it, and the
// plugin puts the file down; the panel goes with its tab, the plugin told;
// and on a phone, the panel a drawer with no room beside it, a tap picks a
// file whose lines the panel shows itself, and a finger scrolls them.
//   node panels-test.js <url> <out.png>   (and <out>-phone.png)
const { spawn, execSync } = require("child_process");
const http = require("http"); const fs = require("fs"); const os = require("os");
const WebSocket = require("ws");
const [url, outPng] = process.argv.slice(2);
const chromePath = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 9333 + Math.floor(Math.random() * 500);
setTimeout(() => { console.error("watchdog"); try { chrome.kill(); } catch {} process.exit(4); }, 120000).unref();
const profile = fs.mkdtempSync(`${os.tmpdir()}/tt-panels-`);
const chrome = spawn(chromePath, ["--headless=new", "--no-first-run", `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, "--window-size=1400,900", "--hide-scrollbars", "--enable-unsafe-webgpu", "about:blank"], { stdio: "ignore" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function browserWs() { for (let i = 0; i < 50; i++) { try { return await new Promise((res, rej) => http.get(`http://127.0.0.1:${port}/json/version`, (r) => { let b = ""; r.on("data", (d) => b += d); r.on("end", () => res(JSON.parse(b).webSocketDebuggerUrl)); }).on("error", rej)); } catch { await sleep(200); } } throw new Error("chrome did not come up"); }
const pids = (pattern) => { try { return execSync(`pgrep -f ${JSON.stringify(pattern)}`).toString().trim().split(/\s+/).filter(Boolean); } catch { return []; } };
// This server's copy of the plugin, not any other running on the machine.
const plugin = `${process.env.PLUGINS_DATA}/plugins/diff/thinkterm-plugin-diff`;

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
  const mouse = (type, x, y, extra = {}) => send("Input.dispatchMouseEvent", { type, x, y, button: "none", ...extra }, s);
  const fail = (what) => { throw new Error(what); };
  const attached = async () => /this browser has/.test(await ev("document.getElementById('status').dataset.summary || ''"));
  // What the panel, or its extended view, paints, as the wasm hands it to
  // the canvas.
  const panel = async (extended = false) => JSON.parse((await ev(`window.thinkterm.client.plugin_panel(${extended})`)) || "null");
  const beside = () => panel(true);
  const texts = (view) => (view?.painting?.ops ?? []).filter((op) => op.op === "text").map((op) => op.text);
  const textOp = (view, match) => (view?.painting?.ops ?? []).find((op) => op.op === "text" && match(op.text));
  const canvasOf = (extended) => extended ? "#extended .plugin canvas" : "#agents .plugin canvas";
  const box = (sel) => ev(`(() => { const e = document.querySelector(${JSON.stringify(sel)}); if (!e) return null; const r = e.getBoundingClientRect(); return { x: r.left, y: r.top, w: r.width, h: r.height }; })()`);
  // Where a text op is on the screen, from the canvas's corner.
  const where = async (text, extended = false) => {
    const op = textOp(await panel(extended), (t) => t === text);
    if (!op) return null;
    const at = await box(canvasOf(extended));
    return { x: at.x + op.x + 4, y: at.y + op.y + op.h / 2 };
  };
  const press = async (at) => {
    await mouse("mousePressed", at.x, at.y, { button: "left", clickCount: 1 });
    await mouse("mouseReleased", at.x, at.y, { button: "left", clickCount: 1 });
  };
  const out = {};

  await send("Page.navigate", { url }, s);
  await until("attach", attached, 20000);
  await sleep(600);

  // --- (a) The right panel, up, offers the plugin's panel under its name.
  if (!(await click("#side [data-action=settings]"))) fail("no gear in the sidebar footer");
  await until("the settings panel", () => ev("!!document.getElementById('settings')"));
  await click("#settings [data-section=sidebar]");
  if (!(await click("#settings input[data-setting=agents-panel]"))) fail("no agents-panel switch");
  await click("#settings .sc");
  await until("the right panel", () => ev("!!document.getElementById('agents')"));
  await until("the plugin's tab", () => ev("!!document.querySelector('#agents .mode[data-mode=\"plugin:diff\"]')"));
  out.tab = await ev("document.querySelector('#agents .mode[data-mode=\"plugin:diff\"]').getAttribute('title')");
  if (out.tab !== "Diff") fail("the tab is not under the plugin's name: " + out.tab);
  if (pids(plugin).length) fail("the plugin runs before its panel is on show");

  // --- (b) Its tab paints the repository's changes, from the terminal's
  // directory: the page told the plugin where that is.
  await click("#agents .mode[data-mode=\"plugin:diff\"]");
  try {
    await until("the changes", async () => texts(await panel()).includes("README.md"), 20000);
  } catch (e) {
    fail(e.message + ": the panel says " + JSON.stringify(texts(await panel())));
  }
  const first = await panel();
  out.first = { state: first.state, ops: first.painting.ops.length, files: texts(first).filter((t) => /\.(md|txt)$/.test(t)) };
  for (const name of ["repo", "README.md", "big.txt", "wide.txt", "new.txt"]) {
    if (!texts(first).includes(name)) fail(`${name} is not shown: ` + texts(first).slice(0, 30));
  }
  if (!texts(first).some((t) => /^main · 4 changed files$/.test(t))) fail("no branch and count: " + texts(first).slice(0, 10));
  // With room beside it, the panel lists the files alone: nothing is
  // picked, and nothing is extended.
  if (texts(first).some((t) => /^\d+$/.test(t))) fail("lines in the panel before a file is picked");
  if (await ev("!!document.getElementById('extended')")) fail("an extended view before a file is picked");
  const pixels = await ev("(() => { const c = document.querySelector('#agents .plugin canvas'); const d = c.getContext('2d').getImageData(0, 0, c.width, c.height).data; let n = 0; for (let i = 3; i < d.length; i += 4) if (d[i]) n++; return n; })()");
  if (!(pixels > 1000)) fail("the canvas is blank: " + pixels);
  out.painted_pixels = pixels;

  // --- (c) The pointer over a file tints its row at once, rounded like the
  // pick.
  const big = await where("big.txt");
  if (!big) fail("the rewritten file is not listed");
  await mouse("mouseMoved", big.x, big.y);
  const tint = await until("the hover", async () => (await panel()).painting.ops.find((op) => op.op === "rect" && op.fill === "bg-hover"));
  if (!(tint.radius > 0)) fail("the hover is square: " + JSON.stringify(tint));
  out.hover = { radius: tint.radius };

  // --- (d) A click reaches the plugin, which asks for its extended view:
  // the page opens it left of the panel, the terminal making room, and the
  // plugin shows the picked file there, its name large and its folder
  // below.
  const termBefore = (await box("#term")).w;
  await press(big);
  await until("the extended view", () => ev("!!document.getElementById('extended')"));
  await until("the picked file beside the panel", async () => {
    const shown = texts(await beside());
    return shown.includes("big.txt") && shown.includes("src");
  });
  const room = await box("#extended");
  const termAfter = (await box("#term")).w;
  if (Math.abs(room.w - 560) > 1) fail("the extended view is not as wide as it starts: " + room.w);
  if (Math.abs(termBefore - termAfter - room.w) > 2) fail(`the terminal made no room: ${termBefore} -> ${termAfter}`);
  if (texts(await panel()).some((t) => /^\d+$/.test(t))) fail("the lines are in the panel too");
  out.picked = { file: "src/big.txt", extended: room.w, terminal: [termBefore, termAfter] };

  // --- (e) The wheel scrolls its ten thousand changed lines at once, and
  // the rows that come into view are asked for and painted.
  const numbers = (view) => texts(view).filter((t) => /^\d+$/.test(t)).map(Number);
  await until("the first rows", async () => numbers(await beside()).length > 5);
  const before = Math.max(...numbers(await beside()));
  const diffArea = { x: room.x + room.w / 2, y: room.y + room.h - 60 };
  const started = Date.now();
  for (let i = 0; i < 20; i++) await send("Input.dispatchMouseEvent", { type: "mouseWheel", x: diffArea.x, y: diffArea.y, deltaX: 0, deltaY: 4000 }, s);
  await until("rows far down", async () => Math.max(...numbers(await beside()), 0) > before + 2000);
  out.scrolled = { before, after: Math.max(...numbers(await beside())), ms: Date.now() - started };

  // --- (e2) A finger drags the lines as the wheel does: up the glass is
  // down the file. Its pointer events are handed to the canvas from the
  // page: a browser with touch turned on is a phone to the page, which
  // shows no extended view.
  // The first line number on show, once there are lines on show.
  const topNumber = async () => {
    const shown = numbers(await beside());
    return shown.length > 0 ? Math.min(...shown) : null;
  };
  const beforeDrag = await until("lines on show", topNumber);
  await ev(`(() => {
    const canvas = document.querySelector('#extended .plugin canvas');
    const finger = (type, y) => canvas.dispatchEvent(new PointerEvent(type, {
      pointerId: 7, pointerType: 'touch', isPrimary: true, bubbles: true, clientX: ${diffArea.x}, clientY: y,
    }));
    finger('pointerdown', ${diffArea.y});
    for (let i = 1; i <= 10; i++) finger('pointermove', ${diffArea.y} - i * 30);
    finger('pointerup', ${diffArea.y} - 300);
    return true;
  })()`);
  await until("the finger to scroll the lines", async () => {
    const top = await topNumber();
    return top !== null && top > beforeDrag + 10;
  });
  out.touched = { from: beforeDrag, to: await topNumber() };

  // --- (f) A line wider than the extended view scrolls sideways; its
  // numbers stay.
  await press(await where("wide.txt"));
  const wideOp = (view) => textOp(view, (t) => t.startsWith("wide000"));
  await until("the wide line", async () => wideOp(await beside()));
  const numberOp = (view) => textOp(view, (t) => t === "2");
  const at = { wide: wideOp(await beside()).x, number: numberOp(await beside()).x };
  for (let i = 0; i < 4; i++) await send("Input.dispatchMouseEvent", { type: "mouseWheel", x: diffArea.x, y: diffArea.y, deltaX: 100, deltaY: 0 }, s);
  await until("the line to move", async () => (wideOp(await beside())?.x ?? at.wide) < at.wide - 300);
  const moved = await beside();
  if (numberOp(moved).x !== at.number) fail("the line numbers moved sideways too");
  const thumbAcross = moved.painting.ops.find((op) => op.op === "thumb" && op.w > op.h);
  if (!thumbAcross) fail("no thumb shows how far across");
  out.sideways = { line: [at.wide, wideOp(moved).x], number: at.number };

  await send("Page.captureScreenshot", { format: "png" }, s).then((r) => fs.writeFileSync(outPng, Buffer.from(r.data, "base64")));

  // --- (g) Its edge sizes it, within what the terminal leaves -- a fifth
  // of the window is the terminal's -- and the width is kept for the
  // plugin.
  const edge = { x: room.x + 1, y: room.y + room.h / 2 };
  await mouse("mousePressed", edge.x, edge.y, { button: "left", clickCount: 1 });
  for (let dx = 10; dx <= 30; dx += 10) await mouse("mouseMoved", edge.x - dx, edge.y, { button: "left", buttons: 1 });
  await mouse("mouseReleased", edge.x - 30, edge.y, { button: "left", clickCount: 1 });
  const wider = (await box("#extended")).w;
  if (Math.abs(wider - room.w - 30) > 2) fail("the edge did not size it: " + wider);
  await mouse("mousePressed", edge.x - 30, edge.y, { button: "left", clickCount: 1 });
  await mouse("mouseMoved", 0, edge.y, { button: "left", buttons: 1 });
  await mouse("mouseReleased", 0, edge.y, { button: "left", clickCount: 1 });
  const widest = (await box("#extended")).w;
  const terminal = (await box("#term")).w;
  if (terminal < 1400 / 5 - 2) fail(`the terminal was left ${terminal}`);
  // Back to the width it was.
  await mouse("mousePressed", room.x + room.w - widest + 1, edge.y, { button: "left", clickCount: 1 });
  await mouse("mouseMoved", edge.x - 30, edge.y, { button: "left", buttons: 1 });
  await mouse("mouseReleased", edge.x - 30, edge.y, { button: "left", clickCount: 1 });
  if (Math.abs((await box("#extended")).w - wider) > 2) fail("it did not come back: " + (await box("#extended")).w);
  const kept = JSON.parse((await ev("localStorage.getItem('thinkterm.plugin-extended-widths')")) || "{}");
  if (Math.abs((kept.diff ?? 0) - wider) > 1) fail("the width is not kept for the plugin: " + JSON.stringify(kept));
  await until("the view told its new width", async () => (await beside())?.painting?.clips?.some((c) => c[2] > room.w + 20));
  out.sized = { from: room.w, to: wider, widest, terminal, kept };

  // --- (h) ThinkTerm's close button, at its top left whatever the plugin
  // draws: the plugin keeps its header clear of it; pressed, the view goes,
  // the terminal taking its room back, and the plugin puts the file down.
  const button = await box("#extended .close");
  const canvas = await box(canvasOf(true));
  if (!button || button.x - room.x > 20 || button.y - room.y > 20) fail("no close button at the top left: " + JSON.stringify(button));
  const title = ((await beside())?.painting?.ops ?? []).find((op) => op.op === "text" && op.size === "title");
  if (!title || canvas.x + title.x < button.x + button.w) fail("the header is under the close button: " + JSON.stringify(title));
  if (texts(await beside()).includes("\u00d7")) fail("the plugin draws a close button of its own");
  await press({ x: button.x + button.w / 2, y: button.y + button.h / 2 });
  await until("the extended view to go", async () => !(await ev("!!document.getElementById('extended')")));
  await until("the terminal to take its room back", async () => Math.abs((await box("#term")).w - termBefore) < 2);
  if ((await beside()) !== null) fail("the extended view is still open");
  await until("the plugin to put the file down", async () => !(await ev("window.thinkterm.client.plugin_panel_extended()")));
  // Picked again, it comes back as wide as it was left.
  await press(await where("README.md"));
  await until("the extended view again", () => ev("!!document.getElementById('extended')"));
  const again = (await box("#extended")).w;
  if (Math.abs(again - wider) > 1) fail("not as wide as it was left: " + again);
  out.closed = { again };

  // --- (i) The panel goes with its tab, and its extended view with it:
  // the plugin is told.
  if (pids(plugin).length !== 1) fail("the plugin does not run while its panel is on show");
  await click("#agents .mode[data-mode=agents]");
  await until("the panel to go", async () => (await panel()) === null && (await beside()) === null);
  if (await ev("!!document.getElementById('extended')")) fail("the extended view stayed");
  out.gone = "ok";

  // --- (j) On a phone the right panel is a drawer with no room beside it,
  // and a finger is all there is: a tap picks a file, whose lines the panel
  // shows below the files, and a drag up the glass scrolls them.
  await send("Emulation.setDeviceMetricsOverride", { width: 390, height: 844, deviceScaleFactor: 3, mobile: true }, s);
  await send("Emulation.setTouchEmulationEnabled", { enabled: true, maxTouchPoints: 1 }, s);
  await until("the phone shape", () => ev("document.body.dataset.mobile !== undefined"));
  await until("the drawer", () => ev("!!document.querySelector('#agents .mode[data-mode=\"plugin:diff\"]')"));
  await click("#agents .mode[data-mode=\"plugin:diff\"]");
  await until("the files in the drawer", async () => texts(await panel()).includes("big.txt"), 20000);
  const finger = (type, at) => send("Input.dispatchTouchEvent", { type, touchPoints: type === "touchEnd" ? [] : [{ x: at.x, y: at.y }] }, s);
  const tapAt = await where("big.txt");
  await finger("touchStart", tapAt);
  await finger("touchEnd", tapAt);
  await until("the picked file in the panel", async () => {
    const view = await panel();
    return texts(view).includes("src/big.txt") && numbers(view).length > 5;
  });
  if (await ev("!!document.getElementById('extended')")) fail("an extended view on a phone");
  const drawer = await box(canvasOf(false));
  const low = { x: drawer.x + drawer.w / 2, y: drawer.y + drawer.h - 80 };
  const topInPanel = async () => {
    const shown = numbers(await panel());
    return shown.length > 0 ? Math.min(...shown) : null;
  };
  const fromTop = await topInPanel();
  await finger("touchStart", low);
  for (let i = 1; i <= 10; i++) await finger("touchMove", { x: low.x, y: low.y - i * 30 });
  await finger("touchEnd", { x: low.x, y: low.y - 300 });
  await until("the finger to scroll the drawer's lines", async () => {
    const top = await topInPanel();
    return top !== null && top > fromTop + 10;
  });
  out.phone = { drawer: drawer.w, from: fromTop, to: await topInPanel() };
  await send("Page.captureScreenshot", { format: "png" }, s).then((r) => fs.writeFileSync(outPng.replace(/(\.png)?$/, "-phone.png"), Buffer.from(r.data, "base64")));

  const exceptions = logs.filter((l) => l.startsWith("EXCEPTION"));
  if (exceptions.length) fail("the page threw: " + exceptions.join("; "));
  console.log(JSON.stringify(out, null, 1));
  ws.close(); chrome.kill(); process.exit(0);
})().catch((e) => { console.error("FAIL", e.message); try { chrome.kill(); } catch {} process.exit(1); });
