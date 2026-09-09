// The settings panel, and what this browser remembers between loads: the
// preferences themselves (thinkterm-web/src/settings.rs, which the wasm
// holds and hands back through `views.settings`), the Space that was on
// show, and the palette's recent picks. The wasm applies what concerns it
// -- the language and the font -- and the page applies the rest.

import { handle } from './client';
import { refreshViews, setLocale, views } from './client.svelte';
import type { LanguageOption, Theme } from './model';

/** Where this browser keeps the preferences, whole, as the wasm's JSON. */
const STORE = 'thinkterm.settings';
/** The Space the sidebar was showing, so the next load opens on it. */
const SPACE = 'thinkterm.space';

export const panel = $state({
  /** The modal is up. */
  open: false,
  /** What the wasm said about the last change; empty when it took it. */
  error: '',
  /** The languages the page can be set to, in the active language. */
  languages: [] as LanguageOption[],
});

export function openSettings() {
  panel.error = '';
  reread();
  panel.open = true;
}

export function closeSettings() {
  if (!panel.open) return;
  panel.open = false;
  // The panel took focus off the field the terminal types through.
  document.getElementById('kbd')?.focus();
}

/** Read the wasm's views back and the language list with them: a change of
    language relabels the list itself. */
function reread() {
  refreshViews();
  const client = handle.client;
  if (!client) return;
  try {
    panel.languages = JSON.parse(client.languages()) as LanguageOption[];
  } catch {
    panel.languages = [];
  }
}

/** Put the preferences, whole, where the next load will find them. */
export function persist() {
  try {
    localStorage.setItem(STORE, JSON.stringify(views.settings));
  } catch {
    // A browser that blocks storage keeps them for this load only.
  }
}

/** The stored preferences as they were written, or nothing. Parsed loosely
    on purpose: the wasm checks them, and one bad field is not a reason to
    lose the rest. */
export function storedSettings(): Record<string, unknown> {
  try {
    const raw = localStorage.getItem(STORE);
    if (!raw) return {};
    const value: unknown = JSON.parse(raw);
    return value && typeof value === 'object' && !Array.isArray(value) ? (value as Record<string, unknown>) : {};
  } catch {
    return {};
  }
}

/** One preference, by its JSON name. The wasm's refusal is shown rather
    than thrown; what it took is read back and stored. */
export function setSetting(key: string, value: unknown) {
  const client = handle.client;
  if (!client) return;
  panel.error = client.set_setting(key, JSON.stringify(value));
  // The wasm activated the language itself; this is how the page learns
  // which locale that came to, for the About line.
  if (key === 'language' && panel.error === '') {
    document.documentElement.lang = setLocale(String(value));
  }
  reread();
  persist();
}

export function storedSpace(): string {
  try {
    return localStorage.getItem(SPACE) ?? '';
  } catch {
    return '';
  }
}

/** Whether the Space this browser was showing has still to be asked for.
    The boot asks once, but the server's tree almost always arrives after
    it, and until it does no Space is known by name. */
let restoring = true;

/** The Space the sidebar is showing. The first time the tree has named one
    -- which is the first time there is a Space to ask for at all -- the one
    this browser was last showing is asked for instead; from then on what is
    on show is what is remembered. */
export function followSpace(showing: string) {
  if (restoring) {
    restoring = false;
    const wanted = storedSpace();
    // False when that Space is gone from the server: the sidebar stays
    // where it landed, and that is what is remembered from here on.
    if (wanted !== '' && wanted !== showing && handle.client?.set_space(wanted)) return;
  }
  try {
    localStorage.setItem(SPACE, showing);
  } catch {
    // As above: this load's only.
  }
}

// The theme is the page's to paint: dark is the default and carries no
// mark, light sets one, and "system" follows the browser until it is
// changed again. Only one listener is ever registered.
let media: MediaQueryList | null = null;
let follow: (() => void) | null = null;

function paint(dark: boolean) {
  if (dark) delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = 'light';
}

export function applyTheme(theme: Theme) {
  if (media && follow) media.removeEventListener('change', follow);
  media = null;
  follow = null;
  if (theme === 'light' || theme === 'dark') {
    paint(theme === 'dark');
    return;
  }
  const query = window.matchMedia('(prefers-color-scheme: dark)');
  const listener = () => paint(query.matches);
  query.addEventListener('change', listener);
  media = query;
  follow = listener;
  paint(query.matches);
}
