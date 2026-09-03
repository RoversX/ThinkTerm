ThinkTerm bundles some components provided by third parties. ThinkTerm as a
whole is distributed under the GNU General Public License v3.0 only (see
[LICENSE.md](../LICENSE.md)), but the components below remain under their own
licenses, and their copyright notices are preserved as those licenses require.
The [NOTICE](../NOTICE) file at the repository root carries the attribution and
license texts that those components require to be shipped with the binaries.

## WezTerm

ThinkTerm is a fork of [WezTerm](https://github.com/wezterm/wezterm) by Wez
Furlong. Terminal emulation, font rendering, GPU drawing, the multiplexer
protocol and the SSH transport all originate there. That code remains under the
MIT license, reproduced verbatim in [LICENSE-MIT](../LICENSE-MIT).

Workspace crates that are still substantially unmodified WezTerm code continue
to declare `license = "MIT"` in their own `Cargo.toml`. That is deliberate and
accurate — the MIT grant on that code is not withdrawn by ThinkTerm's GPL-3.0-only
license on the combined work.

## Bundled fonts

The font files live in `assets/fonts/`, along with the full license texts:
`assets/fonts/LICENSE_OFL.txt` (SIL Open Font License 1.1) and
`assets/fonts/LICENSE_POWERLINE_EXTRA.txt` (MIT, Copyright (c) 2016 Ryan L
McIntyre, covering the Powerline Extra Symbols).

The following notice is preserved verbatim from WezTerm:

> WezTerm bundles `JetBrains Mono`, `Noto Color Emoji` and `Roboto` fonts.
> Those are distributed under the terms of the OFL 1.1, the text of which
> can be found in the assets/fonts directory.
>
> WezTerm bundles `Symbols Nerd Font Mono`, built from only those icon sets
> available from https://github.com/ryanoasis/nerd-fonts which are clearly
> distributed under the terms of the OFL 1.1.
> Note that WezTerm excludes the Pomicons icon set from this collection.

`FiraCode-Regular.ttf` is also shipped in `assets/fonts/`. Fira Code is
distributed by its authors under the OFL 1.1; the inherited notice above
predates it and does not enumerate it.

## ANGLE

See [ANGLE.md](ANGLE.md).

## Agent detection manifests

The agent manifests under `mux/src/agent_status/manifests/` are sourced from
[herdr](https://github.com/herdrdev/herdr) and remain under the Apache License
2.0, reproduced verbatim in
[`mux/src/agent_status/manifests/LICENSE`](../mux/src/agent_status/manifests/LICENSE).
Each bundled manifest carries an attribution header naming the upstream commit
it was taken from and any local changes.

## Icons

Icons come from [Lucide](https://github.com/lucide-icons/lucide),
[Simple Icons](https://github.com/simple-icons/simple-icons),
[Lobe Icons](https://github.com/lobehub/lobe-icons), and material-icon-theme.
Their provenance and license texts are recorded in [NOTICE](../NOTICE).

## Bundled Windows runtime components

Windows packages include ANGLE, Microsoft Terminal ConPTY components, and a
Mesa software OpenGL fallback. Their notices are recorded in [NOTICE](../NOTICE).
