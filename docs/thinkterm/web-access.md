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

## Reaching a remote server

The recommended way to use a server on another machine is to keep it on
loopback and forward the port over ssh, which makes it a loopback origin on
your side too and needs no certificate:

```console
$ ssh -L 8088:127.0.0.1:8088 server.hostname
$ thinkterm cli --prefer-mux web-token mint   # on the server, or over ssh
```

The local port need not match: with `ssh -L 9000:127.0.0.1:8088` the page is
at `http://localhost:9000/`, and a loopback listener accepts any loopback
name on any port, because the port a forward uses is the client's choice.
Put the minted URL's token fragment on that address. A listener that is
reached under some other name (a reverse proxy, a non-loopback bind) must
list that name in `allowed_origins`; the minted URLs then point there.

Serving TLS directly needs a certificate the browser trusts; the server's own
TLS PKI is regenerated every time it starts and is not suitable for that.

## Building and serving the bundle

`ci/build-web.sh` builds `thinkterm-web` for `wasm32-unknown-unknown`, runs
`wasm-bindgen` (the CLI must match the version pinned in `Cargo.toml`) and
copies the fonts into `thinkterm-web/www`. The release tarballs and the macOS
app carry that directory as `share/thinkterm/web` (or `Contents/Resources/web`),
which is where the server looks by default; `static_dir` in a `web_servers`
entry or the `THINKTERM_WEB_STATIC_DIR` environment variable point it elsewhere,
for instance at a development checkout.

## What the first version does not do

Images (kitty, sixel, iTerm2) are not drawn; tabs, splits and the ThinkTerm
tree are not shown -- the page mirrors one pane, chosen when it connects,
and does not follow the desktop to another; the cursor does not blink. The input
method's hidden field follows the terminal cursor, including scrolling and
resizing, so the browser can place its candidate window there; pre-edit
text and its underline are not yet drawn in the grid.

A dropped socket is reopened by the page itself, with a backoff, on the same
pane; the status line says so while it is down. It gives up only when there
is nothing to come back to -- the pane was closed, or the server speaks a
different protocol version -- and says which.

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
