// Two pages on one server: A switches the server to Handoff and holds the
// terminal; B sees the full-area card naming A's host, presses its button,
// and gets the terminal; A then sees the card. node card-test.js <urlA> <urlB> <cli> <out.png>
const { spawn, execSync } = require("child_process");
const http = require("http"); const fs = require("fs"); const os = require("os");
const WebSocket = require("ws");
const [urlA, urlB, cli, outPng] = process.argv.slice(2);
const chromePath = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 9333 + Math.floor(Math.random() * 500);
setTimeout(() => { console.error("watchdog"); try { chrome.kill(); } catch {} process.exit(4); }, 60000).unref();
const profile = fs.mkdtempSync(`${os.tmpdir()}/tt-card-`);
const chrome = spawn(chromePath, ["--headless=new", "--no-first-run", `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, "--window-size=1100,700", "--hide-scrollbars", "--enable-unsafe-webgpu", "about:blank"], { stdio: "ignore" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function browserWs() { for (let i = 0; i < 50; i++) { try { return await new Promise((res, rej) => http.get(`http://127.0.0.1:${port}/json/version`, (r) => { let b = ""; r.on("data", (d) => b += d); r.on("end", () => res(JSON.parse(b).webSocketDebuggerUrl)); }).on("error", rej)); } catch { await sleep(200); } } throw new Error("chrome did not come up"); }
(async () => {
  const ws = new WebSocket(await browserWs()); await new Promise((r) => ws.on("open", r));
  let id = 0; const waiting = new Map(); const logs = [];
  ws.on("message", (m) => { const msg = JSON.parse(m); if (msg.id && waiting.has(msg.id)) { waiting.get(msg.id)(msg.result || msg.error); waiting.delete(msg.id); }
    if (msg.method === "Runtime.consoleAPICalled") logs.push(msg.params.args.map((a) => a.value ?? a.description).join(" "));
    if (msg.method === "Runtime.exceptionThrown") logs.push("EXCEPTION " + JSON.stringify(msg.params.exceptionDetails.exception?.description || msg.params.exceptionDetails.text)); });
  const send = (method, params = {}, sessionId) => new Promise((r) => { const i = ++id; waiting.set(i, r); ws.send(JSON.stringify({ id: i, method, params, sessionId })); });
  const page = async (url) => {
    const { targetId } = await send("Target.createTarget", { url: "about:blank" });
    const s = (await send("Target.attachToTarget", { targetId, flatten: true })).sessionId;
    await send("Runtime.enable", {}, s); await send("Page.enable", {}, s);
    const ev = async (expr) => (await send("Runtime.evaluate", { expression: expr, returnByValue: true }, s)).result?.value;
    const until = async (what, expr, ms = 10000) => { const t = Date.now(); while (Date.now() - t < ms) { const v = await ev(expr); if (v) return v; await sleep(100); } throw new Error("timeout waiting for " + what + ": " + JSON.stringify({ status: await ev("document.getElementById('status')?.dataset.summary"), card: await ev("JSON.stringify(JSON.parse(window.thinkterm.client.status()).card)") })); };
    await send("Page.navigate", { url }, s);
    await until("attach", "/this browser has|mirroring|another device/.test((document.getElementById('status')?.dataset.summary || ''))", 20000);
    return { s, ev, until };
  };
  const card = "JSON.parse(window.thinkterm.client.status()).card";
  const out = {};
  const A = await page(urlA);
  // A takes the server into Handoff through the tab menu's action.
  out.a_switch = await A.ev("JSON.parse(window.thinkterm.client.menu_action('access:handoff')).handled");
  await A.until("A owns in handoff", "/this browser has/.test(document.getElementById('status').dataset.summary)");
  out.a_card_before = await A.ev(`${card}`);
  const B = await page(urlB);
  out.b_card = await B.until("B sees the card", `${card} && ${card}.state === 'busy' && ${card}`);
  out.b_card_box = await B.ev("(() => { const c = document.getElementById('card'); const t = document.getElementById('term'); const r = c.getBoundingClientRect(), q = t.getBoundingClientRect(); return { covers: Math.abs(r.left - q.left) < 1 && Math.abs(r.width - q.width) < 1 && Math.abs(r.height - q.height) < 1, hidden: c.hidden, bg: getComputedStyle(c).backgroundColor, button: c.querySelector('.go')?.textContent }; })()");
  await sleep(600);
  out.b_text = await B.ev("(() => { const t = document.querySelector('#card .title'); const cs = getComputedStyle(t); return { text: t.textContent, w: t.offsetWidth, h: t.offsetHeight, color: cs.color, font: cs.font, vis: cs.visibility, op: cs.opacity, cardOp: getComputedStyle(document.getElementById('card')).opacity, anim: getComputedStyle(document.getElementById('card')).animationName }; })()");
  const shot = await send("Page.captureScreenshot", { format: "png" }, B.s); fs.writeFileSync(outPng, Buffer.from(shot.data, "base64"));
  await B.ev("document.querySelector('#card .go').click(); 1");
  out.b_owns = await B.until("B got it", `${card} === null && /this browser has/.test(document.getElementById('status').dataset.summary)`) && "ok";
  out.a_card_after = await A.until("A sees the card", `${card} && ${card}.state === 'busy' && ${card}`);
  out.a_hidden = await A.ev("document.getElementById('card').hidden");
  out.logs = logs.filter((l) => !/INFO/.test(l)).slice(-5);
  console.log(JSON.stringify(out));
  ws.close(); chrome.kill(); process.exit(0);
})().catch((e) => { console.error(e); chrome.kill(); process.exit(1); });
