// Whether the page is drawn for a thumb. Termux is the shape this follows:
// the terminal takes the whole window, the panels become drawers over it,
// and a key bar supplies what a soft keyboard has not got. Everything the
// phone shape changes hangs off `body[data-mobile]`, so a desktop browser
// is served exactly the page it was before.

/** A window narrower than this is the phone shape whatever the pointer is. */
const NARROW = 720;

/** The key bar's height. The canvas and the drawers leave room for it, and
    it is what the bar is offset by from the foot of the visual viewport. */
export const KEYBAR = 44;

export const mobile = $state({
  /** The page is in its phone shape. */
  on: false,
  /** The left panel is out as a drawer. Only read in the phone shape. */
  side: false,
});

function paint(on: boolean) {
  if (mobile.on === on) return;
  mobile.on = on;
  if (on) {
    document.body.dataset.mobile = '';
    return;
  }
  delete document.body.dataset.mobile;
  // The drawer is the phone shape's; a window that grew back into the
  // desktop shape shows the panel beside the canvas again.
  mobile.side = false;
}

/** Follow the pointer and the window's width from here on. */
export function watchMobile() {
  const coarse = window.matchMedia('(pointer: coarse)');
  const read = () => paint(coarse.matches || window.innerWidth < NARROW);
  coarse.addEventListener('change', read);
  window.addEventListener('resize', read);
  read();
}

/** Put the left drawer out, or away. */
export function openSide(open: boolean) {
  if (mobile.side === open) return;
  mobile.side = open;
  // The soft keyboard would cover most of the drawer, so it goes away while
  // the drawer is out; the next tap on a pane asks for it back.
  if (open) (document.getElementById('kbd') as HTMLTextAreaElement | null)?.blur();
}
