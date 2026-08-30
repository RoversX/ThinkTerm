# ThinkTerm

A workspace-first terminal built for heavy AI workflows. Built on [WezTerm](https://github.com/wezterm/wezterm), keeping its fast, low-memory terminal core.

> *[main window screenshot]*

---

## Why ThinkTerm exists

I've used a lot of terminals. Some eat absurd amounts of memory. Some I just don't like looking at. Some are fast and good-looking but closed — you can't change anything. I never found one that satisfied me on memory, appearance, and openness at the same time.

What actually made me start building was AI agents. Once you have five or six agents running long tasks in different project directories, a normal tab bar stops meaning anything — a dozen tabs, all titled `zsh`, or all renamed by the program to the same string, each one scrolling output. To find out which is still working, which is waiting on you, and which finished twenty minutes ago, you have to click through them one by one.

The more sessions and the more output, the worse it gets. Which is exactly what an agent workflow looks like all day.

ThinkTerm organizes sessions into three layers — **Space / Project / Thread** — and gives every Thread its own work status.

More importantly, that structure doesn't belong to any one machine. Your laptop, your work dev box, a VPS — each runs its own mux server, but their workspaces **all show up in the same sidebar**. Local Spaces on top, then one group per server with its own connection state. Switching between them feels no different from switching a local tab.

Sessions are held by the server, so you can close your laptop, connect from another device, and find your work exactly as you left it. Several devices connected at once see the same structure. **In practice, your machines fuse into one.**

And since every agent lives in a pane the mux can address, they also have a channel to talk to each other.

---

## Core concepts

| Layer | What it is |
|---|---|
| **Space** | A working context. It can be local, bound to a remote mux server, or bound to an Obsidian-compatible Vault for notes |
| **Project** | A directory |
| **Thread** | A session with its own split layout; can be pinned or marked unread |

Threads carry one of four states, shown live in the sidebar and filterable: **running / needs attention / done / idle**. When a Thread you aren't watching finishes — or starts waiting on you — a short sound plays.

---

## Remote that feels local

There's a large gap between "it connects to a remote host" and "it feels like it's running here." Most of the engineering in ThinkTerm went into closing it:

- **Typing doesn't wait for the round trip** — predictive local echo puts keystrokes on screen immediately, without waiting for the server to confirm
- **Splits aren't a local-only luxury** — split trees, the second-level tab bar, drag-to-split, and dragging dividers to resize all work in remote sessions, and behave the same as local
- **Sizes don't jitter** — the resize races in remote layouts have been eliminated one by one; dragging a divider no longer causes snap-back, squeeze, or resync storms
- **The scroll wheel is accurate** — in mouse-mode programs like vim and less, the original notch count is preserved, so scrolling isn't too fast or too slow
- **Client state stays yours** — focus and selection are locally authoritative and won't be scrambled by server echo; each client holds its own palette, so changing the theme on one device doesn't change it on another
- **It comes back on its own** — after a network drop it reconnects and restores the session. "Disconnect (server keeps running)" and "delete on the server" are strictly separate actions

The result: a remote Space feels no different from a local one. You usually forget you're connected to another machine.

---

## The same workspace, in a plain terminal

```bash
thinkterm tui [DOMAIN ...]
```

The TUI is a subcommand of the `thinkterm` binary, not a second program — it costs nothing when it isn't running, and exiting detaches from the server **without ending any session**.

It isn't a read-only mirror of the GUI. It's a peer client of the same authoritative server: Space/Project/Thread data is accepted only from server snapshots, and every change waits for the server to acknowledge it before the display updates, so having the GUI and TUI open at once never leaves them disagreeing. It can do what the GUI can — new tabs, splits, resizing, closing panes, reordering the sidebar, copy mode — with full mouse support.

The use case is straightforward: SSH into a machine, or just skip the GUI, and your workspace is still right there. See [thinkterm-tui/README.md](thinkterm-tui/README.md) for keys and configuration.

---

## Agents can talk to each other

Because the mux can address any pane across windows, Threads, and connected clients, one agent can push a task into another agent's pane and read the result back:

```
Agent A ──send-text──► Agent B's terminal input
                       Agent B works, prints a response
Agent A ◄──get-text─── reads the result
```

This uses two CLI primitives that already exist — `thinkterm cli send-text` and `get-text` — and **requires no changes to the agents themselves**. Codex, Claude Code, a shell script, any interactive terminal program can take part.

It has already produced a real collaboration: Claude Code, running in one Thread, noticed that Codex in another Thread had reached a wrong conclusion (Codex's process-list command ran inside an isolated PID namespace and found no mux server). Claude sent a correction into Codex's pane; Codex re-investigated, corrected itself, and printed a response; Claude read it back and reported to the user.

Today this works by injecting terminal input. A more structured task protocol is being designed — see [Agent Collaboration](docs/thinkterm/agent-collaboration.md).

---

## Features

**Remote files** — Browse remote directories over SFTP, with uploads, downloads, recursive transfers, conflict handling, and retry on failure. Files can be dropped straight onto a remote terminal to upload. Idle connections close after a configurable timeout.

**SSH host book** — Your own host list, alongside a read-only view of `~/.ssh/config`. Each host can connect over plain SSH, **Mosh**, or **ThinkTerm Connect (persistent mux)**. Saved passwords are encrypted at rest with AES-256-GCM, with the key stored separately (mode 0600) — that protects against sync, backups, and someone glancing at the file; anyone holding both the key and the ciphertext can still decrypt, which is the known trade-off of a key-on-disk scheme versus the system keychain.

**Content sidebar** — Three panels: *Files* (tree, syntax-highlighted preview, fuzzy search, drag and drop, Open With), *Notes* (a built-in Markdown editor bound to an Obsidian-compatible Vault, with tables, code highlighting, spell check, remote images, and autosave), and *Snippets* (a command snippet library).

**The terminal itself** — WezTerm's GPU rendering, ligatures, color emoji, true color, hyperlinks, copy mode, inline images, and shell integration.

**Interface** — A native settings window, per-area font sizes, and a theme that follows the system or is pinned light or dark. Available in **English, 简体中文, 日本語, and Français**, switching instantly.

---

## Command line

```
thinkterm start              Start the GUI (alias -e)
thinkterm tui [DOMAIN ...]   Open the ThinkTerm interface in the current terminal
thinkterm connect <name>     Connect to a ThinkTerm multiplexer
thinkterm ssh / serial       SSH session / serial port
thinkterm cli <subcommand>   Interact with the mux server
thinkterm imgcat <file>      Print an image to the terminal
```

Run `thinkterm --help` for the full list.

---

## Configuration and data

ThinkTerm uses a WezTerm-compatible Lua config on its own paths, and **never reads or writes your existing WezTerm config**. It takes the first of `$THINKTERM_CONFIG_FILE`, `~/.config/thinkterm/thinkterm.lua`, `~/.config/thinkterm/wezterm.lua`, `~/.thinkterm.lua`. The options themselves match WezTerm — see the [WezTerm config reference](https://wezterm.org/config/files.html).

Settings → Compatibility can load an existing WezTerm config and import chosen fields one by one. That's a one-time copy; the two never share live state.

Workspace data lives in `~/Library/Application Support/ThinkTerm/` (`~/.local/share/ThinkTerm/` on Linux): the Space/Project/Thread structure, the SSH host book (passwords as ciphertext), snippets, and the encryption key `secret.key`. A Notes Vault is a directory you choose; its contents are ordinary Markdown files, and ThinkTerm only records the binding.

---

## Roadmap

**Mobile clients** (design stage, not yet implemented) — iOS and Android as pure mux clients: sessions are always held by the desktop or server, so the phone app being killed by the OS never ends a session. A shared Rust core bridged to native UI through UniFFI, terminal rendering via wgpu, QR pairing with per-device mTLS, and private keys held in Secure Enclave / Keystore. Not planned: a local shell on the phone, a mobile mux daemon, or promises of a permanent background connection.

Groundwork has been landing: the mux protocol now supports moving panes between stacks, per-client palette state, and a **session tree owned by the server**. `thinkterm tui` is the first implementation of this thin-client architecture.

**Structured agent collaboration** — see above.

---

## Privacy

ThinkTerm collects no usage data. Its only outbound request is an update check: once every 24 hours it asks the GitHub Releases API whether a newer version exists, and you can turn that off with `check_for_updates = false`. Scrollback lives in memory only.

---

## Credits and license

ThinkTerm is built on [WezTerm](https://github.com/wezterm/wezterm), written in Rust by [@wez](https://github.com/wez/). Terminal emulation, font rendering, GPU drawing, the multiplexer protocol, and SSH transport all come from that project. Thank you.

ThinkTerm is released under **GPL-3.0** — see [LICENSE.md](LICENSE.md). Code originating from WezTerm remains under its original MIT license, preserved verbatim in [LICENSE-MIT](LICENSE-MIT). Bundled fonts and other third-party components are listed in [licenses/README.md](licenses/README.md).

Icons from [Lucide](https://github.com/lucide-icons/lucide), [Simple Icons](https://github.com/simple-icons/simple-icons), and material-icon-theme. Agent detection manifests from [herdr](https://github.com/herdrdev/herdr), under the Apache License 2.0. Full third-party attributions are in [NOTICE](NOTICE).

Contributing: see [CONTRIBUTING.md](CONTRIBUTING.md).
