// The room a plugin panel's extended view has, left of the right panel:
// sized as the desktop sizes it (`right_sidebar.rs`, the file preview's
// widths), and kept per plugin in this browser.

export const EXTENDED_MIN = 360;
export const EXTENDED_DEFAULT = 560;

/** The most the extended view can have: what is left of the window beside
    the two panels, less the fifth the terminal keeps. None on a phone,
    whose panels are drawers over the terminal. */
export function extendedRoom(): number {
  if (document.body.dataset.mobile !== undefined) return 0;
  const style = getComputedStyle(document.body);
  const px = (name: string) => parseFloat(style.getPropertyValue(name)) || 0;
  return window.innerWidth - px('--side-w') - px('--agents-w') - window.innerWidth / 5;
}

/** Whether there is room for an extended view at all. */
export function canExtend(): boolean {
  return extendedRoom() >= EXTENDED_MIN;
}

const STORE = 'thinkterm.plugin-extended-widths';

function stored(): Record<string, number> {
  try {
    const raw = localStorage.getItem(STORE);
    const widths = raw ? (JSON.parse(raw) as unknown) : null;
    if (widths && typeof widths === 'object') return widths as Record<string, number>;
  } catch {
    // A browser that blocks storage starts every plugin at the default.
  }
  return {};
}

/** How wide plugin `plugin`'s extended view was last made here. */
export function storedWidth(plugin: string): number {
  const width = stored()[plugin];
  return typeof width === 'number' && Number.isFinite(width) ? width : EXTENDED_DEFAULT;
}

/** Keeps `width` for plugin `plugin`, and forgets the plugins no longer
    among `plugins` -- every one the host lists, on or off, as the desktop
    keeps them; `null` while the list is not known, when none is forgotten. */
export function storeWidth(plugin: string, width: number, plugins: string[] | null) {
  const widths: Record<string, number> = {};
  for (const [id, kept] of Object.entries(stored())) {
    if (plugins === null || plugins.includes(id)) widths[id] = kept;
  }
  widths[plugin] = Math.round(width);
  try {
    localStorage.setItem(STORE, JSON.stringify(widths));
  } catch {
    // As above: the width is then this page load's only.
  }
}
