// The Snippets tab end to end, against a throwaway server whose plugin host
// keeps one snippet already: the tab lists it, a snippet added in the
// editor reaches the host's file, the search filters, Run types a snippet
// into the terminal, another client's change arrives by itself, a delete
// takes two presses, a Run lands in the pane focused at the press even when
// focus moves before the host answers, and nowhere when the answer comes
// too late; a killed host is replaced; and through a link with 50 ms each
// way added, a search is timed and typing's requests are counted.
//   node snippets-test.js <url> <out.png>
// Reads SNIPPETS_FILE (the host's file), TEST_HOME (the server's HOME) and
// MUX_CLI (`thinkterm cli` pinned to the server) from the environment.
const { spawn, execSync } = require("child_process");
const http = require("http"); const fs = require("fs"); const os = require("os"); const net = require("net");
const WebSocket = require("ws");
const [url, outPng] = process.argv.slice(2);
const { SNIPPETS_FILE, TEST_HOME, MUX_CLI, SLOW_PORT } = process.env;
const chromePath = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 9333 + Math.floor(Math.random() * 500);
setTimeout(() => { console.error("watchdog"); try { chrome.kill(); } catch {} process.exit(4); }, 240000).unref();
const profile = fs.mkdtempSync(`${os.tmpdir()}/tt-snippets-`);
const chrome = spawn(chromePath, ["--headless=new", "--no-first-run", `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, "--window-size=1400,800", "--hide-scrollbars", "--enable-unsafe-webgpu", "about:blank"], { stdio: "ignore" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function browserWs() { for (let i = 0; i < 50; i++) { try { return await new Promise((res, rej) => http.get(`http://127.0.0.1:${port}/json/version`, (r) => { let b = ""; r.on("data", (d) => b += d); r.on("end", () => res(JSON.parse(b).webSocketDebuggerUrl)); }).on("error", rej)); } catch { await sleep(200); } } throw new Error("chrome did not come up"); }

const socketPath = `${TEST_HOME}/.local/share/thinkterm/plugins-sock`;
const hostPids = () => { try { return execSync(`pgrep -f ${JSON.stringify(socketPath)}`).toString().trim().split(/\s+/).filter(Boolean); } catch { return []; } };
const onDisk = () => { try { return fs.readFileSync(SNIPPETS_FILE, "utf8"); } catch { return ""; } };
// A pane's text, or every pane's.
const paneText = (pane) => execSync(`${MUX_CLI} get-text --pane-id ${pane}`).toString();
const allText = () => JSON.parse(execSync(`${MUX_CLI} list --format json`).toString())
  .map((p) => paneText(p.pane_id)).join("\n");
const printed = (text, marker) => text.split("\n").some((l) => l.trim() === marker);

// A link to the server on `port` that holds every chunk back `delay` ms
// each way, in order: a browser some way off.
function slowLink(port, target, delay) {
  return new Promise((resolve) => {
    const server = net.createServer((client) => {
      const upstream = net.connect(target, "127.0.0.1");
      const relay = (from, to) => {
        let last = 0;
        // Everything, the end included, leaves in the order it came.
        const later = (act) => {
          const at = Math.max(Date.now() + delay, last);
          last = at;
          setTimeout(act, at - Date.now());
        };
        from.on("data", (chunk) => later(() => to.write(chunk)));
        from.on("end", () => later(() => to.end()));
        from.on("error", () => later(() => to.destroy()));
      };
      relay(client, upstream);
      relay(upstream, client);
    });
    server.listen(port, "127.0.0.1", () => resolve(server));
  });
}

// Another client of the host, as the desktop is: one frame out, the hello
// and the answer back.
function saveFromElsewhere(title, body) {
  return new Promise((resolve, reject) => {
    const sock = net.createConnection(socketPath);
    let buf = Buffer.alloc(0); let frames = 0;
    sock.on("data", (d) => {
      buf = Buffer.concat([buf, d]);
      while (buf.length >= 4 && buf.length >= 4 + buf.readUInt32LE(0)) {
        const len = buf.readUInt32LE(0); const msg = JSON.parse(buf.subarray(4, 4 + len).toString()); buf = buf.subarray(4 + len);
        frames++;
        if (frames === 1) {
          if (!msg.hello) return reject(new Error("no hello: " + JSON.stringify(msg)));
          const payload = Buffer.from(JSON.stringify({ call: { id: 1, plugin: "snippets", body: { op: "save", title, body } } }));
          const head = Buffer.alloc(4); head.writeUInt32LE(payload.length, 0);
          sock.write(Buffer.concat([head, payload]));
        } else if (msg.ok) { sock.end(); return resolve(); }
        else if (msg.error) { sock.end(); return reject(new Error(msg.error.message)); }
      }
    });
    sock.on("error", reject);
  });
}

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
  const type = (sel, text) => ev(`(() => { const el = document.querySelector(${JSON.stringify(sel)}); if (!el) return false; el.value = ${JSON.stringify(text)}; el.dispatchEvent(new Event('input', { bubbles: true })); return true; })()`);
  const titles = () => ev("[...document.querySelectorAll('#agents .sn .sn-title')].map((e) => e.textContent)");
  const card = (title) => `[...document.querySelectorAll('#agents .sn')].find((c) => c.querySelector('.sn-title').textContent === ${JSON.stringify(title)})`;
  const cardButton = (title, n) => ev(`(() => { const c = ${card(title)}; if (!c) return false; c.querySelectorAll('.sn-btn')[${n}].click(); return true; })()`);
  const fail = (what) => { throw new Error(what); };
  const attached = async () => /this browser has/.test(await ev("document.getElementById('status').dataset.summary || ''"));
  const out = {};

  await send("Page.navigate", { url }, s);
  await until("attach", attached, 20000);
  await sleep(600);
  if (hostPids().length) fail("a plugin host runs before anything asked for one");

  // --- (a) the panel's Snippets tab lists what the host keeps.
  if (!(await click("#tabs [data-action=agents]"))) fail("no right-panel button in the tab row");
  await until("the panel", () => ev("!!document.getElementById('agents')"));
  if (!(await click("#agents .mode[data-mode=snippets]"))) fail("no Snippets tab in the selector");
  await until("the seeded snippet", async () => (await titles()).includes("List files"));
  out.heading = await ev("document.querySelector('#agents .hd .ti').textContent");
  if (out.heading !== "Snippets") fail("the heading is " + JSON.stringify(out.heading));
  out.preview = await ev(`${card("List files")}.querySelector('.sn-preview').textContent`);
  if (out.preview !== "ls -la") fail("the preview is " + JSON.stringify(out.preview));
  out.hosts = hostPids().length;
  if (out.hosts !== 1) fail(`${out.hosts} plugin hosts run`);
  out.list = "ok";

  // --- (b) one added in the editor reaches the host's file.
  const marker = `snippet-ran-${process.pid}`;
  if (!(await click("#agents .sbar .sb-icon"))) fail("no new-snippet button");
  await until("the editor", () => ev("!!document.getElementById('snippet-body')"));
  await type("#snippet-title", "Say hi");
  await type("#snippet-body", `echo ${marker}`);
  if (!(await click("#agents .sb-save"))) fail("no save button");
  await until("the new card", async () => (await titles()).includes("Say hi"));
  await until("the host to save it", () => onDisk().includes(marker));
  out.order = await titles();
  if (out.order[0] !== "Say hi") fail("the new snippet is not first: " + JSON.stringify(out.order));
  out.add = "ok";

  // --- (c) the search filters as the desktop's does.
  await type("#agents .sb-search input", "SAY");
  await until("the filter", async () => JSON.stringify(await titles()) === JSON.stringify(["Say hi"]));
  await type("#agents .sb-search input", "nothing like it");
  await until("the empty list", () => ev("document.querySelector('#agents .sn-empty-t')?.textContent === 'No matching snippets'"));
  await type("#agents .sb-search input", "");
  await until("the whole list", async () => (await titles()).length === 2);
  out.search = "ok";

  // --- (d) Run types it into the terminal and runs it.
  if (!(await cardButton("Say hi", 0))) fail("no Run button");
  await until("the command's output", () => printed(allText(), marker), 10000);
  out.run = "ok";

  // --- (e) another client's change arrives by itself.
  await saveFromElsewhere("From elsewhere", "uptime");
  await until("the other client's snippet", async () => (await titles()).includes("From elsewhere"));
  out.pushed = "ok";

  const shot = await send("Page.captureScreenshot", { format: "png" }, s); fs.writeFileSync(outPng, Buffer.from(shot.data, "base64"));

  // --- (f) a delete takes two presses.
  if (!(await cardButton("Say hi", 2))) fail("no delete button");
  await sleep(200);
  if (!(await titles()).includes("Say hi")) fail("one press deleted it");
  out.armed = await ev(`${card("Say hi")}.querySelector('.sn-del').textContent`);
  if (out.armed !== "delete?") fail("the armed button says " + JSON.stringify(out.armed));
  await cardButton("Say hi", 2);
  await until("the card to go", async () => !(await titles()).includes("Say hi"));
  await until("the host to forget it", () => !onDisk().includes(`"body": "echo ${marker}"`));
  out.delete = "ok";

  // --- (h) a Run lands in the pane focused when it was pressed, though
  // focus moves on while the host is held up; one whose answer comes too
  // late is not sent at all.
  const navs = async () => ev("[...document.querySelectorAll('#panes .nav')].map((n) => Number(n.dataset.nav))");
  const focusedPane = async () => Number(await ev("document.querySelector('#panes .nav.focused')?.dataset.nav"));
  const focusPane = async (pane) => {
    await ev(`window.thinkterm.client.chrome_click('pane', ${pane}, undefined)`);
    await until(`pane ${pane} to be focused`, async () => (await focusedPane()) === pane);
  };
  const [left, right] = await until("two panes on show", async () => { const n = await navs(); return n.length === 2 ? n : null; }).catch(async (e) => {
    console.error("navs:", JSON.stringify(await navs()), "tabs:", await ev("window.thinkterm.client.tabs()"), "list:", execSync(`${MUX_CLI} list`).toString());
    throw e;
  });
  const target = `snippet-target-${process.pid}`;
  const late = `snippet-late-${process.pid}`;
  for (const [title, body] of [["Target", `echo ${target}`], ["Late", `echo ${late}`]]) {
    await saveFromElsewhere(title, body);
    await until(`the ${title} card`, async () => (await titles()).includes(title));
  }
  await focusPane(left);
  const [stalled] = hostPids();
  process.kill(Number(stalled), "SIGSTOP");
  await cardButton("Target", 0);
  await sleep(150);
  await focusPane(right);
  await sleep(500);
  process.kill(Number(stalled), "SIGCONT");
  await until("the run in the pane it was pressed in", () => printed(paneText(left), target), 10000);
  await sleep(500);
  if (printed(paneText(right), target)) fail("the run landed in the pane focused afterwards");
  out.run_target = "ok";

  process.kill(Number(stalled), "SIGSTOP");
  await cardButton("Late", 0);
  await sleep(3500);
  process.kill(Number(stalled), "SIGCONT");
  await sleep(1500);
  if (printed(allText(), late)) fail("a run whose text came 3.5 s late was sent");
  out.run_late = "dropped";

  // A save made while the host is held up waits for it, and lands once it
  // resumes; the editor stays up meanwhile, with what was typed.
  process.kill(Number(stalled), "SIGSTOP");
  if (!(await click("#agents .sbar .sb-icon"))) fail("no new-snippet button");
  await until("the editor", () => ev("!!document.getElementById('snippet-body')"));
  await type("#snippet-body", "echo saved-while-held");
  await click("#agents .sb-save");
  await sleep(1500);
  const held = await ev("!!document.getElementById('snippet-body') && document.querySelector('#agents .sb-save').disabled");
  process.kill(Number(stalled), "SIGCONT");
  if (!held) fail("the editor did not wait for the held-up host");
  await until("the held save to land", () => onDisk().includes("saved-while-held"));
  await until("the editor to close", () => ev("!document.getElementById('snippet-body')"));
  out.save_held = "ok";

  // --- (g) a killed host is replaced, and the list works on.
  const [first] = hostPids();
  process.kill(Number(first), "SIGKILL");
  await until("a new host", () => { const now = hostPids(); return now.length === 1 && now[0] !== first; }, 20000);
  await until("the list to be followed again", () => ev("JSON.parse(window.thinkterm.client.snippets()).state === 'ready'"));
  if (!(await click("#agents .sbar .sb-icon"))) fail("no new-snippet button after the restart");
  await until("the editor again", () => ev("!!document.getElementById('snippet-body')"));
  await type("#snippet-body", "echo after-the-restart");
  await click("#agents .sb-save");
  await until("the new host to save it", () => onDisk().includes("after-the-restart"), 20000);
  out.restart = "ok";

  // --- (j) a search timed, and typing's requests counted: here, and in a
  // second page whose link adds 50 ms each way. Timed in the page, from the
  // keystroke to the rows answering it.
  const measure = async (page) => {
    const run = (body) => send("Runtime.evaluate", { expression: `(async () => { ${body} })()`, awaitPromise: true, returnByValue: true }, page.session).then((r) => r.result?.value);
    const helpers = `
      const view = () => JSON.parse(window.thinkterm.client.snippets());
      const box = document.querySelector('#agents .sb-search input');
      const type = (q) => { box.value = q; box.dispatchEvent(new Event('input', { bubbles: true })); };
      const settle = async (q, t0) => { while (view().answers !== q) { if (performance.now() - t0 > 10000) return -1; await new Promise((r) => setTimeout(r, 2)); } return performance.now() - t0; };`;
    const res = {};
    res.search_ms = [];
    for (const query of ["list", "", "up", "", "restart", ""]) {
      res.search_ms.push(Math.round(await run(`${helpers} const t0 = performance.now(); type(${JSON.stringify(query)}); return await settle(${JSON.stringify(query)}, t0);`)));
    }
    // Typing "uptime" at 40 ms a key: the requests it made, and how long
    // after the last key its rows came.
    Object.assign(res, await run(`${helpers}
      const before = view().asked;
      for (let i = 1; i <= 6; i++) { type("uptime".slice(0, i)); await new Promise((r) => setTimeout(r, 40)); }
      const t0 = performance.now() - 40;
      const settled = await settle("uptime", t0);
      return { typing_requests: view().asked - before, typing_settle_ms: Math.round(settled) };`));
    await run(`${helpers} type(""); return await settle("", performance.now());`);
    return res;
  };
  const page = (session) => ({ session, ev: async (expr) => (await send("Runtime.evaluate", { expression: expr, returnByValue: true }, session)).result?.value });
  out.local = await measure(page(s));

  const link = await slowLink(Number(SLOW_PORT), Number(new URL(url).port), 50);
  const slowUrl = url.replace(/:\d+\//, `:${SLOW_PORT}/`);
  const { targetId: slowTarget } = await send("Target.createTarget", { url: "about:blank" });
  const s2 = (await send("Target.attachToTarget", { targetId: slowTarget, flatten: true })).sessionId;
  await send("Runtime.enable", {}, s2); await send("Page.enable", {}, s2);
  const ev2 = page(s2).ev;
  await send("Page.navigate", { url: slowUrl }, s2);
  await until("the slow page to attach", async () => (await ev2("document.getElementById('status')?.dataset.summary || ''")).length > 0, 40000).catch(async (e) => {
    console.error("slow page:", await ev2("location.href + ' | ' + document.readyState + ' | ' + (document.body?.innerText || '').slice(0, 300)"), "\nlogs:", logs.slice(-12).join("\n"));
    throw e;
  });
  await ev2(`(() => { document.querySelector('#tabs [data-action=agents]')?.click(); return true; })()`);
  await until("the slow page's panel", () => ev2("!!document.getElementById('agents')"), 20000);
  await ev2(`(() => { document.querySelector('#agents .mode[data-mode=snippets]')?.click(); return true; })()`);
  const expected = (await titles()).length;
  await until("the slow page's snippets", async () => JSON.parse(await ev2("window.thinkterm.client.snippets()")).rows.length === expected, 20000);
  out.slow_50ms_each_way = await measure(page(s2));
  link.close();

  out.titles = await titles();
  out.logs = logs.filter((l) => !/^\s*$/.test(l) && !/INFO/.test(l)).slice(-6);
  console.log(JSON.stringify(out));
  ws.close(); chrome.kill(); process.exit(0);
})().catch((e) => { console.error(e); chrome.kill(); process.exit(1); });
