# Connect reverse terminal access

Connect reverse access lets programs on a connected server read or send input to your eligible
local terminals. It uses the existing ThinkTerm Connect session and opens no new public port.
Access is off until you authorize that server and complete local system authentication.

Use matching versions of the desktop app, CLI, and mux server. An older component can prevent
the entire connection from attaching.

## Approve access

1. Connect to the server using ThinkTerm Connect.
2. Open **Settings → Agents → Connect Reverse Access**.
3. Select the connected server.
4. Choose either **Read only** or **Read and input**. The scope is **All local terminals**,
   including eligible terminals opened while this authorization is active.
5. Review the duration and select **Authenticate**, then complete system authentication on the
   desktop. On macOS the system's Touch ID window opens straight away and names the access, its
   duration, and the server. On Windows and Linux a confirmation window shows these first. Both
   follow the app's language.

The default duration is 30 minutes and can be changed to 1–120 minutes. Changing this default
does not change an existing grant. Active grants show their server, scope, permissions, remaining
time, and **Revoke** and **Renew** actions. Renewal requires fresh authentication. Cancelling
or failing a renewal keeps the previous grant's original deadline.

Each server connection has its own authorization. Terminal creation and closure are handled
automatically; there is no terminal selection list.

The app automatically uses its active local session host, or its own local terminals when no
local session host is attached. The authorization stays with that host and covers only terminals
the connected desktop currently owns. Remote mirrors and separate manual terminal hosts are
outside this scope. Changing the local host requires new authentication. Up to 256 eligible
terminals are supported per connection; exceeding the limit ends access instead of silently
publishing an incomplete list.

Only the current Connect session's verified, complete password-only SSH authentication chain
qualifies. Public-key, SSH-agent, certificate, keyboard-interactive, and externally reused
connections do not qualify. Authentication evidence that cannot distinguish a rejected key
from a partially accepted key is rejected conservatively. This restriction applies to reverse
access and does not change ordinary Connect authentication preferences.

The server's host key must be known or accepted. A host you accept at the first-connection
prompt counts as verified, and can be authorized on that same connection.

## Use the existing CLI on the server

Run `list` to obtain the current reverse target name. The route suffix changes when a new
route is established; copy the name returned by the current list.

```sh
thinkterm cli list --json
thinkterm cli get-text --pane desktop-0123abcd/21 --trim
thinkterm cli send-text --pane desktop-0123abcd/21 --enter 'Hello'
thinkterm cli send-key --pane desktop-0123abcd/21 ctrl+c
```

Reverse list entries contain the target, route, label, sanitized title, permission, and
remaining duration. They do not expose hidden terminals, working directories, account names,
or process information. Identical or ambiguous names are rejected.

Plain-text reads support the existing line range, `--trim`, and `--unwrap` behavior. Reads
include the selected terminal's screen and scrollback, limited to 2,000 physical lines and
1 MiB per request. Request a smaller range if either limit is exceeded. Clipboard data,
images, VT/HTML output, event subscriptions, and `--wait` are not part of reverse access.

Each connection allows about ten requests per second, with short bursts of twenty. `list` and
`get-text` wait their turn at that limit, so reading many terminals in a row just takes longer.
Input does not wait: it is refused with `Busy`, having submitted nothing.

Text input is limited to 64 KiB before encoding; key input is limited to 64 keys. On read-only
access these commands say so and send nothing. `--no-paste` and `--enter` are supported. Text
and its optional Enter occupy one ordered input operation, with the same short wait between
them as on a local terminal, so that a program reading the text as a paste does not take the
Enter as part of it. Revoking access during that wait stops the Enter. Input never takes
terminal ownership or changes the active layout implicitly.

A successful input result means the bytes reached the terminal's PTY, not that a command
completed successfully. If a program stops reading its input, the request ends after about
eight seconds and reports how many bytes were submitted. On cancellation or transport loss
the result may be partial or unknown. Do not automatically retry input. Cancellation during
bracketed paste can leave the application in paste mode; the error reports this possibility.
ThinkTerm does not inject a cleanup sequence after authorization has been revoked.

Stopping a CLI command does not stop what it already sent. A request that has reached the
desktop runs to its end or to its deadline, and its result is dropped.

Programs under the connected server account share the approved capability. It is not
restricted to one particular agent or process under that account.

## When authorization ends

Terminal removal or loss of ownership removes that terminal from the authorization. A later
eligible terminal receives a fresh export identity; old requests cannot address it. These changes
do not extend the deadline or revoke the entire connection-wide grant when no terminals remain.
Revoke, expiry, reconnect, host replacement, desktop session changes, and process exit end
the connection's authorization. Updates and mux handoff do not
transfer grants. Start a new approval after reconnecting or upgrading.

One request going wrong does not end it. A closed terminal, a timeout, a stopped CLI, or a
request over the limit fails that request and leaves the authorization in place. Only losing
the channel to the desktop ends it. A long read over a connection slower than about 256 kbit/s
can count as that, so read smaller ranges there.

Deadlines use the terminal host's monotonic clock and include suspend time. Changing the wall
clock or time zone cannot extend a grant. Native session monitoring and the local controller
have bounded heartbeats; detected monitoring failure cancels access and a missing heartbeat
expires within five seconds. Physical link failure is detected through an event or timeout.
Bytes already submitted to the operating system or delivered to the terminal cannot be
recalled by revocation.

Remote requests cannot open an authentication window. The GUI must initiate each approval.
Ordinary CLI commands, configuration settings, and plugins cannot submit authentication
success or extend a grant.

## Platform requirements

| Platform | Authentication and required local environment |
| --- | --- |
| macOS | Packaged Swift helper beside the app executables; active local console session. LocalAuthentication uses the system device-owner policy, which can allow Touch ID, the account password, or a paired Apple Watch according to system configuration. |
| Windows | Packaged `thinkterm-connect-auth.exe` beside the executables. Windows 11 or later uses Windows Hello where it is set up. Otherwise the system credential prompt asks for the password of the current local account. Domain, Microsoft, and Entra accounts have no password path, and need a working Hello. |
| Linux desktop | Packaged helper, `/usr/bin/zenity`, a protected `/etc/pam.d/thinkterm-connect-auth`, systemd-logind, and an active local graphical session with GNOME or freedesktop screensaver notifications. Root, forwarded X displays, ambiguous sessions, and missing or unverifiable lock monitoring are rejected. |

macOS authentication returns a system decision. Windows password fallback and Linux PAM
authentication handle the entered secret inside a short-lived local helper. It is never
sent over Connect, placed in the business protocol, or retained as a ThinkTerm password.
The Windows and Linux helpers clear their own copies; the system's credential prompt and PAM
own theirs, so complete in-memory erasure is not claimed.

No helper process starts merely because reverse access is available in Settings.

Debian/Ubuntu, Fedora, openSUSE, and Arch desktop packages install the PAM policy. An AppImage
or portable archive cannot install a protected system policy automatically. An administrator
must install the appropriate supplied policy with owner `root`, mode `0644`, at
`/etc/pam.d/thinkterm-connect-auth` and install Zenity. The supplied policy variants are in
`share/thinkterm/connect-auth/pam` (under `usr` in an AppImage). Use the variant matching the
distribution's existing PAM stack; missing or writable policy files are rejected.

A headless server can use already approved desktop access without PAM, Hello, or a server-side
dialog. Creating a new grant for a headless server's own terminals is unsupported. Remote
mirrors, tmux/control-mode terminals, terminals you joined through someone else's share, and
third-device relays cannot be exported through this feature. A terminal you are sharing out
is still one of your local terminals, and is covered like any other.

## Troubleshooting

If approval is unavailable, check the connection's authentication eligibility, local terminal
ownership, matching component versions, helper packaging, and the platform requirements above.
Unknown authentication or session state always rejects access.
