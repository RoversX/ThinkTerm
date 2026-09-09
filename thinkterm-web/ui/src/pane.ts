// Which pane a point on the canvas is in, from the layout the wasm
// publishes. Its own module because two callers need it: the right-click
// on the canvas (App.svelte) and the long press that stands in for one on
// a phone (touch.ts).

type Layout = {
  cell?: [number, number];
  focused?: number;
  panes?: { id: number; left: number; top: number; cols: number; rows: number }[];
};

/** The pane under a point on the canvas: the grid starts a cell in from the
    left and half a cell down, as the desktop pads it (App::pad), and a
    placement's box is in those cells. */
export function paneAt(canvas: HTMLCanvasElement, clientX: number, clientY: number): number | null {
  let layout: Layout;
  try {
    layout = JSON.parse(canvas.dataset.layout || '{}') as Layout;
  } catch {
    return null;
  }
  const cell = layout.cell;
  const focused = typeof layout.focused === 'number' ? layout.focused : null;
  if (!cell || !(cell[0] > 0) || !(cell[1] > 0) || !Array.isArray(layout.panes)) return focused;
  const box = canvas.getBoundingClientRect();
  const col = Math.floor((clientX - box.left - cell[0]) / cell[0]);
  const row = Math.floor((clientY - box.top - cell[1] / 2) / cell[1]);
  const hit = layout.panes.find(
    (p) => col >= p.left && col < p.left + p.cols && row >= p.top && row < p.top + p.rows,
  );
  // A press in a divider or past the tab is still a press in this tab: the
  // menu is the focused pane's, as the desktop's would be.
  return hit ? hit.id : focused;
}

/** A pane's cell height in CSS px, for a gesture that scrolls by cells;
    `fallback` when the layout has not named one yet. */
export function cellHeight(canvas: HTMLCanvasElement, fallback: number): number {
  try {
    const layout = JSON.parse(canvas.dataset.layout || '{}') as Layout;
    const h = layout.cell?.[1];
    return typeof h === 'number' && h > 1 ? h : fallback;
  } catch {
    return fallback;
  }
}
