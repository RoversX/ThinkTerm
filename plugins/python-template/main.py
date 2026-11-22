"""A ThinkTerm plugin, made by `thinkterm plugin new` to start from.

ThinkTerm runs this file with the Python on this machine. It draws the
plugin's panel in the right sidebar, and its page; thinkterm.py beside it
does the talking to ThinkTerm, and says what `ui` can draw.

    thinkterm plugin do <id> open    opens its page
    thinkterm plugin reload <id>     starts it again: it does by itself too,
                                     once a .py file here changes
    thinkterm plugin logs <id>       what it printed to standard error, and
                                     why it stopped
    thinkterm plugin call <id> '{}'  calls it, as call() below answers
"""

import thinkterm


class Hello(thinkterm.Plugin):
    def __init__(self):
        self.clicks = 0
        self.by_two = False
        self.name = thinkterm.Field()

    def call(self, body):
        # `thinkterm plugin call <id> '{"op": "reset"}'`, say.
        if isinstance(body, dict) and body.get("op") == "reset":
            self.clicks = 0
            self.redraw()
        return {"clicks": self.clicks, "name": self.name.text}

    def draw(self, ui):
        # Called for each view when it opens, and again after every click
        # and keystroke in it: what the user did comes back from the widget.
        ui.title("Hello")
        ui.text("Drawn by main.py, in " + (thinkterm.ID or "this plugin") + "'s directory.", muted=True)
        ui.space()
        with ui.row():
            if ui.button("Click me", primary=True):
                self.clicks += 2 if self.by_two else 1
            if ui.button("Reset"):
                self.clicks = 0
        ui.text("Clicked %d times" % self.clicks)
        self.by_two = ui.toggle("Count by two", self.by_two)
        ui.space()
        if ui.field(self.name, placeholder="Your name"):
            # Return pressed in the field.
            self.clicks += 10
        if self.name.text:
            ui.text("Hello, %s!" % self.name.text)
        if ui.page is not None:
            ui.space()
            ui.text("This is the plugin's page: a tab of its own.", muted=True, small=True)


thinkterm.run(Hello())
