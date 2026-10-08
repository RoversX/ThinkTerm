// The one search palette the page has open: what the wasm found for what
// was typed (thinkterm-web/src/palette.rs), which entry the keyboard is
// on, and what running a pick came to. The wasm owns what can be found,
// how a query ranks it and what a pick does; this owns only the fact that
// the palette is open and the picks this browser remembers.

import { handle } from './client';
import { refreshViews, views } from './client.svelte';
import { toggleSidebar } from './chrome';
import { hidePanel, openPanel, togglePanel } from './machines.svelte';
import type { Hotkey, PaletteEntry, PaletteOutcome, PaletteResults } from './model';
import { openSettings, setSetting } from './settings.svelte';
import { focusTerminal, openSide } from './mobile.svelte';

/** Where this browser keeps the picks, most recent first. */
const RECENT = 'thinkterm.recent';

const EMPTY: PaletteResults = { sections: [], placeholder: '', empty: '' };

export const palette = $state({
  open: false,
  /** What has been typed, which is what the results are for. */
  query: '',
  results: EMPTY as PaletteResults,
  /** The row the keyboard is on, as an index into `entries()`. */
  selected: 0,
  /** One section only, when opened for it (the sidebar's thread search). */
  only: '' as '' | 'threads',
});

/** Every entry on show, in the order the arrows walk them. */
/** The sections on show: every one, or the one the palette was opened for. */
export function shownSections() {
  return palette.only === '' ? palette.results.sections : palette.results.sections.filter((s) => s.group === palette.only);
}

export function entries(): PaletteEntry[] {
  return shownSections().flatMap((section) => section.entries);
}

/** Ask the wasm what `query` finds. Synchronous and cheap: it ranks what
    the model already has. */
export function search(query: string) {
  palette.query = query;
  const client = handle.client;
  if (!client) {
    palette.results = EMPTY;
    palette.selected = 0;
    return;
  }
  try {
    palette.results = JSON.parse(client.palette(query)) as PaletteResults;
  } catch {
    palette.results = EMPTY;
  }
  palette.selected = 0;
}

export function openPalette(only: '' | 'threads' = '') {
  palette.only = only;
  palette.open = true;
  search('');
}

export function closePalette() {
  if (!palette.open) return;
  palette.open = false;
  palette.only = '';
  palette.query = '';
  palette.results = EMPTY;
  palette.selected = 0;
  // The palette took focus off the field the terminal types through.
  focusTerminal();
}

/** The keyboard's row, `delta` rows on, wrapping through every section. */
export function move(delta: number) {
  const count = entries().length;
  if (count === 0) {
    palette.selected = 0;
    return;
  }
  palette.selected = (((palette.selected + delta) % count) + count) % count;
}

/** Put the keyboard on an entry the pointer is over. */
export function select(id: string) {
  const at = entries().findIndex((entry) => entry.id === id);
  if (at >= 0) palette.selected = at;
}

/** The entry the keyboard is on; nothing when the query found none. */
export function selectedId(): string {
  return entries()[palette.selected]?.id ?? '';
}

/** Do what a pick asks, then close. What only the page can do comes back
    as `page`; everything else the wasm has already done. */
export function run(id: string) {
  const client = handle.client;
  if (!client || id === '') return;
  let outcome: PaletteOutcome;
  try {
    outcome = JSON.parse(client.palette_run(id)) as PaletteOutcome;
  } catch {
    closePalette();
    return;
  }
  closePalette();
  if (!outcome.handled) return;
  // Every pick but the page's own acts on a terminal -- a thread, a tab, a
  // pane, a Space, a command -- and that terminal comes in front of the
  // Remote Hosts page, as it does on the desktop, and of the drawer the
  // palette may have been opened from on a phone.
  if (outcome.page === null) {
    openSide(false);
    hidePanel();
  }
  // What the pick changed is on show before this returns, as it is for a
  // menu row: the client's own notice is a frame away.
  refreshViews();
  rememberPicks(outcome.recent);
  const page = outcome.page;
  if (page === 'toggle-sidebar') toggleSidebar();
  else if (page === 'settings') openSettings();
  else if (page === 'remote-hosts') togglePanel();
  else if (page === 'add-remote-host') openPanel(true);
  else if (page !== null && page.startsWith('lang:')) {
    // The wasm activated the language; the preference is the page's to
    // keep, and storing it is what makes the next load open in it.
    setSetting('language', page.slice('lang:'.length));
  }
}

function rememberPicks(ids: string[]) {
  try {
    localStorage.setItem(RECENT, JSON.stringify(ids));
  } catch {
    // A browser that blocks storage keeps them for this load only.
  }
}

/** The picks this browser remembers, for the wasm to rank an empty query by. */
export function storedPicks(): string[] {
  try {
    const raw = localStorage.getItem(RECENT);
    if (!raw) return [];
    const value: unknown = JSON.parse(raw);
    return Array.isArray(value) ? value.filter((id): id is string => typeof id === 'string') : [];
  } catch {
    return [];
  }
}

/** Cmd on a Mac, Ctrl everywhere else. `navigator.platform` is deprecated
    but still the only thing every browser answers; the newer field wins
    where there is one. */
export function onMac(): boolean {
  const data = (navigator as Navigator & { userAgentData?: { platform?: string } }).userAgentData;
  return /mac|iphone|ipad|ipod/i.test(data?.platform || navigator.platform || '');
}

function isHotkey(ev: KeyboardEvent, hotkey: Hotkey): boolean {
  const key = ev.key.toLowerCase();
  const primary = onMac() ? ev.metaKey && !ev.ctrlKey : ev.ctrlKey && !ev.metaKey;
  switch (hotkey) {
    case 'cmd-k': return primary && !ev.altKey && !ev.shiftKey && key === 'k';
    case 'cmd-shift-p': return primary && !ev.altKey && ev.shiftKey && key === 'p';
    case 'ctrl-shift-p': return ev.ctrlKey && !ev.metaKey && !ev.altKey && ev.shiftKey && key === 'p';
  }
}

/** Watch for the shortcut the settings name. In the capture phase so the
    terminal's keyboard field never sees the press. */
export function watchHotkey() {
  window.addEventListener(
    'keydown',
    (ev) => {
      if (palette.open || !isHotkey(ev, views.settings['palette-hotkey'])) return;
      ev.preventDefault();
      ev.stopPropagation();
      openPalette();
    },
    true,
  );
}
