# Contributing to ThinkTerm

Thanks for considering it. Anything from a typo fix upward is welcome.

ThinkTerm is a fork of [WezTerm](https://github.com/wezterm/wezterm). A large
part of this repository is still Wez Furlong's code, and changes to those parts
are often best sent upstream instead — they will reach more people there, and
ThinkTerm picks them up on the next merge. Changes to ThinkTerm's own features
belong here.

## License, and what contributing means

ThinkTerm is GPL-3.0-only — see [LICENSE.md](LICENSE.md). Code inherited from WezTerm
remains under its original MIT license, preserved in
[LICENSE-MIT](LICENSE-MIT); other third-party components are listed in
[licenses/README.md](licenses/README.md).

### Contributor agreement (version 1.2)

ThinkTerm may move to the MIT License in the future. To make that possible,
contributions are accepted under the following terms, which you agree to by
opening a pull request. A contribution means the code, documentation,
or other material you intentionally submit for inclusion in that pull request,
including your subsequent updates to it.

1. **You retain your copyright.** This agreement does not transfer ownership
   of your contribution to ThinkTerm.
2. **Current license.** You license your contribution under GPL-3.0-only,
   subject to the third-party exclusions below.
3. **Permission for a future MIT release.** For the copyright you own or are
   authorized to license, you additionally grant RoversX and the ThinkTerm
   project maintainers a perpetual, worldwide, non-exclusive, no-charge,
   royalty-free, irrevocable copyright license to use, reproduce, modify,
   prepare derivative works of, publish, distribute, and sublicense your
   contribution and those derivative works under the MIT License. This permits
   a future MIT release without asking you for further consent. Applicable
   copyright and license notices must be preserved. This permission does not
   require ThinkTerm to change its license or set a date for doing so.
4. **Patent license.** You grant RoversX, the ThinkTerm project maintainers
   and everyone who receives ThinkTerm a perpetual, worldwide, non-exclusive,
   no-charge, royalty-free, irrevocable (except as stated in this clause)
   patent license to make, have made, use, offer to sell, sell, import, and
   otherwise transfer your contribution and ThinkTerm. It covers only the
   patent claims you can license that your contribution infringes, alone or
   combined with ThinkTerm as it stood when you submitted it. If anyone starts
   patent litigation (including a cross-claim or counterclaim) alleging that
   your contribution or ThinkTerm infringes a patent, the patent licenses
   granted to them under this agreement for that contribution or ThinkTerm end
   on the date the litigation is filed.
5. **Authority to contribute.** You confirm that you have the right to make
   these grants. If your employer or another party owns rights in your work,
   you must obtain the necessary authorization before agreeing.
6. **Third-party material.** Identify any material you did not create, its
   source, and its license in the pull request, and preserve its notices.
   This agreement does not relicense third-party material or grant rights you
   do not control. Maintainers must review any such material separately for
   compatibility with both the current license and a possible MIT release.

This agreement does not revoke GPL rights already granted for released copies.
Version 1.1 also covers pull requests opened before 22 October 2026, when agreeing
came with opening one: their contributors agree in the same way unless they
object, in that pull request or in an issue. Contributions merged before that
date are not affected: they keep the terms they came under. The patent license
(clause 4) came with version 1.2 on 29 October 2026 and covers only pull
requests opened from that date. Beyond that, changes to this agreement
do not expand an earlier grant without new consent.

### Recording your agreement

Opening a pull request records it: you agree to the ThinkTerm Contributor
Agreement version 1.2 for your contributions in that pull request, including
the permission for a future MIT release and the patent license, and confirm that you are authorized
to make these grants and have identified any third-party material. The pull
request template says so; there is nothing to check.

If a pull request includes work by someone else, they agree by saying so in
the pull request from their own account, unless an authorized rights holder
grants permission for all of the identified contributions. If you cannot
agree, explain that in the pull request so the licensing issue can be
resolved before merging.

## Getting set up

System dependencies are handled by the `get-deps` script:

```console
$ ./get-deps
$ cargo build --release
```

That produces four binaries, three of whose names do not match their crate
directories:

| Binary | Crate | What it is |
| --- | --- | --- |
| `thinkterm-gui` | `wezterm-gui/` | The terminal itself |
| `thinkterm-mux-server` | `wezterm-mux-server/` | The multiplexer server |
| `thinkterm` | `wezterm/` | The CLI |
| `thinkterm-plugin-server` | `thinkterm-plugin-server/` | Runs the plugins (Snippets built in, installed ones as processes of their own) apart from the mux; started when first needed |

`wezterm/` also builds a `wezterm` binary. That is a deliberate compatibility
shim, not a leftover — the environment variables ThinkTerm sets promise that a
`wezterm` command exists on `PATH`, and third-party tooling relies on it.

## Where things are

| Path | What lives there |
| --- | --- |
| `term/` | The core terminal model: escape sequences, the cell grid. Windowing-agnostic. |
| `termwiz/` | The lower-level terminal library — parsing, capabilities, surfaces. |
| `wezterm-gui/` | The GUI. Rendering, windowing, and most ThinkTerm-specific UI. |
| `mux/` | The multiplexer model: panes, tabs, windows, domains. |
| `codec/` | The mux wire protocol. |
| `thinkterm-plugin-channel/` | How clients reach the plugin host; the mux carries a browser's frames there unread. |
| `thinkterm-proto/` | Pure data types shared by the protocol. |
| `thinkterm-core/`, `thinkterm-tui/`, `thinkterm-syntax/` | ThinkTerm's own crates. |
| `wezterm-client/` | The mux client used by the GUI and the TUI. |
| `config/` | Configuration, including the Lua layer. |

For terminal escape sequence work, `term/` is almost always the right place, and
<https://invisible-island.net/xterm/ctlseqs/ctlseqs.html> is the reference to
match — compatibility with `xterm` behavior is the goal.

## Two rules that will bite you

**`thinkterm-proto` must keep building for `wasm32-unknown-unknown`.** That
constraint is what keeps a pure client — web, mobile — able to speak the
protocol. Do not add dependencies to it that touch processes, files, sockets or
threads. The same warning is at the top of `thinkterm-proto/Cargo.toml`.

**Changing the wire protocol means bumping `CODEC_VERSION`** in
`codec/src/lib.rs`. A GUI and a mux server that disagree on it refuse to talk to
each other, so a silent change is worse than a breaking one. There is a test
asserting the current value, which will fail and remind you; update it in the
same commit.

## Iterating

`cargo check` type-checks without generating code and is much faster than a full
build:

```console
$ cargo check
```

To run a debug build with better backtraces:

```console
$ RUST_BACKTRACE=1 cargo run --bin thinkterm-gui
```

## Tests

Please include tests covering your changes.

```console
$ cargo test --all
```

There are helpers for writing terminal behavior tests — `term/src/test/` has
examples of asserting that terminal contents match expectations after a sequence
of input. Comments explaining a test's intent are appreciated; the intent is
usually harder to recover later than the mechanics.

## Submitting a pull request

Before you open it:

```console
$ rustup component add rustfmt            # once
$ cargo fmt --all
$ cargo test --all
```

Formatting settings live in `.rustfmt.toml`.

### CI is manual

`.github/workflows/ci.yml` builds and runs `cargo test --workspace` on Linux
and on Windows, and `wasm.yml` checks that the terminal core and the mux
protocol still compile for `wasm32-unknown-unknown`. Neither runs on its own:
start them from the Actions tab when you want the answer. Nothing runs
automatically on a push or a pull request, so the commands above are the only
check between a change and `main`; please actually run them.

## Documentation

Documentation changes are welcome and there is never enough of it. To preview
the docs site locally — this uses Docker or Podman:

```console
$ ci/build-docs.sh serve
```

Then open the URL it prints after the first build. Arguments are passed through
to `mkdocs`.

Note that `docs/` is still largely inherited WezTerm text and has not been
adapted for ThinkTerm yet, so expect inconsistencies there.
