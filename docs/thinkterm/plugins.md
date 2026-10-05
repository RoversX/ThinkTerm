# Plugins

**Status:** Version 2. A plugin provides data and functions: it answers
calls, and tells the clients that watch it when something changed. It can
also add a panel to the right sidebar, which it draws by hand: rectangles,
text, lines and filled areas, the places a click reaches it, and fields to
type in; and it can be given the keyboard (see [Panels](#panels)). The
desktop and the browser offer the panel in the sidebar's selector, list
the plugin in Settings › Sidebar & Plugins, and show nothing else of it;
nothing of a plugin shows outside the sidebar. `thinkterm plugin call`
calls one from the command line.

A plugin you install does not run until you let it: see [New
plugins](#new-plugins).

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
  processes when it does. While a plugin runs always, the desktop and the
  mux each keep a connection to it for as long as they run, so it stays up
  as long as ThinkTerm runs on the machine (see [How long a plugin
  runs](#how-long-a-plugin-runs)).
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

`thinkterm plugin dir` prints the directory. There is no restart. ThinkTerm
finds a new plugin the next time it looks: when Settings › Sidebar opens, or
when you run `thinkterm plugin list`. It lists it as new, and runs nothing
of it until you let it: press Allow beside it in Settings › Sidebar &
Plugins, on the desktop or in the browser, or run `thinkterm plugin enable
<id>`. Deleting the directory uninstalls the plugin.

### New plugins

ThinkTerm starts a plugin's program only once you let it run, from the
directory it is in then. Until you do it is `new`: its panel is not
offered, calls to it are refused, and one whose manifest says it runs
always is not started. Allowing it is turning it on; `plugins.json` keeps
where you let it run from, with links followed.

It is new again when it is anywhere else -- moved, a link pointed at
another copy, another plugin under its id, its manifest giving it another
id -- and a running one is stopped. A plugin's own updates in place, and
rebuilding one you work on through a link, ask nothing again. A new plugin
is written in `plugins.json` as off, so that a ThinkTerm older than this,
sharing the file, does not run it either; one turned off so only because
it was somewhere else is on again once it is back where you let it run
from. A plugin you turned off yourself stays off.

A plugin's directory can be a symbolic link. That is the easiest way to work
on one: link your source directory in, and rebuild. ThinkTerm restarts the
plugin the next time it is used after its program changes.

## `plugin.toml`

```toml
id = "text-tools"            # required: a-z, 0-9 and "-", at most 64 characters
name = "Text Tools"          # required
version = "0.1.0"            # required; shown, not compared
description = "Decodes text, and makes UUIDs."
api = 1                      # required: the plugin API version it speaks, 1 or 2

# Optional. The systems it runs on: "macos", "linux", "windows". Default: all.
platforms = ["macos", "linux", "windows"]

[run]
# Found beside plugin.toml first, then on PATH. On Windows, ".exe" is
# tried too.
program = "thinkterm-plugin-example"
args = []
# Optional. How long the program runs while nothing uses it: "always",
# "briefly" (the default) or "never". The user can choose otherwise.
background = "briefly"

# Optional overrides for one system: [run.macos], [run.linux], [run.windows].
[run.windows]
program = "thinkterm-plugin-example.exe"

# Optional translations, by language tag ("zh-CN") or language ("zh").
[locales.zh-CN]
name = "文本工具"
description = "解码文字，生成 UUID。"
```

A plugin with a panel says so, and speaks API 2:

```toml
api = 2

# The panel it adds to the right sidebar, offered in the selector under the
# plugin's name. The icon is one of: puzzle, activity, bug, calendar,
# chart-candlestick, chart-line, cpu, database, gauge, git-branch,
# git-compare, list-todo. Another shows as the puzzle piece.
[panel]
icon = "git-compare"
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

- **A panel.** A plugin whose manifest has a `[panel]` draws one in the
  right sidebar, on the desktop and in the browser alike. See
  [Panels](#panels).
- **Typing and keys.** A panel can have fields to type in, and be sent the
  keys it takes while the user gives it the keyboard. See [Fields and the
  keyboard](#fields-and-the-keyboard).
- **Another machine's files.** When the terminal beside its panel runs on
  another machine reached over SSH, a plugin has ThinkTerm run programs and
  read files there. See [A terminal on another
  machine](#a-terminal-on-another-machine).
- **Running in the background.** A plugin whose manifest says so runs for as
  long as ThinkTerm does. See [How long a plugin
  runs](#how-long-a-plugin-runs).

That is all. A plugin has no commands and sends no notifications: nothing of
a plugin shows outside the sidebar.

## Panels

A panel is drawn by hand. The plugin sends a *frame*: a list of items,
painted in order, in the panel's own units -- CSS pixels, points on a Mac --
from its top left corner. There are no widgets: a button is a rounded
rectangle, its label, and a hit over both. The one exception is a field,
which the client draws and edits itself. Every ThinkTerm client paints a
frame with the same *player* (`thinkterm-plugin-panel`), with its own
renderer and fonts and in ThinkTerm's colours, so a panel looks alike on the
desktop and in a browser without the plugin knowing which it is on.

The player never waits for the plugin. Hovering and scrolling happen in it
at once; a click is sent to the plugin, which answers with a new frame. A
long list is sent a page of rows at a time, as the player asks for them, so
a list of ten thousand rows costs the few dozen that show.

```
 plugin ──frame, rows──► plugin server ──► desktop:  player ─► sidebar quads
   ▲                          │       └──► browser:  player (wasm) ─► canvas
   └─────env, input, rows wanted─────┘
```

### Items

Every item is a JSON object with one key, its kind. Numbers are in panel
units; fields shown with a default may be left out. An item a client cannot
read -- a number that is `null`, a kind it does not know -- is left out, and
the rest are drawn.

| Item     | Fields                                                                                                   |
|----------|----------------------------------------------------------------------------------------------------------|
| `rect`   | `x`, `y`, `w`, `h`; `fill`; `radius` (0); `border` (a one-unit line inside the edge)                     |
| `text`   | `x`, `y`, `w`, `h`, `text`; `color` (`text`); `font` (`ui` or `mono`); `size` (`small`, `body`, `title`: the `ui` font's); `bold`; `align` (`left`, `center`, `right`) |
| `line`   | `points` (x, y, x, y ...), `color`; `width` (1)                                                          |
| `area`   | `points` (left to right), `base`, `color`; `fade` (thins to nothing at `base`): the fill under a chart   |
| `hit`    | `id`, `x`, `y`, `w`, `h`; `hover` (a colour to tint it with while the pointer is over it); `radius` (0, the tint's corners); `cursor` (`pointer`, `arrow`) |
| `scroll` | `id`, `x`, `y`, `w`, `h`, `height` (of what scrolls), `items` (in its own units); `top`                  |
| `list`   | `id`, `x`, `y`, `w`, `h`, `count`, `row` (a row's height), `key`; `version` (0); `width`, `fixed` (0); `top` |
| `field`  | `id`, `x`, `y`, `w`, `h`; `kind` (`line`, `lines`, `secret`); `value`, `seq` (0); `placeholder`; `font` (`ui` or `mono`); `max` (0: as many as a field holds); `focus`; `icon` (`search`, `plus` or `pencil`); `clear` |

- Text is one line, centred in its box's height and cut to its width: with
  an ellipsis in the `ui` font, at the edge in the `mono` font, whose
  columns a plugin lines up itself -- the environment says how wide one is,
  and a character most scripts write in one column takes two in CJK.
- A hit's tint is painted where the hit is in the list: put the hit before
  what it should be under. Where hits overlap, the last one answers: a row
  can take clicks across its whole width and be tinted only in a rounded
  part of it.
- A `scroll` scrolls without the plugin; a `list` asks for its rows as they
  come near the view (`rows` below) and forgets the far ones. A new `key`
  says the rows are other ones: the old are dropped and the list starts at
  its top. A new `version` says the same rows look different -- one was
  picked, a price moved: the rows on show are asked for again and stay until
  the new ones come. So they are when the panel's size or fonts change,
  which rows are drawn for, and when the plugin starts again. `top`,
  `{"to": 12, "seq": 1}`, scrolls a list to a row (a scroll area to a
  height) once for each new `seq`; with `"through": 13` it scrolls only as
  far as it takes to show from `to` to there, which keeps a row picked
  with the keys in view.
- A list whose rows are `width` wide, wider than it, scrolls sideways too:
  with a trackpad, or the wheel with Shift. What starts within `fixed` of a
  row's left edge stays put -- line numbers beside lines of code -- and what
  starts past it moves, cut where the fixed part ends.
- Scroll areas and lists do not go inside one another, or inside a row.
  Nor does a field go in either; of two fields with one id the first is
  kept. See [Fields and the keyboard](#fields-and-the-keyboard).

Colours are ThinkTerm's own, by name, which follow its theme: `text`,
`text-muted`, `text-faint`, `bg`, `bg-raised`, `bg-hover`, `bg-selected`,
`border`, `accent`, `on-accent`, `positive`, `negative`, `warning`,
`positive-bg`, `negative-bg`, `positive-bg-strong`, `negative-bg-strong`.
A fixed colour is written `#rrggbb` or `#rrggbbaa`. A name a ThinkTerm does
not know draws as `text`.

### Messages

ThinkTerm tells the plugin about a panel, one of its *views*: the same
plugin can be on show in two windows and a browser at once, each a view with
its own number and size.

```json
{"type":"open","view":3,"env":{"width":320,"height":640,"scale":2,"dark":true,
  "small":{"size":11,"line":15},"body":{"size":13,"line":17},"title":{"size":15,"line":20},
  "mono":{"size":12,"line":16,"advance":7.2},"locale":"en-US","cwd":"/home/user/project"}}
{"type":"env","view":3,"env":{...}}
{"type":"input","view":3,"input":{"click":{"id":"file:4","x":12,"y":6,"button":"left","count":1,"mods":{"shift":true}}}}
{"type":"rows","view":3,"wanted":{"list":"diff","key":"file-4","version":1,"layout":2,"from":64,"to":96}}
{"type":"close","view":3}
```

`env` is the panel's size, how large ThinkTerm's text is there (a `line` is
how tall one is), whether the theme is dark, the language ThinkTerm speaks
(`locale`), `cwd`: the directory of the terminal beside the panel -- the
pane in focus in its window -- when that terminal runs on the machine the
plugin runs on, and left out when there is none or it does not; `remote`
when it runs on another machine instead (see [A terminal on another
machine](#a-terminal-on-another-machine)); `can_extend` (see [The
extended view](#the-extended-view)); and `features`, what the client does
besides drawing and sending clicks: `fields` when it shows fields, `keys`
when it gives a panel the keyboard on the user's key (see [Fields and the
keyboard](#fields-and-the-keyboard)). It comes with `open` and again when any of it
changes -- no more than ten times a second while a window is resized, the
last always -- and the `rows` asked for after it are drawn for it. A click's
`x` and `y` are within the hit. A `version` or a `layout` of 0, and a
`can_extend` that is false, are left out.

The plugin draws a view whenever it likes, and answers `rows`:

```json
{"type":"frame","view":3,"frame":{"items":[{"text":{"x":12,"y":10,"w":200,"h":20,"text":"Changes","size":"title","bold":true}}]}}
{"type":"rows","view":3,"rows":{"list":"diff","key":"file-4","version":1,"layout":2,"from":64,"rows":[[{"rect":{"x":0,"y":0,"w":320,"h":18,"fill":"positive-bg"}}]]}}
```

A row's items are in its own units: from the list's left edge and the row's
top. The answer repeats what was asked -- `list`, `key`, `version`,
`layout`, `from` -- and one that is not what the list waits for any more is
dropped. Rows asked for and left out are drawn empty.

A client is sent one frame at a time: while it has not taken in the last,
only the newest the plugin drew is kept, so a plugin drawing faster than a
client keeps up never queues frames. Rows are sent only as they were asked
for. A frame or rows too long to go on to a client -- within a few bytes of
the longest message -- are dropped, and the plugin server's log says so. A
panel is closed when it goes off show, when its client goes, and when the
plugin stops -- the client opens it again after a restart or a reload, and
says why when the plugin cannot be started.

### The extended view

A panel with more to show than a sidebar has room for can ask for its
*extended view*: a wide area left of the sidebar, as wide as the user drags
it -- remembered for the plugin -- within what the terminal leaves, which is
never less than a fifth of the window. The panel's frame asks with
`"extend": true`, and keeps asking in every frame it wants it for; a frame
that does not ask lets it go.

```json
{"type":"frame","view":3,"frame":{"items":[...],"extend":true}}
```

The client opens it as a view of its own -- drawn, clicked, scrolled and
asked for rows as the panel is, with an `env` of its own size -- whose
`open` names the panel it extends:

```json
{"type":"open","view":4,"env":{"width":560,"height":900,...},"extends":3}
```

It is closed before its panel is, whenever the panel closes, and its own
frames ask for nothing. A panel's `env` says `"can_extend":true` when the
client has room for an extended view and shows them; where it does not -- a
window too narrow, a phone -- a plugin fits what it would show there into
the panel itself.

Every extended view has ThinkTerm's close button at its top left, where the
file preview has its own, drawn over whatever the plugin draws. Its `env`
says where, in the view's units -- `"close":{"x":5,"y":2,"size":22}` -- for
the plugin to keep that square empty and line its first row up with it.
Pressed, it closes the view at once, whatever the plugin does, and sends it

```json
{"type":"input","view":4,"input":"close"}
```

for the plugin to stop asking: the panel is given no other extended view
until its frames have stopped asking for one, and ask again.

### Fields and the keyboard

A `field` is a box the user types in. The client draws it -- the box, the
text, the caret and what is selected, in ThinkTerm's font and colours --
and edits it without waiting for the plugin, input methods included. What
it holds is the client's while it shows: `value` is taken when the field
first comes, and again for each new `seq`, so the frames drawn while the
user types do not undo it. A plugin empties a field after a submit by
sending `""` with the next `seq`. `lines` takes several lines, and Return
starts a new one; `secret` shows dots, cannot be copied or cut out of, and
is sent only when submitted. A field on one line is round at its ends, as
the sidebar's search is; `icon` puts one before its text -- `search`,
`plus` or `pencil` -- and `"clear": true` a button at its end that empties
it, which the client draws and does itself.

```json
{"type":"frame","view":3,"frame":{"items":[{"field":{"id":"add","x":12,"y":40,"w":296,"h":28,"placeholder":"Add a symbol","max":24,"icon":"plus"}}],"keys":["ArrowUp","ArrowDown"]}}
{"type":"input","view":3,"input":{"focus":{"id":"add"}}}
{"type":"input","view":3,"input":{"text":{"id":"add","text":"TS"}}}
{"type":"input","view":3,"input":{"submit":{"id":"add","text":"TSM"}}}
{"type":"input","view":3,"input":{"key":{"key":"ArrowDown","mods":{}}}}
{"type":"input","view":3,"input":"blur"}
```

`text` comes as what the field holds changes -- typed, pasted, cut, never
the half of a word an input method is still composing -- and `submit` when
the user presses Return, Command-Return (Ctrl+Return off a Mac) in a field
of `lines`. Both say the `seq` of the text it was typed over, when that is
not 0: one that is not the plugin's latest was typed before its text came,
and the field holds the plugin's, so the plugin keeps that (`FieldText`
does). `focus` says the panel has the keyboard, in the field `id` or,
without one, in none of them; `blur` that it went.

The keyboard is the terminal's until the user gives it to a panel:

- **Pressing in a field** gives it the keyboard there. A press anywhere
  else in a panel leaves the keyboard where it is: a click is a click.
- **The user's key for the panel.** The desktop has an action for it,
  which you bind in your configuration; it opens the right sidebar on the
  panel, and gives it the keyboard in its first field -- or, without
  fields, in none, outlined -- and pressed again gives it back:

  ```lua
  config.keys = {
    { key = 'S', mods = 'CMD|SHIFT', action = wezterm.action.FocusPluginPanel 'stocks' },
  }
  ```

  A browser offers no such key: there a panel has the keyboard through
  its fields.

While a panel has it, the keys its frame names in `keys` go to the plugin
as `key`, named as a browser names them (`KeyboardEvent.key`: `ArrowDown`,
`Enter`, `j`), with whether Shift was held. The field with the keyboard
keeps the keys it edits with; any other -- and any pressed with Ctrl, Alt
or Command, which are never a panel's -- is left to the app's own keys,
and never reaches the terminal. Escape gives the keyboard back to the
terminal, and Tab moves it between the panel's fields; neither is sent.

The user gives the keyboard back with Escape or a press on the terminal.
The plugin can move it between its own fields -- `focus`, once for each
new seq -- and let go of it with the frame's `release`, once for each new
seq, but only while its panel has it already: it can never take it from
the terminal, nor give it to the terminal. When the panel loses it without
the user -- a `release`, the field it is in going from the frame, the
panel going off show, its plugin stopped or started again -- what the user
types next was meant for the panel, and goes nowhere until they press
Escape or press somewhere. What a stopped plugin, or one starting again,
last drew takes no typing: a press in its field gives it no keyboard.

### A terminal on another machine

When the terminal beside a panel runs on another machine reached over SSH --
a host from the host list, or a mux reached through SSH -- the desktop tells
the plugin which, and the terminal's directory there, instead of `cwd`:

```json
"remote":{"host":"server-a","machine":"5f0e2c9a41d7b3e8","cwd":"/home/user/project"}
```

`host` is the name to show. `machine` stands for the machine and the account
ThinkTerm reaches it as -- two accounts on one host share a `host`, not a
`machine` -- and is what the plugin names when it asks something of it.

The plugin runs on this machine, and its own files and programs are this
machine's. What it wants done there it asks ThinkTerm, which does it over
the connection the Files panel reaches that machine with -- so the same way
for a host with or without a mux:

```json
{"type":"ask","view":3,"id":7,"machine":"5f0e2c9a41d7b3e8","ask":{"op":"run","args":["git","status","--porcelain"],"cwd":"/home/user/project","limit":1048576}}
{"type":"ask","view":3,"id":8,"machine":"5f0e2c9a41d7b3e8","ask":{"op":"read","path":"/home/user/project/notes.txt","limit":262144}}
{"type":"ask","view":3,"id":9,"machine":"5f0e2c9a41d7b3e8","ask":{"op":"stat","path":"/home/user/project/link"}}
```

`id` is the plugin's to pick, and `machine` is the one the panel's env named.
An ask is done only while the terminal beside the panel is still on that
machine; asked for one it has left, it fails. `run` runs a program with its
arguments in `cwd`, under `sh`, with nothing on its input, keeping at most
`limit` bytes of what it prints; `read` reads at most `limit` bytes of a
file; `stat` says what is at a path, a link not followed. Each is answered
once, by its `id`:

```json
{"type":"answer","view":3,"id":7,"answer":{"result":"ran","status":0,"out":"TSBSRUFETUUubWQK","cut":false}}
{"type":"answer","view":3,"id":8,"answer":{"result":"read","bytes":"aGk=","cut":false}}
{"type":"answer","view":3,"id":9,"answer":{"result":"stat","entry":{"kind":"link","len":7,"modified":1790000000,"target":"elsewhere"}}}
{"type":"answer","view":3,"id":7,"answer":{"result":"failed","why":"ThinkTerm is not connected to server-a","connect":true}}
```

Bytes are base64. A `status` of `null` means the program did not end by
itself: it printed more than `limit`, which then says `"cut":true`, and was
stopped, or a signal ended it. One that runs longer than 20 seconds is
stopped too, and the ask fails, as it does when the connection closes while
the program runs. `kind` is `file`, `dir`, `link` or `other`; `entry` is
`null` when nothing is there.

ThinkTerm connects to a machine only once you have let it -- in Files, or
with the Connect button the panel shows in place of what the plugin drew
while it waits. Until then an ask fails with `"connect":true`, and a later
one works. A panel that closes before its asks are answered has them fail.
A panel beside a terminal on the plugin's own machine gets none of this:
its plugin reaches the files itself.

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
| `THINKTERM_PLUGIN_API`  | `2`, the newest this ThinkTerm speaks              |

`WEZTERM_PANE` and `WEZTERM_UNIX_SOCKET` are not passed on: the pane or mux
the plugin server happened to be started from says nothing about a plugin.

The program's first line must be, with the API it speaks -- 1, or 2 for a
plugin with a panel -- and no newer than `THINKTERM_PLUGIN_API`: a ThinkTerm
takes none newer than its own. The Rust SDK says the older of its own and
ThinkTerm's.

```json
{"type":"ready","api":2}
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
  other threads, and have every panel of the plugin drawn again with
  `redraw` once its data changed there.
- A panel is drawn in `draw(&mut self, view: &View, frame: &mut Frame)`,
  called when it opens, when its size changes, after each `input`, and when
  `Cx::redraw` or `Emitter::redraw` asks -- once for what arrives together,
  after all of it is taken in. A frame the same as the last is not sent.
  `rows` answers a list's rows, at once; `closed` says a view went. The
  items are in `thinkterm_plugin_sdk::panel`, with builders:
  `Rect::new(x, y, w, h).fill(Token::BgRaised).radius(6.0)`.
- A field is `Field::new(id, x, y, w, h)`, kept with a `FieldText`: it
  takes what an `Input::Text` or `Input::Submit` says with `heard` --
  unless it was typed over text the plugin has put there since -- and
  `set` or `clear` put text there, which its `field(id, x, y, w, h)` sends
  with the next `seq`. `frame.keys(&["ArrowDown"])` names the keys the panel
  takes, and `view.focus` says where the keyboard is in the view while it
  has it. `Input` will grow: match the inputs a plugin knows, and let the
  rest be.
- A panel asks for its extended view with `frame.extend(true)`, and the
  same `draw` draws that, with `view.extended()` true. `view.panel()` names
  the panel either belongs to, for what the two share; a panel's
  `view.extension` is its extended view while one is on show, and the panel
  is drawn again when it comes and when it goes. A click in either draws
  both again. `Input::Close` in an extended view is its close button:
  `view.env.close` is where that is.
- `Emitter::ask(view, machine, &ask, wait)` asks ThinkTerm to do something
  on the machine the terminal beside panel `view` runs on, when that is
  another -- `machine` from `view.env.remote` -- and waits for the answer.
  It is best asked from a thread of the plugin's own: drawing waits for it.
- Snippets implements the same trait. It runs inside the server, and it can
  also run out of process: `thinkterm-plugin-server --serve-plugin snippets`.
  The server's tests use that to exercise the protocol end to end.

`thinkterm-plugin-example`, in `plugins/example`, is a complete plugin: a
manifest and two calls. To install it and try it:

```sh
cargo build --release -p thinkterm-plugin-example
dir="$(thinkterm plugin dir)/text-tools"
mkdir -p "$dir"
cp plugins/example/plugin.toml target/release/thinkterm-plugin-example "$dir/"
thinkterm plugin list
thinkterm plugin call text-tools '{"op":"decode","text":"aGk="}'
```

Two more, in `plugins/diff` and `plugins/stocks`, show panels, and are
installed the same way:

- `thinkterm-plugin-diff` (`diff`): the changes in the git repository of the
  terminal beside the panel since its last commit, shown the way GitHub
  Desktop shows them -- the files in the panel, and the picked file's lines
  in its extended view, with what changed in each line marked; long lines
  scroll sideways under their line numbers. Where there is no room for an
  extended view, the lines are shown below the files. It runs `git` in that
  directory every two seconds while a panel is on show, less often in a
  repository git takes long over, without the programs a repository's
  configuration can name for it (an fsmonitor, an external diff, a text
  converter). Beside a terminal on another machine, it has git run there
  through ThinkTerm, and does not count the lines of new files, which would
  cost a trip each. While it runs, it keeps what it found in the last few
  directories, and the file picked in each: a panel shown again -- after the
  sidebar went to another of its panels, say -- shows them at once. A field
  above the files filters them by name; with the keyboard in the panel,
  up and down pick the file before or after, and Return in the filter the
  first it leaves.
- `thinkterm-plugin-stocks` (`stocks`): a watchlist of quotes from Yahoo
  Finance with each symbol's day as a line, and a chart of the one picked
  over a range. It fetches on a thread of its own while a panel is on show,
  and keeps its watchlist in its data directory: a symbol typed in the
  field at the panel's top is added to it, and so is one called for,
  `thinkterm plugin call stocks '{"op":"add","symbol":"TSM"}'`; with the
  keyboard in the panel, up and down pick the symbol before or after.
  Yahoo's chart API is not an official one, and may refuse or change.

## Lifecycle

Each plugin has a state. Clients show it, and the plugin list in Settings ›
Sidebar shows the reason for a failed state.

| State         | Meaning                                                    | Leaves it when                                  |
|---------------|------------------------------------------------------------|-------------------------------------------------|
| `new`         | Installed, and never let run from where it is              | turned on, which lets it                        |
| `off`         | Turned off                                                 | turned on                                       |
| `idle`        | On, and not running. Built-in plugins are always `idle`.   | it is used                                      |
| `starting`    | The program started and has not said `ready` yet           | `ready`, exit, or 10 seconds pass               |
| `running`     | Ready, and answering                                       | exit, turned off, reload, unused for long enough, or the server exits |
| `crashed`     | Exited unexpectedly; the reason is kept                    | it is used again, which restarts it             |
| `failed`      | Crashed 3 times within a minute, or cannot be started      | reload, turned off and on, or its files change  |
| `invalid`     | `plugin.toml` cannot be used; the reason is kept           | the manifest is fixed                           |
| `unsupported` | `platforms` does not include this system                   | the manifest changes                            |

A client that uses a plugin starts it: a call from the command line, or a
panel on show. Only a plugin that runs always starts without one. Nothing
starts a `new` plugin. Calls that
are waiting when a plugin stops are answered with an error. Nothing is
retried on the plugin's behalf, but a plugin that runs always is started
again 2 seconds after it stopped by itself, until it is `failed`.

### How long a plugin runs

A plugin is in use while a panel of it is on show anywhere -- a window, a
browser -- a call to it waits for its answer, for 70 seconds at most, or a
client watches it. Once it is not, its program runs on for as long as the
plugin may run unused:

| Background          | Starts                          | Stops                                         |
|---------------------|---------------------------------|-----------------------------------------------|
| `always`            | with ThinkTerm on the machine   | when ThinkTerm on the machine is gone         |
| `briefly` (default) | when it is used                 | 2 minutes after it was last used              |
| `never`             | when it is used                 | 10 seconds after it was last used             |

The manifest says which the plugin needs (`[run] background`); the user can
choose another in Settings › Sidebar & Plugins, on the desktop and in the
browser, which is kept in `plugins.json` and wins. Each machine keeps its
own: a plugin can run always on a server and briefly on a laptop.

"ThinkTerm on the machine" is the desktop, or the mux server: while some
plugin runs always -- which the plugin server writes in
`<data>/plugins-always`, and removes when none does -- each keeps a
connection to it while it runs, saying it is ThinkTerm running there
(`keep`). Each looks at that file every 2 seconds, so a plugin chosen to run
always starts soon after. A plugin installed while the server was not
running is not in the file yet: when a manifest in the plugins directory
says `background = "always"` and the server has not looked at it as it is,
the desktop and the mux start the server to look, and it runs the plugin if
it is on. While one is connected, the plugins that run always do; once none
is, they run as briefly ones do, and when nothing is connected at all the
server exits 30 seconds later and stops every plugin. On a host with no
desktop, that is the mux server.

## Reload

| What                         | How it happens                                                                       |
|------------------------------|--------------------------------------------------------------------------------------|
| Find new and removed plugins | Each `list`: opening Settings › Sidebar & Plugins, `thinkterm plugin list`; a `keep` |
| A changed `plugin.toml`      | Read again at the next `list` or use. A running plugin is stopped first.             |
| A rebuilt program            | Checked before each use. A running plugin is stopped and started again.              |
| One plugin, by hand          | `thinkterm plugin reload <id>`: stop, read again, and clear `crashed`/`failed`       |
| Everything, by hand          | Reload in Settings › Sidebar & Plugins, or `thinkterm plugin reload`                 |
| The on/off switches          | `plugins.json` is read again when another server changes it                          |
| A newer ThinkTerm            | Its clients replace the older server, and the older server stops its plugins         |

A client reconnects after the server restarts, and then asks for everything
it shows again: the plugin list, a panel's rows, and so on. A client never
treats what it last saw as current once the connection is gone.

## State

| What                        | Where                                                | Owner               |
|-----------------------------|------------------------------------------------------|---------------------|
| On/off switches, background | `<data>/plugins.json`                                | the plugin server   |
| The plugins that run always | `<data>/plugins-always` (`-debug` for a debug build) | the plugin server   |
| A plugin's own files        | `<data>/plugin-data/<id>/`                           | that plugin         |
| Snippets                    | `<data>/snippets.json`                               | the Snippets plugin |
| Plugin list, states, calls  | memory                                               | the plugin server   |
| What a client shows         | memory, per window or page                           | that client         |

`<data>` is the parent of the plugins directory. An installed plugin runs
only once `plugins.json` says you let it run from where it is (`allowed`,
its directory); one that is not listed is new, and is written in as off.
Turning a plugin on, which lets it, turning it off, and choosing how long it
runs unused are written there, and the entry stays when the plugin is
removed, so reinstalling it where it was keeps it as it was. A built-in
plugin is ThinkTerm's own: it is on unless turned off. Builds share the
file: what one does not know, a newer one's, it keeps as written.

### What a client keeps

Every client follows the same rules. The desktop keeps one connection for all
of its windows, and a browser page keeps one through the mux.

- **The plugin list** is asked for when the client connects, and again when
  the server says it changed. Only one request is in flight at a time, and
  changes that arrive meanwhile are folded into one more request. The last
  list stays on show while a new one is on its way.
- **A switch**, or a choice of how long a plugin runs unused, shows the new
  position while the change is on its way. It goes back if the server
  refuses, and the reason is shown.
- **A call** waits up to 10 seconds for its answer.
- **When the connection is lost**, a client shows that plugins are
  unavailable, and asks again once it reconnects.
- **The desktop** remembers whether Snippets was on the last time it heard,
  so the right sidebar does not flicker at startup. The server's answer wins
  when it arrives. It remembers the plugins' panels the same way, and asks
  for the list at startup only when some plugin is installed.
- **A plugin's panel** is opened when it comes on show and closed when it
  goes; the plugin then runs on as its background says. The client opens it
  again after the plugin restarts or the connection comes back, showing the
  last frame meanwhile; when the server cannot be reached, the panel says
  why.

## Command line

```
thinkterm plugin list                        plugins and their states
thinkterm plugin enable <id>                 turn on; lets a new one run
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
running `thinkterm cli`: only a sandbox would. ThinkTerm runs nothing of a
plugin until you let it run, from the directory it is in: one that turns up
in the plugins directory without you -- left by an installer, unpacked
there -- does not run. Once let, it starts on its own only when its
manifest, or you, say it runs always; otherwise it runs when a client uses
it. Nothing downloads or installs plugins for you.

A panel never takes the keyboard: you give it, by pressing in its field
or with your key for it, and it goes back on Escape or a press on the
terminal. Nor can it hand what you type to the terminal: if it lets go of
the keyboard, or stops, while you type, your keys go nowhere until you
press Escape or click. While a panel has it, its plugin hears only the
keys it named and what its fields hold; never a key pressed with Ctrl, Alt
or Command, never a field's text while an input method composes it, and a
secret field's only when you submit it. Nothing can be copied out of a secret
field, and the clipboard reaches a plugin only as what you paste into its
field.

Beside a terminal on another machine, a plugin can have ThinkTerm run
programs and read files there, as you, over ThinkTerm's own connection --
once you have let ThinkTerm connect to that machine. It reaches only the
machine of the terminal beside its panel, and only while the panel is on
show.

## Limits

| Limit                                    | Value      |
|------------------------------------------|------------|
| Message size, either direction           | 16 MiB     |
| Calls waiting on one plugin              | 64         |
| Time a call waits for its answer         | 70 seconds |
| Asks waiting on one panel                | 64         |
| What an ask brings back                  | 8 MiB      |
| A program run on another machine         | 20 seconds |
| Unused, `briefly` / `never`              | 2 minutes / 10 seconds |
| Time to say `ready`                      | 10 seconds |
| Time to exit after `stop`                | 2 seconds  |
| Crashes before `failed`                  | 3 in 60 s  |
| Standard error logged, all plugins       | 4 MiB      |
| Items in a frame, or a page of rows      | 20,000     |
| Rows a list keeps                        | 2,048      |
| Characters of one text drawn             | 2,000      |
| Numbers in one line or area              | 8,192      |
| Characters a field holds                 | 16,384     |
| Keys a frame names, and one name's length | 64, 32 characters |
