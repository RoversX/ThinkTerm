# Privacy Policy for ThinkTerm

No data about your device(s) or ThinkTerm usage is collected by the ThinkTerm
project.

## Data Maintained by ThinkTerm

ThinkTerm maintains some historical data, such as recent searches or action
usage, in some of its overlays such as the debug overlay and character
selector, in order to make your usage more convenient. It is used only
by the local process, and care is taken to limit access for the associated
files on disk to only your local user identity.

ThinkTerm tracks the output from the commands that you have executed in
a scrollback buffer. At the time of writing, that scrollback buffer
is an in-memory structure that is not visible to other users of the machine.
In the future, if ThinkTerm expands to offload scrollback information to
your local disk, it will do so in such a way that other users on the
same system will not be able to inspect it.

## macOS and Data permissions

On macOS, when a GUI application that has a "bundle" launches child processes
(eg: ThinkTerm, running your shell, and your shell running the programs which
you direct it to run), any permissioned resource access that may be attempted
by those child processes will be reported as though ThinkTerm is attempting to
access those resources.

The result is that from time to time you may see a dialog about ThinkTerm
accessing your Contacts if you run a `find` command that happens to step through
the portion of your filesystem where the contacts are stored. Or perhaps you
are running a utility that accesses your camera; it will appear as though
ThinkTerm is accessing those resources, but it is not: there is no logic within
ThinkTerm to attempt to access your contacts, camera or any other sensitive
information.

## Update Checking

By default, once every 24 hours, ThinkTerm makes an HTTP request to GitHub's
release API in order to determine if a newer version is available and to
notify you if that is the case.

The content of that request is private between your machine and GitHub. The
contributors to ThinkTerm cannot see inside that request and therefore cannot
infer any information from it.

If you wish, you can disable update checking by setting
`check_for_updates = false`.

## Browser Access

ThinkTerm's multiplexer can serve a browser client, so that a phone or
another machine can open the terminals running on this one. It is off
unless you turn it on: no port is opened until a `web_servers` entry is
configured, or until you switch one on in Settings → Web or with
`thinkterm cli web-server on`. Nothing about it reports anywhere. The
page, its JavaScript, its wasm and its fonts are all served by your own
machine, the page makes no request to any third party, and no usage of it
is collected.

A browser is admitted by a web token that you mint yourself with
`thinkterm cli web-token mint`. A token is a login as your user on that
machine: anyone holding it can open a shell and read every pane, so treat
it like an ssh key. The page takes it out of the address bar as soon as it
loads and keeps it in that tab's `sessionStorage`, which the browser
discards when the tab closes.

While a listener is on, ThinkTerm keeps two things on the local disk,
for your user, and sends neither anywhere. On Linux and macOS both are
written readable by their owner alone. Windows has no equivalent to set:
they take the permissions of the directory they land in, which for a
default user profile is you, SYSTEM and the Administrators group, and
nobody else. An administrator of the machine can therefore read them --
though an administrator can read the terminals themselves and does not
need either file to do it.

The first is the tokens, and only if a `token_file` is configured: unset
means memory only, and restarting the server forgets every token. What is
written for each is its id, the name you gave it, a sha256 digest of the
token -- never the token itself, so a copied file admits nobody -- when it
was created, when it expires, when it was last used, and a coarse
description of the browser that last used it, taken from the User-Agent it
sent ("iPhone · Safari"). No address of any device that connected is
recorded, and the server's log names the token that was admitted rather
than who used it.

The second is a self-signed certificate and its private key, under
`web-tls/` in ThinkTerm's data directory (`~/.local/share/thinkterm` on
Linux, `~/Library/Application Support/thinkterm` on macOS,
`%APPDATA%\thinkterm` on Windows). It is made
only when you bind a listener off loopback without supplying a certificate
of your own, and it names the machine's hostname and every non-loopback
address it has, so that a browser reaching it by any of them lands in a
secure context. Those names are shown to whoever connects to that port,
which is what a certificate is for; nothing else is in it.

The browser keeps your own choices for that page -- language, theme, font
size, colour scheme, the Space it was showing, panel widths, recent
palette picks -- in its `localStorage`, on your device. Clearing the
site's data forgets them.

## Third-Party Builds

The above is true of the ThinkTerm source code and the binaries produced by
ThinkTerm's release workflow and made available from
https://github.com/RoversX/thinkterm/.

If you obtained a pre-built ThinkTerm binary from some other source, be aware
that the person(s) building those versions may have modified them to behave
differently from the source version.
