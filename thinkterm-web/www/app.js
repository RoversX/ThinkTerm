// The page: pick up the token, fetch the fonts, start the wasm client.
import init, { start, color_check, fallback_check, graphics_check } from "./pkg/thinkterm_web.js";

const status = document.getElementById("status");
const fail = (msg) => { status.textContent = msg; status.classList.add("error"); console.error(msg); };

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
const requested = Number(params.get("font"));
const fontSize = Number.isFinite(requested) && requested >= 6 && requested <= 72 ? requested : 12;
// The CSS font stack the glyph fallback draws with. This is also how the
// regional shape of a Han character is chosen; see canvas.rs.
const glyphFont = params.get("glyphfont") || "";

const FONTS = [
  ["JetBrains Mono", "./fonts/JetBrainsMono-Regular.ttf"],
  ["Symbols Nerd Font Mono", "./fonts/SymbolsNerdFontMono-Regular.ttf"],
];

async function fetchFont(url) {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${url}: ${res.status}`);
  return new Uint8Array(await res.arrayBuffer());
}

try {
  await init();
  const check = params.get("check");
  if (check === "fallback") {
    // Deliberately before the WebGPU check: this one draws on a 2D canvas
    // and needs no GPU, no token and no pane, which is what lets it run on
    // Safari, on Firefox, and on a machine with hardly any fonts installed
    // -- the place the glyph fallback is most likely to fail.
    const names = FONTS.map(([n]) => n);
    const data = await Promise.all(FONTS.map(([, u]) => fetchFont(u)));
    status.textContent = "FALLBACK " + fallback_check(names, data, fontSize, glyphFont);
  } else if (!navigator.gpu) {
    throw new Error("WebGPU is not available: open the page over https or http://localhost in a browser with WebGPU enabled");
  } else if (check === "color") {
    status.textContent = "COLOR " + await color_check("term");
  } else if (check === "graphics") {
    const names = FONTS.map(([n]) => n);
    const data = await Promise.all(FONTS.map(([, u]) => fetchFont(u)));
    status.textContent = "GRAPHICS " + await graphics_check(names, data, fontSize);
  } else {
    if (!token) throw new Error("no token: open the URL that `thinkterm cli web-token mint` printed");
    const names = FONTS.map(([n]) => n);
    const data = await Promise.all(FONTS.map(([, u]) => fetchFont(u)));
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    await start("term", "kbd", "status", `${scheme}://${location.host}/ws`, token, names, data, fontSize, glyphFont);
  }
} catch (e) {
  fail(String(e && e.message ? e.message : e));
}
