// The search palette end to end, against a throwaway server: the shortcut
// opens it with the keyboard in its field, a typed name finds the thread it
// names, Escape hands the keyboard back to the terminal, a command really
// splits the page's tab, and a `?lang=` load is in that language.
//   node palette-test.js <url> <cli prefix> <out.png>
const { spawn, execSync } = require("child_process");
const http = require("http"); const fs = require("fs"); const os = require("os");
const WebSocket = require("ws");
const [url, cli, outPng] = process.argv.slice(2);
const chromePath = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 9333 + Math.floor(Math.random() * 500);
setTimeout(() => { console.error("watchdog"); try { chrome.kill(); } catch {} process.exit(4); }, 150000).unref();
const profile = fs.mkdtempSync(`${os.tmpdir()}/tt-palette-`);
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
  const until = async (what, fn, ms = 12000) => { const t = Date.now(); while (Date.now() - t < ms) { const v = await fn(); if (v) return v; await sleep(120); } throw new Error("timeout waiting for " + what + ": palette=" + JSON.stringify(await picks()) + " status=" + (await ev("document.getElementById('status').textContent")) + " logs=" + JSON.stringify(logs.slice(-6))); };
  const click = (sel) => ev(`(() => { const el = document.querySelector(${JSON.stringify(sel)}); if (!el) return false; el.click(); return true; })()`);
  const list = () => JSON.parse(sh(`${cli} list --format json`));
  const fail = (what) => { throw new Error(what); };
  const open = () => ev("!!document.getElementById('palette')");
  // Every entry on show, in the order the arrows walk them.
  const picks = () => ev("Array.from(document.querySelectorAll('#palette .pe')).map((e) => [e.dataset.id, e.querySelector('.t').textContent])");
  const key = async (k, code, vk, modifiers = 0) => {
    await send("Input.dispatchKeyEvent", { type: "rawKeyDown", key: k, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk, modifiers }, s);
    await send("Input.dispatchKeyEvent", { type: "keyUp", key: k, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk, modifiers }, s);
  };
  // The default shortcut is Cmd+K on a Mac and Ctrl+K everywhere else, which
  // is exactly what the page decides from the platform.
  const HOTKEY = process.platform === "darwin" ? 4 : 2;
  const hotkey = () => key("k", "KeyK", 75, HOTKEY);
  // Typed the way the field sees it: the palette re-ranks on every input.
  const type = (text) => ev(`(() => { const q = document.querySelector('#palette .q'); if (!q) return false;
    q.value = ${JSON.stringify(text)}; q.dispatchEvent(new Event('input', { bubbles: true })); return true; })()`);
  const out = {};

  await send("Page.navigate", { url }, s);
  await until("attach", async () => /this browser has/.test(await ev("document.getElementById('status').dataset.summary || ''")), 20000);
  await sleep(900);

  // --- two threads to find: the server's landing one, named "main", and a
  // second so the ranking has something to beat.
  await click("[data-action=new-thread]");
  const landing = await until("the landing thread", async () => (await threads()).find((t) => t.selected), 15000);
  if (landing.name !== "main") fail("the landing thread is named " + JSON.stringify(landing.name));
  await click("[data-action=new-thread]");
  await until("a second thread", async () => (await threads()).length === 2, 15000);

  // --- (a) the shortcut opens it, with the keyboard in its field.
  await hotkey();
  await until("the palette", open);
  if (!(await ev("document.activeElement === document.querySelector('#palette .q')"))) {
    fail("the field did not take the keyboard: " + (await ev("document.activeElement.id || document.activeElement.className")));
  }
  out.open = "ok";

  // --- (b) a typed name finds the thread it names, first of all.
  await type("mai");
  const ranked = await until("a ranked list", async () => { const p = await picks(); return p.length > 0 ? p : null; });
  out.first = ranked[0];
  if (ranked[0][0] !== `thread:${landing.id}` || ranked[0][1] !== "main") fail("the first entry is " + JSON.stringify(ranked.slice(0, 3)));
  out.rank = "ok";

  // --- (c) Escape closes it and the terminal has the keyboard again.
  await key("Escape", "Escape", 27);
  await until("the palette is gone", async () => !(await open()));
  if (!(await ev("document.activeElement.id === 'kbd'"))) fail("the keyboard did not go back to the terminal: " + (await ev("document.activeElement.id || document.activeElement.className")));
  out.escape = "ok";

  // --- (d) a command really acts: Split Right splits the page's tab.
  const tab = (await layout()).tab;
  const before = list().filter((p) => p.tab_id === tab).length;
  await hotkey();
  await until("the palette", open);
  await type("split");
  const commands = await until("a ranked list", async () => { const p = await picks(); return p.length > 0 ? p : null; });
  if (!/^cmd:split-right/.test(commands[0][0])) fail("the first entry is " + JSON.stringify(commands.slice(0, 3)));
  await key("Enter", "Enter", 13);
  await until("the palette is gone", async () => !(await open()));
  await sleep(1500);
  const panes = list().filter((p) => p.tab_id === tab);
  if (before !== 1 || panes.length !== 2) fail(`tab ${tab} went from ${before} to ${panes.length} panes`);
  out.command = "ok";

  const shot = await send("Page.captureScreenshot", { format: "png" }, s); fs.writeFileSync(outPng, Buffer.from(shot.data, "base64"));

  // --- (e) a `?lang=` load is in that language, the palette included.
  await send("Page.navigate", { url: url.replace("#", "?lang=de-DE#") }, s);
  out.placeholder = await until("the German palette", async () => {
    const raw = await ev("window.thinkterm && window.thinkterm.client ? window.thinkterm.client.palette('') : ''");
    return raw ? JSON.parse(raw).placeholder : null;
  }, 25000);
  if (out.placeholder !== "Befehle suchen…") fail("the placeholder is " + JSON.stringify(out.placeholder));
  out.german = "ok";

  out.logs = logs.filter((l) => !/^\s*$/.test(l) && !/INFO/.test(l)).slice(-6);
  console.log(JSON.stringify(out));
  ws.close(); chrome.kill(); process.exit(0);
})().catch((e) => { console.error(e); chrome.kill(); process.exit(1); });
