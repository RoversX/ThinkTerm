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
