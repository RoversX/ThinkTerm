---
tags:
  - tab_bar
---
# `tab_max_width`

Specifies the maximum width, measured in cells, that a tab can have in the
retro tab bar: the bar of tabs that
[use_fancy_tab_bar = false](use_fancy_tab_bar.md) adds inside the terminal
area. It is also the width handed to
[format-tab-title](../window-events/format-tab-title.md) as `max_width`, and
it sets how wide the chips of a collapsed pane stack are.

It does not apply to the tabs in the window's top tab row, which are always
drawn in the fancy style: setting it changes nothing there. Their width is
[window_tab_width](window_tab_width.md), or **Settings › Appearance › Tab
Width**. Nor does it apply to the tabs in a pane's header, whose width is
[pane_tab_width](pane_tab_width.md).

Defaults to 16 glyphs in width.

```lua
config.tab_max_width = 16
```
