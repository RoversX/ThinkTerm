// The context menus end to end, against a throwaway server: a right-click
// on the canvas offers the pane's list and its Split Right really splits,
// a right-click on a thread row offers the thread's and its Pin really
// pins, a right-click on a capsule offers the tab's, and Escape closes the
// menu and hands the keyboard back to the terminal.
//   node menus-test.js <url> <cli prefix> <out.png>
const { spawn, execSync } = require("child_process");
const http = require("http"); const fs = require("fs"); const os = require("os");
const WebSocket = require("ws");
const [url, cli, outPng] = process.argv.slice(2);
const chromePath = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 9333 + Math.floor(Math.random() * 500);
setTimeout(() => { console.error("watchdog"); try { chrome.kill(); } catch {} process.exit(4); }, 120000).unref();
const profile = fs.mkdtempSync(`${os.tmpdir()}/tt-menus-`);
const chrome = spawn(chromePath, ["--headless=new", "--no-first-run", `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, "--window-size=1400,800", "--hide-scrollbars", "--enable-unsafe-webgpu", "about:blank"], { stdio: "ignore" });
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
  const rows = async () => JSON.parse(await ev("document.getElementById('side')?.dataset.rows || '[]'"));
  const threads = async () => (await rows()).filter((r) => r.kind === "thread");
  const layout = async () => JSON.parse(await ev("document.getElementById('term').dataset.layout || '{}'"));
  const until = async (what, fn, ms = 12000) => { const t = Date.now(); while (Date.now() - t < ms) { const v = await fn(); if (v) return v; await sleep(120); } throw new Error("timeout waiting for " + what + ": menu=" + JSON.stringify(await labels()) + " status=" + (await ev("document.getElementById('status').textContent"))); };
  const click = (sel) => ev(`(() => { const el = document.querySelector(${JSON.stringify(sel)}); if (!el) return false; el.click(); return true; })()`);
  const list = () => JSON.parse(sh(`${cli} list --format json`));
  const fail = (what) => { throw new Error(what); };
  // Every panel's rows, in order, the headers and separators aside.
  const labels = () => ev("Array.from(document.querySelectorAll('.menu .mi .lb')).map((e) => e.textContent)");
  const open = () => ev("!!document.getElementById('menu')");
  // A real right-click: the press the browser turns into a contextmenu.
  const rightClick = async (x, y) => {
    await send("Input.dispatchMouseEvent", { type: "mousePressed", x, y, button: "right", buttons: 2, clickCount: 1 }, s);
    await send("Input.dispatchMouseEvent", { type: "mouseReleased", x, y, button: "right", buttons: 0, clickCount: 1 }, s);
  };
  const rightClickOn = async (sel) => {
    const at = await ev(`(() => { const el = document.querySelector(${JSON.stringify(sel)}); if (!el) return null; const r = el.getBoundingClientRect();
      return [Math.round(r.left + Math.min(24, r.width / 2)), Math.round(r.top + r.height / 2)]; })()`);
    if (!at) fail("nothing to right-click at " + sel);
    await rightClick(at[0], at[1]);
  };
  const key = async (k, code, vk) => { await send("Input.dispatchKeyEvent", { type: "rawKeyDown", key: k, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk }, s); await send("Input.dispatchKeyEvent", { type: "keyUp", key: k, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk }, s); };
  const out = {};

  await send("Page.navigate", { url }, s);
  await until("attach", async () => /this browser has/.test(await ev("document.getElementById('status').dataset.summary || ''")), 20000);
  await sleep(900);

  // --- (a) the pane's menu, from a right-click in the terminal itself.
  let l = await until("a laid-out tab", async () => { const v = await layout(); return v.panes && v.panes.length === 1 ? v : null; });
  const tab = l.tab;
  // The grid starts a cell in from the left and half a cell down (App::pad);
  // a few rows below the pane's own bar, mid-pane.
  const point = await ev(`(() => { const r = document.getElementById('term').getBoundingClientRect();
    const l = JSON.parse(document.getElementById('term').dataset.layout); const p = l.panes[0];
    return [Math.round(r.left + l.cell[0] * (1 + p.left + Math.floor(p.cols / 2))), Math.round(r.top + l.cell[1] * (0.5 + p.top + 5))]; })()`);
  await rightClick(point[0], point[1]);
  await until("the pane's menu", open);
  out.pane_labels = await labels();
  const wanted = ["Copy", "Paste", "Split Right", "Split Left", "Split Down", "Split Up", "Frontend access"];
  if (JSON.stringify(out.pane_labels) !== JSON.stringify(wanted)) fail("the pane's menu is " + JSON.stringify(out.pane_labels));
  out.pane_menu = "ok";

  const before = list().filter((p) => p.tab_id === tab).length;
  if (!(await click('[data-id^="split:right:"]'))) fail("no Split Right row");
  await until("the menu is gone", async () => !(await open()));
  await sleep(1500);
  const panes = list().filter((p) => p.tab_id === tab);
  if (before !== 1 || panes.length !== 2) fail(`tab ${tab} went from ${before} to ${panes.length} panes`);
  await until("two placements", async () => (await layout()).panes?.length === 2);
  out.split = "ok";

  // --- (c) a capsule's menu: with one tab, only the tab it can offer.
  await rightClickOn("#tabs .tab");
  await until("the tab's menu", open);
  out.tab_labels = await labels();
  if (out.tab_labels[0] !== "New Terminal Tab to Right") fail("the tab's menu is " + JSON.stringify(out.tab_labels) + " over " + (await ev("document.querySelectorAll('#tabs .tab').length")) + " tabs");
  out.tab_menu = "ok";

  // --- (d) Escape closes it and the terminal has the keyboard again.
  if (await ev("document.activeElement.id === 'kbd'")) fail("the menu did not take the keyboard");
  await key("Escape", "Escape", 27);
  await until("the menu is gone", async () => !(await open()));
  if (!(await ev("document.activeElement.id === 'kbd'"))) fail("the keyboard did not go back to the terminal: " + (await ev("document.activeElement.id || document.activeElement.className")));
  out.escape = "ok";

  // --- (b) a thread row's menu, and the pin it offers.
  await click("[data-action=new-thread]");
  const thread = await until("the landing thread", async () => (await threads()).find((t) => t.selected), 15000);
  await rightClickOn(`.row.thread[data-thread="${thread.id}"]`);
  await until("the thread's menu", open);
  out.thread_labels = await labels();
  if (out.thread_labels[0] !== "Pin Thread") fail("the thread's menu is " + JSON.stringify(out.thread_labels));
  out.thread_menu = "ok";

  if (!(await click('[data-id^="pin:"]'))) fail("no Pin Thread row");
  await until("the menu is gone", async () => !(await open()));
  await until("pinned, under the Pinned header", async () => {
    const r = await rows();
    const header = r.findIndex((x) => x.kind === "pinned");
    const t = r.find((x) => x.kind === "thread" && x.id === thread.id);
    return header >= 0 && t && t.pinned && r.indexOf(t) > header;
  });
  out.pin = "ok";

  const shot = await send("Page.captureScreenshot", { format: "png" }, s); fs.writeFileSync(outPng, Buffer.from(shot.data, "base64"));
  out.logs = logs.filter((l) => !/^\s*$/.test(l) && !/INFO/.test(l)).slice(-6);
  console.log(JSON.stringify(out));
  ws.close(); chrome.kill(); process.exit(0);
})().catch((e) => { console.error(e); chrome.kill(); process.exit(1); });
