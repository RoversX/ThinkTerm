// A plugin's page in a browser, against a throwaway server with the example
// plugin installed, a terminal, and the plugin's page open in a tab of its
// own: the page draws the plugin's page in its pane's place, under the
// pane's bar, with what it was opened with decoded, over a socket the
// plugin's frames have to themselves; nothing typed there
// reaches a terminal; the page has an id of its own and the terminal it was
// opened from, and what is typed in its field its plugin keeps as what it
// opens with; the palette lists the plugin's actions once the list
// is heard, one types a UUID in the terminal in focus and one opens another
// page, which the page moves to; and a page's view is let go when its tab
// goes off show.
//   node pages-test.js <url> <cli prefix> <terminal pane> <page pane> <out.png>
const { spawn, execSync } = require("child_process");
const http = require("http"); const fs = require("fs"); const os = require("os");
const WebSocket = require("ws");
const [url, cli, terminalArg, pageArg, outPng] = process.argv.slice(2);
const terminal = Number(terminalArg); const page = Number(pageArg);
const chromePath = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 9333 + Math.floor(Math.random() * 500);
setTimeout(() => { console.error("watchdog"); try { chrome.kill(); } catch {} process.exit(4); }, 120000).unref();
const profile = fs.mkdtempSync(`${os.tmpdir()}/tt-pages-`);
const chrome = spawn(chromePath, ["--headless=new", "--no-first-run", `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, "--window-size=1400,900", "--hide-scrollbars", "--enable-unsafe-webgpu", "about:blank"], { stdio: "ignore" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function browserWs() { for (let i = 0; i < 50; i++) { try { return await new Promise((res, rej) => http.get(`http://127.0.0.1:${port}/json/version`, (r) => { let b = ""; r.on("data", (d) => b += d); r.on("end", () => res(JSON.parse(b).webSocketDebuggerUrl)); }).on("error", rej)); } catch { await sleep(200); } } throw new Error("chrome did not come up"); }
const sh = (cmd) => execSync(cmd, { encoding: "utf8" });

(async () => {
  const ws = new WebSocket(await browserWs()); await new Promise((r) => ws.on("open", r));
  let id = 0; const waiting = new Map(); const logs = [];
  // The sockets the page opens, by what the server says each is for, and
  // how many messages came in on each.
  const sockets = new Map();
  ws.on("message", (m) => { const msg = JSON.parse(m); if (msg.id && waiting.has(msg.id)) { waiting.get(msg.id)(msg.result || msg.error); waiting.delete(msg.id); }
    if (msg.method === "Network.webSocketHandshakeResponseReceived") { const headers = msg.params.response.headers; const key = Object.keys(headers).find((name) => name.toLowerCase() === "sec-websocket-protocol"); sockets.set(msg.params.requestId, { protocol: headers[key], received: 0, open: true }); }
    if (msg.method === "Network.webSocketFrameReceived" && sockets.has(msg.params.requestId)) sockets.get(msg.params.requestId).received += 1;
    if (msg.method === "Network.webSocketClosed" && sockets.has(msg.params.requestId)) sockets.get(msg.params.requestId).open = false;
    if (msg.method === "Runtime.consoleAPICalled") logs.push(msg.params.args.map((a) => a.value ?? a.description).join(" "));
    if (msg.method === "Runtime.exceptionThrown") logs.push("EXCEPTION " + JSON.stringify(msg.params.exceptionDetails.exception?.description || msg.params.exceptionDetails.text)); });
  const send = (method, params = {}, sessionId) => new Promise((r) => { const i = ++id; waiting.set(i, r); ws.send(JSON.stringify({ id: i, method, params, sessionId })); });
  const { targetId } = await send("Target.createTarget", { url: "about:blank" });
  const s = (await send("Target.attachToTarget", { targetId, flatten: true })).sessionId;
  await send("Runtime.enable", {}, s); await send("Page.enable", {}, s); await send("Network.enable", {}, s);
  const ev = async (expr) => (await send("Runtime.evaluate", { expression: expr, returnByValue: true }, s)).result?.value;
  const until = async (what, fn, ms = 15000) => { const t = Date.now(); while (Date.now() - t < ms) { const v = await fn(); if (v) return v; await sleep(120); } throw new Error("timeout waiting for " + what); };
  const fail = (what) => { throw new Error(what); };
  const attached = async () => /this browser has/.test(await ev("document.getElementById('status').dataset.summary || ''"));
  const layout = async () => JSON.parse((await ev("window.thinkterm.client.layout()")) || "{}");
  // What a plugin's page paints, as the wasm hands it to the canvas.
  const view = async (pane) => JSON.parse((await ev(`window.thinkterm.client.plugin_panel("page:${pane}")`)) || "null");
  const texts = (shown) => (shown?.painting?.ops ?? []).filter((op) => op.op === "text").map((op) => op.text);
  const box = (sel) => ev(`(() => { const e = document.querySelector(${JSON.stringify(sel)}); if (!e) return null; const r = e.getBoundingClientRect(); return { x: r.left, y: r.top, w: r.width, h: r.height }; })()`);
  const run = (pick) => ev(`JSON.parse(window.thinkterm.client.palette_run(${JSON.stringify(pick)})).handled`);
  const found = async (query) => JSON.parse(await ev(`window.thinkterm.client.palette(${JSON.stringify(query)})`)).sections.flatMap((section) => section.entries);
  const out = {};

  await send("Page.navigate", { url }, s);
  await until("attach", attached, 20000);
  await sleep(600);

  // --- (a) The page's tab is the one on show: its pane is marked a page,
  // and the page draws the plugin's page in its place, under its bar.
  if (!(await run(`pane:${page}`))) fail("the page's pane cannot be moved to");
  const marked = await until("the pane to be listed as a page", async () => {
    const pane = ((await layout()).panes ?? []).find((pane) => pane.id === page);
    return pane?.page?.plugin === "text-tools" ? pane : null;
  });
  out.offset = marked.page.offset;
  await until("the page's room", () => box("#panes .ppage canvas"));
  const shown = await until("the plugin to draw its page", async () => {
    const shown = await view(page);
    return texts(shown).includes("hello page") ? shown : null;
  });
  out.texts = texts(shown);
  if (!shown.live) fail("the page is not the one its plugin drew on this opening");
  const room = await box("#panes .ppage");
  const canvas = await box("#term");
  const bar = await box(`#panes .nav[data-nav="${page}"]`);
  out.room = room;
  if (!(room.w > 300 && room.h > 200)) fail(`the page has no room: ${JSON.stringify(room)}`);
  if (room.x < canvas.x - 1 || room.x + room.w > canvas.x + canvas.w + 1) fail("the page is not within the terminal's area");
  if (bar && room.y < bar.y + bar.h - 1) fail("the page is drawn over its pane's bar");
  out.tab = await ev("[...document.querySelectorAll('#tabs .tab.current')].map((tab) => tab.textContent.trim()).join('|')");
  if (!/Text Tools/.test(out.tab)) fail(`the tab is called ${JSON.stringify(out.tab)}`);
  await send("Page.captureScreenshot", { format: "png" }, s).then((r) => fs.writeFileSync(outPng, Buffer.from(r.data, "base64")));

  // --- The plugin's frames came on a socket of their own, beside the
  // terminals', and the two are one browser to the server.
  const open = (protocol) => [...sockets.values()].filter((socket) => socket.open && socket.protocol === protocol);
  const own = open("thinkterm.plugins.v1");
  out.sockets = [...sockets.values()].map((socket) => `${socket.protocol}${socket.open ? "" : " (closed)"}: ${socket.received}`);
  if (open("thinkterm.v1").length !== 1) fail(`the terminals' sockets are ${JSON.stringify(out.sockets)}`);
  if (own.length !== 1 || own[0].received === 0) fail(`the plugin's frames have no socket of their own: ${JSON.stringify(out.sockets)}`);
  out.browsers = JSON.parse(sh(`${cli} web-token list --format json`)).map((link) => link.live_connections);
  if (JSON.stringify(out.browsers) !== "[1]") fail(`the server counts ${JSON.stringify(out.browsers)} browsers`);

  // --- (b) What is typed while the page's pane is the one in focus is no
  // terminal's.
  await ev("document.getElementById('kbd').focus()");
  for (const key of ["l", "s"]) {
    await send("Input.dispatchKeyEvent", { type: "keyDown", key, text: key }, s);
    await send("Input.dispatchKeyEvent", { type: "keyUp", key }, s);
  }
  await sleep(300);
  out.typed = sh(`${cli} get-text --pane-id ${terminal}`).includes("ls");
  if (out.typed) fail("what was typed at the page reached the terminal");

  // --- The page is the terminal's it was opened from, with an id of its
  // own; what is typed in its field, its plugin keeps as what the page
  // opens with, which the server lists.
  const listed = () => JSON.parse(sh(`${cli} list --format json`)).find((pane) => pane.pane_id === page)?.page;
  const before = listed();
  if (!/^[0-9a-f]{16}$/.test(before?.id ?? "") || before.terminal !== terminal || !before.cwd) fail(`the page is listed as ${JSON.stringify(before)}`);
  await ev(`window.thinkterm.client.plugin_panel_field_focus("page:${page}", "typed")`);
  await ev(`window.thinkterm.client.plugin_panel_field_text("page:${page}", "typed", "MTcwMDAwMDAwMA==")`);
  out.kept = await until("the page to open with what was typed in it", async () => {
    const now = listed();
    return now?.args?.text === "MTcwMDAwMDAwMA==" ? now.args : null;
  });
  if (listed().id !== before.id) fail("the page changed its id");
  await ev(`window.thinkterm.client.plugin_panel_blur("page:${page}")`);

  // --- (c) The palette lists the plugin's actions once the plugin list is
  // heard, under the plugin's name.
  await ev("window.thinkterm.client.plugins_want('palette', true)");
  const actions = await until("the plugin's actions in the palette", async () => {
    const entries = (await found("text tools")).filter((entry) => entry.id.startsWith("plugin:"));
    return entries.length >= 2 ? entries : null;
  });
  out.actions = actions.map((entry) => `${entry.id} = ${entry.title}`);
  if (!actions.some((entry) => entry.id === "plugin:text-tools.type-uuid" && entry.title === "Text Tools: Type a new UUID")) {
    fail(`the actions are ${JSON.stringify(out.actions)}`);
  }

  // --- (d) One types in the terminal the user is at.
  if (!(await run(`pane:${terminal}`))) fail("the terminal cannot be moved to");
  await until("the terminal's tab", async () => (await layout()).focused === terminal);
  await until("the page's view to be let go with its tab", async () => (await view(page)) === null && !(await box("#panes .ppage")));
  if (!(await run("plugin:text-tools.type-uuid"))) fail("the action was not done");
  const uuid = /[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}/;
  out.uuid = await until("the UUID in the terminal", async () => (uuid.exec(sh(`${cli} get-text --pane-id ${terminal}`)) ?? [null])[0]);

  // --- (e) One opens the plugin's page, in a tab the page moves to.
  if (!(await run("plugin:text-tools.open"))) fail("the page action was not done");
  const opened = await until("another page to open and be moved to", async () => {
    const now = await layout();
    const pane = (now.panes ?? []).find((pane) => pane.id === now.focused);
    return pane && pane.id !== page && pane.page?.plugin === "text-tools" ? pane.id : null;
  });
  out.opened = opened;
  await until("the new page to be drawn", async () => texts(await view(opened)).length > 0);
  if (await run("plugin:text-tools.nope")) fail("an action the plugin does not have was done");
  await ev("window.thinkterm.client.plugins_want('palette', false)");

  const exceptions = logs.filter((l) => l.startsWith("EXCEPTION"));
  if (exceptions.length) fail("the page threw: " + exceptions.join("; "));
  console.log(JSON.stringify(out, null, 1));
  ws.close(); chrome.kill(); process.exit(0);
})().catch((e) => { console.error("FAIL", e.message); try { chrome.kill(); } catch {} process.exit(1); });
