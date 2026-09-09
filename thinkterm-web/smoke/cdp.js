// Drive headless Chrome over the DevTools protocol for the browser smoke
// (not part of the served bundle):
// load a page, wait for its status line to match, optionally type into the
// terminal through the real keydown path, screenshot, print the console.
//
//   node cdp.js <url> <out.png> [--wait-status <regex>] [--type <text>] [--timeout <ms>]
//
// Needs the `ws` npm package on NODE_PATH and Google Chrome installed.
const { spawn } = require("child_process");
const http = require("http");
const fs = require("fs");
const os = require("os");
const WebSocket = require("ws");

const args = process.argv.slice(2);
const url = args.shift();
const outPng = args.shift();
const opt = (name, def) => { const i = args.indexOf(name); return i >= 0 ? args[i + 1] : def; };
const waitStatus = opt("--wait-status", null);
const typeText = opt("--type", null);
const timeoutMs = Number(opt("--timeout", 15000));
const chromePath = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 9333 + Math.floor(Math.random() * 500);
// Whatever happens, this process ends.
setTimeout(() => { console.error("cdp: watchdog"); try { chrome.kill(); } catch {} process.exit(4); }, timeoutMs + 30000).unref();

// A profile of our own. Without one, launching the app bundle hands the
// request to the Chrome the user already has open and exits, so this script
// would drive their tabs -- or nothing at all -- instead of the page.
const profile = fs.mkdtempSync(`${os.tmpdir()}/tt-cdp-`);
const chrome = spawn(chromePath, [
  "--headless=new", "--no-first-run", `--remote-debugging-port=${port}`,
  `--user-data-dir=${profile}`,
  "--window-size=1240,720", "--hide-scrollbars", "--enable-unsafe-webgpu", "about:blank",
], { stdio: "ignore" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const trace = (m) => { if (process.env.CDP_TRACE) console.error("cdp: " + m); };

async function browserWs() {
  for (let i = 0; i < 50; i++) {
    try {
      return await new Promise((res, rej) => http.get(`http://127.0.0.1:${port}/json/version`, (r) => {
        let b = ""; r.on("data", (d) => b += d); r.on("end", () => res(JSON.parse(b).webSocketDebuggerUrl));
      }).on("error", rej));
    } catch { await sleep(200); }
  }
  throw new Error("chrome did not come up");
}

(async () => {
  const ws = new WebSocket(await browserWs());
  await new Promise((r) => ws.on("open", r));
  trace("browser websocket open");
  let id = 0; const waiting = new Map(); const logs = [];
  ws.on("message", (m) => {
    const msg = JSON.parse(m);
    if (msg.id && waiting.has(msg.id)) { waiting.get(msg.id)(msg.result || msg.error); waiting.delete(msg.id); }
    if (msg.method === "Runtime.consoleAPICalled") logs.push(msg.params.args.map((a) => a.value ?? a.description).join(" "));
    if (msg.method === "Runtime.exceptionThrown") logs.push("EXCEPTION " + JSON.stringify(msg.params.exceptionDetails.exception?.description || msg.params.exceptionDetails.text));
  });
  const send = (method, params = {}, sessionId) => new Promise((r) => { const i = ++id; trace(`-> ${method}`); waiting.set(i, (v) => { trace(`<- ${method}`); r(v); }); ws.send(JSON.stringify({ id: i, method, params, sessionId })); });
  const { targetId } = await send("Target.createTarget", { url: "about:blank" });
  const session = (await send("Target.attachToTarget", { targetId, flatten: true })).sessionId;
  await send("Runtime.enable", {}, session);
  await send("Page.enable", {}, session);
  await send("Page.navigate", { url }, session);

  const status = async () => (await send("Runtime.evaluate", { expression: "document.getElementById('status')?.textContent || ''", returnByValue: true }, session)).result?.value || "";
  const deadline = Date.now() + timeoutMs;
  let text = "";
  const re = waitStatus ? new RegExp(waitStatus) : null;
  // The probes (?check=graphics, ?check=fallback, the colour check) end in
  // a JSON line whose `ok` says whether they passed; a probe line that says
  // false is a failure whatever else the status looks like.
  const probeFailed = (t) => /^(FALLBACK|GRAPHICS|COLOR)\b/.test(t) && !/"ok"\s*:\s*true\b/.test(t);
  const hardFailed = (t) => /^(disconnected|no token|WebGPU|error)/i.test(t) || /^FAIL/.test(t) || probeFailed(t);
  while (Date.now() < deadline) {
    text = await status();
    if (re ? re.test(text) : text && !/…$/.test(text)) break;
    if (hardFailed(text)) break;
    await sleep(250);
  }
  console.log("STATUS " + text);

  if (typeText) {
    const expr = `(() => {
      const kbd = document.getElementById('kbd'); kbd.focus();
      const fire = (key, code) => kbd.dispatchEvent(new KeyboardEvent('keydown', { key, code, bubbles: true, cancelable: true }));
      for (const ch of ${JSON.stringify(typeText)}) fire(ch, ch === ' ' ? 'Space' : 'Key' + ch.toUpperCase());
      fire('Enter', 'Enter');
      return 'typed';
    })()`;
    logs.push("harness: " + JSON.stringify((await send("Runtime.evaluate", { expression: expr, returnByValue: true }, session)).result?.value));
    await sleep(2000);
    console.log("STATUS " + await status());
  }
  const shot = await send("Page.captureScreenshot", { format: "png" }, session);
  fs.writeFileSync(outPng, Buffer.from(shot.data, "base64"));
  console.log(logs.join("\n"));
  ws.close(); chrome.kill();
  // Without --wait-status this used to exit 0 no matter what the page said,
  // so a smoke run that reported an error still passed CI.
  // A probe that reports ok:false fails even when --wait-status matched it:
  // the pattern says the probe finished, the JSON says how.
  const failed = (re ? !re.test(text) : !text) || hardFailed(text);
  process.exit(failed ? 3 : 0);
})().catch((e) => { console.error(e); chrome.kill(); process.exit(1); });
