# Contributing to ThinkTerm

Thanks for considering it. Anything from a typo fix upward is welcome.

ThinkTerm is a fork of [WezTerm](https://github.com/wezterm/wezterm). A large
part of this repository is still Wez Furlong's code, and changes to those parts
are often best sent upstream instead — they will reach more people there, and
ThinkTerm picks them up on the next merge. Changes to ThinkTerm's own features
belong here.

## License, and what contributing means

ThinkTerm is GPL-3.0 — see [LICENSE.md](LICENSE.md). Code inherited from WezTerm
remains under its original MIT license, preserved in
[LICENSE-MIT](LICENSE-MIT); other third-party components are listed in
[licenses/README.md](licenses/README.md).

By opening a pull request you agree to two things:

1. Your contribution is licensed under GPL-3.0.
2. You grant the ThinkTerm project a perpetual, worldwide, non-exclusive,
   royalty-free and irrevocable right to relicense your contribution under
   different terms, including a more permissive license such as MIT.

The second point is not boilerplate. ThinkTerm expects to move to MIT once the
project is more established, and without that grant the change would require
tracking down every past contributor for consent. If you are not comfortable
granting it, say so in the pull request rather than staying quiet — that is a
conversation worth having before the code is written, not after.

## Getting set up

System dependencies are handled by the `get-deps` script:

```console
$ ./get-deps
$ cargo build --release
```

That produces three binaries, whose names do not match their crate directories:

| Binary | Crate | What it is |
| --- | --- | --- |
| `thinkterm-gui` | `wezterm-gui/` | The terminal itself |
| `thinkterm-mux-server` | `wezterm-mux-server/` | The multiplexer server |
| `thinkterm` | `wezterm/` | The CLI |

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

### There is no CI on pull requests

Everything in `.github/workflows/` runs only on manual dispatch, a push to the
`ci/probe` branch, or a `v*` tag. Nothing runs automatically when you open a
pull request. The commands above are the only check between a change and `main`,
so please actually run them.

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
