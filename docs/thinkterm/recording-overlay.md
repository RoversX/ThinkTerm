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

The canvas is transparent outside the masks. Masks stay at fixed window positions;
they do not follow text when the terminal scrolls. They belong to the current window
and are discarded when it closes. Up to 64 masks can be added.

This is a visual cover for screen recording. Terminal contents, clipboard copies,
logs, and remote clients retain the original text. Check the recording preview
after changing the window layout or scrolling sensitive content.
