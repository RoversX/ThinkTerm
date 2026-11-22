# Making a ThinkTerm plugin

A plugin adds a panel to ThinkTerm's right sidebar, a page (a tab of its own)
and commands of its own. It is a program ThinkTerm runs, speaking JSON lines
over standard input and output. The quickest kind is a Python script, run by
the Python on the user's machine, with ThinkTerm's helper beside it.

## Steps

1. `thinkterm plugin new <id> --name "Its Name"`. The id is a-z, 0-9 and "-".
   This makes `<plugins dir>/<id>/` holding plugin.toml, main.py and
   thinkterm.py, turns that plugin on and checks that it runs.
2. Edit main.py: `call()` answers calls, and `draw(ui)` draws the panel and the
   page. Leave thinkterm.py as it is.
3. It starts again by itself once a `.py` file in its directory changes, or
   with `thinkterm plugin reload <id>`.
4. Check it, every time:
   - `thinkterm plugin call <id> '{"op": "..."}'` shows what call() answers.
   - `thinkterm plugin list` shows its state: running, crashed, failed, and why.
   - `thinkterm plugin logs <id>` shows what it printed to standard error,
     tracebacks included.
   - `thinkterm plugin do <id> open` opens its page beside you.
   - `thinkterm plugin shot <id>` saves a PNG of its open panel or page as
     ThinkTerm drew it, and prints the path: read the image.
5. Tell the user where it is (its icon in the right sidebar's selector, or
   the command palette) and what to try.

## main.py

```python
import thinkterm

class Notes(thinkterm.Plugin):
    def __init__(self):
        self.note = thinkterm.Field()
        self.notes = []
        self.newest_first = False

    def call(self, body):              # `plugin call`, and actions
        return {"notes": self.notes}

    def draw(self, ui):                # each view: the panel, a page
        ui.title("Notes")
        if ui.field(self.note, placeholder="A note"):   # Return pressed
            self.notes.append(self.note.text)
            self.note.clear()
        with ui.row():
            if ui.button("Add", primary=True) and self.note.text:
                self.notes.append(self.note.text)
                self.note.clear()
            if ui.button("Clear all", danger=True):
                self.notes = []
        self.newest_first = ui.toggle("Newest first", self.newest_first)
        shown = reversed(self.notes) if self.newest_first else self.notes
        for note in shown:
            ui.text(note)
        if not self.notes:
            ui.text("No notes yet", muted=True)

thinkterm.run(Notes())
```

draw() runs when a view opens, after every click and keystroke in it, and
after `self.redraw()`. Each widget says what the user did with it:
`button()` is true in the draw right after it was pressed; `toggle(label,
on)` and `checkbox(label, on)` hand back the new state; `field(f)` keeps
what is typed in `f.text` (a `thinkterm.Field`), is true when Return was
pressed, and `f.set(text)` / `f.clear()` change it. Two widgets with the same
label need `key="..."` to be told apart.

What `ui` has:
- `title(text)`, `text(text, muted=, small=, mono=, color=, bold=)`, `space()`,
  `separator()`, `progress(value)` (0 to 1).
- `button(label, primary=, danger=, key=)`, `toggle(label, on)`,
  `checkbox(label, on)`, `field(f, placeholder=, secret=, lines=)`.
- `with ui.row():` puts what is inside it side by side.
- `ui.width`, `ui.height`, `ui.dark`, `ui.locale`, and `ui.page`: None in the
  panel, a dict of what a page was opened with. `ui.env` holds the rest,
  such as `cwd`, the directory of the terminal beside the view.
- At a place of your own, in the view's units from its top left corner:
  `rect(x, y, w, h, fill=, radius=, border=)`, `label(x, y, w, h, text, ...)`,
  `line([x, y, x, y, ...])`, `hit(id, x, y, w, h)` (true once clicked), and
  `item({...})` for any item the docs list.
- Colours go by ThinkTerm's names, so they follow the theme: text, text-muted,
  text-faint, accent, on-accent, positive, negative, warning, border, bg,
  bg-raised, bg-hover. Or "#rrggbb".

Work in the background: `thinkterm.every(seconds, fn)` calls `fn` on a
thread of its own and then draws again. Or start a thread yourself and call
`self.redraw()` once something changed. Keep files in `thinkterm.DATA`, a
directory of the plugin's own. `thinkterm.CLI` is ThinkTerm's command line:
`thinkterm cli get-text --pane-id N` reads a terminal.

Standard output is ThinkTerm's: never print there. What
`print(..., file=sys.stderr)` prints goes to `thinkterm plugin logs`.
Use only Python's standard library, unless the user wants a package
installed. Python 3.8 is the oldest it may run on.

## plugin.toml

```toml
id = "notes"
name = "Notes"
version = "0.1.0"
description = "Notes beside the terminal."
api = 3

[run]
python = "main.py"     # or: program = "a-binary", args = ["--stdio"]

[panel]
icon = "list-todo"     # puzzle activity bug calendar chart-candlestick
                       # chart-line cpu database gauge git-branch
                       # git-compare list-todo
[page]
icon = "list-todo"

[[action]]             # in the command palette, and `plugin do`
id = "open"
title = "Open Notes"
page = {}              # or call = { op = "refresh" }
```

An action's call reaches call() with "at" added: where the user was,
`{"cwd": "...", "terminal": {"pane": 3}}`.

## When it does not work

- A call fails with "NameError: ... (main.py, line 12)": call() raised that.
- The panel says "draw() failed", with the error and the line: draw() raised.
- `plugin list` says failed: it stopped 3 times within a minute, and
  ThinkTerm stopped starting it. Fix it: it runs again once a file changes,
  or with `plugin reload <id>`.
- "it needs Python 3.8 or later, and none was found": the user has to
  install Python. Say how, as the message does.

## Rules

- Change only what is in the plugin's own directory. Leave other plugins,
  ThinkTerm's settings and plugins.json alone.
- A plugin that is on runs as the user. Turn on only what the user asked
  for. `plugin new` turns on the one it made.
- Done means checked: call it, look at `plugin list` and `plugin logs`, and
  at a shot of it when you can.

More is in docs/thinkterm/plugins.md in ThinkTerm's repository: the
protocol, every item, extended views, and compiled plugins in Rust.
