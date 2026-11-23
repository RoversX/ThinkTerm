# `thinkterm cli spawn`

*Run `thinkterm cli spawn --help` to see more help*

Spawn a command into a new tab or window.  Outputs the pane-id for the newly
created pane on success.

When run with no arguments, it will spawn a new tab running the default
program; this example spawns a new pane with id 1 running that default program
(most likely: your shell):


```
$ thinkterm cli spawn
1
```

You may spawn an alternative program by passing the argument list; it is
recommended that you use `--` to denote the end of the arguments being passed
to `thinkterm cli spawn` so that any parameters you may wish to pass to the
program are not confused with parameters to `thinkterm cli spawn`.  This example
launches `top` in a new tab:

```
$ thinkterm cli spawn -- top
2
```

This example explicitly runs bash as a login shell:

```
$ thinkterm cli spawn -- bash -l
3
```

The following options affect the behavior:

* `--cwd CWD` - Specifies the current working directory that should be set for the spawned program
* `--domain-name DOMAIN_NAME` - Spawn into the named multiplexer domain. The default is to spawn into the domain of the current pane.
* `--new-window` - Spawns the tab into a window of its own.
* `--workspace WORKSPACE` - when using `--new-window`, set the workspace name rather than using the default name of `"default"`. This names the mux workspace only and makes no thread, and the ThinkTerm sidebar shows a window only in a workspace that a thread has: a `thinkterm:...` name that no thread has is refused before anything is spawned (exit status 3), as is the workspace of a thread in an archived project (exit status 2), and any other name that no thread has is spawned with a warning. When the check cannot be made (the server does not answer, say), a `thinkterm:...` name is refused and any other is spawned with a warning. Use `--thread` to put the program in a thread.
* `--thread THREAD` - Put the program in this thread: its id or its name (see [Threads](#threads) below). An id names its thread wherever it is. A name is looked for in `--project`, else in the project of the current pane's thread, else, outside any thread, in every project. When several threads have the name, or the current pane's project has none of that name but other projects do, they are listed (Space, project, thread and id) and nothing is started: name one by its id, or its project with `--project`. A name that no thread has makes a thread of that name in that project. A thread with no window open, unless it was made just now, is opened first as the sidebar opens it (its saved tabs, or a shell). The program goes in a new tab of the thread's window, or in a window of its own with `--new-window`. It starts in the project's directory, or in the one `--cwd` names, and nothing is started when that directory cannot be opened. Commands run at the same moment for the same new name share one thread. Like `thinkterm cli thread`, this never starts a server.
* `--project PROJECT` - with `--thread`: the project the thread is in, or is to be made in; its id or its name. A name that several projects have (in different Spaces, say) is the one in the current pane's Space when just one is there; otherwise they are listed and nothing is started.
* `--window-id WINDOW_ID` - Spawn the tab into the specified window, rather than using the current window

This example starts an agent in a new thread of the project `myapp`; the
thread shows in the sidebar, and the pane id is printed as usual:

```
$ thinkterm cli spawn --thread "fix the build" --project myapp -- my-agent
made the thread "fix the build" (thread-1795262878025-34ea9001) in the project "myapp"
7
```


## Threads

A thread is a row of the ThinkTerm sidebar (Space > project > thread), and its
terminals live in a mux workspace that the thread names. A window in a
workspace that no thread names still runs, but no sidebar shows it, which is
why `--workspace` warns about one and refuses a `thinkterm:...` name. These
commands deal with threads directly:

* `thinkterm cli thread list` - the threads of the projects that are not
  archived: their Space, project, name, id and workspace. `--project PROJECT`
  shows one project's; `--format json` (or `--json`) suits scripts.
  `thinkterm cli list` shows which thread each pane is in.
* `thinkterm cli thread create --name NAME [--project PROJECT]` - makes a
  thread and prints its id; `--json` prints its id, name, project and
  workspace. It shows in the sidebar at once, and its terminal starts when it
  is first opened there or when `spawn --thread` opens it to put a program in
  it. The default project is the one of the current pane's thread.
* `thinkterm cli thread rename THREAD --name NAME [--project PROJECT]` -
  renames a thread, named by its id or its name, and prints the name it
  took: a name another thread of its project has comes back numbered, as
  the sidebar numbers them. A name is looked for in every project that is
  not archived, or only in `--project`; when several threads have it they
  are listed (Space, project, thread and id) and nothing is renamed. A name
  that leaves the thread as it is changes nothing. `--json` prints the
  thread's id, name, project and workspace.

The threads are those of the server the command reaches. A mux server keeps
its own: on a remote host, the ones every device connected to it shows. On
the machine running the ThinkTerm window, they are the window's, and the
commands work with them when the window keeps its local terminals in a
background session server (Settings > General > Session server > *Keep local
terminals in a background session server*, the default for new installs).
When the window runs its terminals itself, they say so and make nothing.
Neither `thinkterm cli thread` nor `spawn --thread` starts a server: one
started for them would hold threads that nothing shows.

## Synopsis

```console
{% include "../../examples/cmd-synopsis-wezterm-cli-spawn--help.txt" %}
```
