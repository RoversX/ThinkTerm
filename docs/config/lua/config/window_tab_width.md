---
tags:
  - tab_bar
---
# `window_tab_width = 360`

How wide each tab in the window's top tab row is. Every tab has this
width, whatever its title; when there are more tabs than fit, the row
scrolls rather than squeezing them.

The width is in design pixels, the unit ThinkTerm's interface is drawn in:
two to a point on macOS, and two to a pixel at 100% display scaling on
Windows and Linux. The default, 360, is 180 points. It is kept between 200
and 1200.

```lua
-- Tabs wider than the default.
config.window_tab_width = 480
```

**Settings › Appearance › Tab Width**, right under the tab bar's font size,
sets the same width. While it is set it wins over this option; its reset
button hands the width back to this option. The two interact: a larger tab
font size fits fewer characters into the same width.

A title too long for its tab fades out at the tab's end. Rest the pointer on
the tab and a card under it shows the whole title.

This option does not size the tabs of the retro tab bar, the bar that
[use_fancy_tab_bar = false](use_fancy_tab_bar.md) adds inside the terminal
area; those follow [tab_max_width](tab_max_width.md). Nor does
`tab_max_width` size these. The tabs in a pane's header have a width of
their own, [pane_tab_width](pane_tab_width.md).
