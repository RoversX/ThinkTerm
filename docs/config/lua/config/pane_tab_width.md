---
tags:
  - tab_bar
---
# `pane_tab_width = 360`

How wide each tab in a pane's header is: the row of tabs above a pane, one
to each pane of a split window. Every tab has this width, whatever its
title; when there are more tabs than fit, the header scrolls rather than
squeezing them.

It is set apart from [window_tab_width](window_tab_width.md), the width of
the tabs in the window's top row, because a header is only as wide as its
pane: in a window split into narrow columns, tabs as wide as the top row's
leave room for fewer of them.

The width is in design pixels, the unit ThinkTerm's interface is drawn in:
two to a point on macOS, and two to a pixel at 100% display scaling on
Windows and Linux. The default, 360, is 180 points. It is kept between 200
and 1200.

```lua
-- Narrower tabs in the pane headers, so more of them fit a split pane.
config.pane_tab_width = 240
```

**Settings › Appearance › Pane Header Tab Width**, right under the pane
header's font size, sets the same width. While it is set it wins over this
option; its reset button hands the width back to this option. The two
interact: a larger pane header font size fits fewer characters into the
same width.

A title too long for its tab fades out at the tab's end. Rest the pointer on
the tab and a card under it shows the whole title.

The chips of a collapsed pane stack are not these tabs: they follow
[tab_max_width](tab_max_width.md).
