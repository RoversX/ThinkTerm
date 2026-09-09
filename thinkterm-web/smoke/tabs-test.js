// End-to-end check of the tab strip, driven through tabs.sh (not part of
// the served bundle): with two tabs on the server the page lists both, a
// click switches panes and typing lands in the new one, and with following
// turned back on a focus change on the server side moves the page back.
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
  await send("Page.navigate", { url }, s);
  await until("attach", "/this browser has|following/.test(document.getElementById('status').textContent)", 20000);
  out.strip_at_start = await until("two tabs", "document.querySelectorAll('#tabs .tab').length === 2 && document.getElementById('tabs').innerText");
  out.current_at_start = await ev("document.querySelector('#tabs .tab.current').dataset.pane");
  // Click the other tab.
  await ev("document.querySelector('#tabs .tab:not(.current)').click()");
  await until("switch", `document.querySelector('#tabs .tab.current').dataset.pane !== ${JSON.stringify(out.current_at_start)}`);
  await sleep(400);
  out.current_after_click = await ev("document.querySelector('#tabs .tab.current').dataset.pane");
  out.follow_after_click = await ev("document.querySelector('#tabs .follow').textContent");
  out.status_after_click = await status();
  // Type into it through the real key path.
  await ev(`(() => { const kbd = document.getElementById('kbd'); kbd.focus(); const fire = (key, code) => kbd.dispatchEvent(new KeyboardEvent('keydown', { key, code, bubbles: true, cancelable: true })); for (const ch of 'echo TAB2-OK') fire(ch, ch === ' ' ? 'Space' : 'Key' + ch.toUpperCase()); fire('Enter', 'Enter'); return 1; })()`);
  await sleep(1500);
  out.pane1_text = sh(`${cli} get-text --pane-id 1`).split("\n").filter((l) => /TAB2-OK/.test(l));
  out.pane0_text = sh(`${cli} get-text --pane-id 0`).split("\n").filter((l) => /TAB2-OK/.test(l));
  // Follow the desktop again, then focus pane 0 from the server side.
  await ev("document.querySelector('#tabs .follow').click()");
  out.follow_after_toggle = await ev("document.querySelector('#tabs .follow').textContent");
  sh(`${cli} activate-pane --pane-id 0`);
  await until("follow back", `document.querySelector('#tabs .tab.current').dataset.pane === ${JSON.stringify(out.current_at_start)}`);
  await sleep(400);
  out.current_after_focus = await ev("document.querySelector('#tabs .tab.current').dataset.pane");
  out.status_after_focus = await status();
  const shot = await send("Page.captureScreenshot", { format: "png" }, s); fs.writeFileSync(outPng, Buffer.from(shot.data, "base64"));
  out.logs = logs.filter((l) => !/^\s*$/.test(l)).slice(-8);
  console.log(JSON.stringify(out, null, 1));
  ws.close(); chrome.kill(); process.exit(0);
})().catch((e) => { console.error(e); chrome.kill(); process.exit(1); });
