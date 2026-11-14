// A thread's pages come back with it after the server is gone and started
// again, against a throwaway server with the example plugin installed: the
// page makes a thread; three pages are opened in it from the command line,
// beside its terminal, over it, and in a tab of their own; the layout the
// server saved holds them; the server is killed outright and a new one
// started; the page attaches again, the thread is moved to and so restored,
// and its pages are where they were, each the plugin's and opened with what
// it was -- the same pages, by their ids, from the same directory, though
// no longer from a terminal: the terminals are numbered anew.
//   node page-restore-test.js <cli prefix> <throwaway dir> <server command> <port>
const { spawn, execSync } = require("child_process");
const http = require("http"); const fs = require("fs"); const os = require("os");
const WebSocket = require("ws");
const [cli, dir, server, portArg] = process.argv.slice(2);
const chromePath = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 9333 + Math.floor(Math.random() * 500);
setTimeout(() => { console.error("watchdog"); try { chrome.kill(); } catch {} process.exit(4); }, 150000).unref();
const profile = fs.mkdtempSync(`${os.tmpdir()}/tt-page-restore-`);
const chrome = spawn(chromePath, ["--headless=new", "--no-first-run", `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, "--window-size=1400,900", "--hide-scrollbars", "--enable-unsafe-webgpu", "about:blank"], { stdio: "ignore" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function browserWs() { for (let i = 0; i < 50; i++) { try { return await new Promise((res, rej) => http.get(`http://127.0.0.1:${port}/json/version`, (r) => { let b = ""; r.on("data", (d) => b += d); r.on("end", () => res(JSON.parse(b).webSocketDebuggerUrl)); }).on("error", rej)); } catch { await sleep(200); } } throw new Error("chrome did not come up"); }
const sh = (cmd) => { try { return execSync(cmd, { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }); } catch (e) { throw new Error(`${cmd.slice(-80)}: ${String(e.stderr || e.message).trim().slice(-300)}`); } };
const mint = () => sh(`${cli} web-token mint --label page-restore --ttl 1h --url-only`).split("\n")[0];
// The panes as the server lists them, and those that hold a page.
const panes = () => JSON.parse(sh(`${cli} list --format json`));
const pages = () => panes().filter((pane) => pane.page);

(async () => {
  const ws = new WebSocket(await browserWs()); await new Promise((r) => ws.on("open", r));
  let id = 0; const waiting = new Map(); const logs = [];
  ws.on("message", (m) => { const msg = JSON.parse(m); if (msg.id && waiting.has(msg.id)) { waiting.get(msg.id)(msg.result || msg.error); waiting.delete(msg.id); }
    if (msg.method === "Runtime.exceptionThrown") logs.push("EXCEPTION " + JSON.stringify(msg.params.exceptionDetails.exception?.description || msg.params.exceptionDetails.text)); });
  const send = (method, params = {}, sessionId) => new Promise((r) => { const i = ++id; waiting.set(i, r); ws.send(JSON.stringify({ id: i, method, params, sessionId })); });
  const { targetId } = await send("Target.createTarget", { url: "about:blank" });
  const s = (await send("Target.attachToTarget", { targetId, flatten: true })).sessionId;
  await send("Runtime.enable", {}, s); await send("Page.enable", {}, s);
  const ev = async (expr) => (await send("Runtime.evaluate", { expression: expr, returnByValue: true }, s)).result?.value;
  const until = async (what, fn, ms = 20000) => { const t = Date.now(); while (Date.now() - t < ms) { const v = await fn(); if (v) return v; await sleep(150); } throw new Error("timeout waiting for " + what); };
  const fail = (what) => { throw new Error(what); };
  const click = (sel) => ev(`(() => { const el = document.querySelector(${JSON.stringify(sel)}); if (!el) return false; el.click(); return true; })()`);
  const attached = async () => /this browser has/.test(await ev("document.getElementById('status')?.dataset.summary || ''"));
  const layout = async () => JSON.parse((await ev("window.thinkterm?.client?.layout()")) || "{}");
  const threads = async () => JSON.parse(await ev("document.getElementById('side')?.dataset.rows || '[]'")).filter((row) => row.kind === "thread");
  const out = {};

  // --- (a) A thread, with a terminal, made by the page.
  await send("Page.navigate", { url: mint() }, s);
  await until("attach", attached);
  await sleep(900);
  await click("[data-action=new-thread]");
  const thread = await until("the thread", async () => (await threads()).find((t) => t.selected));
  const terminal = await until("the thread's terminal", async () => {
    const now = await layout();
    return (now.panes ?? []).length === 1 && now.focused !== undefined ? now.focused : null;
  });
  out.thread = thread.name;
  // The thread's panes are those of its workspace: the server has a
  // terminal of its own besides.
  const workspace = panes().find((pane) => pane.pane_id === terminal)?.workspace;
  if (!workspace) fail("the thread's terminal is not listed");
  const ofThread = () => panes().filter((pane) => pane.workspace === workspace);

  // --- (b) Three pages in it: beside the terminal, over it, and in a tab.
  sh(`${cli} open-page text-tools --pane-id ${terminal} --right --title Beside --args '{"where":"beside"}'`);
  sh(`${cli} open-page text-tools --pane-id ${terminal} --stack --title Over --icon list-todo --args '{"where":"over"}'`);
  sh(`${cli} open-page text-tools --pane-id ${terminal} --title "A tab" --args '{"where":"tab"}'`);
  const before = await until("the three pages", async () => (pages().length === 3 ? ofThread() : null));
  // Each page with what it was opened with and its size; the terminal
  // without its size, which under the page over it is whatever it was
  // when last on show.
  const shape = (all) => all
    .map((pane) => (pane.page ? `page ${JSON.stringify(pane.page.args ?? null)} ${pane.size.cols}x${pane.size.rows}` : "terminal"))
    .sort();
  const titles = (all) => all.filter((pane) => pane.page).map((pane) => pane.title).sort();
  // Which page each is, and where it was opened from.
  const origins = (all) => all
    .filter((pane) => pane.page)
    .map((pane) => `${pane.page.id} ${pane.page.cwd ? "in a directory" : "nowhere"} ${pane.page.terminal ?? "no terminal"}`)
    .sort();
  out.before = shape(before);
  out.titles = titles(before);
  const opened = origins(before);
  if (new Set(before.filter((pane) => pane.page).map((pane) => pane.page.id)).size !== 3) fail(`the pages share ids: ${opened}`);
  if (!before.filter((pane) => pane.page).every((pane) => pane.page.terminal === terminal && pane.page.cwd)) fail(`the pages are not from the terminal: ${opened}`);
  const tabsOf = (all) => new Set(all.map((pane) => pane.tab_id)).size;
  out.tabs = tabsOf(before);
  if (out.tabs !== 2) fail(`the thread has ${out.tabs} tabs, not the terminal's and the page's`);

  // --- (c) The server saved them with the thread's layout.
  const saved = await until("the saved layout", async () => {
    const found = sh(`find ${JSON.stringify(dir)} -name thinkterm_layout.json`).trim().split("\n")[0];
    if (!found) return null;
    const text = fs.readFileSync(found, "utf8");
    // What each was opened with is kept as it was written, as text.
    return (text.match(/where/g) ?? []).length === 3 ? text : null;
  }, 15000);
  out.saved = (saved.match(/text-tools/g) ?? []).length;

  // --- (d) The server is killed outright, and a new one started: none of
  // the old one's pages is left.
  // This throwaway server and no other: the one started with its config.
  const old = sh(`pgrep -f ${JSON.stringify(`thinkterm-mux-server.*${dir}/web.lua`)}`).trim().split(/\s+/);
  for (const pid of old) process.kill(Number(pid), "SIGKILL");
  await sleep(1500);
  sh(server);
  await until("the new server", async () => { try { sh(`curl -s -o /dev/null http://127.0.0.1:${portArg}/`); return true; } catch { return false; } });
  out.fresh = pages().length;
  if (out.fresh !== 0) fail(`the new server started with ${out.fresh} pages`);

  // --- (e) The page attaches again, and the thread comes back with its
  // pages where they were.
  // Opened anew, as after the browser was closed: a link that differs only
  // past its "#" would not load the page again.
  await send("Page.navigate", { url: "about:blank" }, s);
  await sleep(300);
  await send("Page.navigate", { url: mint() }, s);
  await until("attach again", attached).catch(async (e) => {
    fail(`${e.message}: the page says ${JSON.stringify(await ev("document.getElementById('status')?.dataset.summary || document.body.innerText.slice(0, 300)"))}; ${logs.slice(-3).join(" | ")}`);
  });
  // A thread whose terminals went with the server is listed, and comes
  // back when it is moved to.
  const back = await until("the thread in the list again", async () => (await threads()).find((t) => t.id === thread.id));
  out.listedAs = back.name;
  if (!(await ev(`JSON.parse(window.thinkterm.client.palette_run(${JSON.stringify(`thread:${back.id}`)})).handled`))) fail("the thread cannot be moved to");
  const after = await until("the thread's panes again", async () => (pages().length === 3 ? ofThread() : null), 30000).catch(async (e) => {
    fail(`${e.message}: panes ${JSON.stringify(panes().map((pane) => [pane.pane_id, pane.workspace, pane.page ? "page" : "terminal"]))}; threads ${JSON.stringify((await threads()).map((t) => [t.name, t.selected]))}; was ${workspace}`);
  });
  out.after = shape(after);
  out.titlesAfter = titles(after);
  out.origins = origins(after);
  const expected = opened.map((origin) => origin.replace(/ \d+$/, " no terminal"));
  if (JSON.stringify(out.origins) !== JSON.stringify(expected)) fail(`restored as ${JSON.stringify(out.origins)}, not ${JSON.stringify(expected)}`);
  out.tabsAfter = tabsOf(after);
  if (JSON.stringify(out.after) !== JSON.stringify(out.before)) fail(`restored as ${JSON.stringify(out.after)}, not ${JSON.stringify(out.before)}`);
  if (JSON.stringify(out.titlesAfter) !== JSON.stringify(out.titles)) fail(`the pages are titled ${JSON.stringify(out.titlesAfter)}`);
  if (out.tabsAfter !== out.tabs) fail(`restored into ${out.tabsAfter} tabs`);
  // And the page draws one of them: the plugin was asked anew.
  const shown = await until("a restored page to be drawn", async () => {
    const now = await layout();
    const pane = (now.panes ?? []).find((pane) => pane.page);
    if (!pane) return null;
    const view = JSON.parse((await ev(`window.thinkterm.client.plugin_panel("page:${pane.id}")`)) || "null");
    return view?.painting ? pane.page.plugin : null;
  }, 30000);
  out.drawn = shown;

  const exceptions = logs.filter((l) => l.startsWith("EXCEPTION"));
  if (exceptions.length) fail("the page threw: " + exceptions.join("; "));
  console.log(JSON.stringify(out, null, 1));
  ws.close(); chrome.kill(); process.exit(0);
})().catch((e) => { console.error("FAIL", e.message); try { chrome.kill(); } catch {} process.exit(1); });
