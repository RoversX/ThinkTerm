// Touch on the terminal canvas, and the drawer's swipes.
//
// The canvas's handlers are installed in the CAPTURE phase on the canvas
// itself, so they run before the wasm's own (thinkterm-web/src/input.rs),
// which sit on the same element without capture. A tap is left to them --
// their pointerdown focuses the pane and takes the terminal over, their
// pointerup completes the click -- while a drag becomes wheel notches and
// a long press becomes the pane's menu; both of those swallow the rest of
// the gesture so the wasm never starts a selection.
//
// The wasm has already seen the pointerdown by the time a gesture turns
// into a drag, so it is told the press is over with a synthetic
// `pointercancel` -- which it handles as an Up -- dispatched at the point
// the press began. At the anchor the selection is empty, which is the one
// case its Up throws the selection away instead of copying it.

import { openMenu } from './menu.svelte';
import { mobile, openSide } from './mobile.svelte';
import { cellHeight, paneAt } from './pane';
import { smoothScroll } from './settings.svelte';

/** How far a finger may wander and still be a tap. */
const SLOP = 8;
/** How long a press has to rest before it is the pane's menu. */
const HOLD = 500;
/** A press shorter than this, that did not wander, is a tap. */
const TAP = 300;
/** A cell to scroll by while the layout has not named one. */
const CELL = 17;
/** How long after a long press the browser's own menu is still refused. */
const SETTLED = 1000;
/** How much of a fling's speed is left after 16ms, and the speed (px/ms)
    below which it has stopped. */
const FLING_DECAY = 0.94;
const FLING_MIN = 0.04;
/** How far two fingers have to spread or close for one font step. */
const PINCH_STEP = 48;

type Finger = { x: number; y: number };

/** What the gesture turned out to be; `press` until it is decided. */
type Mode = 'press' | 'scroll' | 'menu';

export function installTouch(canvas: HTMLCanvasElement): () => void {
  const fingers = new Map<number, Finger>();
  /** The finger the gesture belongs to: the first one down. */
  let first: number | null = null;
  let startX = 0;
  let startY = 0;
  let startAt = 0;
  let mode: Mode = 'press';
  let hold: ReturnType<typeof setTimeout> | null = null;
  /** Where the last wheel was measured from, and the pixels not yet worth
      a whole cell. */
  let lastY = 0;
  let pending = 0;
  /** When the long press last opened a menu, so the browser's own -- which
      some phones fire on top of it -- can be refused. */
  let opened = 0;
  /** The finger's speed down the screen (px/ms, smoothed) and when it was
      last measured, for the fling that carries on after it lifts. */
  let vy = 0;
  let lastMoveAt = 0;
  let lastAt: Finger = { x: 0, y: 0 };
  let fling: number | null = null;
  /** The distance between two fingers the last time a font step was
      taken, so a pinch steps once per PINCH_STEP of travel. */
  let pinchBase = 0;

  const stopHold = () => {
    if (hold === null) return;
    clearTimeout(hold);
    hold = null;
  };

  const stopFling = () => {
    if (fling === null) return;
    cancelAnimationFrame(fling);
    fling = null;
  };

  /** Keep scrolling after the finger lifts, slowing down until it stops.
      Each frame hands the same synthetic wheels over as the finger did. */
  const startFling = () => {
    if (Math.abs(vy) < FLING_MIN) return;
    let last = performance.now();
    const tick = (now: number) => {
      const dt = Math.min(now - last, 64);
      last = now;
      scrollBy(vy * dt, lastAt);
      vy *= Math.pow(FLING_DECAY, dt / 16);
      if (Math.abs(vy) < FLING_MIN) {
        fling = null;
        return;
      }
      fling = requestAnimationFrame(tick);
    };
    fling = requestAnimationFrame(tick);
  };

  /** How far apart the two fingers are, or 0 with fewer than two. */
  const spread = (): number => {
    const pts = [...fingers.values()];
    if (pts.length < 2) return 0;
    return Math.hypot(pts[0].x - pts[1].x, pts[0].y - pts[1].y);
  };

  /** One font step for the pane under the fingers: a Ctrl+wheel the wasm
      knows is the page's (untrusted), since the browser's own pinch-zoom is
      off on the canvas. Spreading is zooming in, as Ctrl+wheel-up is. */
  const zoomStep = (dir: 1 | -1, at: Finger) => {
    canvas.dispatchEvent(
      new WheelEvent('wheel', {
        deltaY: -dir,
        deltaMode: 0,
        ctrlKey: true,
        bubbles: true,
        cancelable: true,
        clientX: at.x,
        clientY: at.y,
      }),
    );
  };

  /** The midpoint of the fingers down, which is what two of them scroll by. */
  const midpoint = (): Finger | null => {
    let x = 0;
    let y = 0;
    let n = 0;
    for (const f of fingers.values()) {
      x += f.x;
      y += f.y;
      n += 1;
    }
    return n === 0 ? null : { x: x / n, y: y / n };
  };

  /** Tell the wasm the press it saw is over, at the point it began. */
  const cancelPress = () => {
    canvas.dispatchEvent(
      new PointerEvent('pointercancel', {
        pointerId: first ?? 1,
        pointerType: 'touch',
        isPrimary: true,
        bubbles: true,
        cancelable: true,
        clientX: startX,
        clientY: startY,
      }),
    );
  };

  /** Scroll by what the fingers moved. In smooth mode every move goes over
      as it is, so the rows follow the finger; in stepped mode the wasm
      rounds any wheel up to a whole notch, so a few pixels handed over one
      event at a time would run away from the finger and the pixels are
      held back here until they are worth a cell. */
  const scrollBy = (dy: number, at: Finger) => {
    let by = dy;
    if (!smoothScroll()) {
      pending += dy;
      const cell = cellHeight(canvas, CELL);
      const cells = Math.trunc(pending / cell);
      if (cells === 0) return;
      pending -= cells * cell;
      by = cells * cell;
    } else if (by === 0) {
      return;
    }
    // Down the screen is back through the scrollback, as a wheel's is.
    canvas.dispatchEvent(
      new WheelEvent('wheel', {
        deltaY: -by,
        deltaMode: 0,
        bubbles: true,
        cancelable: true,
        clientX: at.x,
        clientY: at.y,
      }),
    );
  };

  /** The gesture is the page's from here on: the wasm's press is undone
      and every event left in the gesture is swallowed. */
  const takeOver = (next: Mode) => {
    stopHold();
    mode = next;
    cancelPress();
    const mid = midpoint();
    lastY = mid ? mid.y : startY;
    pending = 0;
    vy = 0;
    pinchBase = spread();
  };

  const down = (ev: PointerEvent) => {
    if (!ev.isTrusted || ev.pointerType !== 'touch') return;
    // A finger on the glass stops whatever a fling was still scrolling.
    stopFling();
    fingers.set(ev.pointerId, { x: ev.clientX, y: ev.clientY });
    if (first === null) {
      first = ev.pointerId;
      startX = ev.clientX;
      startY = ev.clientY;
      startAt = ev.timeStamp;
      lastY = ev.clientY;
      lastMoveAt = ev.timeStamp;
      lastAt = { x: ev.clientX, y: ev.clientY };
      pending = 0;
      vy = 0;
      mode = 'press';
      hold = setTimeout(() => {
        hold = null;
        if (mode !== 'press') return;
        takeOver('menu');
        opened = Date.now();
        const pane = paneAt(canvas, startX, startY);
        if (pane !== null) openMenu('pane', String(pane), startX, startY);
      }, HOLD);
      // The press itself goes to the wasm: it focuses the pane and takes
      // the terminal over, which is what a tap is for.
      return;
    }
    // A second finger is the page's, not the wasm's: two fingers scroll,
    // and pinch to change the font.
    ev.stopImmediatePropagation();
    ev.preventDefault();
    if (mode === 'press') takeOver('scroll');
    else pinchBase = spread();
  };

  const move = (ev: PointerEvent) => {
    if (!ev.isTrusted || ev.pointerType !== 'touch') return;
    if (!fingers.has(ev.pointerId)) return;
    fingers.set(ev.pointerId, { x: ev.clientX, y: ev.clientY });
    if (mode === 'menu') {
      ev.stopImmediatePropagation();
      return;
    }
    if (mode === 'press') {
      const far = Math.abs(ev.clientX - startX) > SLOP || Math.abs(ev.clientY - startY) > SLOP;
      if (!far) return;
      takeOver('scroll');
    }
    ev.stopImmediatePropagation();
    const mid = midpoint();
    if (!mid) return;
    lastAt = mid;
    // Two fingers: the change in their distance is a pinch, one font step
    // per PINCH_STEP of it; what is left of their motion scrolls.
    if (fingers.size >= 2) {
      const now = spread();
      if (pinchBase === 0) pinchBase = now;
      const grown = now - pinchBase;
      if (Math.abs(grown) >= PINCH_STEP) {
        zoomStep(grown > 0 ? 1 : -1, mid);
        pinchBase = now;
      }
    }
    const dy = mid.y - lastY;
    lastY = mid.y;
    const dt = ev.timeStamp - lastMoveAt;
    if (dt > 0) {
      // Smoothed, so one slow last sample does not cancel a quick flick and
      // one jittery sample does not launch one.
      vy = vy * 0.6 + (dy / dt) * 0.4;
      lastMoveAt = ev.timeStamp;
    }
    if (dy !== 0) scrollBy(dy, mid);
  };

  const up = (ev: PointerEvent) => {
    if (!ev.isTrusted || ev.pointerType !== 'touch') return;
    if (!fingers.delete(ev.pointerId)) return;
    if (mode !== 'press') {
      // The wasm was told the press was cancelled; the release must not
      // reach it as a click in whatever cell the finger ended over.
      ev.stopImmediatePropagation();
    } else if (
      ev.pointerId === first &&
      ev.timeStamp - startAt < TAP &&
      Math.abs(ev.clientX - startX) <= SLOP &&
      Math.abs(ev.clientY - startY) <= SLOP
    ) {
      // A tap goes through untouched. The soft keyboard is the page's to
      // ask for, and asking has to happen inside the press itself.
      document.getElementById('kbd')?.focus();
    }
    if (ev.pointerId !== first) {
      // One of two fingers lifted: the other carries on scrolling from
      // where it is, not from where the pair's midpoint was.
      const mid = midpoint();
      if (mid) lastY = mid.y;
      pinchBase = 0;
      return;
    }
    stopHold();
    // A flick keeps the scrollback moving; a finger that stopped before
    // lifting (more than a frame ago) does not.
    if (mode === 'scroll' && ev.timeStamp - lastMoveAt < 80) startFling();
    first = null;
    mode = 'press';
    fingers.clear();
  };

  const contextmenu = (ev: Event) => {
    // The long press already put the pane's menu up; a phone that fires
    // its own contextmenu on top of that must not open a second one.
    if (Date.now() - opened > SETTLED) return;
    ev.preventDefault();
    ev.stopImmediatePropagation();
  };

  canvas.addEventListener('pointerdown', down, true);
  canvas.addEventListener('pointermove', move, true);
  canvas.addEventListener('pointerup', up, true);
  canvas.addEventListener('pointercancel', up, true);
  canvas.addEventListener('contextmenu', contextmenu, true);
  return () => {
    stopHold();
    stopFling();
    canvas.removeEventListener('pointerdown', down, true);
    canvas.removeEventListener('pointermove', move, true);
    canvas.removeEventListener('pointerup', up, true);
    canvas.removeEventListener('pointercancel', up, true);
    canvas.removeEventListener('contextmenu', contextmenu, true);
  };
}

/** How near the window's left edge a swipe has to start to be the drawer's. */
const EDGE = 20;
/** How far it has to travel before it counts. */
const TRAVEL = 60;

/** The drawer's swipes: from the window's left edge to bring the panel out,
    back to the left to put it away. Watched on the window in the capture
    phase, so a finger on the canvas is seen before the canvas's own
    handlers swallow it. */
export function installSwipe(): () => void {
  let id: number | null = null;
  let x0 = 0;
  let y0 = 0;
  /** What this gesture could still do, once it has travelled far enough. */
  let want: 'open' | 'close' | null = null;

  const down = (ev: PointerEvent) => {
    if (!ev.isTrusted || ev.pointerType !== 'touch' || id !== null || !mobile.on) return;
    if (mobile.side) want = 'close';
    else if (ev.clientX <= EDGE) want = 'open';
    else return;
    id = ev.pointerId;
    x0 = ev.clientX;
    y0 = ev.clientY;
  };

  const move = (ev: PointerEvent) => {
    if (!ev.isTrusted || ev.pointerId !== id || want === null) return;
    const dx = ev.clientX - x0;
    const dy = ev.clientY - y0;
    // A finger going mostly down the screen is scrolling a list, not
    // moving the drawer.
    if (Math.abs(dx) < TRAVEL || Math.abs(dx) <= Math.abs(dy)) return;
    if (want === 'open' && dx > 0) openSide(true);
    else if (want === 'close' && dx < 0) openSide(false);
    want = null;
  };

  const up = (ev: PointerEvent) => {
    // The canvas cancels the press it handed the wasm when a gesture turns
    // into a drag, and that cancellation reaches this listener too: it is
    // not the finger leaving the glass, and must not end the swipe.
    if (!ev.isTrusted || ev.pointerId !== id) return;
    id = null;
    want = null;
  };

  window.addEventListener('pointerdown', down, true);
  window.addEventListener('pointermove', move, true);
  window.addEventListener('pointerup', up, true);
  window.addEventListener('pointercancel', up, true);
  return () => {
    window.removeEventListener('pointerdown', down, true);
    window.removeEventListener('pointermove', move, true);
    window.removeEventListener('pointerup', up, true);
    window.removeEventListener('pointercancel', up, true);
  };
}
