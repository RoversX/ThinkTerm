# Interface colours

**Status:** both `config.ui_colors` and Settings → Appearance → Theme →
*Follow terminal colours* have landed.

ThinkTerm paints two sets of colours. [`colors`](../config/lua/config/colors.md)
and `color_scheme` set the palette your *programs* draw with. `ui_colors` sets
the colours ThinkTerm draws *around* them: the sidebars, the tab bar, the bars
above each pane, the settings window and the menus.

The interface ships two hand-tuned palettes, one light and one dark, chosen by
Settings → Appearance → Theme. A fourth choice there, **Follow terminal
colours**, derives them from whichever scheme the terminal is using instead, so
a Gruvbox terminal gets a Gruvbox sidebar. `ui_colors` overrides either, slot by
slot.

## Follow terminal colours

The derivation does not invent a palette per scheme. It takes the hand-tuned
one and *moves* it onto the scheme's background, keeping every relationship
that palette already encodes -- which surface sits above which, by how much, at
what alpha -- and taking the scheme's tint. Light or dark is then read off the
scheme's own background rather than being a separate choice.

Two deliberate limits:

- **The accent stays.** A scheme's own blue is frequently unreadable against
  its own background, and the accent is the one slot where being wrong is loud.
  `accent`, `accent_hover`, `on_accent`, `selected_bg`, `danger` and
  `spelling_error` keep their values; set them through `ui_colors` if you want
  them to match too.
- **Text is held to a contrast floor** against the hardest of the surfaces it
  is read on -- 7:1 for primary, 4.5:1 for secondary, 3:1 for muted. The
  Spaces strip is recessed from the window, so a colour held to its floor
  against the window alone would sit under it on the strip. A scheme whose
  foreground sits close to its background would otherwise produce a sidebar
  whose secondary text is a rumour. This is why a derived colour is sometimes
  not exactly the shade the arithmetic asked for.
- **The ramp is slid to fit, not clipped.** On a scheme whose background is at
  the very top or bottom of the range there is no room above or below it for
  every surface, so the whole set moves together rather than each surface
  pinning to the end on its own. A pure-white scheme therefore gets a faintly
  grey sidebar: the alternative is a control, its hover state and a card all
  being the same white.

Previewing a scheme in the command palette moves the interface too, so what
you see while arrowing through the list is what you get when you press Enter.
The preview is shown, never sent: a remote pane renders it without telling the
mux server, and Escape leaves nothing behind.

One thing does not follow yet:

- **The Live Overview** draws from its own set of colours.

## Setting it

Every field is optional and overrides exactly one slot. Whatever you leave
unset keeps the colour the interface would have used, so a file that names two
colours changes two colours:

```lua
config.ui_colors = {
  sidebar_bg = '#282828',
  accent = '#d79921',
}
```

Colours are written the same way as everywhere else in the configuration:
`#RGB`, `#RRGGBB`, `#RRGGBBAA`, `rgb:`/`rgba:`, `hsl:`, or a CSS colour name.

Use `wezterm.config_builder()` and a misspelled slot is a startup error that
names the closest matches; with a plain table it is silently ignored, as it is
for every other option.

## The slots

**Surfaces** -- `window_bg` (the ground behind everything), `sidebar_bg`,
`workspace_sidebar_bg` (the Spaces strip, recessed from the sidebar),
`header_bg` (the strip above the tabs), `card_bg` (a grouped card floating on
`window_bg`).

**Controls** -- `control_bg` at rest, `control_hover_bg` under the pointer,
`control_pressed_bg` while held, `control_border` for the hairline outline,
`track_off` for the off half of a switch, `separator` for rules.

**The sidebar** -- `sidebar_button_bg` and `sidebar_button_hover_bg` for the
round buttons; `sidebar_row_hover_bg`, `sidebar_row_pressed_bg`,
`sidebar_row_active_bg` and `sidebar_row_active_border` for thread rows.

**Accent and state** -- `accent` (focus rings, switches, the active state),
`accent_hover`, `on_accent` (what is drawn on top of the accent), `selected_bg`,
`danger`.

**Text** -- `text`, `secondary_text`, `muted_text`, `selected_text`,
`scrollbar_thumb`, `spelling_error`.

## Two things to know

A colour you name wins over everything else, including the tuning the context
and command menus apply to their own cards. If you set `control_bg`, every
control gets it.

Some slots are translucent by default -- `separator`, `control_border`,
`card_bg` and `scrollbar_thumb` among them -- so that they pick up whatever is
painted behind. A value you set replaces the whole colour, alpha included, so
give an opaque colour only if you want an opaque result.

Like every other option this can be set per window from lua:
`window:set_config_overrides { ui_colors = { ... } }`.
