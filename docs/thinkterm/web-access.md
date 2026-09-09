# Browser access

**Status:** Landing in phases (server entry and token CLI first, browser client next)

ThinkTerm's multiplexer can serve a browser client. One TCP port per
`web_servers` entry delivers the page, its JavaScript and wasm over HTTP and
accepts a WebSocket that speaks the same protocol as every other client. The
browser mirrors a pane the way the desktop does and takes over input the
moment you type into it.

## `web_servers`

`config.web_servers` is a list of `WebServer` objects, read when
`thinkterm-mux-server` starts, like `tls_servers`; reloading the configuration
does not open or close web ports. While the server runs, the port is opened
and closed from the desktop's Settings → Web, or with
`thinkterm cli web-server on|off|status`; an address that is in
`web_servers` brings its TLS, origins and bundle directory with it, and one
that is not gets the loopback, plain-HTTP defaults.

```lua
config.web_servers = {
  {
    -- The address:port to listen on. Loopback by default. Browsers only
    -- expose WebGPU (which the client renders with) to secure contexts:
    -- http://localhost is one, plain http:// to any other host is not.
    bind_address = '127.0.0.1:8088',

    -- Set both to serve https/wss. Required off loopback.
    -- pem_cert = '/path/to/cert.pem',
    -- pem_private_key = '/path/to/key.pem',
    -- pem_ca = '/path/to/chain.pem',

    -- Where index.html, the JavaScript and the wasm live. Defaults to the
    -- share/thinkterm/web directory next to the installed executable.
    -- static_dir = '/usr/share/thinkterm/web',

    -- Where minted tokens are kept, as digests. Unset means memory only:
    -- restarting the server forgets every token.
    -- token_file = wezterm.home_dir .. '/.local/share/thinkterm/web-tokens.json',

    -- Origins allowed to open the WebSocket. Empty means this listener's
    -- own address, which is what a page served from this port sends.
    -- allowed_origins = { 'https://terminal.example.net' },

    -- Refuse to start without TLS when bind_address is not loopback.
    -- require_tls_off_loopback = true,
  },
}
```

## Tokens

```console
$ thinkterm cli web-token mint --label laptop --ttl 12h   # prints the URL to open
$ thinkterm cli web-token list
$ thinkterm cli web-token revoke <id>                      # or --all
```

A token is minted where the server already trusts you: over the unix socket,
or through `thinkterm cli` over ssh. It is a login as your user on that
machine; anyone holding it can open a shell and read every pane. Revoking a
token drops the browsers it admitted at once. The server keeps digests only,
so a copied token file reveals nothing.

## Reaching a remote server, or this one from a phone

Two ways, and both end with the page in a secure context, which is what a
browser needs before it exposes WebGPU.

**Over ssh** (nothing to trust): keep the listener on loopback and forward
the port, which makes it a loopback origin on your side too:

```console
$ ssh -L 8088:127.0.0.1:8088 server.hostname
$ thinkterm cli --prefer-mux web-token mint   # on the server, or over ssh
```

The local port need not match: with `ssh -L 9000:127.0.0.1:8088` the page is
at `http://localhost:9000/`, and a loopback listener accepts any loopback
name on any port, because the port a forward uses is the client's choice.
Put the minted URL's token fragment on that address.

**Directly, over https** (a phone on the same Wi-Fi, a tailnet, a LAN):
bind the listener to every address --

```console
$ thinkterm cli --prefer-mux web-server on --bind-address 0.0.0.0:8088
$ thinkterm cli --prefer-mux web-token mint
```

-- and the server makes itself a certificate: self-signed, for the
machine's hostname and every address it has, kept under
`~/.local/share/thinkterm/web-tls/` and remade only when an address it
does not name appears. `mint` then prints one URL per address, Tailscale
ones first, and `--url-only` picks the first that another device can use.
The browser warns once about the certificate; continuing puts the page in
a secure context. Two things to know: a name or address the machine gains
later is not in the certificate until the server restarts, and iOS Safari
has been known to refuse the WebSocket behind a certificate it was told to
continue past -- if the page attaches on a laptop but not on the phone,
that is why, and the ssh forward or a certificate the phone trusts (set
`pem_cert`/`pem_private_key` to one) are the ways around it.

A listener that is reached under some other name (a reverse proxy) must
list that name in `allowed_origins`; the minted URLs then point there. A
non-loopback listener with `require_tls_off_loopback = false` serves plain
http and no certificate is made; a browser reaching it has no WebGPU.

## The page's chrome

Everything around the terminal is the model's data drawn by the page:
the Rust side (`thinkterm-web/src/{tree,menu,palette,settings,agents}.rs`)
decides what the sidebar lists, what a context menu offers, what the
search finds and what a pick does; the Svelte page (`thinkterm-web/ui`)
only draws it and hands clicks back. A native client draws the same data
with its own widgets.

- **Spaces.** The sidebar shows one Space; the `…` beside its name lists
  the others, makes a new one, renames or deletes the current one (never the
  last). The page remembers the Space it was showing.
- **Context menus** on a pane (copy, paste, the four splits, the frontend
  access mode), a tab (close the tabs to the left/right/others, a new tab,
  zoom), a thread (pin, rename, delete, mark unread), a project (rename,
  new thread, collapse, archive -- with a warning when it would end
  programs -- remove) and an archived project (unarchive, delete for good).
  Tab rename/move and Reset Terminal have no server operation and are not
  offered.
- **Search** (Cmd+K, or Ctrl+Shift+P; the shortcut is a setting): threads
  in every Space, this window's tabs and panes, Spaces, and commands, ranked
  as the desktop's palette ranks them, recent picks first for an empty query.
- **Settings** (the gear): language (the desktop's catalogues, now under
  `thinkterm-i18n/`, English, 简体中文, 日本語, Français, Deutsch), theme
  (dark, light, follow the system), font size (follow the desktop's cell, or
  a fixed size), sidebar hover reveal, the Agents panel, the search
  shortcut. Kept in this browser's storage; the URL's `?lang=`, `?theme=`
  and `?font=` override them for one load.
- **Agents panel** (right, toggle at the tab row's end): one row per pane an
  agent runs in, with its state, as the desktop's panel shows them; a row
  brings that pane on show.
- **Phone.** A coarse pointer or a narrow window puts the page in its phone
  layout: the sidebar and the Agents panel become drawers, a key bar above
  the soft keyboard carries Esc, Tab, sticky Ctrl/Alt, the arrows, Home/End,
  PgUp/PgDn and the symbols a shell wants, a finger scrolls, a long press
  opens the pane's menu. Reach the server over https first (above).

## Building and serving the bundle

`ci/build-web.sh` builds `thinkterm-web` for `wasm32-unknown-unknown`, runs
`wasm-bindgen` (the CLI must match the version pinned in `Cargo.toml`),
copies the fonts into `thinkterm-web/www`, and builds the page around the
terminal from `thinkterm-web/ui` with Vite (Svelte 5 and TypeScript; Node 22
or newer with npm is needed, `npm ci` runs from the committed lockfile).
The page is only the chrome -- the tab row, the pane bars, the sidebar, the
toasts -- and reads what it shows from the wasm through the `Client` handle
`start()` returns; the terminal itself is the canvas, untouched by it.
`npm run check` in `thinkterm-web/ui` type-checks the page. The release workflow builds it once
and every package carries that directory: `share/thinkterm/web` in the
tarballs, deb and rpm, `Contents/Resources/web` in the macOS app, `web` beside
the executables on Windows. Those are where the server looks by default;
`static_dir` in a `web_servers` entry or the `THINKTERM_WEB_STATIC_DIR`
environment variable point it elsewhere, for instance at a development
checkout. `ci/deploy.sh` refuses to package without the bundle in CI and warns
on a laptop; `ci/macos-package.sh --build` builds it.

## What the page shows

The page is the desktop, as far as the wire allows. Its chrome is the
desktop's: the 40 px tab row of 160 px capsules (the tabs of the window on
show; `+` opens one, the x on a tab closes it, asking twice), a bar above
every pane like the desktop's pane nav bar (the pane's capsule with its
title -- a bare shell is "Terminal" -- and, on the right, new tab, split
down, split right and zoom; a stack's members are its capsules), and on the
left the sidebar described below. The palette is the desktop's dark set;
`?theme=light` picks the other. The page speaks the desktop's languages
(the catalogues in `thinkterm-i18n/i18n/`, shared with the desktop): the
browser's own language list picks one, `?lang=de-DE` picks for this load.
There is no status line: refusals and
reconnects are a passing remark at the bottom right, and a page that does
not hold the terminal in Handoff mode sees the desktop's card ("Terminal is
being used on another device -- Click or scroll to continue") over the
mirror, which stays visible.

The tab keeps the desktop's size and shape. The page's own font size is
derived from the desktop's cell so that a cell here is as large as one
there, and the desktop's pixel geometry (the bar above each pane, the
padding around the grid) lands on the page one to one; the page clips when
the tab is larger than the window and says so in the tab row. `?font=`
pins a size instead; Ctrl+Shift+F fits the tab to this window until the
desktop next takes it. Every claim the page makes is pane by pane
(`ClientViewport::Native`): taking the terminal over from the desktop
changes no pane's grid, and a tab a CLI laid out gets the same rows above
each pane the desktop would leave.

Taking the terminal works as it does on the desktop: a click, a scroll or a
keystroke in a pane takes it, and the first one is not lost -- the click's
focus is passed on and the key is typed once the server has handed over.
Chrome clicks (tabs, bars, sidebar) never take it. Ctrl+Shift+T asks
explicitly; Ctrl+Shift with an arrow moves the focus between panes.

The sidebar is the server's own tree of Spaces, Projects and Threads
(`ThinkTermSessionState`, `ThinkTermTree`), drawn like the desktop's with
the same status marks: a spinner while an agent in the thread works, an
alert when one waits on you, a check when it finished unseen, a dot
otherwise. Clicking a thread shows its active tab, or has the server create
its terminal in the project's directory (`EnsureThinkTermThread`); New
Thread, the `+` on a project, double-click to rename, pin, delete (asking
twice; its programs end), a new project from a path (`~/dir` or `/dir`, with
a `main` thread) and archive/restore all send the desktop's own tree
operations, and a refusal -- the server answers only with its tree, so one
is inferred from it -- is a remark. Windows no thread claims are listed
under "Other windows" so nothing is out of reach. The panel's width is
dragged at its edge and kept per browser; the button at the start of the
tab row hides it.

One thing to know: a desktop's own sidebar is its own store. It mirrors a
server's tree only for Spaces bound to a remote host, and its local session
host is excluded, so a page attached to the desktop's own machine sees the
server's tree, not the desktop's sidebar, until the desktop is taught to
keep its local Space on that server too. Against a remote mux server the
page and the desktop show the same tree.

Images (kitty, sixel, iTerm2) are not drawn; the cursor does not blink. The
input method's hidden field follows the terminal cursor, including
scrolling and resizing, so the browser can place its candidate window
there; pre-edit text and its underline are not yet drawn in the grid.

A dropped socket is reopened by the page itself, with a backoff, on the same
tab; the remark stays while it is down. It gives up only when there is
nothing to come back to -- the server has no panes, or speaks a different
protocol version -- and says which.

## Fonts

The page ships two faces (JetBrains Mono, Symbols Nerd Font Mono) and shapes
with them. Anything they do not cover -- CJK, Hangul, emoji, a scattering of
symbols -- is drawn with **your own machine's fonts**, on a 2D canvas, and
kept in the same glyph atlas as everything else. Braille is drawn from the
dot pattern rather than from a font, as the desktop does.

Blocks, fractions, shades and box-drawing use the desktop's shared geometric
glyph renderer instead of font outlines, so adjacent cells meet at their
edges. These glyphs are cached in the same atlas and obey its allocation
freeze and recovery rules. They do not use the Canvas font fallback budget.

`?glyphfont=` sets the CSS font stack used for that, which is also how the
regional shape of a Han character is chosen: pass a Japanese face to get
Japanese forms. A grapheme your machine has no font for either is still a
missing-glyph box, the same as it would be in a native terminal there.

Right-to-left scripts stay as boxes on purpose: the page does not do bidi,
so drawing them would put correct glyphs in the wrong order, which is harder
to notice than a box.

`?check=fallback` draws a sample of every kind and prints what it measured,
as JSON, in the status line. It needs no server, no token and no WebGPU, so
it is the way to check a browser or a machine before relying on it.

`?check=graphics` checks the real WebGPU line renderer for block seams,
cache reuse, recovery of a frozen cache miss, and the input field's DOM
position. It needs WebGPU but no token or pane. It reports CPU emission
timings for its sample, not end-to-end terminal latency. Use a real input
method to verify the native candidate window; the DOM check cannot inspect
that operating-system UI.
