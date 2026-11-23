---
name: thinkterm
description: Use when running inside ThinkTerm (THINKTERM=1 is set) and you need to look at other terminals, check on or coordinate other coding agents, send them text, or open new panes; or when asked to make a ThinkTerm plugin -- a panel, page or command of ThinkTerm's own.
---

You are running inside ThinkTerm. Its command line, `thinkterm cli`, lets you see every
pane, read what other panes show, type into them, open new panes, and check on or
coordinate the coding agents running in them.

Run `thinkterm cli --help` for all commands and `thinkterm cli <command> --help` for
details. If `thinkterm` is not on PATH, it is in the folder named by the
WEZTERM_EXECUTABLE_DIR environment variable (thinkterm.exe on Windows).

Panes on the servers the user's ThinkTerm window is connected to are listed by
`thinkterm cli list` as `MACHINE/ID` (in JSON too, as a string, not a number), and
named the same way, or as `MACHINE/LABEL`, with `--pane` -- never `--pane-id` -- in
`get-text`, `send-text`, `send-key`, `wait` and `events`; an event of one says so in
its `pane` field. They are
there only as far as the user lets programs reach them in ThinkTerm's Settings ›
Agents: not at all (the default), to read, or to read and type.

To start work the user should see, put it in a thread: `thinkterm cli spawn
--thread NAME -- PROGRAM` puts it in the thread of that name in the project of
the thread you run in (or `--project PROJECT`), making the thread when no
thread has that name, and the user finds it in the sidebar. When several
threads have the name, or only other projects' threads do, it lists them and
starts nothing: name the one you mean by its id. `--workspace` alone makes no
thread, and the sidebar shows no window in a workspace without one.
`thinkterm cli thread list` shows the threads, and `thinkterm cli list` which
thread each pane is in; `thinkterm cli thread rename THREAD --name NAME` renames
one (by its id when several share the name).

Some commands affect what the user or other agents are using, such as typing into a
pane you did not open, closing panes or stopping the server; you may want to ask the
user first. Whether to ask is up to you.

To make a ThinkTerm plugin -- a panel in its right sidebar, a page, commands of its
own -- read plugins.md beside this file first: `thinkterm plugin new` starts one in
Python, and `thinkterm plugin` checks it.
