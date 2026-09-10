// The settings panel, and what this browser remembers between loads: the
// preferences themselves (thinkterm-web/src/settings.rs, which the wasm
// holds and hands back through `views.settings`), the Space that was on
// show, and the palette's recent picks. The wasm applies what concerns it
// -- the language and the font -- and the page applies the rest.

import { handle } from './client';
import { refreshViews, setLocale, views } from './client.svelte';
import type { LanguageOption, Scheme, Theme } from './model';

/** Where this browser keeps the preferences, whole, as the wasm's JSON. */
const STORE = 'thinkterm.settings';
/** The Space the sidebar was showing, so the next load opens on it. */
const SPACE = 'thinkterm.space';
/** The picked scheme, colours and all, so a boot needs no `schemes.json`. */
const SCHEME = 'thinkterm.scheme';
/** The `terminal-scheme` value that means "whatever the server is set to". */
export const FOLLOW_DESKTOP = 'desktop';

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


// The colour schemes. The table is a build output (ci/build-web.sh runs
// `thinkterm cli color-schemes --json` into www/schemes.json), a megabyte
// or so, so it is fetched when the picker first opens and never at boot:
// the picked scheme's own colours are stored with its name.

export const schemes = $state({
  /** The table, once fetched. */
  all: [] as Scheme[],
  /** A fetch is in flight. */
  loading: false,
  /** Why the table could not be read; empty when it could. */
  error: '',
});

export async function loadSchemes() {
  if (schemes.all.length > 0 || schemes.loading) return;
  schemes.loading = true;
  schemes.error = '';
  try {
    const res = await fetch(new URL('schemes.json', document.baseURI).href);
    if (!res.ok) throw new Error(String(res.status));
    schemes.all = (await res.json()) as Scheme[];
  } catch (e) {
    schemes.error = String((e as { message?: string })?.message ?? e);
  } finally {
    schemes.loading = false;
  }
}

/** The scheme this browser picked, as it was stored, or nothing. */
export function storedScheme(): Scheme | null {
  try {
    const raw = localStorage.getItem(SCHEME);
    if (!raw) return null;
    const value = JSON.parse(raw) as Scheme;
    return value && typeof value.name === 'string' && Array.isArray(value.ansi) ? value : null;
  } catch {
    return null;
  }
}

/** Draw with `scheme` without keeping it: what a hover shows. `null` goes
    back to the server's own scheme. */
export function previewScheme(scheme: Scheme | null) {
  const client = handle.client;
  if (!client) return;
  client.set_terminal_palette(scheme ? JSON.stringify(scheme) : undefined);
}

/** Keep `scheme` for this browser: the colours beside the name, so the next
    boot draws with it before `schemes.json` is anywhere near. */
export function pickScheme(scheme: Scheme | null) {
  previewScheme(scheme);
  try {
    if (scheme) localStorage.setItem(SCHEME, JSON.stringify(scheme));
    else localStorage.removeItem(SCHEME);
  } catch {
    // A browser that blocks storage keeps it for this load only.
  }
  setSetting('terminal-scheme', scheme ? scheme.name : FOLLOW_DESKTOP);
}
