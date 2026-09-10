// The sidebar end to end, against a throwaway server: the landing tree
// appears after "New Thread", a second thread is created, renamed, pinned
// and deleted, a project is created from a path and its `main` thread
// spawns there, archiving the last project is refused with a remark, and a
// second Space is made, renamed and still on show after a reload.
//   node sidebar-test.js <url> <cli prefix> <out.png>
const { spawn, execSync } = require("child_process");
const http = require("http"); const fs = require("fs"); const os = require("os");
const WebSocket = require("ws");
const [url, cli, outPng] = process.argv.slice(2);
const chromePath = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 9333 + Math.floor(Math.random() * 500);
setTimeout(() => { console.error("watchdog"); try { chrome.kill(); } catch {} process.exit(4); }, 120000).unref();
const profile = fs.mkdtempSync(`${os.tmpdir()}/tt-side-`);
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
  const until = async (what, fn, ms = 10000) => { const t = Date.now(); while (Date.now() - t < ms) { const v = await fn(); if (v) return v; await sleep(120); } throw new Error("timeout waiting for " + what + ": rows=" + JSON.stringify(await rows()) + " status=" + (await ev("document.getElementById('status').textContent"))); };
  const click = (sel) => ev(`(() => { const el = document.querySelector(${JSON.stringify(sel)}); if (!el) return false; el.click(); return true; })()`);
  const dblclick = (sel) => ev(`(() => { const el = document.querySelector(${JSON.stringify(sel)}); if (!el) return false; el.dispatchEvent(new MouseEvent('click', { bubbles: true, detail: 2 })); return true; })()`);
  const typeInto = async (text) => { await ev(`(() => { const i = document.querySelector('#side input'); i.value = ${JSON.stringify(text)}; i.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true })); return 1; })()`); };
  const list = () => JSON.parse(sh(`${cli} list --format json`));
  const fail = (what) => { throw new Error(what); };
  const out = {};

  await send("Page.navigate", { url }, s);
  await until("attach", async () => /this browser has/.test(await ev("document.getElementById('status').dataset.summary || ''")), 20000);
  await sleep(800);
  out.rows_at_start = (await rows()).map((r) => r.kind);
  if (!(await rows()).some((r) => r.kind === "others")) fail("the CLI window is not listed under Other windows");

  // --- New Thread on an empty tree: the landing thread
  await click("[data-action=new-thread]");
  const landing = await until("landing thread on show", async () => (await threads()).find((t) => t.selected), 15000);
  out.landing = landing;
  const ws1 = list().find((p) => /^thinkterm:/.test(p.workspace));
  if (!ws1) fail("no window in a thread workspace: " + JSON.stringify(list().map((p) => p.workspace)));

  // --- a second thread in the same project, renamed, pinned, deleted
  await click("[data-action=new-thread]");
  const second = await until("second thread", async () => (await threads()).find((t) => t.id !== landing.id), 15000);
  await until("second thread on show", async () => (await threads()).find((t) => t.id === second.id)?.selected, 15000);
  await dblclick(`[data-action=rename-thread][data-thread="${second.id}"]`);
  await until("rename field", () => ev("!!document.querySelector('#side input.rename')"));
  await typeInto("renamed by the page");
  await until("renamed", async () => (await threads()).find((t) => t.id === second.id)?.name === "renamed by the page");
  await click(`[data-action=pin][data-thread="${second.id}"]`);
  await until("pinned", async () => { const r = await rows(); const i = r.findIndex((x) => x.kind === "pinned"); const t = r.find((x) => x.kind === "thread" && x.id === second.id); return i >= 0 && t && t.pinned && r.indexOf(t) > i; });
  const before = list().length;
  // One press deletes, as the desktop's menu item does.
  await click(`[data-action=delete][data-thread="${second.id}"]`);
  await until("deleted", async () => !(await threads()).some((t) => t.id === second.id), 15000);
  await until("its pane is gone", () => list().length < before, 15000);
  out.thread_ops = "ok";

  // --- a project from a path: its main thread spawns there
  const dir = fs.mkdtempSync(`${os.tmpdir()}/tt-proj-`);
  await click("[data-action=new-project]");
  await until("path field", () => ev("!!document.querySelector('#side .path input')"));
  await typeInto(dir);
  const project = await until("project row", async () => (await rows()).find((r) => r.kind === "project" && r.name === require("path").basename(dir)), 15000);
  const main = await until("main thread on show", async () => (await threads()).find((t) => t.project === project.id && t.selected), 15000);
  out.project = { name: project.name, main: main.name };
  await until("cwd is the project", () => list().some((p) => p.cwd && p.cwd.replace(/\/$/, "").endsWith(dir.replace(/^\/private/, "").replace(/\/$/, ""))), 15000);

  // --- archiving: the second project can go, the last one cannot
  await click(`[data-action=archive][data-project="${project.id}"]`);
  await until("archived", async () => (await rows()).some((r) => r.kind === "archived" && r.count === 1), 15000);
  const last = (await rows()).find((r) => r.kind === "project" && !r.archived);
  await click(`[data-action=archive][data-project="${last.id}"]`);
  await until("refusal shown", () => ev("/last project/.test(document.getElementById('status').textContent) && !document.getElementById('status').hasAttribute('hidden')"), 10000);
  out.archive = "ok";

  // --- Spaces: the row's menu makes one, renames it, and the page opens
  // on the one it was showing.
  const menu = () => ev("Array.from(document.querySelectorAll('.menu .mi')).map((e) => e.dataset.id)");
  const space = async () => (await rows())[0];
  const openSpaceMenu = async () => {
    if (!(await click("#side [data-action=space-menu]"))) fail("no Space menu button");
    await until("the Space menu", () => ev("!!document.getElementById('menu')"));
    return menu();
  };
  let ids = await openSpaceMenu();
  out.space_menu = ids;
  if (!ids.includes("new-space")) fail("the Space menu is " + JSON.stringify(ids));
  if (ids.some((i) => /^delete-space:/.test(i))) fail("the last Space can be deleted: " + JSON.stringify(ids));
  const landed = await space();
  if (landed.kind !== "space") fail("the first row is " + JSON.stringify(landed));
  if (!(await click('[data-id="new-space"]'))) fail("no New Space row");
  const made = await until("a second Space on show", async () => {
    const row = await space();
    return row.kind === "space" && row.id !== landed.id ? row : null;
  }, 15000);
  if (made.name !== "Space 2") fail("the new Space is named " + JSON.stringify(made.name));
  ids = await openSpaceMenu();
  if (!ids.some((i) => /^delete-space:/.test(i))) fail("a Space of two cannot be deleted: " + JSON.stringify(ids));
  if (!(await click('[data-id^="rename-space:"]'))) fail("no Rename row");
  await until("the Space's rename field", () => ev("!!document.querySelector('#side .row.space input.rename')"));
  await typeInto("Work");
  await until("renamed", async () => (await space()).name === "Work");

  // A real reload, not a jump to the same address with the token back in
  // the fragment, which the browser would not reload for at all. The token
  // is in this tab's session storage by now, so the address is enough.
  await ev("window.__landed = 1");
  await send("Page.reload", { ignoreCache: true }, s);
  await until("a fresh document", () => ev("window.__landed === undefined"), 20000);
  await until("attach again", async () => /this browser has/.test(await ev("document.getElementById('status').dataset.summary || ''")), 20000);
  const reopened = await until("the same Space on show", async () => {
    const row = await space();
    return row && row.kind === "space" && row.id === made.id ? row : null;
  }, 20000);
  if (reopened.name !== "Work") fail("the Space came back as " + JSON.stringify(reopened.name));
  out.space = "ok";

  const shot = await send("Page.captureScreenshot", { format: "png" }, s); fs.writeFileSync(outPng, Buffer.from(shot.data, "base64"));
  out.logs = logs.filter((l) => !/^\s*$/.test(l) && !/INFO/.test(l)).slice(-6);
  console.log(JSON.stringify(out));
  ws.close(); chrome.kill(); process.exit(0);
})().catch((e) => { console.error(e); chrome.kill(); process.exit(1); });
