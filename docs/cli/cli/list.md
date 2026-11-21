# `thinkterm cli list`

*Run `thinkterm cli list --help` to see more help*

Lists the set of windows, tabs and panes that are being managed.

The default output is tabular:

```
$ thinkterm cli list
WINID TABID PANEID WORKSPACE                    SIZE  TITLE    CWD                        LABEL SPACE PROJECT THREAD
    0     0      0 default                      80x24 zsh      file://foo/home/wez/
    1     1      1 thinkterm:project-1:thread-2 80x24 my-agent file://foo/home/wez/myapp/       Work  myapp   fix the build
```

Each row describes a pane.  The meaning of the fields are:

* `WINID` - the window id of the window that contains the pane
* `TABID` - the tab id of the tab that contains the pane
* `PANEID` - the pane id
* `WORKSPACE` - the workspace that the pane is associated with
* `SIZE` - the dimensions of the pane, measured in terminal cell columns x rows
* `TITLE` - the pane title
* `CWD` - the current working directory associated with the pane
* `LABEL` - the label the pane was given (`thinkterm cli set-pane-label`, or
  `spawn --label`), if any
* `SPACE`, `PROJECT`, `THREAD` - the sidebar thread whose workspace the pane
  is in: its Space, its project and its name. They are blank for a workspace
  that no thread has, and so for a window the sidebar does not show. The
  server the command reaches names the threads it keeps; through a ThinkTerm
  window, the window names the others (those of the servers it is connected
  to, and its own when it runs its terminals itself), and it gives their
  project and name but not their Space.

The columns after `CWD` come last so that scripts that pick columns by their
place still find the others where they were.

{{since('20220624-141144-bd1b7c5d')}}

You may request JSON output:

```
$ thinkterm cli list --format json
[
  {
    "window_id": 1,
    "tab_id": 1,
    "pane_id": 1,
    "label": null,
    "machine": null,
    "workspace": "thinkterm:project-1:thread-2",
    "size": {
      "rows": 24,
      "cols": 80
    },
    "title": "my-agent",
    "cwd": "file://foo/home/wez/myapp/",
    "space": "Work",
    "space_id": "space-1",
    "project": "myapp",
    "project_id": "project-1",
    "thread": "fix the build",
    "thread_id": "thread-2"
  }
]
```

The thread's fields are `null` for a workspace that no thread has, and the
ids are `null` too where only a window names the thread.

## Synopsis

```console
{% include "../../examples/cmd-synopsis-wezterm-cli-list--help.txt" %}
```
