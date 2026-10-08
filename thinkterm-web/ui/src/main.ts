// The page: pick up the token, fetch the fonts, start the wasm client.
import { flushSync, mount } from 'svelte';
import App from './App.svelte';
import { handle } from './client';
import { activate, attach, setBoot, views } from './client.svelte';
import { configure as configureMachines, fetchTabIcons } from './machines.svelte';
import { dress, setTabIcons } from './tabicons.svelte';
import { watchMobile } from './mobile.svelte';
import { storedPicks, watchHotkey } from './palette.svelte';
import { applyTheme, FOLLOW_DESKTOP, previewScheme, storedScheme, storedSettings, storedSpace } from './settings.svelte';
import type { Theme } from './model';
import './tokens.css';

// Which shape the page is in -- the desktop's, or the phone's -- before it
// is drawn, so the first frame is already the right one and the canvas is
// never sized for a key bar that is about to appear.
watchMobile();
// The chrome goes up first and synchronously: #side, #term and #kbd are what
// the client looks for by id, so none of them may arrive a tick late. The
// boot's own progress goes through the store, since #status is drawn here.
mount(App, { target: document.getElementById('app') as HTMLElement });
flushSync();

// The wasm-bindgen glue is served next to the page, not bundled with it: it
// resolves its own .wasm from its own URL, so it has to keep that URL. Its
// types are the ones wasm-bindgen wrote beside it (ci/build-web.sh).
type Glue = typeof import('../../www/pkg/thinkterm_web.js');

const fail = (msg: string) => { setBoot(msg, true); console.error(msg); };

// The token arrives once in the URL fragment -- never sent to the server,
// never in a Referer -- and is kept for this tab only; the address bar is
// cleaned so it is not copied or bookmarked by accident.
const params = new URLSearchParams(location.search);
const fragment = new URLSearchParams(location.hash.replace(/^#/, ""));
// A browser that blocks storage throws on the first touch; the token then
// lives only in this page load, and the failure is reported, not silent.
let token = fragment.get("token");
if (token) {
  // Out of the address bar first, and outside the try: a browser that
  // blocks storage throws on setItem, and doing this after it meant the
  // token stayed in the URL -- visible in the bar, in a screenshot, and in
  // whatever the user pastes next -- in exactly the case the comment said
  // it would not.
  history.replaceState(null, "", location.pathname + location.search);
}
try {
  if (token) {
    sessionStorage.setItem("thinkterm.token", token);
  } else {
    token = sessionStorage.getItem("thinkterm.token");
  }
} catch (e) {
  console.warn("session storage unavailable; the token is kept for this load only", e);
}
// What this browser last chose, and the URL's overrides for this load
// only: `?lang=`, `?theme=` and `?font=` are for a probe or a link, and
// are never written back.
const settings = storedSettings();
const lang = params.get("lang");
if (lang) settings.language = lang;
const theme = params.get("theme");
if (theme === "light" || theme === "dark" || theme === "system") settings.theme = theme;
const requested = Number(params.get("font"));
if (Number.isFinite(requested) && requested >= 6 && requested <= 72) settings.font = { mode: "pinned", pt: requested };

// Without a pinned size the page takes the desktop's cell size once
// attached; 12 is only what it draws with until then.
const font = settings.font as { mode?: string; pt?: number } | undefined;
const pinned = font?.mode === "pinned" ? Number(font.pt) : NaN;
const fontPinned = Number.isFinite(pinned) && pinned >= 6 && pinned <= 72;
const fontSize = fontPinned ? pinned : 12;
// The CSS font stack the glyph fallback draws with. This is also how the
// regional shape of a Han character is chosen; see canvas.rs.
const glyphFont = params.get("glyphfont") || "";
// The desktop is dark unless told otherwise; so is the page. Painted here
// rather than after the client attaches, so the boot is not the wrong
// colour for as long as the wasm takes to arrive.
applyTheme(typeof settings.theme === "string" ? (settings.theme as Theme) : "dark");
// The interface language: the stored preference, else the browser's own
// list ("system").
const locale = typeof settings.language === "string" && settings.language !== "" ? settings.language : "system";
const languages = Array.from(navigator.languages ?? [navigator.language]);
// The palette's shortcut is watched from here on, whatever the settings
// name it; before the client attaches it is the default.
watchHotkey();

const FONTS: [string, string][] = [
  ["JetBrains Mono", "./fonts/JetBrainsMono-Regular.ttf"],
  ["Symbols Nerd Font Mono", "./fonts/SymbolsNerdFontMono-Regular.ttf"],
];

async function fetchFont(url: string): Promise<Uint8Array> {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${url}: ${res.status}`);
  return new Uint8Array(await res.arrayBuffer());
}

try {
  setBoot("Loading ThinkTerm…", false, 0);
  const glueUrl = new URL("pkg/thinkterm_web.js", document.baseURI).href;
  const mod = await import(/* @vite-ignore */ glueUrl) as Glue;
  await mod.default();
  const check = params.get("check");
  if (check === "fallback") {
    // Deliberately before the WebGPU check: this one draws on a 2D canvas
    // and needs no GPU, no token and no pane, which is what lets it run on
    // Safari, on Firefox, and on a machine with hardly any fonts installed
    // -- the place the glyph fallback is most likely to fail.
    const names = FONTS.map(([n]) => n);
    const data = await Promise.all(FONTS.map(([, u]) => fetchFont(u)));
    setBoot("FALLBACK " + mod.fallback_check(names, data, fontSize, glyphFont));
  } else if (!navigator.gpu) {
    throw new Error("WebGPU is not available: open the page over https or http://localhost in a browser with WebGPU enabled");
  } else if (check === "color") {
    setBoot("COLOR " + await mod.color_check("term"));
  } else if (check === "graphics") {
    const names = FONTS.map(([n]) => n);
    const data = await Promise.all(FONTS.map(([, u]) => fetchFont(u)));
    setBoot("GRAPHICS " + await mod.graphics_check(names, data, fontSize));
  } else {
    if (!token) throw new Error("no token: open the URL that `thinkterm cli web-token mint` printed");
    setBoot("Loading fonts…", false, 1);
    const names = FONTS.map(([n]) => n);
    const data = await Promise.all(FONTS.map(([, u]) => fetchFont(u)));
    setBoot("Connecting…", false, 2);
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    const hub = await mod.start("term", "kbd", `${scheme}://${location.host}/ws`, token, names, data, fontSize, glyphFont, fontPinned, locale, languages, undefined);
    handle.client = hub;
    // The preferences go over whole: the wasm checks them and applies the
    // language and the font itself. A refusal is worth a line in the
    // console, not a failed boot -- the defaults are perfectly usable.
    const refused = handle.client.apply_settings(JSON.stringify(settings));
    if (refused !== "") console.warn("stored settings: " + refused);
    // The picked scheme's colours were stored with its name, so the first
    // frame is already in it and nothing waits on schemes.json.
    if (settings["terminal-scheme"] && settings["terminal-scheme"] !== "desktop") previewScheme(storedScheme());
    attach(hub);
    activate(hub);
    // Which locale the preference came to, for the settings panel to name.
    document.documentElement.lang = handle.client.set_locale(locale, languages);
    handle.client.set_recent(storedPicks());
    // The Space this browser was last showing; gone from the server since,
    // and the sidebar simply stays where it landed.
    const space = storedSpace();
    if (space !== "") handle.client.set_space(space);
    // Other machines, through this server: each a client of its own, with
    // this browser's preferences as they stand when it opens.
    configureMachines({
      // The language as it stands now, not as this load began: start()
      // activates the one it is given, for the whole page.
      start: (canvas, textarea, opener) =>
        mod.start(canvas, textarea, "", token as string, names, data, fontSize, glyphFont, fontPinned, String(views.settings.language || "system"), languages, opener),
      token: token as string,
      prepare: (client) => {
        client.apply_settings(JSON.stringify(views.settings));
        client.set_locale(String(views.settings.language), languages);
        if (views.settings["terminal-scheme"] !== FOLLOW_DESKTOP) {
          const picked = storedScheme();
          if (picked) client.set_terminal_palette(JSON.stringify(picked));
        }
        client.set_recent(storedPicks());
        dress(client);
      },
    }, hub);
    // The cards this server's desktop dresses its tabs with; without them
    // (a relay that cannot say) the tabs keep the plain terminal mark.
    void fetchTabIcons().then((catalog) => {
      if (catalog !== null) setTabIcons(catalog);
    });
  }
} catch (e) {
  const err = e as { message?: string } | null;
  fail(String(err && err.message ? err.message : e));
}
