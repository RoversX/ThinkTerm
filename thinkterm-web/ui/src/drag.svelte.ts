// Dragging a row, a tab or a pane, as the desktop does it (wezterm-gui
// mouseevent.rs `sidebar_row_drag`, `pane_tab_drag`): a press that travels
// ~5px -- or a finger that rests -- becomes a drag, a ghost follows the
// pointer, and where it would land is shown until it is let go.
//
// Nothing here decides what a drop means. Every one goes to `client.drop`,
// which performs it or refuses it; a refusal is a snap back and no more.
// What is offered as a target is read from the model's own rows (a thread's
// project, a row's pinned flag) and from this frame's rectangles.

import { handle } from './client';
import { refreshViews, views } from './client.svelte';
import { focusTerminal } from './mobile.svelte';

/** How far a press travels before it is a drag (the desktop's ~5px). */
const THRESHOLD = 5;
/** How long a finger rests on a row before it is one. */
const HOLD = 500;
/** The band at a list's edge that scrolls it, how far a tick moves it,
    and how often a parked pointer ticks -- the desktop's numbers. */
const HOT = 32;
const STEP = 16;
const TICK = 40;

export type DragKind = 'thread' | 'project' | 'tab' | 'pane';
export type Edge = 'left' | 'right' | 'top' | 'bottom';

/** What a press has picked up: the row, capsule or tab under it. */
export type Spec = { kind: DragKind; id: string; label: string; project?: string };

type Rect = { left: number; top: number; width: number; height: number };

/** Where letting go would put it: the argument for `client.drop`, plus the
    rectangle that says so on screen. */
type Target =
  | { kind: 'before'; before: string | undefined; line: Rect }
  | { kind: 'at'; at: number; line: Rect }
  | { kind: 'pane'; target: string; edge: Edge; zone: Rect };

/** What the drag layer draws. `kind` is null while nothing is dragging. */
export const drag = $state({
  kind: null as DragKind | null,
  label: '',
  x: 0,
  y: 0,
  /** Letting go here would land somewhere; else the pointer says no. */
  ok: false,
  line: null as Rect | null,
  zone: null as Rect | null,
});

let spec: Spec | null = null;
let pointer: number | null = null;
let startX = 0;
let startY = 0;
let active = false;
let target: Target | null = null;
let hold: ReturnType<typeof setTimeout> | null = null;
let scroll: ReturnType<typeof setTimeout> | null = null;
/** When a drag last ended, so the click that press produces is swallowed
    and a later, unrelated one is not. */
let swallow = 0;

const box = (el: Element): Rect => {
  const r = el.getBoundingClientRect();
  return { left: r.left, top: r.top, width: r.width, height: r.height };
};

/** The sibling rows a sidebar drag may land between, in the model's order:
    the same project's unpinned threads, or the Space's live projects. */
function siblings(): string[] {
  if (!spec) return [];
  if (spec.kind === 'thread') {
    return views.sidebar.rows
      .filter((r) => r.kind === 'thread' && !r.pinned && r.project === spec?.project)
      .map((r) => (r.kind === 'thread' ? r.id : ''));
  }
  return views.sidebar.rows
    .filter((r) => r.kind === 'project' && !r.archived)
    .map((r) => (r.kind === 'project' ? r.id : ''));
}

function rowRects(): { id: string; rect: Rect }[] {
  if (!spec) return [];
  const attr = spec.kind === 'thread' ? 'data-thread' : 'data-project';
  const out: { id: string; rect: Rect }[] = [];
  for (const id of siblings()) {
    const el = document.querySelector(`#side .row[${attr}="${CSS.escape(id)}"]`);
    if (el) out.push({ id, rect: box(el) });
  }
  return out;
}

/** The gap the pointer is in, as `sidebar_insert_position` picks it: before
    the first row whose middle is below it, else last. */
function sideTarget(x: number, y: number): Target | null {
  const side = document.getElementById('side');
  const list = document.querySelector('#side .list');
  if (!side || !list || !spec) return null;
  const panel = box(side);
  if (x < panel.left || x >= panel.left + panel.width) return null;
  const rows = rowRects();
  if (rows.length === 0) return null;
  const first = rows[0].rect;
  const last = rows[rows.length - 1].rect;
  // A thread pointer that leaves its own list is aiming at no gap of it, so
  // the drag is refused rather than snapped to an end -- which is what keeps
  // a thread dragged onto another project a no-op. A project's list is the
  // whole panel, so anywhere below it simply means the end.
  if (spec.kind === 'thread') {
    const slack = first.height;
    if (y < first.top - slack || y > last.top + last.height + slack) return null;
  }
  const inset = box(list);
  const line = (top: number): Rect => ({ left: inset.left, top: top - 1, width: inset.width, height: 2 });
  for (const row of rows) {
    if (y < row.rect.top + row.rect.height / 2) {
      return { kind: 'before', before: row.id, line: line(row.rect.top) };
    }
  }
  return { kind: 'before', before: undefined, line: line(last.top + last.height) };
}

/** Which gap of the strip the pointer is in, and the index the tab lands on
    once it has left its own place. */
function tabTarget(x: number, y: number): Target | null {
  const strip = document.getElementById('tabs');
  if (!strip || !spec) return null;
  const bounds = box(strip);
  if (x < bounds.left || x >= bounds.left + bounds.width) return null;
  if (y < bounds.top || y >= bounds.top + bounds.height) return null;
  const caps = Array.from(strip.querySelectorAll('.tab')).filter(
    (el) => el.getAttribute('data-tab') !== spec?.id,
  );
  const rects = caps.map(box);
  const at = rects.filter((r) => r.left + r.width / 2 < x).length;
  const edge = at < rects.length ? rects[at].left : rects.length > 0 ? rects[at - 1].left + rects[at - 1].width : x;
  const top = rects.length > 0 ? rects[0].top : bounds.top + 4;
  const height = rects.length > 0 ? rects[0].height : bounds.height - 8;
  return { kind: 'at', at, line: { left: edge - 1, top, width: 2, height } };
}

type Layout = {
  cell?: [number, number];
  panes?: { id: number; left: number; top: number; cols: number; rows: number }[];
};

/** The pane under the pointer and the half of it nearest the pointer, from
    the layout the wasm publishes: its boxes are in cells, and the grid
    starts a cell in and half a cell down (App::pad), as `paneAt` reads it. */
function paneTarget(x: number, y: number): Target | null {
  const canvas = document.getElementById('term');
  if (!(canvas instanceof HTMLCanvasElement) || !spec) return null;
  let layout: Layout;
  try {
    layout = JSON.parse(canvas.dataset.layout || '{}') as Layout;
  } catch {
    return null;
  }
  const cell = layout.cell;
  if (!cell || !(cell[0] > 0) || !(cell[1] > 0) || !Array.isArray(layout.panes)) return null;
  const bounds = box(canvas);
  const x0 = bounds.left + cell[0];
  const y0 = bounds.top + cell[1] / 2;
  for (const p of layout.panes) {
    const rect = {
      left: x0 + p.left * cell[0],
      top: y0 + p.top * cell[1],
      width: p.cols * cell[0],
      height: p.rows * cell[1],
    };
    if (x < rect.left || x >= rect.left + rect.width || y < rect.top || y >= rect.top + rect.height) continue;
    if (String(p.id) === spec.id) return null;
    const fx = (x - rect.left) / Math.max(rect.width, 1);
    const fy = (y - rect.top) / Math.max(rect.height, 1);
    let edge: Edge = 'left';
    let near = fx;
    for (const [d, e] of [[1 - fx, 'right'], [fy, 'top'], [1 - fy, 'bottom']] as [number, Edge][]) {
      if (d < near) {
        near = d;
        edge = e;
      }
    }
    const zone =
      edge === 'left' ? { ...rect, width: rect.width / 2 }
      : edge === 'right' ? { ...rect, left: rect.left + rect.width / 2, width: rect.width / 2 }
      : edge === 'top' ? { ...rect, height: rect.height / 2 }
      : { ...rect, top: rect.top + rect.height / 2, height: rect.height / 2 };
    return { kind: 'pane', target: String(p.id), edge, zone };
  }
  return null;
}

function retarget(x: number, y: number) {
  if (!spec) return;
  target =
    spec.kind === 'pane' ? paneTarget(x, y)
    : spec.kind === 'tab' ? tabTarget(x, y)
    : sideTarget(x, y);
  drag.ok = target !== null;
  drag.line = target && target.kind !== 'pane' ? target.line : null;
  drag.zone = target && target.kind === 'pane' ? target.zone : null;
}

/** What scrolls the tab row: its strip on a phone, where the buttons at
    either end stay put over it; elsewhere the strip is no box of its own,
    and the row scrolls. */
function tabScroller(): HTMLElement | null {
  const strip = document.querySelector<HTMLElement>('#tabs .strip');
  return strip && getComputedStyle(strip).display !== 'contents' ? strip : document.getElementById('tabs');
}

/** The list under the pointer scrolls while the drag rests at its edge; a
    parked pointer sends no more moves, so each applied step queues the next. */
function autoscroll(x: number, y: number) {
  if (!spec || spec.kind === 'pane') return;
  const el =
    spec.kind === 'tab'
      ? tabScroller()
      : document.querySelector<HTMLElement>('#side .list');
  if (!el) return;
  const bounds = box(el);
  const across = spec.kind === 'tab';
  const at = across ? x : y;
  const start = across ? bounds.left : bounds.top;
  const end = start + (across ? bounds.width : bounds.height);
  const delta = at < start + HOT ? -STEP : at > end - HOT ? STEP : 0;
  if (delta === 0) return;
  const before = across ? el.scrollLeft : el.scrollTop;
  if (across) el.scrollLeft = before + delta;
  else el.scrollTop = before + delta;
  if ((across ? el.scrollLeft : el.scrollTop) === before) return;
  if (scroll !== null) return;
  scroll = setTimeout(() => {
    scroll = null;
    if (!active) return;
    autoscroll(drag.x, drag.y);
    retarget(drag.x, drag.y);
  }, TICK);
}

function begin(x: number, y: number) {
  if (!spec || active) return;
  active = true;
  drag.kind = spec.kind;
  drag.label = spec.label;
  document.body.classList.add('dnd');
  retarget(x, y);
}

/** Put everything back; `commit` says whether the drop is performed first. */
/** The next click is the press's, not the row's: set when a drag or a
    cancel ate a press a click will still follow. */
let swallowNext = false;

function finish(commit: boolean, clickFollows = false) {
  if (hold !== null) {
    clearTimeout(hold);
    hold = null;
  }
  if (scroll !== null) {
    clearTimeout(scroll);
    scroll = null;
  }
  // Only a press that will produce a click is swallowed: a finger that
  // travelled and a cancelled pointer produce none, and the next tap
  // must not pay for them.
  if (clickFollows) {
    swallow = Date.now();
    swallowNext = true;
  }
  const landed = commit && active ? target : null;
  const picked = spec;
  spec = null;
  pointer = null;
  active = false;
  target = null;
  drag.kind = null;
  drag.line = null;
  drag.zone = null;
  drag.ok = false;
  document.body.classList.remove('dnd', 'dnd-no');
  if (!landed || !picked || !handle.client) return;
  // The model performs it or refuses it; a refusal draws nothing.
  if (landed.kind === 'pane') handle.client.drop('pane', picked.id, landed.target, landed.edge, null);
  else if (landed.kind === 'at') handle.client.drop('tab', picked.id, null, null, landed.at);
  else handle.client.drop(picked.kind, picked.id, landed.before ?? null, null, null);
  refreshViews();
  // The terminal keeps the keyboard, as it had it before the press.
  focusTerminal();
}

function onMove(ev: PointerEvent) {
  if (!spec || ev.pointerId !== pointer) return;
  // A release the page never saw (over another window, a Cmd+Tab) ends
  // it on the next move with no button down.
  if (ev.pointerType === 'mouse' && ev.buttons === 0) {
    finish(false);
    return;
  }
  drag.x = ev.clientX;
  drag.y = ev.clientY;
  if (!active) {
    const dx = ev.clientX - startX;
    const dy = ev.clientY - startY;
    if (dx * dx + dy * dy < THRESHOLD * THRESHOLD) return;
    // A finger that wandered before it rested was scrolling the list.
    if (ev.pointerType === 'touch') {
      finish(false);
      return;
    }
    begin(ev.clientX, ev.clientY);
  }
  autoscroll(ev.clientX, ev.clientY);
  retarget(ev.clientX, ev.clientY);
  document.body.classList.toggle('dnd-no', !drag.ok);
}

function onUp(ev: PointerEvent) {
  if (!spec || ev.pointerId !== pointer) return;
  const up = ev.type === 'pointerup';
  // A mouse release produces a click; a lifted finger that dragged
  // does not.
  finish(up, up && active && ev.pointerType === 'mouse');
}

function onKey(ev: KeyboardEvent) {
  if (spec && ev.key === 'Escape') {
    ev.stopPropagation();
    // The button is still down: its release will click whatever is
    // under it, and that click is the cancelled drag's.
    finish(false, active);
  }
}

function onBlur() {
  if (spec) finish(false);
}

/** A drag ate the press, so the click it produced is not the row's. */
function onClick(ev: MouseEvent) {
  if (!swallowNext || Date.now() - swallow > 1500) {
    swallowNext = false;
    return;
  }
  swallowNext = false;
  swallow = 0;
  ev.stopPropagation();
  ev.preventDefault();
}

/** A finger that has taken over must not scroll the page under it. */
function onTouchMove(ev: TouchEvent) {
  if (active && ev.cancelable) ev.preventDefault();
}

/** Arm a drag on this press. It becomes one once the pointer travels, or
    once a finger has rested; until then the press is still a click. */
export function armDrag(ev: PointerEvent, picked: Spec) {
  if (spec !== null) return;
  if (ev.pointerType === 'mouse' && ev.button !== 0) return;
  spec = picked;
  pointer = ev.pointerId;
  startX = ev.clientX;
  startY = ev.clientY;
  drag.x = ev.clientX;
  drag.y = ev.clientY;
  active = false;
  target = null;
  if (ev.pointerType === 'touch') {
    hold = setTimeout(() => {
      hold = null;
      begin(drag.x, drag.y);
    }, HOLD);
  }
}

/** The window listeners the drag runs on, installed once by the drag layer. */
export function installDrag(): () => void {
  window.addEventListener('pointermove', onMove);
  window.addEventListener('pointerup', onUp);
  window.addEventListener('pointercancel', onUp);
  window.addEventListener('blur', onBlur);
  window.addEventListener('keydown', onKey, true);
  window.addEventListener('click', onClick, true);
  window.addEventListener('touchmove', onTouchMove, { passive: false });
  return () => {
    window.removeEventListener('pointermove', onMove);
    window.removeEventListener('pointerup', onUp);
    window.removeEventListener('pointercancel', onUp);
    window.removeEventListener('blur', onBlur);
    window.removeEventListener('keydown', onKey, true);
    window.removeEventListener('click', onClick, true);
    window.removeEventListener('touchmove', onTouchMove);
  };
}
