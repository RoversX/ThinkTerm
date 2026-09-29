// Plugins in a browser end to end, against a throwaway server with the
// example plugin installed beside a broken one and a second copy claiming
// the example's id: Settings › Sidebar & Plugins lists them all and the built-in
// Snippets; nothing of a plugin shows outside the sidebar -- the palette
// offers none of it, and calls made from the command line leave nothing on
// the page; a plugin installed while the page is open is found
// when the settings open again; a plugin turned off stops, one chosen to run
// always starts by itself, and Snippets turned off leaves the right panel; a
// reload stops the running plugin; and a plugin whose program is gone says
// why in its row.
//   node plugins-test.js <url> <out.png>
// Reads TEST_HOME (the server's HOME), MUX_CLI (`thinkterm cli` pinned to
// the server), PLUGIN_CLI (`thinkterm plugin` under that HOME) and
// PLUGINS_DATA (the data directory the plugins are installed under).
const { spawn, execSync } = require("child_process");
const http = require("http"); const fs = require("fs"); const os = require("os");
const WebSocket = require("ws");
const [url, outPng] = process.argv.slice(2);
const { TEST_HOME, MUX_CLI, PLUGIN_CLI, PLUGINS_DATA, EXAMPLE_DIR, EXAMPLE_BIN } = process.env;
const chromePath = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 9333 + Math.floor(Math.random() * 500);
setTimeout(() => { console.error("watchdog"); try { chrome.kill(); } catch {} process.exit(4); }, 180000).unref();
const profile = fs.mkdtempSync(`${os.tmpdir()}/tt-plugins-`);
const chrome = spawn(chromePath, ["--headless=new", "--no-first-run", `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, "--window-size=1400,800", "--hide-scrollbars", "--enable-unsafe-webgpu", "about:blank"], { stdio: "ignore" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function browserWs() { for (let i = 0; i < 50; i++) { try { return await new Promise((res, rej) => http.get(`http://127.0.0.1:${port}/json/version`, (r) => { let b = ""; r.on("data", (d) => b += d); r.on("end", () => res(JSON.parse(b).webSocketDebuggerUrl)); }).on("error", rej)); } catch { await sleep(200); } } throw new Error("chrome did not come up"); }

const socketPath = `${TEST_HOME}/.local/share/thinkterm/plugins-sock`;
const pids = (pattern) => { try { return execSync(`pgrep -f ${JSON.stringify(pattern)}`).toString().trim().split(/\s+/).filter(Boolean); } catch { return []; } };
const allText = () => JSON.parse(execSync(`${MUX_CLI} list --format json`).toString())
  .map((p) => execSync(`${MUX_CLI} get-text --pane-id ${p.pane_id}`).toString()).join("\n");
const UUID = /[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}/;
const switches = () => { try { return fs.readFileSync(`${PLUGINS_DATA}/plugins.json`, "utf8"); } catch { return ""; } };

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
  const fail = (what) => { throw new Error(what); };
  const attached = async () => /this browser has/.test(await ev("document.getElementById('status').dataset.summary || ''"));
  const rows = () => ev("[...document.querySelectorAll('#settings [data-plugin]')].map((r) => ({ id: r.dataset.plugin, name: r.querySelector('.lab').textContent, detail: r.querySelector('.desc').textContent, on: r.querySelector('input')?.checked ?? null }))");
  // The row a plugin's id is used by: a copy claiming it is listed too, without a switch.
  const row = async (pluginId) => {
    const all = (await rows()).filter((r) => r.id === pluginId);
    return all.find((r) => r.on !== null) ?? all[0];
  };
  const paletteIds = () => ev("[...document.querySelectorAll('#palette .pe')].map((e) => e.dataset.id)");
  const cliList = () => execSync(`${PLUGIN_CLI} list`).toString();
  // The whole palette opens by its shortcut: Cmd+K on a Mac, Ctrl+K elsewhere.
  const openPalette = async (query) => {
    const press = (mods) => ev(`window.dispatchEvent(new KeyboardEvent('keydown', { key: 'k', bubbles: true, ${mods} }))`);
    await press("metaKey: true");
    if (!(await ev("!!document.querySelector('#palette input')"))) await press("ctrlKey: true");
    await until("the palette", () => ev("!!document.querySelector('#palette input')"));
    await type("#palette input", query);
  };
  const closePalette = () => ev("document.querySelector('#palette input')?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))");
  const out = {};

  await send("Page.navigate", { url }, s);
  await until("attach", attached, 20000);
  await sleep(600);
  if (pids(socketPath).length) fail("a plugin host runs before anything asked for one");

  // --- (a) Settings › Sidebar & Plugins lists the built-in, the example and the broken one.
  if (!(await click("[data-action=settings]"))) fail("no settings button");
  await until("the settings panel", () => ev("!!document.querySelector('#settings .sec[data-section=sidebar]')"));
  await click("#settings .sec[data-section=sidebar]");
  await until("the plugin rows", async () => (await rows()).length === 4);
  const listed = await rows();
  out.rows = listed.map((r) => `${r.id}:${r.on}`);
  const snippets = listed.find((r) => r.id === "snippets");
  const tools = listed.find((r) => r.id === "text-tools" && r.on !== null);
  const twin = listed.find((r) => r.id === "text-tools" && r.on === null);
  const broken = listed.find((r) => r.id === "broken");
  if (!twin || !/text-tools has the same id/.test(twin.detail)) fail("the copy claiming the example's id: " + JSON.stringify(twin));
  if (!snippets || !/Built in · Provides the Snippets panel/.test(snippets.detail) || snippets.on !== null) fail("the built-in Snippets row: " + JSON.stringify(listed));
  if (!tools || tools.name !== "Text Tools" || !/0\.1\.0/.test(tools.detail) || tools.on !== true) fail("the example's row: " + JSON.stringify(tools));
  if (!broken || broken.on !== null || !/plugin\.toml, line 1/.test(broken.detail)) fail("the broken row: " + JSON.stringify(broken));
  if (pids(socketPath).length !== 1) fail(`${pids(socketPath).length} plugin hosts run`);
  if (pids("thinkterm-plugin-example").length) fail("the example runs before anything used it");
  out.settings = "ok";
  await click("#settings .sc");

  // --- (b) Nothing of a plugin outside the sidebar: the palette offers none
  // of it, and calls made from the command line leave nothing here.
  await openPalette("uuid");
  await sleep(800);
  const offered = (await paletteIds()).filter((id) => id.startsWith("plugin:"));
  if (offered.length) fail("plugin commands in the palette: " + offered.join(", "));
  await closePalette();
  const printed = execSync(`${PLUGIN_CLI} call text-tools '{"op":"uuid"}'`).toString();
  if (!UUID.test(printed)) fail("the command line got no UUID: " + printed);
  const decoded = execSync(`${PLUGIN_CLI} call text-tools '{"op":"decode","text":"aGk="}'`).toString();
  if (!/"text": "hi"/.test(decoded)) fail("the command line got no decoding: " + decoded);
  await sleep(800);
  if (await ev("!!document.getElementById('plugin-notice')")) fail("a plugin's notice is on the page");
  if (UUID.test(allText())) fail("a plugin's outcome went into a pane");
  out.outside_sidebar = "nothing";
  if (pids("thinkterm-plugin-example").length !== 1) fail("the example is not running after its command");
  // It is running now, and the settings say so.
  await click("[data-action=settings]");
  await click("#settings .sec[data-section=sidebar]");
  await until("Running", async () => /Running/.test((await row("text-tools"))?.detail || ""));
  await click("#settings .sc");

  // --- (b2) A plugin installed now is found when the settings open again.
  const more = `${PLUGINS_DATA}/plugins/more-tools`;
  fs.mkdirSync(more, { recursive: true });
  fs.writeFileSync(`${more}/plugin.toml`, fs.readFileSync(`${EXAMPLE_DIR}/plugin.toml`, "utf8").replace('id = "text-tools"', 'id = "more-tools"').replace('name = "Text Tools"', 'name = "More Tools"'));
  fs.copyFileSync(EXAMPLE_BIN, `${more}/thinkterm-plugin-example`);
  fs.chmodSync(`${more}/thinkterm-plugin-example`, 0o755);
  await click("[data-action=settings]");
  await click("#settings .sec[data-section=about]");
  await click("#settings .sec[data-section=sidebar]");
  await until("the new plugin in the settings", async () => !!(await row("more-tools")));
  await click("#settings .sc");
  fs.rmSync(more, { recursive: true, force: true });
  out.installed_meanwhile = "ok";

  // --- (c) Turned off, its program stops, and the switch is on disk.
  await click("[data-action=settings]");
  await click("#settings .sec[data-section=sidebar]");
  await until("the rows", async () => (await rows()).length >= 4);
  await click('#settings [data-plugin-switch="text-tools"]');
  await until("the example off", async () => (await row("text-tools"))?.on === false && /text-tools/.test(switches()));
  await until("its program to stop", () => pids("thinkterm-plugin-example").length === 0, 5000);
  if (!/text-tools\s+off/.test(cliList())) fail("the command line does not see it off:\n" + cliList());
  await click('#settings [data-plugin-switch="text-tools"]');
  await until("the example on", async () => (await row("text-tools"))?.on === true);
  out.switch = "ok";

  // --- (c2) How long it runs unused: chosen here, kept on disk beside its
  // switch, and "always" has the server's machine keep it running -- the
  // mux, ThinkTerm there, keeps the plugin host up for it and it starts by
  // itself. The manifest's own choice again is no choice at all.
  const picker = '#settings select[data-plugin-background="text-tools"]';
  const choose = (value) => ev(`(() => { const select = document.querySelector('${picker}'); select.value = '${value}'; select.dispatchEvent(new Event('change', { bubbles: true })); return true; })()`);
  const chosen = () => ev(`document.querySelector('${picker}')?.value ?? null`);
  await until("the background beside the switch", async () => (await chosen()) === "briefly");
  const always = () => { try { return fs.readFileSync(`${PLUGINS_DATA}/plugins-always`, "utf8"); } catch { return ""; } };
  if (always()) fail("a plugin runs always before any was chosen to: " + always());
  await choose("always");
  await until("always, on disk", async () => (await chosen()) === "always" && /"background": "always"/.test(switches()) && /text-tools/.test(always()));
  await until("it to start by itself", () => pids("thinkterm-plugin-example").length === 1, 30000);
  await choose("briefly");
  await until("the manifest's own again", async () => (await chosen()) === "briefly" && !/background/.test(switches()) && !always());
  out.background = "ok";

  // --- (d) Snippets off takes its tab out of the right panel.
  await click("#settings .sc");
  if (!(await click("#tabs [data-action=agents]"))) fail("no right-panel button");
  await until("the Snippets tab", () => ev("!!document.querySelector('#agents .mode[data-mode=snippets]')"));
  await click("[data-action=settings]");
  await click("#settings .sec[data-section=sidebar]");
  await until("the rows", async () => (await rows()).length >= 4);
  // Snippets is a built-in plugin: its switch is its panel's, among the
  // panels, and its row in the list has none.
  if (await ev("!!document.querySelector('#settings [data-plugin=snippets] input')")) fail("the built-in row has a switch");
  await click('#settings [data-panel-switch="snippets"]');
  await until("Snippets off", () => ev("document.querySelector('#settings [data-panel-switch=snippets]')?.checked === false"));
  await until("the Snippets tab to go", () => ev("!document.querySelector('#agents .mode[data-mode=snippets]')"));
  await click('#settings [data-panel-switch="snippets"]');
  await until("the Snippets tab back", () => ev("!!document.querySelector('#agents .mode[data-mode=snippets]')"));
  out.snippetsTab = "ok";

  // --- (e) Reload stops the running plugin; a missing program is a failure its row explains.
  await click("#settings .sc");
  execSync(`${PLUGIN_CLI} call text-tools '{"op":"uuid"}'`);
  await until("the example running", () => pids("thinkterm-plugin-example").length === 1, 8000);
  await click("[data-action=settings]");
  await click("#settings .sec[data-section=sidebar]");
  await click('#settings [data-action="reload-plugins"]');
  await until("it stopped", () => pids("thinkterm-plugin-example").length === 0, 5000);
  await until("the row back to idle", async () => !/Running/.test((await row("text-tools"))?.detail || "Running"));
  fs.rmSync(`${PLUGINS_DATA}/plugins/text-tools/thinkterm-plugin-example`);
  let why = "";
  try { execSync(`${PLUGIN_CLI} call text-tools '{"op":"uuid"}'`, { stdio: "pipe" }); } catch (err) { why = String(err.stderr); }
  if (!/cannot find its program/.test(why)) fail("the command line was not told why: " + why);
  const failed = await until("the failure in its row", async () => { const r = await row("text-tools"); return r && /cannot find its program/.test(r.detail) && r.detail; });
  out.failed = failed;
  if (await ev("!!document.getElementById('plugin-notice')")) fail("a plugin's failure is on the page outside the sidebar");

  const shot = await send("Page.captureScreenshot", { format: "png" }, s);
  if (shot.data) fs.writeFileSync(outPng, Buffer.from(shot.data, "base64"));
  out.exceptions = logs.filter((l) => l.startsWith("EXCEPTION"));
  if (out.exceptions.length) fail("the page threw: " + out.exceptions.join("; "));
  console.log(JSON.stringify(out, null, 1));
  chrome.kill(); process.exit(0);
})().catch((err) => { console.error("FAIL", err.message); try { chrome.kill(); } catch {} process.exit(1); });
