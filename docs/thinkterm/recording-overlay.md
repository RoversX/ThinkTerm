# Recording masks

Right-click a terminal and choose **Edit Recording Masks…**. If a terminal
application captures the mouse, hold the mouse-reporting bypass modifier
(Shift by default) while right-clicking.

- The floating toolbar starts near the bottom of the window. Drag its top handle
  or the gaps between buttons to move it.
- Drag an empty area to add an opaque black rectangle.
- Drag a rectangle to move it, or drag its lower-right handle to resize it.
- Use **Delete Selected** or Backspace/Delete to remove the selected rectangle.
- Choose **Done** or press Escape to keep the masks and resume normal terminal input.
- Reopen **Edit Recording Masks…** to adjust them, or choose **Clear Recording Masks**
  from the terminal context menu to remove them all.

Each terminal pane owns its own transparent masking layer, with up to 64 masks.
Switching terminals or splitting a tab does not transfer masks to other panes.
Masks follow their pane between windows in the same GUI process and scale with it,
including its Live Overview thumbnail and opening/closing transition. Clearing masks
only affects the selected pane. The floating toolbar remains a window-level editor.

Masks stay at fixed relative positions within the pane; they do not follow text
when the terminal scrolls. They are local visual state, discarded when the pane
closes or the GUI process exits. They are not synchronized to other GUI clients.

This is a visual cover for screen recording. Terminal contents, clipboard copies,
logs, and remote clients retain the original text. Check the recording preview
after changing the window layout or scrolling sensitive content.

## Rendering and validation

Recorded previews and transition captures remove the covered source quads before
adding the black masks. This keeps text, images, and emoji hidden when the entire
picture fades or scales. A mask edit invalidates the affected thumbnail and any
partial rebuild; the card may briefly show its background while rebuilding.
Snapshots retain their pane's mask layer even after the pane closes. The preview
mapping uses the terminal renderer's actual text-grid geometry, including padding,
the navigation bar, split boundaries, and per-pane font metrics.

Regression tests cover partial-opacity/scaled quad replay, texture and gradient
coordinates, overlapping masks, geometry mapping, and snapshot/cache lifetime.
These checks do not replace a GUI screen-recording check on the target backend.
