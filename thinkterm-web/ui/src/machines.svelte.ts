// Other machines, reached through this page's server (web_relay.rs there):
// the ones that server knows, the ones this page has open, and the one on
// show. Every open machine is a client of its own -- its own canvas, its
// own keyboard field, its own connection -- and the chrome draws whichever
// is active: `handle.client` is always that one. This server is `HERE`,
// the client the page booted with.
//
// The relay asks its questions (a password, a host key, whether to
// install) before the mux connection starts; this module answers them from
// the Remote Hosts page, for the first connect and for every reconnect.
// That page is a tab of its own, as the desktop's is: after the terminal's
// in the tab row, in the terminal's place while it is on show.

import { flushSync } from 'svelte';
import { clients, handle, type Client } from './client';
import { activate, attach, refreshViews, s } from './client.svelte';
import { focusTerminal, openSide } from './mobile.svelte';
import type { MenuItem } from './model';

export const HERE = 'here';
const RELAY = 'thinkterm.relay.v1';
/** The machines this browser had open, reopened on the next load. */
const OPEN_STORE = 'thinkterm.machines';
/** Lines of a machine's progress kept for the page. */
const LOG_LINES = 40;
/** What a reconnect may meet and still try again: the network, or the
    host being in the middle of something. Anything else -- a refused key,
    a declined install -- would only be asked again and again. */
const RETRYABLE = new Set(['unreachable', 'failed']);

export type MachineSource = 'ssh-config' | 'saved' | 'web';
export type MachineEntry = {
  id: string;
  label: string;
  endpoint: string;
  source: MachineSource;
  /** A password is kept for it on the server. */
  password: boolean;
  /** ThinkTerm may be installed there without asking. */
  install: boolean;
  /** Forgetting it changes something here. */
  forgettable: boolean;
};
export type Step = 'connecting' | 'authenticating' | 'checking' | 'installing' | 'updating' | 'starting';
export type AskKind = 'host-key' | 'password' | 'secret' | 'text' | 'install' | 'replace' | 'stop-server';
export type Ask = { id: number; kind: AskKind; prompt: string; detail: string; remember: boolean };
export type Failure = { reason: string; message: string; ssh_domain: string | null };

export type OpenMachine = {
  id: string;
  label: string;
  /** Names its canvas and keyboard field while it is not on show. */
  n: number;
  state: 'connecting' | 'ready' | 'reconnecting' | 'failed';
  step: Step | null;
  percent: number | null;
  /** The relay's question on show, until it is answered. */
  ask: Ask | null;
  log: string[];
  failure: Failure | null;
  /** Shown once it is ready: the person asked for it, not a reload. */
  showWhenReady: boolean;
  /** Reopened on the next load: it came from the last one, or it has been
      up. A machine that only ever failed is not tried again on every load. */
  remembered: boolean;
};

export const machines = $state({
  /** This server's own name. */
  here: '',
  list: [] as MachineEntry[],
  listed: false,
  /** Reaching other machines is turned on at this server; off, the list
      is empty and the page says how to turn it on. */
  enabled: true,
  /** Why the list or the last change to it failed; empty when it did not. */
  error: '',
  open: [] as OpenMachine[],
  /** `HERE`, or the id of the open machine on show. */
  active: HERE,
  /** The Remote Hosts tab is in the tab row. */
  tab: false,
  /** It is the tab on show: the page is in the terminal's place. */
  panel: false,
  /** The page's host form is open. */
  adding: false,
});

/** What starting another client takes; set by the boot (`configure`). */
export type Boot = {
  start: (canvas: string, textarea: string, opener: () => Promise<WebSocket>) => Promise<Client>;
  token: string;
  /** Brings a new client to this browser's preferences. */
  prepare: (client: Client) => void;
};
let boot: Boot | null = null;
let counter = 0;
/** The sockets still in their questions, by machine. */
const asking = new Map<string, { socket: WebSocket; cancel: () => void }>();

function relaySocket(): WebSocket {
  const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
  return new WebSocket(`${scheme}://${location.host}/relay`, [RELAY, `tt-token.${boot?.token ?? ''}`]);
}

/** The page booted with this server's client: open the machines it had
    open last time, in the background. */
export function configure(next: Boot, hub: Client) {
  boot = next;
  clients.set(HERE, hub);
  const reopen = storedOpen();
  if (reopen.length === 0) return;
  // The list first: it names them. Without one (the relay down for a
  // moment) nothing is opened, and nothing is forgotten either.
  void refreshList().then(() => {
    if (!machines.listed || machines.error !== '') return;
    for (const id of reopen) if (machines.list.some((m) => m.id === id)) void connect(id, false, true);
    storeOpen();
  });
}

function storedOpen(): string[] {
  try {
    const value: unknown = JSON.parse(localStorage.getItem(OPEN_STORE) ?? '[]');
    return Array.isArray(value) ? value.filter((v): v is string => typeof v === 'string') : [];
  } catch {
    return [];
  }
}

function storeOpen() {
  try {
    localStorage.setItem(OPEN_STORE, JSON.stringify(machines.open.filter((m) => m.remembered).map((m) => m.id)));
  } catch {
    // A browser that blocks storage reopens nothing next time.
  }
}

/** One request to the relay that ends with the list: it opens a socket,
    asks, and closes. Resolves with the id a save came to, if it was one. */
function request(message: object): Promise<string | null> {
  return new Promise((resolve) => {
    const ws = relaySocket();
    let done = false;
    let saved: string | null = null;
    const finish = (error: string) => {
      if (done) return;
      done = true;
      machines.error = error;
      ws.close();
      resolve(error === '' ? saved : null);
    };
    ws.onopen = () => ws.send(JSON.stringify(message));
    ws.onmessage = (ev) => {
      if (typeof ev.data !== 'string') return;
      const reply = JSON.parse(ev.data);
      if (reply.op === 'machines') {
        machines.here = reply.here;
        machines.enabled = reply.enabled !== false;
        machines.list = reply.machines;
        machines.listed = true;
        for (const m of machines.open) m.label = machines.list.find((e) => e.id === m.id)?.label ?? m.label;
        finish('');
      } else if (reply.op === 'saved') {
        saved = reply.id;
      } else if (reply.op === 'error') {
        finish(reply.message);
      }
    };
    ws.onerror = () => finish(s('web-machines-failed-failed'));
    ws.onclose = () => finish(done ? '' : s('web-machines-failed-failed'));
  });
}

export async function refreshList(): Promise<void> {
  await request({ op: 'list' });
}

/** The cards this server dresses its tabs with (`tab-icons`), as its
    desktop settings make them; null when the relay could not say. */
export function fetchTabIcons(): Promise<unknown> {
  return new Promise((resolve) => {
    const ws = relaySocket();
    const finish = (catalog: unknown) => {
      ws.onmessage = null;
      ws.onerror = null;
      ws.onclose = null;
      ws.close();
      resolve(catalog);
    };
    ws.onopen = () => ws.send(JSON.stringify({ op: 'tab-icons' }));
    ws.onmessage = (ev) => {
      if (typeof ev.data !== 'string') return;
      const reply = JSON.parse(ev.data);
      finish(reply.op === 'tab-icons' ? reply : null);
    };
    ws.onerror = () => finish(null);
    ws.onclose = () => finish(null);
  });
}

export type NewMachine = { label: string; host: string; port: string; user: string; password: string };

/** Add a machine to the server's list and connect to it. */
export async function addMachine(form: NewMachine): Promise<boolean> {
  const port = form.port.trim() === '' ? null : Number(form.port.trim());
  // Never quietly 22: that would be another machine, or the same one's
  // kept password answering for it.
  if (port !== null && !(Number.isInteger(port) && port > 0 && port < 65536)) {
    machines.error = s('web-machines-bad-port');
    return false;
  }
  // The server keys a machine by its address: one typed in again is the
  // one already listed, and that is the one opened.
  const id = await request({
    op: 'save',
    machine: {
      label: form.label.trim() || null,
      host: form.host.trim(),
      port: port !== null && Number.isInteger(port) && port > 0 && port < 65536 ? port : null,
      user: form.user.trim() || null,
      password: form.password === '' ? null : form.password,
    },
  });
  if (id === null) return false;
  void connect(id, true);
  return true;
}

export async function forgetMachine(id: string): Promise<void> {
  await request({ op: 'forget', id });
}

/** The page in the terminal's place, for what hands the keys back to the
    terminal (focusTerminal, the wasm's own): `body[data-page]` names it,
    and the keys stay with it. Set at once, not when the page is next
    drawn: the terminal is often focused in the same task. */
function mark() {
  const body = globalThis.document?.body;
  if (!body) return;
  if (machines.panel) body.dataset.page = 'machines';
  else delete body.dataset.page;
}

function present() {
  machines.tab = true;
  machines.panel = true;
  mark();
  // On a phone the drawer it was opened from (the sidebar's globe, the
  // Space menu's rows) would cover it: the drawer goes.
  openSide(false);
}

/** Open the Remote Hosts tab and put it on show. `form` opens it on a
    blank host form, as the desktop's Add Remote Host does. */
export function openPanel(form = false) {
  present();
  if (form) machines.adding = true;
  void refreshList();
}

/** The sidebar's globe, as the desktop's: the page on show is closed, and
    one behind the terminal comes back. */
export function togglePanel() {
  if (machines.panel) closePanel();
  else openPanel();
}

/** Put the page behind the terminal and keep its tab, as picking another
    tab -- or a terminal anywhere else -- does on the desktop. */
export function hidePanel() {
  if (!machines.panel) return;
  machines.panel = false;
  mark();
  focusTerminal();
}

/** Close the Remote Hosts tab; the terminal comes back. */
export function closePanel() {
  if (!machines.tab) return;
  const shown = machines.panel;
  machines.tab = false;
  machines.panel = false;
  machines.adding = false;
  mark();
  if (shown) focusTerminal();
}

function find(id: string): OpenMachine | undefined {
  return machines.open.find((m) => m.id === id);
}

/** The socket the relay hands over once a machine is reached, after its
    questions are answered here. Called for the first connect and again by
    the client for every reconnect. */
function opener(id: string, n: number): () => Promise<WebSocket> {
  return () =>
    new Promise<WebSocket>((resolve, reject) => {
      const m = find(id);
      if (!m || m.n !== n || m.state === 'failed') {
        reject(new Error('closed'));
        return;
      }
      const ws = relaySocket();
      ws.binaryType = 'arraybuffer';
      let settled = false;
      const settle = () => {
        settled = true;
        if (asking.get(id)?.socket === ws) asking.delete(id);
        ws.onmessage = null;
        ws.onclose = null;
        ws.onerror = null;
        ws.onopen = null;
      };
      const cancel = () => {
        if (settled) return;
        settle();
        ws.close();
        reject(new Error('closed'));
      };
      asking.set(id, { socket: ws, cancel });
      ws.onopen = () => {
        if (find(id)?.n !== n) return cancel();
        ws.send(JSON.stringify({ op: 'open', id }));
      };
      ws.onmessage = (ev) => {
        if (settled) return;
        if (typeof ev.data !== 'string') return;
        const reply = JSON.parse(ev.data);
        const m = find(id);
        if (!m || m.n !== n) return cancel();
        switch (reply.op) {
          case 'step':
            m.step = reply.step;
            m.percent = reply.percent ?? null;
            break;
          case 'log':
            m.log = [...m.log, reply.text].slice(-LOG_LINES);
            break;
          case 'ask':
            m.ask = reply as Ask;
            // The page comes on show for a machine the person just asked
            // to open. One reconnecting behind the scenes waits to be
            // noticed (the Space row says so): taking the terminal's place
            // would take the keys being typed there.
            if (m.showWhenReady) present();
            break;
          case 'ready':
            m.ask = null;
            m.step = null;
            m.percent = null;
            m.failure = null;
            settle();
            resolve(ws);
            break;
          case 'failed':
            m.ask = null;
            m.percent = null;
            m.failure = reply as Failure;
            settle();
            ws.close();
            reject(new Error(reply.message));
            if (!RETRYABLE.has(reply.reason)) {
              // Not worth opening on the next load either.
              m.remembered = false;
              storeOpen();
              if (clients.has(id)) giveUp(id);
            }
            break;
          case 'error':
            m.log = [...m.log, reply.message].slice(-LOG_LINES);
            break;
        }
      };
      // A question left on show after its socket went is one nobody can
      // answer any more.
      const lost = () => {
        if (settled) return;
        settle();
        const m = find(id);
        if (m?.n === n) {
          m.ask = null;
          m.percent = null;
        }
        reject(new Error(s('web-machines-failed-failed')));
      };
      ws.onerror = lost;
      ws.onclose = lost;
    });
}

/** A machine whose reconnect cannot succeed by trying again: it stays in
    the list, failed, where Try again starts it afresh. */
function giveUp(id: string) {
  const m = find(id);
  const client = clients.get(id);
  if (!m || !client) return;
  m.state = 'failed';
  if (machines.active === id) showMachine(HERE);
  clients.delete(id);
  // Once the reconnect that asked has its answer.
  setTimeout(() => {
    client.close();
    client.free();
  }, 0);
}

/** Answer the question on show: a value, or null to decline it. */
export function answer(id: string, value: string | null, remember = false) {
  const m = find(id);
  const ws = asking.get(id)?.socket;
  if (!m || !m.ask || !ws) return;
  ws.send(JSON.stringify({ op: 'answer', id: m.ask.id, value, remember }));
  m.ask = null;
}

/** Open a machine: its own client, on a canvas of its own. `show` brings it
    on show once it is ready. */
export async function connect(id: string, show: boolean, remembered = false): Promise<void> {
  if (!boot) return;
  const existing = find(id);
  if (existing) {
    if (existing.state === 'failed') {
      // Again, from the start.
      close(id);
    } else {
      if (show) {
        // One with a terminal already -- up, or reconnecting behind the
        // scenes -- comes on show now; one on its first connect, once it
        // is ready. (Only a first connect ever reads the flag again: set
        // for a reconnecting one, it stayed set, and brought the page up
        // for some later reconnect's question.)
        if (clients.has(id)) showMachine(id);
        else existing.showWhenReady = true;
      }
      return;
    }
  }
  const known = machines.list.find((m) => m.id === id);
  const n = ++counter;
  machines.open.push({
    id,
    label: known?.label ?? id,
    n,
    state: 'connecting',
    step: 'connecting',
    percent: null,
    ask: null,
    log: [],
    failure: null,
    showWhenReady: show,
    remembered,
  });
  storeOpen();
  // The canvas and the field have to be in the page before the client
  // looks them up.
  flushSync();
  let client: Client;
  try {
    client = await boot.start(`term-${n}`, `kbd-${n}`, opener(id, n));
  } catch (e) {
    const m = find(id);
    if (m?.n === n) {
      m.state = 'failed';
      m.step = null;
      m.failure ??= { reason: 'failed', message: String((e as { message?: string })?.message ?? e), ssh_domain: null };
    }
    return;
  }
  const m = find(id);
  if (!m || m.n !== n) {
    // Closed while it was opening.
    client.close();
    client.free();
    return;
  }
  clients.set(id, client);
  client.set_shown(false);
  boot.prepare(client);
  attach(client, () => onChange(id, n));
  m.state = 'ready';
  m.step = null;
  m.remembered = true;
  storeOpen();
  // Brought on show as asked -- unless the person is in the middle of
  // answering something else, whose keys would land in this terminal.
  if (m.showWhenReady && !typingElsewhere() && !machines.open.some((o) => o.ask)) {
    showMachine(id);
    closePanel();
  }
  m.showWhenReady = false;
}

/** The person is typing an answer or a path, not at a terminal. (No
    document at all where the module runs on its own, in its tests.) */
function typingElsewhere(): boolean {
  const active = globalThis.document?.activeElement;
  return !!active?.closest('#machines input, #machines textarea, #machines select, #side .path');
}

/** A client's views moved, which is also how a dropped connection, and
    the reconnect that ends it, reach the page. */
function onChange(id: string, n: number) {
  const m = find(id);
  const client = clients.get(id);
  if (!m || m.n !== n || !client) return;
  const up = client.connected();
  if (!up && m.state === 'ready') m.state = 'reconnecting';
  else if (up && m.state === 'reconnecting') m.state = 'ready';
}

/** Put a machine's terminal on show, and the chrome with it; the keys go
    to it too unless `focus` says otherwise. */
export function showMachine(key: string, focus = true) {
  const next = clients.get(key);
  if (!next || machines.active === key) {
    if (next && focus) focusTerminal();
    return;
  }
  const previous = handle.client;
  previous?.set_shown(false);
  machines.active = key;
  // The keyed panels capture this client when they come up again.
  activate(next);
  // The ids move with the machine on show: `#term` and `#kbd` are its.
  flushSync();
  next.set_shown(true);
  if (focus) focusTerminal();
}

/** Close a machine's connection and let its client go. */
export function close(id: string) {
  const m = find(id);
  if (!m) return;
  asking.get(id)?.cancel();
  asking.delete(id);
  if (machines.active === id) showMachine(HERE);
  const client = clients.get(id);
  clients.delete(id);
  machines.open = machines.open.filter((o) => o.id !== id);
  storeOpen();
  if (client) {
    client.close();
    client.free();
  }
}

/** A plain ssh terminal on this server, for a machine ThinkTerm could not
    be set up on: a new tab here, in that machine's ssh domain. */
export function openPlainSsh(id: string) {
  const m = find(id);
  const domain = m?.failure?.ssh_domain;
  const hub = clients.get(HERE);
  if (!m || !domain || !hub) return;
  close(id);
  showMachine(HERE);
  hub.new_tab_in(domain);
  closePanel();
}

/** Where a machine stands, in a few words; empty when it is simply up. */
export function stateText(m: OpenMachine): string {
  if (m.ask) return s('web-machines-needs-you');
  switch (m.state) {
    case 'ready':
      return '';
    case 'reconnecting':
      return s('web-machines-reconnecting');
    case 'failed':
      return m.failure ? s(`web-machines-failed-${m.failure.reason}`) : s('web-machines-not-connected');
    case 'connecting':
      return m.step ? s(`web-machines-step-${m.step}`) : s('web-machines-step-connecting');
  }
}

/** Add a workspace on another machine: its tree comes on show with the
    field open there, and the one open here is put away. */
export function addWorkspaceOn(key: string) {
  const next = clients.get(key);
  if (!next || machines.active === key) return;
  handle.client?.side_key('Escape', '');
  // The path is typed next: the keys stay in the field, not the terminal.
  showMachine(key, false);
  next.side_click('new-project');
  refreshViews();
  document.querySelector<HTMLInputElement>('#side .path input')?.focus();
}

/** The machines a workspace can be added on: this server and every open
    one that is up. */
export function workspaceMachines(): { key: string; label: string }[] {
  return [
    { key: HERE, label: s('web-machines-here') },
    ...machines.open.filter((m) => m.state === 'ready').map((m) => ({ key: m.id, label: m.label })),
  ];
}

/** What the Space button calls the machine on show; empty for this one. */
export function activeLabel(): string {
  if (machines.active === HERE) return '';
  return find(machines.active)?.label ?? '';
}

// ---- the Space menu ---------------------------------------------------------

/** The page's own rows in the Space menu, by their place in it. */
let pageActions: (() => void)[] = [];

type SpaceEntry = { id: string; name: string; current: boolean; default: boolean };

/** A machine's Spaces; one that has none yet has the one its sidebar
    shows, which is where switching to it lands. */
function spacesOf(client: Client): SpaceEntry[] {
  try {
    const spaces = JSON.parse(client.spaces()) as SpaceEntry[];
    if (spaces.length > 0) return spaces;
    const row = (JSON.parse(client.sidebar()) as { rows: { kind: string; name?: string }[] }).rows.find((r) => r.kind === 'space');
    return row ? [{ id: '', name: row.name ?? '', current: true, default: true }] : [];
  } catch {
    return [];
  }
}

function pageItem(label: string, icon: string | null, run: () => void, checked = false, enabled = true): MenuItem {
  pageActions.push(run);
  return { id: `page:${pageActions.length - 1}`, label, icon, kind: 'item', enabled, checked, submenu: [] };
}

function header(label: string): MenuItem {
  return { id: '', label, icon: null, kind: 'header', enabled: false, checked: false, submenu: [] };
}

function separator(): MenuItem {
  return { id: '', label: '', icon: null, kind: 'separator', enabled: false, checked: false, submenu: [] };
}

/** Switch to `space` on machine `key`: the machine comes on show with it,
    and its terminal in front of the Remote Hosts page. An empty `space` is
    the machine's only one. */
function switchSpace(key: string, space: string) {
  const client = clients.get(key);
  if (!client) return;
  // On a phone the drawer the menu was opened from would cover it.
  openSide(false);
  hidePanel();
  showMachine(key);
  if (space !== '') client.set_space(space);
  refreshViews();
}

/** The Space menu the way the desktop lays it out: this server's Spaces,
    then each other machine under its name with its own, then Add Remote
    Host, then what the active machine's menu offers for its current Space
    (New, Rename, Delete). `own` is the active client's menu. */
export function spaceMenu(own: MenuItem[]): MenuItem[] {
  pageActions = [];
  const items: MenuItem[] = [];
  const spaceRow = (key: string, space: SpaceEntry) =>
    pageItem(space.name, space.default ? 'house' : 'layers', () => switchSpace(key, space.id), machines.active === key && space.current);
  const hub = clients.get(HERE);
  // With other machines in the list, this server's Spaces are headed too.
  if (machines.open.length > 0) items.push(header(machines.here ? `${s('web-machines-here')} · ${machines.here}` : s('web-machines-here')));
  if (hub) for (const space of spacesOf(hub)) items.push(spaceRow(HERE, space));
  for (const m of machines.open) {
    items.push(separator());
    const state = stateText(m);
    items.push(header(state === '' ? m.label : `${m.label} · ${state}`));
    const client = clients.get(m.id);
    if (client && m.state !== 'failed') {
      for (const space of spacesOf(client)) items.push(spaceRow(m.id, space));
      items.push(
        pageItem(s('menu-new-space-here'), 'plus', () => {
          // Looked up now: the machine may have been closed meanwhile.
          const now = clients.get(m.id);
          if (!now) return;
          openSide(false);
          hidePanel();
          showMachine(m.id);
          now.menu_action('new-space');
          refreshViews();
        }),
      );
    } else {
      items.push(pageItem(s('web-machines-show'), 'server', () => openPanel()));
    }
  }
  items.push(separator());
  items.push(pageItem(s('menu-add-remote-host'), 'network', () => openPanel(true)));
  // The active machine's own actions on its current Space.
  const cut = own.findIndex((item) => item.kind === 'separator');
  if (cut >= 0) items.push(...own.slice(cut));
  return items;
}

/** Run one of the page's own rows; false for any other id. */
export function runPageItem(id: string): boolean {
  if (!id.startsWith('page:')) return false;
  pageActions[Number(id.slice(5))]?.();
  return true;
}

// For probes and the smoke tests, beside `window.thinkterm.client`.
(window as unknown as { thinkterm: Record<string, unknown> }).thinkterm.machines = machines;
