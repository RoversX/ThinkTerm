# Agent Status Protocol

ThinkTerm can track the state of coding agents (Claude Code, Codex, Pi,
OpenCode, …) running in its panes: **working**, **blocked** (waiting for a
user decision), or **idle**. The state feeds the thread status indicators
in the left sidebar and the Agents panel in the right sidebar
(Settings → Agents → *Agent Panel*, off by default).

Detection arbitrates three signal layers, strongest contract first:

1. **The `THINKTERM_AGENT` user var** — the agent (or a hook script acting
   for it) reports identity, session, and state itself. This is the
   public protocol described below, and the only layer that survives any
   UI redesign of the agent.
2. **Screen rules** — herdr-compatible TOML manifests matched against the
   pane's live screen tail and OSC title/progress. Bundled rules can be
   overridden per agent by dropping a manifest into
   `~/.config/thinkterm/agent-detection/` (the `id` *inside* the file
   selects which bundled manifest it replaces; the filename is
   ignored) and pressing *Reload detection rules* in the Agents panel.
3. **Native escape sequences** — OSC 9;4 progress and the leading
   title-spinner marker; also the entire behavior when the feature toggle
   is off.

A matching screen rule outranks the contract's *state* (never its
identity): reporters commonly miss the "user pressed Esc" and "permission
prompt cancelled" transitions, and both end with the screen visibly idle.

**Identity is arbitrated the other way around.** A live foreground
process that matches a manifest is ground truth for *what is running
now* and always wins; a user var is a message from the past and never
overrides it. A contract establishes identity only while its emitter is
provably alive — while the pane's foreground process-group leader is
unchanged since the contract was last emitted — so a var left behind by
an exited agent cannot classify whatever runs in the pane next. In
short: *process identity > contract identity*, but *screen state >
contract state*.

Screen-derived transitions out of **blocked** and from **working to
idle** are debounced: they publish only after ~450 ms of consistent
observations (three 150 ms rechecks, 700 ms hard cap), because TUI
redraws are not atomic and a half-drawn frame must not replay
notifications. Contract-reported transitions bypass the debounce —
an explicit report is not a guess.

## Reporting state from your own agent

Emit one escape sequence to your own stdout/tty whenever your state
changes:

```
ESC ] 1337 ; SetUserVar=THINKTERM_AGENT=<base64(value)> BEL
```

For example, from a shell:

```sh
v="v1;agent=my-agent;state=working;session=$SESSION_ID;ts=$(date +%s)"
printf '\033]1337;SetUserVar=THINKTERM_AGENT=%s\007' \
  "$(printf %s "$v" | base64 | tr -d '\n')"
```

The sequence renders nothing and moves no cursor, so it is safe to
interleave with TUI output. It works over ssh and ThinkTerm multiplexer
domains: whichever terminal owns the pty parses it, and ThinkTerm mirrors
the value across mux connections automatically.

### Value grammar (version 1)

```
v1;agent=<id>;state=<working|idle|blocked>[;session=<id>][;pid=<n>][;ts=<unix-seconds>][;ended=1]
```

- `v1` — grammar version; unknown major versions are ignored entirely.
- `agent` (required) — a short stable identifier, e.g. `claude`, `soul`.
- `state` (required) — `working`, `idle`, or `blocked`. Anything else
  parses as *unknown* and does not drive the indicators.
- `session` — your native session id, reserved for session restore.
  May be empty.
- `pid` — accepted for forward compatibility; currently ignored.
- `ts` — emission time as unix seconds. A non-idle report older than six
  hours stops counting as a state authority (panes on a mux server can
  outlive the sessions that wrote to them), and a timestamp more than
  five minutes in the future is treated the same way — a fast clock
  must not make a report eternally fresh. On panes where ThinkTerm can
  never observe your process (ssh, tmux and serial panes), that same
  six-hour window also retires your *identity*: it is the only signal
  left that you may have crashed without saying `ended=1`. If your
  session can legitimately sit non-idle longer than that (an approval
  prompt waiting overnight), re-emit your current state periodically —
  any re-emission resets the window. Idle reports and reports without
  `ts` are trusted indefinitely.
- `ended=1` — the session is over. While your process is still the
  pane's foreground leader the flag is ignored (a live process outranks
  its own farewell); once the process exits, the pane stops being
  classified entirely.
- Unknown keys are ignored, so the grammar is forward-extensible.

Send `working` when a turn starts, `idle` when your agent is ready for
input, `blocked` the moment you show an approval/question prompt, and the
appropriate state again when the prompt resolves — including when the
user cancels or interrupts. If your runtime observes every one of those
transitions (it is your own event loop, not an external hook API), your
reports are complete and ThinkTerm needs no screen rules for you at all.

## Claude Code integration

Screen rules only — there is nothing to install. An earlier design
shipped a Claude hooks installer that reported state through the hooks
API's `terminalSequence` field; that field rejects OSC 1337 outright
(Claude Code's documented security allowlist admits only the bell, OSC
0/2 titles and CSI styling), so the channel can never carry the
`THINKTERM_AGENT` variable and the integration was removed. If session
identity is ever needed again (e.g. for resuming a thread with
`claude --resume`), the working shape is a SessionStart-only hook that
reports back over `thinkterm cli` — herdr's current approach — not a
terminal sequence. (No such subcommand exists yet; `thinkterm cli agent`
is query-only today.)

## Screen-rule manifests

Bundled manifests live in the ThinkTerm source under
`mux/src/agent_status/manifests/` and are sourced from the
[herdr](https://github.com/herdrdev/herdr) project (Apache-2.0). The
format is herdr's agent-detection manifest format; a user override for an
agent replaces the bundled file of the same id wholesale.

The `osc_title` and `osc_progress` regions see only what the application
actually emitted: both are empty until an OSC title / OSC 9;4 progress
report arrives, and they are cleared when the pane's agent changes.
Display fallbacks (the process-name tab title, an assumed "no progress")
never reach the rules, so an idle rule like `regex = ['\S']` on
`osc_title` cannot match a pane whose app never spoke.
