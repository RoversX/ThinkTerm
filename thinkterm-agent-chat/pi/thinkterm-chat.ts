// ThinkTerm chat extension
//
// Lets ThinkTerm show this Pi session as a chat (on its phone app) while
// Pi goes on running in the terminal as usual. ThinkTerm put this file
// here; it does nothing unless Pi runs in a ThinkTerm pane.
//
// Inside a pane it listens on a Unix socket only this user can reach, and
// leaves a note saying where, under ~/.local/state/thinkterm/agent-chat.
// ThinkTerm's bridge (`thinkterm cli agent chat`) connects there; the two
// exchange one JSON object per line: the conversation and what happens
// in it from here, what to say and when to stop from there.

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import * as crypto from "node:crypto";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";

/// Raised with every change: ThinkTerm puts a newer one in place of an
/// older one when a chat opens.
const EXTENSION_VERSION = 9;
/// 2: what is said may carry files and pictures; the model can be changed.
/// 3: the commands the session takes are listed (`commands`), and how full
/// its context is said (`context`).
const PROTOCOL = 3;
const MAX_LINE = 8 << 20;
const OUTPUT_KEPT = 6 << 10;
const DIFF_KEPT = 24 << 10;
/// The most history sent in one line, in bytes: well under the line the
/// bridge takes in.
const HISTORY_BUDGET = 4 << 20;
/// Items sent at once; older ones come as the person scrolls up.
const PAGE_ITEMS = 200;
/// What is kept of what Pi said or thought, each, and of a command.
const TEXT_KEPT = 256 << 10;
/// What is shown of a command asked about: its start, said to go on.
const ASK_KEPT = 16 << 10;
/// The diffs of one change, all its files together.
const DIFFS_KEPT = 8 * DIFF_KEPT;
/// A picture sent from the phone larger than this goes by its path.
const PICTURE_KEPT = 20 << 20;
/// Pictures copied out of sessions for the phone, untouched this long,
/// are let go (`pictures`).
const PICTURES_KEPT_MS = 30 * 24 * 3600 * 1000;
/// How hard a model can think, as Pi words it, least first: Pi's own
/// rule is asked (`getSupportedThinkingLevels`); this, where it cannot be.
const LEVELS = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
/// Pi's own commands the chat runs, as Pi words them.
const OWN_COMMANDS = [
  { name: "/new", description: "Start a new session" },
  { name: "/compact", description: "Manually compact the session context", args: "[instructions]" },
  { name: "/name", description: "Set session display name", args: "<name>" },
  { name: "/reload", description: "Reload keybindings, extensions, skills, prompts, themes, and context files" },
];
/// Pi's (and OMP's) own commands that need its terminal: said so, rather
/// than sent to the model as words.
const TERMINAL_COMMANDS = new Set([
  "settings", "tree", "scoped-models", "export", "import", "share", "bug", "copy", "session", "changelog",
  "hotkeys", "fork", "clone", "trust", "login", "logout", "resume", "quit", "exit",
]);
/// The command this extension runs `/new` and `/reload` through: only a
/// command is given what starts a session or reloads one.
const RUN = "thinkterm-chat";
/// What is shown of a command's description.
const DESCRIPTION_KEPT = 300;
/// How often what Pi is writing is sent as it grows, in milliseconds.
const WRITING_EVERY = 150;
/// How long after a change the context is reckoned again, in milliseconds.
const CONTEXT_AFTER = 400;
/// What a picture is reckoned to take of the context, in tokens.
const PICTURE_TOKENS = 1200;
/// Pi's own room kept free before it compacts by itself, when its
/// settings do not say.
const RESERVE_TOKENS = 16384;

type State = "working" | "idle" | "blocked";

export default function (pi: ExtensionAPI) {
  const pane = process.env.WEZTERM_PANE;
  if (!pane || !/^\d+$/.test(pane)) return;
  // OMP (Oh My Pi), Pi's fork, runs this extension too, from a folder of
  // its own; the chat names the agent it is.
  const agent = process.argv.some((arg) => arg.includes("@oh-my-pi") || /(^|\/)omp$/.test(arg)) ? "omp" : "pi";

  const dir = path.join(os.homedir(), ".local", "state", "thinkterm", "agent-chat");
  const socketPath = path.join(dir, `pi-${process.pid}.sock`);
  const notePath = path.join(dir, `pi-${process.pid}.json`);
  const clients = new Set<net.Socket>();
  // The oldest item each client was sent: where older history goes on from.
  const oldest = new Map<net.Socket, string>();
  let server: net.Server | undefined;
  let ctx: any;
  let state: State = "idle";
  let since = Math.floor(Date.now() / 1000);
  // What each running tool was asked, which its end does not repeat.
  const toolArgs = new Map<string, any>();
  // The approvals OMP waits on, by tool call.
  const asking = new Map<string, any>();
  // The look, just after a run's end, whether the agent rests.
  let settling: ReturnType<typeof setTimeout> | undefined;
  // The model in use, as last chosen (here or in the terminal).
  let model: any;
  // What Pi is writing now (the message, as it grows), and the send of it
  // waiting its turn.
  let writing: { id: string; message: any } | undefined;
  let writingTimer: ReturnType<typeof setTimeout> | undefined;
  // The context reckoned again, a moment after it changed.
  let contextTimer: ReturnType<typeof setTimeout> | undefined;
  // The command `/new` and `/reload` go through was taken (`RUN`).
  let registered = false;
  // Where each picture of the sessions read is, as a file, by the message
  // part it is in: worked out once (a picture is hashed to name its copy).
  const pictureFiles = new Map<string, string>();
  // The files pictures sent from the phone came from, by the hash their
  // copies would be named by: the phone knows its pictures by those.
  const sentFrom = new Map<string, string>();
  // The agent's own word on how hard a model can think, once found.
  let supportedLevels: ((m: any) => string[]) | undefined;
  (async () => {
    for (const name of ["@earendil-works/pi-ai", "@oh-my-pi/pi-ai", "@mariozechner/pi-ai"]) {
      try {
        const ai: any = await import(name);
        if (typeof ai?.getSupportedThinkingLevels === "function") {
          supportedLevels = ai.getSupportedThinkingLevels;
          return;
        }
      } catch {}
    }
  })();

  const tidy = () => {
    for (const file of [notePath, socketPath]) {
      try {
        fs.unlinkSync(file);
      } catch {}
    }
  };

  function send(ev: object, to?: net.Socket) {
    const line = JSON.stringify(ev) + "\n";
    for (const client of to ? [to] : clients) {
      if (!client.destroyed) client.write(line);
    }
  }

  // Whether Pi works now: as it says, or as last told.
  function busy(): boolean {
    return typeof ctx?.isIdle === "function" ? !ctx.isIdle() : state !== "idle";
  }

  function setState(next: State) {
    if (next === state) return;
    state = next;
    since = Math.floor(Date.now() / 1000);
    send({ ev: "state", state, since });
  }

  function start() {
    if (server) return;
    // Taken off again when the session ends: Pi loads its extensions anew
    // for each session (/new, /resume, /fork), in the same process.
    process.on("exit", tidy);
    fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
    forgetOldPictures();
    try {
      fs.chmodSync(dir, 0o700);
    } catch {}
    tidy();
    const listening = net.createServer(welcome);
    listening.on("error", () => {});
    listening.listen(socketPath, () => {
      try {
        fs.chmodSync(socketPath, 0o600);
      } catch {}
      const note = {
        agent,
        pid: process.pid,
        pane: Number(pane),
        mux_socket: process.env.WEZTERM_UNIX_SOCKET ?? null,
        socket: socketPath,
        version: EXTENSION_VERSION,
      };
      fs.writeFileSync(notePath, JSON.stringify(note), { mode: 0o600 });
    });
    server = listening;
  }

  function stop() {
    // Nothing of this session's runs after it: its context is no longer
    // Pi's to use (Pi throws for one used after the session is replaced).
    clearTimeout(settling);
    settling = undefined;
    clearTimeout(writingTimer);
    writingTimer = undefined;
    writing = undefined;
    clearTimeout(contextTimer);
    contextTimer = undefined;
    // What this session asked goes with it.
    withdraw();
    for (const client of clients) client.destroy();
    clients.clear();
    oldest.clear();
    server?.close();
    server = undefined;
    tidy();
    process.off("exit", tidy);
  }

  // All a client is told of the session, from the start.
  function greet(client: net.Socket) {
    send({ ev: "hello", protocol: PROTOCOL, agent, title: pi.getSessionName() ?? null, guessed: false }, client);
    sendHistory(client);
    send({ ev: "state", state, since }, client);
    for (const ask of asking.values()) send({ ev: "ask", ask }, client);
    sendModels(client);
    sendCommands(client);
    try {
      sendContext(client);
    } catch {}
  }

  // The context said again a moment after it changed: once for a burst.
  function contextSoon() {
    if (!server || !clients.size || contextTimer) return;
    contextTimer = setTimeout(() => {
      contextTimer = undefined;
      try {
        sendContext();
      } catch {}
    }, CONTEXT_AFTER);
  }

  // Roughly what `text` takes of a model's context: a token for four
  // characters, one for each of a wide script's.
  function tokensIn(text: string): number {
    let wide = 0;
    let other = 0;
    for (let i = 0; i < text.length; i++) {
      if (text.charCodeAt(i) >= 0x2e80) wide++;
      else other++;
    }
    return wide + other / 4;
  }

  function partsOf(content: any, kind: string, add: (kind: string, tokens: number) => void) {
    if (typeof content === "string") return add(kind, tokensIn(content));
    for (const part of Array.isArray(content) ? content : []) {
      if (part?.type === "image") add(kind, PICTURE_TOKENS);
      else if (typeof part?.text === "string") add(kind, tokensIn(part.text));
    }
  }

  // What one message the model is sent takes, by what it is.
  function countMessage(message: any, add: (kind: string, tokens: number) => void) {
    switch (message?.role) {
      case "user":
        return partsOf(message.content, "user", add);
      case "assistant":
        for (const block of Array.isArray(message.content) ? message.content : []) {
          if (block?.type === "text") add("assistant", tokensIn(String(block.text ?? "")));
          else if (block?.type === "thinking") add("thinking", tokensIn(String(block.thinking ?? "")));
          else if (block?.type === "toolCall") add("tool_use", tokensIn(String(block.name ?? "") + JSON.stringify(block.arguments ?? {})));
        }
        return;
      case "toolResult":
        return partsOf(message.content, "tool_use", add);
      case "bashExecution":
        return add("tool_use", tokensIn(String(message.command ?? "") + String(message.output ?? "")));
      case "compactionSummary":
      case "branchSummary":
        return add("summary", tokensIn(String(message.summary ?? "")));
      default:
        return partsOf(message?.content ?? JSON.stringify(message ?? ""), "other", add);
    }
  }

  // How full the model's context is: the total as Pi reckons it, and what
  // it is made of -- the system prompt, the tools, each message the model
  // is sent -- each counted, then scaled to that total. With what the
  // session has used so far, every turn's.
  function sendContext(to?: net.Socket) {
    const parts = new Map<string, number>();
    const add = (kind: string, tokens: number) => {
      if (tokens > 0) parts.set(kind, (parts.get(kind) ?? 0) + tokens);
    };
    try {
      add("system", tokensIn(String(ctx?.getSystemPrompt?.() ?? "")));
    } catch {}
    try {
      const active = new Set<string>(pi.getActiveTools?.() ?? []);
      for (const tool of (pi.getAllTools?.() ?? []) as any[]) {
        if (active.size && !active.has(tool?.name)) continue;
        add("tools", tokensIn(JSON.stringify({ name: tool?.name, description: tool?.description, parameters: tool?.parameters })));
      }
    } catch {}
    let messages: any[] = [];
    try {
      const sessions = ctx?.sessionManager;
      messages = sessions?.buildSessionProjection?.()?.messages ?? sessions?.buildSessionContext?.()?.messages ?? [];
    } catch {}
    for (const message of messages) countMessage(message, add);
    const counted = [...parts.values()].reduce((a, b) => a + b, 0);
    let usage: any;
    try {
      usage = ctx?.getContextUsage?.();
    } catch {}
    const current = model ?? ctx?.model;
    const window: number | null = usage?.contextWindow || current?.contextWindow || null;
    // Not known just after a compaction, until the next answer: the parts
    // are then as counted.
    const used = typeof usage?.tokens === "number" ? usage.tokens : null;
    const scale = used && counted ? used / counted : 1;
    let compactsAt: number | null = null;
    try {
      const compaction: any = pi.getSettings?.()?.compaction;
      if (window && compaction?.enabled !== false) compactsAt = window - (compaction?.reserveTokens ?? RESERVE_TOKENS);
    } catch {}
    const totals: any = {};
    for (const entry of (ctx?.sessionManager?.getBranch?.() ?? []) as any[]) {
      const spent = entry?.type === "message" && entry.message?.role === "assistant" ? entry.message.usage : undefined;
      if (!spent) continue;
      totals.input = (totals.input ?? 0) + (spent.input ?? 0);
      totals.output = (totals.output ?? 0) + (spent.output ?? 0);
      totals.cache_read = (totals.cache_read ?? 0) + (spent.cacheRead ?? 0);
      totals.cache_write = (totals.cache_write ?? 0) + (spent.cacheWrite ?? 0);
      totals.cost = (totals.cost ?? 0) + (spent.cost?.total ?? 0);
    }
    send(
      {
        ev: "context",
        context: {
          used,
          window,
          parts: [...parts].map(([kind, tokens]) => ({ kind, tokens: Math.round(tokens * scale) })),
          compacts_at: compactsAt && compactsAt > 0 ? compactsAt : null,
          totals,
        },
      },
      to,
    );
  }

  // What can be typed for the session to do: Pi's own the chat runs,
  // then the extensions', prompt templates and skills, as Pi lists them.
  function sendCommands(to?: net.Socket) {
    // `/new` and `/reload` only where the command they go through is
    // there to run them.
    const commands: object[] = OWN_COMMANDS.filter((c) => runs() || (c.name !== "/new" && c.name !== "/reload"));
    let listed: any[] = [];
    try {
      listed = typeof pi.getCommands === "function" ? pi.getCommands() : [];
    } catch {}
    const seen = new Set(commands.map((c: any) => c.name));
    for (const command of listed) {
      const name = `/${String(command?.name ?? "")}`;
      if (name === "/" || name === `/${RUN}` || seen.has(name)) continue;
      seen.add(name);
      const description = String(command?.description ?? "").replace(/\s+/g, " ").trim();
      // What these take is not said: each is given room to be followed.
      commands.push({ name, description: description.slice(0, DESCRIPTION_KEPT), args: "" });
    }
    send({ ev: "commands", commands }, to);
  }

  // A command of Pi's own, sent as words: run here as the terminal would,
  // or said to need the terminal. False for words to send on.
  function ownCommand(text: string, refuse: (message: string) => void): boolean {
    const match = /^\/([\w:-]+)(?:\s+([\s\S]*))?$/.exec(text.trim());
    if (!match) return false;
    const [, name, rest = ""] = match;
    const args = rest.trim();
    // Another extension's command of the same name is its own.
    if (listedCommand(name)) return false;
    const idle = () => {
      if (!busy()) return true;
      refuse(`/${name} waits until ${agent} is done (or stopped)`);
      return false;
    };
    switch (name) {
      case "new":
      case "reload":
        if (!runs()) refuse(`/${name} works in the terminal only`);
        else if (idle()) run(`/${RUN} ${name}`, refuse);
        return true;
      case "compact":
        if (!idle()) return true;
        if (typeof ctx?.compact !== "function") {
          refuse(`${agent} cannot compact from here`);
          return true;
        }
        ctx.compact({
          customInstructions: args || undefined,
          onError: (err: unknown) => refuse(String((err as any)?.message ?? err)),
        });
        return true;
      case "name":
        if (!args) refuse("/name needs a name");
        else if (typeof pi.setSessionName === "function") pi.setSessionName(args);
        else refuse(`${agent} cannot be named from here`);
        return true;
      case "model":
        if (args) setModel(args, undefined, refuse);
        else refuse("/model takes a model's id: pick one from /model in the list");
        return true;
      case "thinking": {
        const levels = levelsOf(model ?? ctx?.model);
        if (typeof pi.setThinkingLevel !== "function") refuse(`${agent} cannot change how hard it thinks from here`);
        else if (!args) refuse("/thinking needs a level");
        else if (levels.length && !levels.includes(args)) refuse(`${agent} has no thinking level ${args} for this model`);
        else {
          pi.setThinkingLevel(args as any);
          sendModels();
        }
        return true;
      }
      default:
        if (!TERMINAL_COMMANDS.has(name)) return false;
        refuse(`/${name} works in the terminal only`);
        return true;
    }
  }

  // Whether the command `/new` and `/reload` go through is registered: an
  // agent that took it lists it.
  function runs(): boolean {
    try {
      return registered && typeof pi.getCommands === "function" && pi.getCommands().some((c: any) => c?.name === RUN);
    } catch {
      return false;
    }
  }

  function listedCommand(name: string): boolean {
    try {
      return typeof pi.getCommands === "function" && pi.getCommands().some((c: any) => c?.name === name && c?.source === "extension");
    } catch {
      return false;
    }
  }

  // Words Pi takes as typed: commands run, templates and skills expand.
  function run(text: string, refuse: (message: string) => void) {
    try {
      Promise.resolve(pi.sendUserMessage(text, { expandPromptTemplates: true } as any)).catch((err: unknown) => refuse(String(err)));
    } catch (err) {
      refuse(String(err));
    }
  }

  // The models this session may use: those it is scoped to, else all with
  // a key.
  function usable(): any[] {
    const scoped = ctx?.scopedModels;
    if (Array.isArray(scoped) && scoped.length) return scoped.map((s: any) => s?.model).filter(Boolean);
    try {
      return ctx?.modelRegistry?.getAvailable?.() ?? [];
    } catch {
      return [];
    }
  }

  function idOf(m: any): string {
    return `${m?.provider ?? ""}/${m?.id ?? ""}`;
  }

  // How hard a model can think, as the agent says: none to choose for
  // one that does not think.
  function levelsOf(m: any): string[] {
    if (!m?.reasoning) return [];
    if (supportedLevels) {
      try {
        const levels = supportedLevels(m);
        if (Array.isArray(levels)) return levels.map(String);
      } catch {}
    }
    return LEVELS.filter((level) => {
      const mapped = m.thinkingLevelMap?.[level];
      if (mapped === null) return false;
      if (level === "xhigh" || level === "max") return mapped !== undefined;
      return true;
    });
  }

  function sendModels(to?: net.Socket) {
    const current = model ?? ctx?.model;
    if (!current) return;
    const models = usable();
    if (!models.some((m) => idOf(m) === idOf(current))) models.unshift(current);
    let effort: string | null = null;
    try {
      effort = typeof pi.getThinkingLevel === "function" ? pi.getThinkingLevel() : (ctx?.thinkingLevel ?? null);
    } catch {}
    send(
      {
        ev: "models",
        current: idOf(current),
        effort,
        models: models.map((m) => ({ id: idOf(m), name: String(m.name ?? m.id ?? ""), efforts: levelsOf(m) })),
      },
      to,
    );
  }

  function setModel(wanted: string, effort: string | undefined, refuse: (message: string) => void) {
    const found = usable().find((m) => idOf(m) === wanted) ?? (idOf(model ?? ctx?.model) === wanted ? (model ?? ctx?.model) : undefined);
    if (!found) {
      refuse(`${agent} has no model ${wanted}`);
      return;
    }
    const level = () => {
      if (effort && typeof pi.setThinkingLevel === "function") pi.setThinkingLevel(effort as any);
      sendModels();
    };
    if (wanted === idOf(model ?? ctx?.model)) {
      level();
      return;
    }
    if (typeof pi.setModel !== "function") {
      refuse(`${agent} cannot change its model from here`);
      return;
    }
    Promise.resolve(pi.setModel(found))
      .then((ok: boolean) => {
        if (ok === false) {
          refuse(`no key for ${wanted}`);
          return;
        }
        model = found;
        level();
      })
      .catch((err: unknown) => refuse(String(err)));
  }

  function welcome(client: net.Socket) {
    clients.add(client);
    client.setEncoding("utf8");
    let buffered = "";
    client.on("data", (chunk: string) => {
      buffered += chunk;
      if (buffered.length > MAX_LINE) {
        client.destroy();
        return;
      }
      let at: number;
      while ((at = buffered.indexOf("\n")) >= 0) {
        const line = buffered.slice(0, at);
        buffered = buffered.slice(at + 1);
        if (!line.trim()) continue;
        // Whatever goes wrong with one line is said to its client, never
        // thrown into Pi.
        try {
          handle(line, client);
        } catch (err) {
          send({ ev: "refused", message: String(err) }, client);
        }
      }
    });
    const gone = () => {
      clients.delete(client);
      oldest.delete(client);
    };
    client.on("close", gone);
    client.on("error", gone);
    try {
      greet(client);
    } catch (err) {
      send({ ev: "error", message: String(err) }, client);
      client.destroy();
    }
  }

  function handle(line: string, client: net.Socket) {
    let op: any;
    try {
      op = JSON.parse(line);
    } catch {
      return;
    }
    const refuse = (message: string) => send({ ev: "refused", message }, client);
    switch (op?.op) {
      case "send": {
        const text = String(op.text ?? "");
        const files: any[] = Array.isArray(op.attachments) ? op.attachments.filter((f: any) => f?.path) : [];
        if (!text.trim() && !files.length) return;
        if (!files.length && ownCommand(text, refuse)) return;
        // Pictures go as pictures; files, and pictures too large to read,
        // by their paths after the words.
        // A model that takes no pictures is told their paths instead.
        const current = model ?? ctx?.model;
        const sees = Array.isArray(current?.input) && current.input.includes("image");
        const pictures: any[] = [];
        const paths: string[] = [];
        for (const file of files) {
          const picture = file.image && sees ? pictureOf(String(file.path)) : undefined;
          if (picture) pictures.push(picture);
          else paths.push(String(file.path));
        }
        const words = [text, ...paths].filter((part) => part.length).join("\n");
        const content: any = pictures.length ? [...(words ? [{ type: "text", text: words }] : []), ...pictures] : words;
        // As Enter in the terminal: while it works, the running turn takes
        // it in before its next step; commands and templates expand.
        const options: any = { expandPromptTemplates: true };
        if (busy()) options.deliverAs = "steer";
        try {
          Promise.resolve(pi.sendUserMessage(content, options)).catch((err: unknown) => refuse(String(err)));
        } catch (err) {
          refuse(String(err));
        }
        return;
      }
      case "set_model":
        setModel(String(op.model ?? ""), typeof op.effort === "string" ? op.effort : undefined, refuse);
        return;
      case "interrupt":
        if (busy() && typeof ctx?.abort === "function") ctx.abort();
        else refuse("nothing is running");
        return;
      case "older": {
        const items = allItems();
        const mark = oldest.get(client);
        const at = mark === undefined ? -1 : items.findIndex((item) => item.id === mark);
        if (at < 0) {
          // The branch is not the one it was sent: all again, from the end.
          sendHistory(client);
          return;
        }
        const { items: older, more } = page(items, at);
        if (older.length) oldest.set(client, older[0].id);
        send({ ev: "history", items: older, more, reset: false }, client);
        return;
      }
      default:
        refuse(`${agent} has no ${String(op?.op)}`);
    }
  }

  // The conversation so far, as chat items: the branch shown, its tool
  // calls met with their results.
  function allItems(): any[] {
    const entries: any[] = ctx?.sessionManager?.getBranch?.() ?? [];
    const results = new Map<string, any>();
    for (const entry of entries) {
      const message = entry?.message;
      if (entry?.type === "message" && message?.role === "toolResult") results.set(message.toolCallId, message);
    }
    const items: any[] = [];
    for (const entry of entries) {
      if (entry?.type === "compaction") items.push(compacted(entry.id));
      if (entry?.type !== "message") continue;
      items.push(...fromMessage(entry.message, entry.id, results));
    }
    return items;
  }

  function compacted(id: string): object {
    return { kind: "note", id: String(id), note: "compacted", text: "" };
  }

  // The newest items before `before` that fit one line, and whether there
  // are older ones still. One at least, whatever its size (its parts are
  // cut short where it is made), so that paging always goes on.
  function page(items: any[], before = items.length): { items: any[]; more: boolean } {
    let size = 0;
    let start = before;
    while (start > 0 && before - start < PAGE_ITEMS) {
      const next = Buffer.byteLength(JSON.stringify(items[start - 1]), "utf8") + 1;
      if (size + next > HISTORY_BUDGET && start < before) break;
      size += next;
      start -= 1;
    }
    return { items: items.slice(start, before), more: start > 0 };
  }

  function sendHistory(client: net.Socket) {
    const { items, more } = page(allItems());
    if (items.length) oldest.set(client, items[0].id);
    else oldest.delete(client);
    send({ ev: "history", items, more, reset: true }, client);
  }

  function fromMessage(message: any, id: string, results?: Map<string, any>): object[] {
    switch (message?.role) {
      case "user": {
        const attachments = picturesIn(message.content, id);
        const item: any = { kind: "user", id, text: wordsOf(message.content).slice(0, TEXT_KEPT) };
        if (attachments.length) item.attachments = attachments;
        return [item];
      }
      case "assistant": {
        const items: object[] = [];
        (message.content ?? []).forEach((block: any, i: number) => {
          if (block?.type === "text" && block.text?.trim())
            items.push({ kind: "text", id: `${id}-${i}`, text: block.text.slice(0, TEXT_KEPT) });
          else if (block?.type === "thinking" && block.thinking?.trim())
            items.push({ kind: "thinking", id: `${id}-${i}`, text: block.thinking.slice(0, TEXT_KEPT) });
          else if (block?.type === "toolCall" && results) items.push(toolItem(block.id, block.name, block.arguments, results.get(block.id)));
        });
        if (message.stopReason === "aborted") items.push({ kind: "note", id: `${id}-end`, note: "interrupted", text: "" });
        else if (message.stopReason === "error")
          items.push({ kind: "note", id: `${id}-end`, note: "failed", text: message.errorMessage ?? "" });
        return items;
      }
      case "bashExecution":
        return [
          {
            kind: "command",
            id,
            command: String(message.command ?? "").slice(0, TEXT_KEPT),
            status: message.cancelled ? "failed" : message.exitCode ? "failed" : "done",
            output: tail(message.output ?? "", OUTPUT_KEPT),
            exit: fit(message.exitCode),
            looks: [],
          },
        ];
      case "compactionSummary":
        return [{ kind: "note", id, note: "compacted", text: "" }];
      default:
        return [];
    }
  }

  // A tool call, finished when `result` is given, running when not.
  function toolItem(id: string, name: string, args: any, result?: any, running = false): object {
    const status = running || !result ? "running" : result.isError ? "failed" : "done";
    const output = result ? tail(textOf(result.content), OUTPUT_KEPT) : "";
    args = args ?? {};
    switch (name) {
      case "bash":
        return {
          kind: "command",
          id,
          command: String(args.command ?? "").slice(0, TEXT_KEPT),
          status,
          output,
          exit: exitOf(result, output),
          looks: [],
        };
      case "read":
        return look(id, `read ${shown(args.path)}`, "read", shown(args.path), status);
      case "grep":
      case "ast_grep":
        return look(id, `${name} ${args.pattern ?? ""}`, "search", args.pattern ?? args.path, status);
      case "find":
      case "glob":
        // Pi's find and OMP's glob take a pattern; OMP's find, a description.
        return look(id, `${name} ${args.pattern ?? args.query ?? ""}`, "search", args.pattern ?? args.query ?? args.path, status);
      case "ls":
        return look(id, `ls ${shown(args.path ?? ".")}`, "list", shown(args.path ?? "."), status);
      case "edit":
      case "write":
      case "ast_edit": {
        const files = capped(editedFiles(name, args, result));
        // An OMP edit names its files in a patch of its own: known once done.
        if (files.length) return { kind: "edit", id, status, files };
        return { kind: "tool", id, name, detail: "", status };
      }
      default:
        return { kind: "tool", id, name, detail: brief(args), status };
    }
  }

  function look(id: string, command: string, kind: string, target: any, status: string): object {
    return { kind: "command", id, command, status, output: "", exit: null, looks: [{ kind, target: String(target ?? "") }] };
  }

  // The files a change touched: as the tool tells once it is done (Pi's
  // patch; OMP's diff, per file when several), else from what it was asked.
  function editedFiles(name: string, args: any, result?: any): object[] {
    const details = result?.details;
    if (details && !result.isError) {
      if (typeof details.patch === "string" && details.patch) return [fileOf(args.path, details.patch)];
      if (Array.isArray(details.perFileResults))
        return details.perFileResults.filter((file: any) => file?.path).map((file: any) => fileOf(file.path, String(file.diff ?? "")));
      if (typeof details.diff === "string" && details.diff && (details.path || args.path))
        return [fileOf(details.path ?? args.path, details.diff)];
    }
    if (!args.path) return [];
    // Made up from what was asked: a bare `@@`, whose lines are all the
    // file's (a written `++i` is not a header).
    if (name === "write") return [fileOf(args.path, "@@\n" + lines(String(args.content ?? "")).map((l) => "+" + l).join("\n"))];
    // Pi: edits of old text for new; OMP: one, or patches.
    const edits: any[] = Array.isArray(args.edits) ? args.edits : [args];
    let diff = "";
    for (const edit of edits) {
      if (typeof edit?.diff === "string") {
        diff += edit.diff.endsWith("\n") ? edit.diff : edit.diff + "\n";
        continue;
      }
      const before = lines(String(edit?.oldText ?? edit?.old_string ?? ""));
      const after = lines(String(edit?.newText ?? edit?.new_string ?? ""));
      diff += `@@\n${[...before.map((l) => "-" + l), ...after.map((l) => "+" + l)].join("\n")}\n`;
    }
    return [fileOf(args.path, diff)];
  }

  // A file's change, counted from its diff, from the first hunk on, read
  // as ThinkTerm reads diffs (thinkterm-agent-chat/src/diff.rs): a hunk's
  // count tells a removed line that reads `--- x` from the next file's
  // header; a bare `@@` runs to the next.
  function fileOf(file: any, diff: string): object {
    const at = diff.search(/^@@/m);
    if (at > 0) diff = diff.slice(at);
    let added = 0;
    let removed = 0;
    // Lines left in a counted hunk, old and new; a bare one is open.
    let old = 0;
    let neu = 0;
    let open = false;
    for (const line of diff.split("\n")) {
      if (line.startsWith("@@")) {
        const counts = /^@@ -\d+(?:,(\d+))? \+\d+(?:,(\d+))? @@/.exec(line);
        open = !counts;
        old = counts ? (counts[1] === undefined ? 1 : Number(counts[1])) : 0;
        neu = counts ? (counts[2] === undefined ? 1 : Number(counts[2])) : 0;
        continue;
      }
      if (line.startsWith("\\")) continue;
      const counted = old > 0 || neu > 0;
      if (open && line.startsWith("diff ")) {
        open = false;
        continue;
      }
      if (!open && !counted && (line.startsWith("+++") || line.startsWith("---"))) continue;
      if (line.startsWith("+")) {
        added += 1;
        if (counted) neu = Math.max(neu - 1, 0);
      } else if (line.startsWith("-")) {
        removed += 1;
        if (counted) old = Math.max(old - 1, 0);
      } else if (counted) {
        old = Math.max(old - 1, 0);
        neu = Math.max(neu - 1, 0);
      }
    }
    return { path: shown(file), added, removed, diff: diff.slice(0, DIFF_KEPT) };
  }

  // A change of many files keeps the diffs of the first, up to a size; the
  // rest are counted.
  function capped(files: any[]): object[] {
    let left = DIFFS_KEPT;
    return files.map((file) => {
      const diff = String(file.diff ?? "");
      const kept = diff.length <= left ? diff : "";
      left -= kept.length;
      return { ...file, diff: kept };
    });
  }

  // A path as the person reads it: from the folder the agent works in.
  function shown(file: any): string {
    const text = String(file ?? "");
    const cwd = String(ctx?.cwd ?? process.cwd()).replace(/\/+$/, "");
    return cwd && text.startsWith(cwd + "/") ? text.slice(cwd.length + 1) : text;
  }

  function lines(text: string): string[] {
    if (!text) return [];
    return (text.endsWith("\n") ? text.slice(0, -1) : text).split("\n");
  }

  // How a command ended: told by OMP, said at the end of what Pi returns.
  function exitOf(result: any, output: string): number | null {
    if (!result) return null;
    const code = result.details?.exitCode;
    if (typeof code === "number") return fit(code);
    const said = /(?:exited with code|\[exit code:?) (-?\d+)/.exec(output.slice(-400));
    if (said) return fit(Number(said[1]));
    return result.isError ? null : 0;
  }

  // An exit code as ThinkTerm holds it, a 32-bit one: Windows' 3221225477
  // is -1073741819, as Windows' own tools show it; none past that.
  function fit(code: any): number | null {
    return typeof code === "number" && Number.isSafeInteger(code) && Math.abs(code) < 2 ** 32 ? code | 0 : null;
  }

  function brief(args: any): string {
    try {
      return JSON.stringify(args).slice(0, 200);
    } catch {
      return "";
    }
  }

  // A picture to send from a file the phone put here: none for one too
  // large, or not a picture Pi can send.
  function pictureOf(file: string): object | undefined {
    const types: Record<string, string> = { png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg", gif: "image/gif", webp: "image/webp" };
    const mimeType = types[path.extname(file).slice(1).toLowerCase()];
    if (!mimeType) return undefined;
    try {
      if (fs.statSync(file).size > PICTURE_KEPT) return undefined;
      const data = fs.readFileSync(file).toString("base64");
      sentFrom.set(hashOf(data), file);
      return { type: "image", data, mimeType };
    } catch {
      return undefined;
    }
  }

  function hashOf(data: string): string {
    return crypto.createHash("sha256").update(data).digest("hex").slice(0, 32);
  }

  // Copies of pictures no session read for a month go: they are copies.
  function forgetOldPictures() {
    const folder = path.join(dir, "pictures");
    try {
      const now = Date.now();
      for (const name of fs.readdirSync(folder)) {
        const file = path.join(folder, name);
        try {
          if (now - fs.statSync(file).mtimeMs > PICTURES_KEPT_MS) fs.unlinkSync(file);
        } catch {}
      }
    } catch {}
  }

  // The pictures in what the person said, each left as a file once (Pi
  // keeps them in the session itself): ThinkTerm makes small ones of them
  // for the phone.
  function picturesIn(content: any, id: string): object[] {
    if (!Array.isArray(content)) return [];
    const found: object[] = [];
    content.forEach((part: any, n: number) => {
      if (part?.type !== "image" || typeof part.data !== "string") return;
      const ext = part.mimeType === "image/jpeg" ? "jpg" : String(part.mimeType ?? "image/png").split("/")[1] || "png";
      const key = `${id}#${n}`;
      let file = pictureFiles.get(key);
      if (!file) {
        const hash = hashOf(part.data);
        // One the phone sent is where the phone put it; any other, a copy
        // of the session's, made once and kept while it is read.
        file = sentFrom.get(hash);
        if (!file || !fs.existsSync(file)) {
          file = path.join(dir, "pictures", hash + "." + ext);
          try {
            if (fs.existsSync(file)) {
              const now = new Date();
              fs.utimesSync(file, now, now);
            } else {
              fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
              fs.writeFileSync(file, Buffer.from(part.data, "base64"), { mode: 0o600 });
            }
          } catch {
            found.push({ name: "image", image: true });
            return;
          }
        }
        pictureFiles.set(key, file);
      }
      found.push({ name: `image.${ext}`, image: true, path: file });
    });
    return found;
  }

  function wordsOf(content: any): string {
    if (typeof content === "string") return content;
    if (!Array.isArray(content)) return "";
    return content
      .filter((part: any) => part?.type === "text" && part.text)
      .map((part: any) => part.text)
      .join("\n");
  }

  function textOf(content: any): string {
    if (typeof content === "string") return content;
    if (!Array.isArray(content)) return "";
    return content
      .map((part: any) => (part?.type === "text" ? part.text : part?.type === "image" ? "🖼" : ""))
      .filter(Boolean)
      .join("\n");
  }

  function tail(text: string, max: number): string {
    if (text.length <= max) return text;
    const cut = text.slice(text.length - max);
    const at = cut.indexOf("\n");
    return at >= 0 && at + 1 < cut.length ? cut.slice(at + 1) : cut;
  }

  // What OMP asks approval for, as the chat shows it: answered in the
  // terminal only, since an extension cannot answer it.
  function askOf(event: any): object {
    const name = String(event?.toolName ?? "");
    const args = toolArgs.get(event?.toolCallId) ?? {};
    const kind = name === "bash" ? "command" : name === "edit" || name === "write" || name === "ast_edit" ? "edit" : "tool";
    const command = String(args.command ?? "");
    const body =
      kind === "command"
        ? command.length > ASK_KEPT
          ? command.slice(0, ASK_KEPT) + "\n…"
          : command
        : kind === "edit"
          ? (args.path ? shown(args.path) : String(args.input ?? "").slice(0, 2000))
          : `${name} ${brief(args)}`;
    return {
      id: String(event?.toolCallId ?? ""),
      kind,
      body,
      reason: event?.reason ?? null,
      cwd: ctx?.cwd ?? process.cwd(),
      choices: [],
    };
  }

  function withdraw() {
    for (const id of asking.keys()) send({ ev: "resolved", id });
    asking.clear();
  }

  // `/new` and `/reload` sent from the chat, run as typed in the terminal.
  // An agent that takes commands otherwise (a fork) goes on without them.
  try {
    if (typeof pi.registerCommand !== "function") throw new Error("no commands");
    pi.registerCommand(RUN, {
      description: "Runs /new or /reload for ThinkTerm's chat",
      handler: async (args: string, command: any) => {
        const what = args.trim();
        if (what === "new" && typeof command?.newSession === "function") await command.newSession();
        else if (what === "reload" && typeof command?.reload === "function") await command.reload();
      },
    });
    registered = true;
  } catch {}

  pi.on("session_start", async (_event: any, context: any) => {
    ctx = context;
    // Only the Pi a person works in, in the terminal: not one it runs for a
    // task of its own (`--mode json`, print, rpc), which inherits the pane.
    if (context?.mode !== "tui") return;
    // A session switched to (/new, /resume, /reload) listens again here; the
    // bridge comes back and is sent the session as it is now.
    start();
  });
  pi.on("session_shutdown", async () => stop());
  // OMP changes sessions under the same extension (/new, /resume, /clear):
  // the chat is sent the session it changed to.
  const changed = async (_event: any, context: any) => {
    ctx = context;
    withdraw();
    for (const client of clients) greet(client);
  };
  pi.on("session_switch", changed);
  pi.on("session_clear", changed);
  // Pi's /tree moves to another branch of the same session.
  pi.on("session_tree", changed);
  // Earlier turns summed up to make room: said where it happened.
  pi.on("session_compact", async (event: any, context: any) => {
    ctx = context;
    const id = event?.compactionEntry?.id;
    send({ ev: "item", item: compacted(id ?? `c${Date.now()}`) });
    contextSoon();
  });
  // A model chosen in the terminal, or here: the phone's button follows.
  pi.on("model_select", async (event: any, context: any) => {
    ctx = context;
    if (event?.model) model = event.model;
    sendModels();
    // Another model, another window.
    contextSoon();
  });
  pi.on("thinking_level_select", async (_event: any, context: any) => {
    ctx = context;
    sendModels();
  });
  pi.on("session_info_changed", async (event: any) => {
    send({ ev: "hello", protocol: PROTOCOL, agent, title: event?.name ?? null, guessed: false });
  });
  pi.on("agent_start", async (_event: any, context: any) => {
    ctx = context;
    setState("working");
  });
  pi.on("agent_settled", async (_event: any, context: any) => {
    ctx = context;
    setState("idle");
  });
  // OMP has no `agent_settled`: the run is over once it is idle after its
  // end (Pi settles later still, on retries and follow-ups, and says so).
  pi.on("agent_end", async (_event: any, context: any) => {
    ctx = context;
    withdraw();
    // Only where a phone may be told (a Pi in its terminal): a print or
    // json run is over, its context gone, by the time a look would come.
    if (!server) return;
    clearTimeout(settling);
    settling = setTimeout(() => {
      settling = undefined;
      try {
        if (ctx?.isIdle?.()) setState("idle");
      } catch {}
    }, 300);
  });
  // OMP asks before some tools run (not by default): the chat shows what,
  // and the person answers in the terminal.
  pi.on("tool_approval_requested", async (event: any, context: any) => {
    ctx = context;
    const ask: any = askOf(event);
    asking.set(ask.id, ask);
    send({ ev: "ask", ask });
    setState("blocked");
  });
  pi.on("tool_approval_resolved", async (event: any, context: any) => {
    ctx = context;
    const id = String(event?.toolCallId ?? "");
    asking.delete(id);
    send({ ev: "resolved", id });
    // Others asked at the same time may wait still.
    setState(asking.size ? "blocked" : "working");
  });
  // What Pi writes, sent as it grows: its words and its thinking, a few
  // times a second. Its tool calls come as they run.
  pi.on("message_update", async (event: any, context: any) => {
    ctx = context;
    const message = event?.message;
    if (!server || !clients.size || message?.role !== "assistant" || message.timestamp === undefined) return;
    writing = { id: `m${message.timestamp}`, message };
    if (writingTimer) return;
    writingTimer = setTimeout(() => {
      writingTimer = undefined;
      const now = writing;
      if (!now) return;
      try {
        for (const item of fromMessage(now.message, now.id) as any[]) {
          if (item.kind === "text" || item.kind === "thinking") send({ ev: "item", item });
        }
      } catch {}
    }, WRITING_EVERY);
  });
  pi.on("message_end", async (event: any, context: any) => {
    ctx = context;
    // Written whole: what came as it grew is done with.
    clearTimeout(writingTimer);
    writingTimer = undefined;
    writing = undefined;
    const message = event?.message;
    contextSoon();
    if (!message || message.role === "toolResult") return;
    const id = `m${message.timestamp ?? Date.now()}`;
    for (const item of fromMessage(message, id)) send({ ev: "item", item });
  });
  pi.on("tool_execution_start", async (event: any, context: any) => {
    ctx = context;
    toolArgs.set(event.toolCallId, event.args);
    send({ ev: "item", item: toolItem(event.toolCallId, event.toolName, event.args, undefined, true) });
  });
  pi.on("tool_execution_end", async (event: any, context: any) => {
    ctx = context;
    const args = event.args ?? toolArgs.get(event.toolCallId);
    toolArgs.delete(event.toolCallId);
    const result = { isError: !!event.isError, content: event.result?.content ?? event.result, details: event.result?.details };
    send({ ev: "item", item: toolItem(event.toolCallId, event.toolName, args, result) });
  });
}
