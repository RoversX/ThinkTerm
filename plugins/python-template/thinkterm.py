"""thinkterm.py: what a ThinkTerm plugin written in Python needs to talk to
ThinkTerm. `thinkterm plugin new` puts it beside main.py.

ThinkTerm starts main.py with the Python it finds on this machine, and talks
to it over standard input and output, one JSON message a line. This file is
that part; main.py says what the plugin does and what it draws:

    import thinkterm

    class Hello(thinkterm.Plugin):
        def __init__(self):
            self.clicks = 0

        def call(self, body):
            # `thinkterm plugin call <id> '{"op": "clicks"}'`, and the
            # actions plugin.toml lists that call.
            return {"clicks": self.clicks}

        def draw(self, ui):
            # Every view of the plugin: its panel in the right sidebar,
            # and its pages.
            ui.title("Hello")
            if ui.button("Click me"):
                self.clicks += 1
            ui.text(f"Clicked {self.clicks} times", muted=True)

    thinkterm.run(Hello())

draw() lays things out down the view, one under another -- or side by side
in a `with ui.row():` -- and is called again after every click and
keystroke, so a widget says what the user did with it: button() is true the
draw after it was pressed, toggle() hands back the new state. Anything
printed to standard error (print(..., file=sys.stderr)) goes to `thinkterm
plugin logs <id>`; standard output is ThinkTerm's, and nothing else may be
printed there.

What else a plugin can draw and say is in docs/thinkterm/plugins.md: this
file writes the same messages, and ui.item() takes any item as JSON.

Version 1, for plugin API 3. Works on Python 3.8 and later.
"""

import json
import math
import os
import sys
import threading
import time
import traceback
from contextlib import contextmanager

#: This file's version.
HELPER = 1
#: The plugin API it speaks. A page needs 3, as plugin.toml says.
API = 3
#: The plugin's id, as plugin.toml gives it.
ID = os.environ.get("THINKTERM_PLUGIN_ID", "")
#: A directory of the plugin's own, to keep its files in across runs.
DATA = os.environ.get("THINKTERM_PLUGIN_DATA", "")
#: ThinkTerm's command line, for `thinkterm cli` (see the docs).
CLI = os.environ.get("THINKTERM_CLI", "thinkterm")


class Error(Exception):
    """Raised by call(): whoever called is told its message, and no more."""


class Plugin:
    """What a plugin does and draws. Give run() one of these, with the
    methods it needs."""

    def call(self, body):
        """Answers a call with what can be written as JSON. `body` is what
        the caller sent: a dict, for an action plugin.toml lists, with
        "at" added -- where the user was."""
        raise Error("this plugin takes no calls")

    def draw(self, ui):
        """Draws one view of the plugin, its panel or a page, with `ui`."""
        ui.text("Nothing to show yet: draw() says what is shown here.")

    def closed(self, view):
        """A view of it closed: the user moved away, or closed its page."""

    def redraw(self):
        """Draws every open view again. Safe from any thread: call it once
        something draw() shows has changed. Given by run()."""


class Field:
    """What a text field holds: what the user typed in it, or what the
    plugin put there with set(). Keep one for each field, and hand it to
    ui.field() in every draw."""

    def __init__(self, text=""):
        self.text = text
        self._seq = 0

    def set(self, text):
        """Puts `text` in the field, over whatever was typed."""
        self.text = text
        self._seq += 1

    def clear(self):
        self.set("")


def every(seconds, do):
    """Calls `do()` every `seconds` on a thread of its own, then draws every
    view again: for a clock, or something looked up now and then. What it
    raises is printed to standard error, and it goes on."""

    def loop():
        while True:
            time.sleep(seconds)
            try:
                do()
            except Exception:
                traceback.print_exc()
            if _running is not None:
                _running.redraw()

    threading.Thread(target=loop, name="every", daemon=True).start()


# -- drawing ----------------------------------------------------------------


def _wide(c):
    """Whether CJK writes `c` in two columns."""
    n = ord(c)
    return (
        0x1100 <= n <= 0x115F
        or 0x2E80 <= n <= 0xA4CF
        or 0xAC00 <= n <= 0xD7A3
        or 0xF900 <= n <= 0xFAFF
        or 0xFE30 <= n <= 0xFE4F
        or 0xFF00 <= n <= 0xFF60
        or 0xFFE0 <= n <= 0xFFE6
    )


class Ui:
    """One draw of one view. Widgets go down the view one under another,
    each as wide as the view but for its margins; in a `with ui.row():`
    they go side by side instead. The low-level calls -- rect(), label(),
    hit(), item() -- draw at a place given in the view's own units, from its
    top left corner. Colours are ThinkTerm's own by name ("text",
    "text-muted", "accent", "positive", "negative", "border", "bg-raised",
    ...) or "#rrggbb", so a view follows the theme by itself."""

    def __init__(self, view, done):
        env = view.env
        #: What ThinkTerm says of the view: its size, fonts, theme, and the
        #: terminal beside it ("cwd", "terminal"). See the docs.
        self.env = env
        self.width = float(env.get("width", 300))
        self.height = float(env.get("height", 400))
        self.dark = bool(env.get("dark", False))
        self.touch = env.get("pointer") == "touch"
        #: What a page was opened with -- a dict -- and None for the panel.
        self.page = view.page
        #: The language ThinkTerm speaks, "en-US" or "zh-CN".
        self.locale = env.get("locale", "")
        self.items = []
        self.margin = 12.0
        self.gap = 8.0
        self.x = self.margin
        self.y = self.margin
        self.w = max(0.0, self.width - 2 * self.margin)
        self._done = done
        self._names = {}
        self._row = None
        self._fields = {}
        self._features = env.get("features") or []

    # Sizes.

    def _font(self, size):
        metrics = self.env.get(size) or {}
        return float(metrics.get("size", 13)), float(metrics.get("line", 18))

    def line_height(self, size="body"):
        """How tall a line of text is: size is "small", "body" or "title"."""
        return self._font(size)[1]

    def control_height(self):
        """How tall a button or a field is."""
        return 44.0 if self.touch else float(round(self._font("body")[1] + 14))

    def text_width(self, text, size="body"):
        """About how wide `text` is: a little more than it is."""
        columns = sum(1.9 if _wide(c) else 1.0 for c in text)
        return float(math.ceil(columns * self._font(size)[0] * 0.58))

    def _lines_for(self, text, w, size):
        if "wrap" not in self._features:
            return 1
        lines = 0
        for paragraph in text.split("\n"):
            lines += max(1, math.ceil(self.text_width(paragraph, size) / max(w, 1.0) * 1.08))
        return max(1, lines)

    # Layout.

    def _id(self, kind, label, key):
        name = key if key is not None else "%s:%s" % (kind, label)
        seen = self._names.get(name, 0)
        self._names[name] = seen + 1
        return name if seen == 0 else "%s#%d" % (name, seen)

    def _place(self, w, h):
        """Where the next widget goes, `w` wide -- None for all the room
        there is -- and `h` tall; moves past it."""
        if self._row is not None:
            x = self._row[0]
            if w is None:
                w = max(0.0, self.x + self.w - x)
            self._row[0] = x + w + self.gap
            self._row[1] = max(self._row[1], h)
            return x, self.y, w
        x, y = self.x, self.y
        self.y += h + self.gap
        return x, y, self.w if w is None else min(w, self.w)

    @contextmanager
    def row(self):
        """Lays the widgets drawn inside it side by side, then goes on under
        the tallest of them."""
        if self._row is not None:
            yield
            return
        self._row = [self.x, 0.0]
        try:
            yield
        finally:
            height = self._row[1]
            self._row = None
            self.y += height + self.gap

    def space(self, height=None):
        """Leaves room."""
        if self._row is not None:
            self._row[0] += self.gap if height is None else height
        else:
            self.y += self.gap if height is None else height

    # Widgets.

    def title(self, text):
        line = self.line_height("title")
        x, y, w = self._place(None, line)
        self.label(x, y, w, line, text, size="title", bold=True)

    def text(self, text, muted=False, small=False, mono=False, color=None, bold=False, wrap=True):
        """Text, wrapped into as many lines as it takes."""
        size = "small" if small else "body"
        line = self.line_height(size)
        width = None if self._row is None else min(self.text_width(text, size), self.w)
        x, y, w = self._place(width, line)
        lines = self._lines_for(text, w, size) if wrap else 1
        if lines > 1:
            # Placed for one line: the rest of them below it.
            self.y += (lines - 1) * line
        color = color or ("text-muted" if muted else "text")
        self.label(x, y, w, line * lines, text, color=color, size=size, bold=bold, mono=mono, wrap=lines > 1)

    def button(self, label, primary=False, danger=False, key=None, width=None):
        """A button. True in the draw after it was pressed."""
        id = self._id("button", label, key)
        h = self.control_height()
        if width is None:
            width = max(self.text_width(label) + 28, 64 if self.touch else 56)
        x, y, w = self._place(width, h)
        if primary:
            fill, border, hover, color = "accent", None, "#ffffff24", "on-accent"
        elif danger:
            fill, border, hover, color = "negative-bg", "negative", "negative-bg-strong", "negative"
        else:
            fill, border, hover, color = "bg-raised", "border", "bg-hover", "text"
        self.rect(x, y, w, h, fill=fill, radius=8, border=border)
        pressed = self.hit(id, x, y, w, h, hover=hover, radius=8)
        self.label(x, y, w, h, label, color=color, align="center", bold=primary)
        return pressed

    def toggle(self, label, on, key=None):
        """A switch with its label. Hands back whether it is on now."""
        id = self._id("toggle", label, key)
        tw, th = (48.0, 28.0) if self.touch else (36.0, 20.0)
        h = max(th, self.line_height())
        width = None if self._row is None else tw + self.gap + self.text_width(label)
        x, y, w = self._place(width, h)
        if self.clicked(id):
            on = not on
        ty = y + (h - th) / 2
        if on:
            self.rect(x, ty, tw, th, fill="accent", radius=th / 2)
        else:
            self.rect(x, ty, tw, th, fill="bg-hover", radius=th / 2, border="border")
        knob = th - 6
        kx = x + tw - knob - 3 if on else x + 3
        self.rect(kx, ty + 3, knob, knob, fill="on-accent" if on else "text-muted", radius=knob / 2)
        self.label(x + tw + self.gap, y, max(0.0, w - tw - self.gap), h, label)
        self.hit(id, x, y, w, h, hover=None)
        return on

    def checkbox(self, label, checked, key=None):
        """A box ticked or not, with its label. Hands back whether it is
        ticked now."""
        id = self._id("checkbox", label, key)
        h = 40.0 if self.touch else self.line_height() + 8
        width = None if self._row is None else 18 + self.gap + self.text_width(label)
        x, y, w = self._place(width, h)
        if self.clicked(id):
            checked = not checked
        side = 18.0
        bx, by = x, y + (h - side) / 2
        if checked:
            self.rect(bx, by, side, side, fill="accent", radius=5)
            self.line([bx + 4.5, by + 9.5, bx + 7.5, by + 12.5, bx + 13.5, by + 5.5], color="on-accent", width=2)
        else:
            self.rect(bx, by, side, side, radius=5, border="text-faint")
        self.label(x + side + self.gap, y, max(0.0, w - side - self.gap), h, label)
        self.hit(id, x - 4, y, w + 4, h, radius=6)
        return checked

    def field(self, field, placeholder="", key=None, secret=False, lines=1, width=None):
        """A text field holding `field`, a Field: what is typed in it lands
        there. True in the draw after the user pressed Return in it --
        Command-Return in one of several lines."""
        id = self._id("field", placeholder, key)
        h = self.control_height() if lines <= 1 else self.line_height() * lines + 14
        x, y, w = self._place(width, h)
        if "fields" not in self._features:
            self.label(x, y, w, h, "This ThinkTerm shows no text fields here.", color="text-faint")
            return False
        self._fields[id] = field
        kind = "secret" if secret else ("lines" if lines > 1 else "line")
        self.item({"field": {
            "id": id, "x": x, "y": y, "w": w, "h": h, "kind": kind,
            "value": "" if secret else field.text, "seq": field._seq,
            "placeholder": placeholder,
        }})
        return self._done == ("submit", id)

    def progress(self, value, color="accent"):
        """A bar filled as far as `value`, from 0 to 1."""
        h = 6.0
        x, y, w = self._place(None, h)
        self.rect(x, y, w, h, fill="bg-hover", radius=3)
        filled = max(0.0, min(1.0, float(value))) * w
        if filled > 0:
            self.rect(x, y, max(filled, h), h, fill=color, radius=3)

    def separator(self):
        x, y, w = self._place(None, 1)
        self.line([x, y + 0.5, x + w, y + 0.5], color="border")

    # Low level: at a place given, nothing laid out.

    def item(self, item):
        """Any item, as the docs write it: {"rect": {...}}, {"text": {...}}."""
        self.items.append(item)

    def rect(self, x, y, w, h, fill=None, radius=0, border=None):
        rect = {"x": x, "y": y, "w": w, "h": h}
        if fill:
            rect["fill"] = fill
        if radius:
            rect["radius"] = radius
        if border:
            rect["border"] = border
        self.item({"rect": rect})

    def label(self, x, y, w, h, text, color="text", size="body", bold=False, align="left", mono=False, wrap=False):
        """Text in a box: one line centred in its height and cut to fit, or
        with `wrap`, as many lines as the box holds."""
        label = {"x": x, "y": y, "w": w, "h": h, "text": str(text)}
        if color != "text":
            label["color"] = color
        if size != "body":
            label["size"] = size
        if bold:
            label["bold"] = True
        if align != "left":
            label["align"] = align
        if mono:
            label["font"] = "mono"
        if wrap:
            label["wrap"] = True
        self.item({"text": label})

    def line(self, points, color="border", width=1):
        """A line through `points`: x, y, x, y, ..."""
        self.item({"line": {"points": list(points), "width": width, "color": color}})

    def hit(self, id, x, y, w, h, hover="bg-hover", radius=0):
        """A place a click reaches the plugin, by `id`. True in the draw
        after it was clicked."""
        hit = {"id": id, "x": x, "y": y, "w": w, "h": h}
        if hover:
            hit["hover"] = hover
        if radius:
            hit["radius"] = radius
        self.item({"hit": hit})
        return self.clicked(id)

    def clicked(self, id):
        """Whether the hit `id` is what was just clicked."""
        return self._done == ("click", id)


# -- talking to ThinkTerm -----------------------------------------------------


def _where(error):
    """Where in the plugin's own files `error` was raised -- not in this
    one -- as "main.py, line 12"; empty when it was not."""
    frames = [
        frame
        for frame in traceback.extract_tb(error.__traceback__)
        if os.path.basename(frame.filename) != "thinkterm.py"
    ]
    if not frames:
        return ""
    return "%s, line %d" % (os.path.basename(frames[-1].filename), frames[-1].lineno)


class _View:
    def __init__(self, env, page):
        self.env = env or {}
        self.page = page
        self.fields = {}
        self.last = None


class _Running:
    """The plugin as it runs: its views, and the line to ThinkTerm."""

    def __init__(self, plugin):
        self.plugin = plugin
        self.views = {}
        self.lock = threading.RLock()
        self.out = sys.stdout.buffer
        self.last_error = None

    def send(self, message):
        # NaN and infinity are not JSON, and a line holding one would be
        # dropped: a ValueError here says so instead.
        line = json.dumps(message, ensure_ascii=False, separators=(",", ":"), allow_nan=False) + "\n"
        with self.lock:
            self.out.write(line.encode("utf-8"))
            self.out.flush()

    def draw(self, number, done=None):
        view = self.views.get(number)
        if view is None:
            return
        if done is not None:
            # What the user did is acted on in one draw, and what came of
            # it shown by the next.
            self._draw_once(view, done)
        ui = self._draw_once(view, None)
        frame = {"items": ui.items}
        if frame != view.last:
            view.last = frame
            try:
                self.send({"type": "frame", "view": number, "frame": frame})
            except ValueError as error:
                sys.stderr.write("draw() drew what is not JSON: %s\n" % error)
                sys.stderr.flush()
                shown = Ui(view, None)
                shown.title("draw() failed")
                shown.text("A number it drew is not finite: %s" % error, color="negative")
                self.send({"type": "frame", "view": number, "frame": {"items": shown.items}})

    def _draw_once(self, view, done):
        ui = Ui(view, done)
        try:
            self.plugin.draw(ui)
        except Exception as error:
            said = traceback.format_exc()
            if said != self.last_error:
                self.last_error = said
                sys.stderr.write(said)
                sys.stderr.flush()
            ui = Ui(view, None)
            ui.title("draw() failed")
            ui.text("%s: %s" % (type(error).__name__, error), color="negative")
            where = _where(error)
            if where:
                ui.text(where, muted=True)
            ui.text("thinkterm plugin logs %s shows the rest." % (ID or "<id>"), muted=True, small=True)
        view.fields = ui._fields
        return ui

    def redraw(self):
        with self.lock:
            for number in list(self.views):
                self.draw(number)

    def answer(self, message):
        number = message.get("id")
        try:
            body = self.plugin.call(message.get("body"))
            json.dumps(body, allow_nan=False)
        except Error as error:
            return self.send({"type": "error", "id": number, "message": str(error)})
        except Exception as error:
            traceback.print_exc()
            sys.stderr.flush()
            text = "%s: %s" % (type(error).__name__, error)
            where = _where(error)
            if where:
                text += " (%s)" % where
            return self.send({"type": "error", "id": number, "message": text})
        self.send({"type": "ok", "id": number, "body": body})

    def input(self, number, given):
        view = self.views.get(number)
        if view is None:
            return
        if isinstance(given, dict) and "click" in given:
            self.draw(number, ("click", given["click"].get("id")))
        elif isinstance(given, dict) and ("text" in given or "submit" in given):
            submitted = "submit" in given
            typed = given["submit"] if submitted else given["text"]
            field = view.fields.get(typed.get("id"))
            # Typed over what the plugin has put there since: that stays,
            # and a Return pressed on it is not taken for the new text's.
            heard = field is not None and typed.get("seq", 0) == field._seq
            if heard:
                field.text = typed.get("text", "")
            self.draw(number, ("submit", typed.get("id")) if submitted and heard else None)
        else:
            self.draw(number)

    def serve(self):
        self.send({"type": "ready", "api": API})
        for raw in sys.stdin.buffer:
            try:
                message = json.loads(raw.decode("utf-8", "replace"))
            except ValueError:
                continue
            if not isinstance(message, dict):
                continue
            kind = message.get("type")
            number = message.get("view")
            with self.lock:
                if kind == "call":
                    self.answer(message)
                elif kind == "open":
                    page = message.get("page")
                    args = None
                    if page is not None:
                        args = page.get("args") if isinstance(page.get("args"), dict) else {}
                    self.views[number] = _View(message.get("env"), args)
                    self.draw(number)
                elif kind == "env" and number in self.views:
                    self.views[number].env = message.get("env") or {}
                    self.draw(number)
                elif kind == "input":
                    self.input(number, message.get("input"))
                elif kind == "close":
                    if self.views.pop(number, None) is not None:
                        try:
                            self.plugin.closed(number)
                        except Exception:
                            traceback.print_exc()
                elif kind == "stop":
                    return


_running = None


def run(plugin):
    """Runs `plugin` for ThinkTerm until ThinkTerm stops it."""
    global _running
    if sys.stdin.isatty():
        sys.exit(
            "This is a ThinkTerm plugin, which ThinkTerm runs: "
            "`thinkterm plugin reload %s` starts it again once it is edited, "
            "and `thinkterm plugin logs %s` shows what it printed."
            % (ID or "<id>", ID or "<id>")
        )
    _running = _Running(plugin)
    plugin.redraw = _running.redraw
    _running.serve()
