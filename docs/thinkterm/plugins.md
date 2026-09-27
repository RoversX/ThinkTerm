# Plugins

**Status:** Version 1. A plugin provides data and functions: it answers
calls, and tells the clients that watch it when something changed. A
plugin's interface belongs in the sidebar, and installed plugins have none
there yet. The desktop and the browser list them in Settings › Sidebar &
Plugins, under the panels, and show nothing else of them; a plugin cannot
show anything on its own. `thinkterm plugin call` calls one from the
command line.

Snippets is a built-in plugin that provides a panel: its switch is that
panel's, among the panels in Settings › Sidebar & Plugins, and its row in
the plugin list says which panel it provides.

A plugin runs in `thinkterm-plugin-server`, a small process separate from
the GUI and the mux. The desktop, a browser and the `thinkterm` command line
are all clients of that process, and none of them holds a plugin's data or
logic. Snippets is the first plugin. It is built into the server and uses the
same interface as an installed plugin.

```
 desktop ─────────────────────────┐
 browser ── mux (PluginFrame) ────┼──► thinkterm-plugin-server ──► built-in plugins (in process)
 thinkterm plugin … ──────────────┘          │
                                             └──► installed plugins (one process each, stdio)
```

- The mux only carries a browser's frames to the server. It does not read
  them, and it keeps no plugin state.
- A client starts the server when it first needs a plugin. The server exits
  30 seconds after the last client disconnects, and stops its plugin
  processes when it does.
- One server runs per user on each machine, and per build profile: a debug
  build never talks to a release build's server.
- The desktop and the command line use the server on their own machine. A
  browser uses the one on the machine whose mux served its page. On a
  headless host that is the server the server packages install there, with
  that host's own snippets and plugins.

## Installing a plugin

A plugin is a directory that holds a `plugin.toml` and the program it runs.
Put the directory under the plugins directory:

| System  | Plugins directory                                 |
|---------|---------------------------------------------------|
| macOS   | `~/Library/Application Support/ThinkTerm/plugins` |
| Linux   | `~/.local/share/ThinkTerm/plugins`                |
| Windows | `%APPDATA%\ThinkTerm\plugins`                     |

`thinkterm plugin dir` prints the directory. There is no install step and no
restart. ThinkTerm finds a new plugin the next time it looks: when Settings ›
Sidebar opens, or when you run `thinkterm plugin list`. Deleting the
directory uninstalls the plugin.

A plugin's directory can be a symbolic link. That is the easiest way to work
on one: link your source directory in, and rebuild. ThinkTerm restarts the
plugin the next time it is used after its program changes.

## `plugin.toml`

```toml
id = "text-tools"            # required: a-z, 0-9 and "-", at most 64 characters
name = "Text Tools"          # required
version = "0.1.0"            # required; shown, not compared
description = "Decodes text, and makes UUIDs."
api = 1                      # required: the plugin API version it speaks

# Optional. The systems it runs on: "macos", "linux", "windows". Default: all.
platforms = ["macos", "linux", "windows"]

[run]
# Found beside plugin.toml first, then on PATH. On Windows, ".exe" is
# tried too.
program = "thinkterm-plugin-example"
args = []

# Optional overrides for one system: [run.macos], [run.linux], [run.windows].
[run.windows]
program = "thinkterm-plugin-example.exe"

# Optional translations, by language tag ("zh-CN") or language ("zh").
[locales.zh-CN]
name = "文本工具"
description = "解码文字，生成 UUID。"
```

The id `plugins` is reserved, and an installed plugin cannot use a built-in
plugin's id. If two installed plugins share an id, the one whose directory
sorts first is used, and the other is shown as invalid.

## What a plugin can do

- **Calls.** A client can send the plugin any JSON value and receive any JSON
  value back. This is how Snippets serves its panel.
  `thinkterm plugin call <id> <json>` sends a call from the command line.
- **Events.** During a call, a plugin can make the caller a watcher and send
  events to its watchers. Snippets uses this to tell every panel on show that
  the snippets changed.

That is all. A plugin has no commands and sends no notifications: nothing of
a plugin shows outside the sidebar.

## The protocol

ThinkTerm starts an installed plugin's program when the plugin is first
needed. It talks to the program over standard input and output. Each message
is one line of JSON, UTF-8, ending in `\n`, and no longer than 16 MiB.
Anything the program writes to standard error goes to the plugin server's log
(`plugins-log` in the runtime directory), a line at a time under the plugin's
id. Past 4 MiB from all plugins together, the rest is dropped until the server
starts again.

The program starts with these environment variables, in its own directory:

| Variable                | Value                                              |
|-------------------------|----------------------------------------------------|
| `THINKTERM_PLUGIN_ID`   | its id                                             |
| `THINKTERM_PLUGIN_DIR`  | its directory                                      |
| `THINKTERM_PLUGIN_DATA` | a directory for its own files, created before start |
| `THINKTERM_PLUGIN_API`  | `1`                                                |

`WEZTERM_PANE` and `WEZTERM_UNIX_SOCKET` are not passed on: the pane or mux
the plugin server happened to be started from says nothing about a plugin.

The program's first line must be:

```json
{"type":"ready","api":1}
```

ThinkTerm then sends it:

```json
{"type":"call","id":1,"body":{"op":"decode","text":"aGk="}}
{"type":"call","id":2,"body":{"op":"frobnicate"}}
{"type":"stop"}
```

The program answers each `call` by its `id`, in any order:

```json
{"type":"ok","id":1,"body":{"kind":"Base64","text":"hi"}}
{"type":"error","id":2,"message":"unknown op"}
```

An answer may add `"watch": true` to make the caller a watcher. The program
can also send an event at any time:

```json
{"type":"event","body":{"event":"changed"}}
```

After `stop`, standard input closes. The program must exit then, or when
standard input closes for any other reason. A program that is still running
2 seconds later is killed. No new program starts under the same id until the
old one is gone, since the two would share the data directory; calls made
meanwhile wait for it. ThinkTerm ignores lines it cannot read and message
types it does not know, and so should the program.

## Rust SDK

`thinkterm-plugin-sdk` implements the protocol. A plugin implements `Plugin`
and hands it to `run`:

```rust
use serde_json::{json, Value};
use thinkterm_plugin_sdk::{Cx, Plugin};

struct Hello;

impl Plugin for Hello {
    fn call(&mut self, body: Value, _cx: &mut Cx) -> anyhow::Result<Value> {
        match body["op"].as_str() {
            Some("greet") => Ok(json!("hello")),
            _ => anyhow::bail!("no such call"),
        }
    }
}

fn main() -> std::io::Result<()> {
    thinkterm_plugin_sdk::run(Hello)
}
```

- `run` returns when ThinkTerm stops the plugin, so `main` can clean up.
  Standard output belongs to the protocol: print with `eprintln!`.
- `run_with` passes the plugin an `Emitter`, which can send events from
  other threads.
- Snippets implements the same trait. It runs inside the server, and it can
  also run out of process: `thinkterm-plugin-server --serve-plugin snippets`.
  The server's tests use that to exercise the protocol end to end.

`thinkterm-plugin-example` is a complete plugin: a manifest and two calls.
To install it and try it:

```sh
cargo build --release -p thinkterm-plugin-example
dir="$(thinkterm plugin dir)/text-tools"
mkdir -p "$dir"
cp thinkterm-plugin-example/plugin.toml target/release/thinkterm-plugin-example "$dir/"
thinkterm plugin list
thinkterm plugin call text-tools '{"op":"decode","text":"aGk="}'
```

## Lifecycle

Each plugin has a state. Clients show it, and the plugin list in Settings ›
Sidebar shows the reason for a failed state.

| State         | Meaning                                                    | Leaves it when                                  |
|---------------|------------------------------------------------------------|-------------------------------------------------|
| `off`         | Turned off                                                 | turned on                                       |
| `idle`        | On, and not running. Built-in plugins are always `idle`.   | it is used                                      |
| `starting`    | The program started and has not said `ready` yet           | `ready`, exit, or 10 seconds pass               |
| `running`     | Ready, and answering                                       | exit, turned off, reload, or the server exits   |
| `crashed`     | Exited unexpectedly; the reason is kept                    | it is used again, which restarts it             |
| `failed`      | Crashed 3 times within a minute, or cannot be started      | reload, turned off and on, or its files change  |
| `invalid`     | `plugin.toml` cannot be used; the reason is kept           | the manifest is fixed                           |
| `unsupported` | `platforms` does not include this system                   | the manifest changes                            |

Nothing starts a plugin except a client that uses it: a call from the
command line, or a panel on show. Calls that are waiting when a plugin
stops are answered with an error. Nothing is retried on the plugin's behalf.

## Reload

| What                         | How it happens                                                                 |
|------------------------------|--------------------------------------------------------------------------------|
| Find new and removed plugins | Each `list`: opening Settings › Sidebar & Plugins, `thinkterm plugin list`     |
| A changed `plugin.toml`      | Read again at the next `list` or use. A running plugin is stopped first.       |
| A rebuilt program            | Checked before each use. A running plugin is stopped and started again.        |
| One plugin, by hand          | `thinkterm plugin reload <id>`: stop, read again, and clear `crashed`/`failed` |
| Everything, by hand          | Reload in Settings › Sidebar & Plugins, or `thinkterm plugin reload`           |
| The on/off switches          | `plugins.json` is read again when another server changes it                    |
| A newer ThinkTerm            | Its clients replace the older server, and the older server stops its plugins   |

A client reconnects after the server restarts, and then asks for everything
it shows again: the plugin list, a panel's rows, and so on. A client never
treats what it last saw as current once the connection is gone.

## State

| What                        | Where                                  | Owner                          |
|-----------------------------|----------------------------------------|--------------------------------|
| On/off switches             | `<data>/plugins.json`                  | the plugin server              |
| A plugin's own files        | `<data>/plugin-data/<id>/`             | that plugin                    |
| Snippets                    | `<data>/snippets.json`                 | the Snippets plugin            |
| Plugin list, states, calls  | memory                                 | the plugin server              |
| What a client shows         | memory, per window or page             | that client                    |

`<data>` is the parent of the plugins directory. A plugin that is not listed
in `plugins.json` is on. Turning a plugin off is written there, and the entry
stays when the plugin is removed, so reinstalling it keeps it off.

### What a client keeps

Every client follows the same rules. The desktop keeps one connection for all
of its windows, and a browser page keeps one through the mux.

- **The plugin list** is asked for when the client connects, and again when
  the server says it changed. Only one request is in flight at a time, and
  changes that arrive meanwhile are folded into one more request. The last
  list stays on show while a new one is on its way.
- **A switch** shows the new position while the change is on its way. It
  goes back if the server refuses, and the reason is shown.
- **A call** waits up to 10 seconds for its answer.
- **When the connection is lost**, a client shows that plugins are
  unavailable, and asks again once it reconnects.
- **The desktop** remembers whether Snippets was on the last time it heard,
  so the right sidebar does not flicker at startup. The server's answer wins
  when it arrives.

## Command line

```
thinkterm plugin list                        plugins and their states
thinkterm plugin enable <id>
thinkterm plugin disable <id>
thinkterm plugin reload [<id>]
thinkterm plugin call <id> <json>
thinkterm plugin dir                         where plugins are installed
```

## Security

A plugin is a program you installed, and it runs as you, with your
permissions. `platforms` in the manifest only decides where it runs; it does
not restrict what it can do. ThinkTerm shows nothing for a plugin, but that
does not stop the program itself from, say, showing a system notification or
running `thinkterm cli`: only a sandbox would. ThinkTerm never starts a
plugin on its own: it runs only when a client uses it. Nothing downloads or
installs plugins for you.

## Limits

| Limit                                    | Value      |
|------------------------------------------|------------|
| Message size, either direction           | 16 MiB     |
| Calls waiting on one plugin              | 64         |
| Time to say `ready`                      | 10 seconds |
| Time to exit after `stop`                | 2 seconds  |
| Crashes before `failed`                  | 3 in 60 s  |
| Standard error logged, all plugins       | 4 MiB      |
