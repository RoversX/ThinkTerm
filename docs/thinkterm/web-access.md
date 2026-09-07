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
does not open or close web ports.

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

Images (kitty, sixel, iTerm2) and colour emoji are not drawn; tabs, splits and
the ThinkTerm tree are not shown -- the page mirrors one pane; there is no
reconnect after the socket drops (reload the page); the cursor does not blink;
box-drawing comes from the font rather than the desktop's custom block glyphs;
font fallback is the bundled list (JetBrains Mono, Symbols Nerd Font Mono), so
scripts outside it show as missing-glyph boxes.
