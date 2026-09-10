// End-to-end check of the page's drag and drop, driven through dnd.sh (not
// part of the served bundle). Two threads made through the panel: dragging
// the second above the first reorders the tree. Two tabs in the window:
// dragging the second before the first reorders the strip. A split pane:
// dragging one capsule onto the other pane's right half rearranges the
// layout (read from `canvas.dataset.layout`). Each drag is photographed
// mid-flight, with its ghost and either its insertion line or its overlay.
//
//   node dnd-test.js <url> <cli prefix> <out.png>
//
// `cli prefix` is a `thinkterm cli` pinned to the same server. Needs the
// `ws` npm package on NODE_PATH and Google Chrome installed.
const { spawn } = require("child_process");
const http = require("http"); const fs = require("fs"); const os = require("os"); const path = require("path");
const WebSocket = require("ws");
const [url, cli, outPng] = process.argv.slice(2);
void cli;
const chromePath = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 9333 + Math.floor(Math.random() * 500);
setTimeout(() => { console.error("watchdog"); try { chrome.kill(); } catch {} process.exit(4); }, 150000).unref();
const profile = fs.mkdtempSync(`${os.tmpdir()}/tt-dnd-`);
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
  const layout = async () => JSON.parse(await ev("document.getElementById('term').dataset.layout || '{}'"));
  const until = async (what, fn, ms = 15000) => { const t = Date.now(); while (Date.now() - t < ms) { const v = await fn(); if (v) return v; await sleep(120); } throw new Error("timeout waiting for " + what); };
  const click = (sel) => ev(`(() => { const el = document.querySelector(${JSON.stringify(sel)}); if (!el) return false; el.click(); return true; })()`);
  const rect = async (sel) => ev(`(() => { const el = document.querySelector(${JSON.stringify(sel)}); if (!el) return null; const r = el.getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2, r.left, r.top, r.width, r.height]; })()`);
  const mouse = (type, x, y, buttons) => send("Input.dispatchMouseEvent", { type, x, y, button: "left", buttons, clickCount: 1 }, s);
  const shot = async (name) => { const png = await send("Page.captureScreenshot", { format: "png" }, s); fs.writeFileSync(path.join(path.dirname(outPng), name), Buffer.from(png.data, "base64")); return name; };
  const fail = (what) => { throw new Error(what); };
  const out = {};

  /** Press at `from`, walk to `to` in steps so the page sees a drag, take a
      picture with the drag still up, then let go. */
  const dragTo = async (from, to, name) => {
    await mouse("mousePressed", from[0], from[1], 1);
    for (let i = 1; i <= 6; i++) {
      await mouse("mouseMoved", from[0] + ((to[0] - from[0]) * i) / 6, from[1] + ((to[1] - from[1]) * i) / 6, 1);
      await sleep(40);
    }
    await sleep(200);
    const seen = await ev("(() => { const g = document.querySelector('#dnd .ghost'); const l = document.querySelector('#dnd .line'); const z = document.querySelector('#dnd .zone'); return { ghost: !!g, line: !!l, zone: !!z }; })()");
    const file = name ? await shot(name) : null;
    await mouse("mouseReleased", to[0], to[1], 0);
    return { ...seen, shot: file };
  };

  await send("Page.navigate", { url }, s);
  await until("attach", async () => /this browser has/.test(await ev("document.getElementById('status').dataset.summary || ''")), 25000);
  await sleep(800);

  // --- two threads in one project, made through the panel
  await click("[data-action=new-thread]");
  const first = await until("first thread", async () => (await threads()).find((t) => t.selected));
  await click("[data-action=new-thread]");
  const second = await until("second thread", async () => (await threads()).find((t) => t.id !== first.id && t.project === first.project));
  await until("both on show", async () => (await threads()).filter((t) => t.project === first.project).length === 2);
  const order = async () => (await threads()).filter((t) => t.project === first.project).map((t) => t.id);
  const before = await order();
  if (before[0] !== first.id) fail("the threads did not start in the order they were made: " + JSON.stringify(before));

  // --- drag the second thread above the first
  const sel = (id) => `#side .row.thread[data-thread="${id}"]`;
  const src = await rect(sel(second.id));
  const dst = await rect(sel(first.id));
  out.thread_drag = await dragTo([src[0], src[1]], [dst[0], dst[3] + 3], "dnd-sidebar.png");
  if (!out.thread_drag.ghost || !out.thread_drag.line) fail("no ghost or insertion line mid-drag: " + JSON.stringify(out.thread_drag));
  await until("the tree reordered", async () => (await order())[0] === second.id);
  out.threads = { before, after: await order() };

  // --- two tabs in this window, the second dragged before the first
  await click("[data-action=new-tab]");
  await until("two tabs", () => ev("document.querySelectorAll('#tabs .tab').length === 2"));
  await sleep(500);
  const strip = () => ev("Array.from(document.querySelectorAll('#tabs .tab')).map((t) => t.dataset.tab).join(',')");
  const tabsBefore = await strip();
  const ids = tabsBefore.split(",");
  const tabSrc = await rect(`#tabs .tab[data-tab="${ids[1]}"]`);
  const tabDst = await rect(`#tabs .tab[data-tab="${ids[0]}"]`);
  out.tab_drag = await dragTo([tabSrc[0], tabSrc[1]], [tabDst[2] + 4, tabDst[1]], "dnd-tabs.png");
  if (!out.tab_drag.ghost || !out.tab_drag.line) fail("no ghost or insertion line over the strip: " + JSON.stringify(out.tab_drag));
  await until("the strip reordered", async () => (await strip()) === ids[1] + "," + ids[0], 20000);
  out.tabs = { before: tabsBefore, after: await strip() };

  // --- a split, then one capsule dropped on the other pane's right half
  await click("[data-action=split-right]");
  await until("two panes", async () => ((await layout()).panes || []).length === 2, 12000);
  await sleep(600);
  let l = await layout();
  const [left, right] = l.panes[0].left < l.panes[1].left ? [l.panes[0], l.panes[1]] : [l.panes[1], l.panes[0]];
  const arrangement = (lay) => lay.panes.slice().sort((a, b) => a.left - b.left).map((p) => p.id).join(",");
  const panesBefore = arrangement(l);
  const cap = await rect(`#panes .nav[data-nav="${left.id}"] .cap[data-pane="${left.id}"]`);
  const box = await ev("(r => [r.left, r.top])(document.getElementById('term').getBoundingClientRect())");
  // Three quarters across the right pane, below its bar: its right half.
  const tx = box[0] + (right.left + right.cols * 0.8) * l.cell[0];
  const ty = box[1] + (right.top + right.rows * 0.5) * l.cell[1];
  out.pane_drag = await dragTo([cap[0], cap[1]], [tx, ty], "dnd-pane.png");
  if (!out.pane_drag.ghost || !out.pane_drag.zone) fail("no ghost or drop overlay over the pane: " + JSON.stringify(out.pane_drag));
  await until("the layout rearranged", async () => { const now = await layout(); return (now.panes || []).length === 2 && arrangement(now) !== panesBefore; }, 20000);
  l = await layout();
  out.panes = { before: panesBefore, after: arrangement(l) };

  // --- the terminal still has the keyboard after a drop
  if (!(await ev("document.activeElement && document.activeElement.id === 'kbd'"))) fail("the terminal lost the keyboard after a drop");
  out.focus = "kbd";

  await shot(path.basename(outPng));
  out.logs = logs.filter((x) => !/^\s*$/.test(x) && !/INFO/.test(x)).slice(-6);
  console.log(JSON.stringify(out));
  ws.close(); chrome.kill(); process.exit(0);
})().catch((e) => { console.error(e); chrome.kill(); process.exit(1); });
