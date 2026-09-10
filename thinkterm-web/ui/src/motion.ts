// What the page animates, and how long for. The desktop animates its
// overlays in; a browser expects the same, and a reader who has asked the
// system for less motion expects none -- so every duration goes through
// here and comes back zero when they have.

const REDUCED = '(prefers-reduced-motion: reduce)';

/** Whether this reader has asked for less motion, read at the moment of the
    transition rather than cached: the setting can change mid-session. */
export function reduced(): boolean {
  try {
    return window.matchMedia(REDUCED).matches;
  } catch {
    return false;
  }
}

/** A duration in ms, or none at all for a reader who asked for less. */
export function ms(duration: number): number {
  return reduced() ? 0 : duration;
}

/** The overlays' own timings: menus and the palette snap, the settings
    window -- the biggest surface the page opens -- takes a beat longer. */
export const POP = 120;
export const WINDOW = 160;
