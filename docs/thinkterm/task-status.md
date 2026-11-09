# Task Status Rules

When a long-running command-line tool works in a pane you are not looking
at — a package manager in another tab, say — ThinkTerm's Dynamic Status, in
the bottom-right corner of the terminal area, can show what it is doing and
how far it has got. Tools that report their own progress (OSC 9;4) need
nothing. For tools that don't, ThinkTerm reads their screen with **task
rules**: one TOML file per tool, in the same spirit as the agent detection
manifests in [agent-status.md](../agent-status.md).

ThinkTerm ships rules for **apt** (including `apt-get`) and **Homebrew**.

Task rules only feed Dynamic Status. They do not change a thread's status in
the sidebar or what the Status panel lists. A tool that has rules and also
reports its own progress is shown once: by what its rules read, when they
read anything, and by its own report otherwise.

## When a tool is read

- The pane's foreground program must be one the rules name (`programs`).
- The screen is read where the pane lives — this machine for its own panes,
  the server for a remote one — and only what was read travels to the
  window, not the screen.
- It is read at most once every two seconds, and only the bottom lines a
  rule asks for are looked at. Panes running anything else are never read.
- It shows only while the pane is out of sight: a tool on screen tells you
  itself.
- It shows as soon as its pane goes out of sight. One that has just begun
  to wait for an answer, or has just shown an error, opens for a moment to
  say so; one that only comes into view because you switched tabs does not.
- When it ends, the usual "finished" notice for a long command takes over,
  marked as failed if the rules last read an error and the shell did not
  report the exit status.

## Writing rules

Put a file in `~/.config/thinkterm/task-detection/` on the machine the
pane runs on. A file whose `id` matches a bundled one replaces it; any other
is added. Rules are loaded when ThinkTerm (or, on a server, its mux server)
starts.

```toml
id = "pacman"
name = "pacman"            # what to call it; defaults to the id
programs = ["pacman"]      # foreground program names it applies to

# "(3/12) installing ripgrep  [######...] 45%"
[[rules]]
id = "installing"
priority = 120
region = "bottom_non_empty_lines(2)"
line_regex = ['^\((?P<done>\d+)/(?P<total>\d+)\) installing (?P<pkg>\S+)']
detail = "Installing {pkg}"

# ":: Proceed with installation? [Y/n]"
[[rules]]
id = "confirm"
state = "waiting"
priority = 300
region = "bottom_non_empty_lines(1)"
line_regex = ['\[Y/n\]\s*$']

[[rules]]
id = "error"
state = "error"
priority = 200
region = "bottom_non_empty_lines(3)"
line_regex = ['^error: ']
```

Each rule has:

| Key | Meaning |
|---|---|
| `id` | A name for the rule. |
| `region` | Which part of the screen to look at. `bottom_non_empty_lines(N)`: the last N lines that are not blank, N from 1 to 40. |
| `line_regex` | Regular expressions tried on each line of the region, from the bottom up. The first line one of them matches is the rule's match. |
| `state` | Optional: `running`, `waiting` (stopped to ask the user) or `error`. |
| `priority` | Higher is weighed first. Defaults to 0. |
| `detail` | Optional text saying what the tool is doing. `{name}` puts in what the group `name` matched. |
| `sum` | Optional, `true` for a tool that draws a line for each of several things at once (parallel downloads). Every line of the region that matches counts, not only the bottom one: see below. |
| `after_regex` | Optional regular expressions for a heading the tool prints before each step. The rule looks no higher than the bottom-most line one of them matches, so what an earlier or interrupted step left on screen is not counted. |

Named groups take values out of the match:

- `percent` — 0 to 100 (a decimal part is dropped).
- `done_size` and `total_size` — how much of how much, as sizes such as
  `10.0MB`, `512 B` or `1.2GiB`; the percent, when there is no `percent`.
- `done` and `total` — how many of how many; used when there is no percent.
- `detail` — used as the detail when the rule has no `detail` text.

With `sum = true`, the sizes (`done_size`, `total_size`) and counts
(`done`, `total`) of every matching line that has both are added up, and
the detail can say `{lines}` (how many lines matched) and `{lines_done}`
(how many have got to their total). Other groups come from the bottom
line. Once every line has got to its total the rule no longer matches, so
whatever the tool goes on to do is read instead.

The reading is put together from every rule that matches: the state from
the highest-priority one that sets a state (otherwise `running`), and the
percent, the count and the detail each from the highest-priority one that
has it.

A file with an unknown key, an unknown state or region, or a regex that
does not compile is left out, and the log says why.
