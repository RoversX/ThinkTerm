// What the chrome draws: the wasm's views, re-read whenever it says
// something changed. The wasm owns the state; this is a copy of what it
// shows, kept in runes so the components follow it.

import { flushSync } from 'svelte';
import { handle, type Client } from './client';
import type { AgentsView, NavsView, SidebarView, StatusView, Strings, TabsView, WebSettings } from './model';

export const views = $state({
  tabs: null as TabsView | null,
  navs: [] as NavsView,
  status: { toast: null, card: null, summary: '' } as StatusView,
  sidebar: {
    rows: [], editing: { kind: 'none' }, space: null, new_project_error: null,
    reveal: { edge: 6, dwell_ms: 150, retreat_ms: 250 }, footer: [], footer_label_min_width: 168,
  } as SidebarView,
  /** The right-hand panel's rows, whether it is on show or not. */
  agents: { rows: [], summary: '', tabs: [], active: 'agents' } as AgentsView,
  /** The page's own preferences, as the wasm holds them (settings.rs). */
  settings: {
    language: 'system',
    theme: 'dark',
    font: { mode: 'follow' },
    'hover-reveal': true,
    'agents-panel': false,
    'palette-hotkey': 'cmd-k',
    'sidebar-width': 220,
  } as WebSettings,
  layout: '',
  /** The page's own labels in the active language; empty until attached. */
  strings: {} as Strings,
  /** What the page says before the client exists: the boot, or its failure. */
  boot: 'loading…',
  bootFailed: false,
  /** The client is attached; the views below are its, not the boot's. */
  ready: false,
});

/** A label by catalogue id; the id itself until the client is attached. */
export function s(id: string): string {
  return views.strings[id] ?? id;
}

/** The boot's progress, and the probes' one-line results. */
export function setBoot(text: string, failed = false) {
  views.boot = text;
  views.bootFailed = failed;
}

/** Re-read every view now. Set by `attach`; a no-op before it. */
let refresh: () => void = () => {};

/** Follow a client's views. Called once, right after `start()` resolves. */
export function attach(client: Client) {
  // The views are re-listed every frame something moved and are mostly the
  // same string as last time; parsing only what changed keeps the whole
  // chrome from being diffed for a cursor blink.
  let lastTabs = '';
  let lastNavs = '';
  let lastStatus = '';
  let lastStrings = '';
  let lastSidebar = '';
  let lastAgents = '';
  let lastSettings = '';
  const read = () => {
    const strings = client.strings();
    if (strings !== lastStrings) {
      lastStrings = strings;
      views.strings = JSON.parse(strings) as Strings;
    }
    const tabs = client.tabs();
    if (tabs !== lastTabs) {
      lastTabs = tabs;
      views.tabs = JSON.parse(tabs) as TabsView;
    }
    const navs = client.navs();
    if (navs !== lastNavs) {
      lastNavs = navs;
      views.navs = JSON.parse(navs) as NavsView;
    }
    const status = client.status();
    if (status !== lastStatus) {
      lastStatus = status;
      views.status = JSON.parse(status) as StatusView;
    }
    const sidebar = client.sidebar();
    if (sidebar !== lastSidebar) {
      lastSidebar = sidebar;
      views.sidebar = JSON.parse(sidebar) as SidebarView;
    }
    const agents = client.agents();
    if (agents !== lastAgents) {
      lastAgents = agents;
      views.agents = JSON.parse(agents) as AgentsView;
    }
    const settings = client.settings();
    if (settings !== lastSettings) {
      lastSettings = settings;
      views.settings = JSON.parse(settings) as WebSettings;
    }
    views.layout = client.layout();
  };
  refresh = read;
  views.ready = true;
  // Registering calls it once, so the first frame is the client's.
  client.on_change(read);
}

/** Re-read every view and put it on show now, for a caller that went to
    the wasm itself and wants what it changed drawn before it returns. */
export function refreshViews() {
  refresh();
  flushSync();
}

/** Switch the interface language; returns the locale it resolved to. */
export function setLocale(preference: string): string {
  if (!handle.client) return '';
  const code = handle.client.set_locale(preference, Array.from(navigator.languages ?? [navigator.language]));
  refresh();
  flushSync();
  return code;
}

/** A click on the tab row or a pane's bar, delivered to the wasm. */
export function chromeClick(action: string, pane: number | null = null, tab: number | null = null): boolean {
  if (!handle.client) return false;
  const handled = handle.client.chrome_click(action, pane, tab);
  if (handled) {
    // What the click changed is on show before the handler returns, as it
    // was when the wasm wrote the markup itself: the client's own notice
    // is a frame away, and a two-press close is read back straight after
    // the press that asked for it.
    refresh();
    flushSync();
  }
  return handled;
}

/** A click in the sidebar, delivered to the wasm. */
export function sideClick(kind: string, id: string | null = null, flag: boolean | null = null): boolean {
  if (!handle.client) return false;
  const handled = handle.client.side_click(kind, id, flag);
  // As for the chrome: what the click changed is on show before the handler
  // returns, since the client's own notice is a frame away and a two-press
  // delete is read back straight after the press that asked for it.
  if (handled) {
    refresh();
    flushSync();
  }
  return handled;
}

/** Enter or Escape in the sidebar's inline input, with what was typed. */
export function sideKey(key: 'Enter' | 'Escape', value: string) {
  if (!handle.client) return;
  handle.client.side_key(key, value);
  refresh();
  flushSync();
}
