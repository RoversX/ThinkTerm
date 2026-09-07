// The page: pick up the token, fetch the fonts, start the wasm client.
import init, { start, color_check } from "./pkg/thinkterm_web.js";

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
try {
  if (token) {
    sessionStorage.setItem("thinkterm.token", token);
    history.replaceState(null, "", location.pathname + location.search);
  } else {
    token = sessionStorage.getItem("thinkterm.token");
  }
} catch (e) {
  console.warn("session storage unavailable; the token is kept for this load only", e);
}
const requested = Number(params.get("font"));
const fontSize = Number.isFinite(requested) && requested >= 6 && requested <= 72 ? requested : 12;

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
  if (!navigator.gpu) {
    throw new Error("WebGPU is not available: open the page over https or http://localhost in a browser with WebGPU enabled");
  }
  if (params.get("check") === "color") {
    status.textContent = "COLOR " + await color_check("term");
  } else {
    if (!token) throw new Error("no token: open the URL that `thinkterm cli web-token mint` printed");
    const names = FONTS.map(([n]) => n);
    const data = await Promise.all(FONTS.map(([, u]) => fetchFont(u)));
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    await start("term", "kbd", "status", `${scheme}://${location.host}/ws`, token, names, data, fontSize);
  }
} catch (e) {
  fail(String(e && e.message ? e.message : e));
}
