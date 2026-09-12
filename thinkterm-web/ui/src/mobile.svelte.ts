// Whether the page is drawn for a thumb. Termux is the shape this follows:
// the terminal takes the whole window, the panels become drawers over it,
// and a key bar supplies what a soft keyboard has not got. Everything the
// phone shape changes hangs off `body[data-mobile]`, so a desktop browser
// is served exactly the page it was before.

/** A window narrower than this is the phone shape whatever the pointer is. */
const NARROW = 720;

/** The key bar's height, safe area aside: 44px keys with 4px of air above
    and below, so every key clears the 44px a thumb needs. The bar reports
    what it really measures as `--keybar`, which is this plus whatever the
    home indicator takes; the canvas and the drawers leave room for that. */
export const KEYBAR = 52;

export const mobile = $state({
  /** The page is in its phone shape. */
  on: false,
  /** The left panel is out as a drawer. Only read in the phone shape. */
  side: false,
  /** The soft keyboard was asked for, with the key bar's keyboard button.
      Only read in the phone shape: a desktop always wants #kbd focused,
      since focusing it costs nothing there. On a phone it costs the lower
      half of the screen, so nothing else -- a tap on the terminal, a menu
      closing, a rename finishing -- may put focus there uninvited. */
  keyboard: false,
});

/** Whether focusing #kbd is wanted right now: always on a desktop, and on
    a phone only once the keyboard button asked for the soft keyboard. */
export function wantKeyboard(): boolean {
  return !mobile.on || mobile.keyboard;
}

/** Put focus back on the field the terminal types through, where that is
    wanted (see `wantKeyboard`). Every "hand focus back to the terminal"
    site goes through here, so a phone never gets the soft keyboard from a
    panel closing under it. */
export function focusTerminal() {
  if (!wantKeyboard()) return;
  document.getElementById('kbd')?.focus();
}

/** The keyboard button: ask for the soft keyboard, or send it away. */
export function toggleKeyboard() {
  const kbd = document.getElementById('kbd');
  if (!(kbd instanceof HTMLTextAreaElement)) return;
  const up = document.activeElement === kbd;
  mobile.keyboard = !up;
  paintKeyboard();
  if (up) kbd.blur();
  else kbd.focus();
}

/** The wasm asks the same question through `body[data-keyboard]`, since
    it focuses #kbd itself after a press or a finished rename. */
function paintKeyboard() {
  if (mobile.on && mobile.keyboard) document.body.dataset.keyboard = '';
  else delete document.body.dataset.keyboard;
}

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
  mobile.keyboard = false;
  paintKeyboard();
}

/** Follow the pointer and the window's width from here on. */
export function watchMobile() {
  const coarse = window.matchMedia('(pointer: coarse)');
  const read = () => paint(coarse.matches || window.innerWidth < NARROW);
  coarse.addEventListener('change', read);
  window.addEventListener('resize', read);
  read();
  // The phone's own "done" key dismisses the soft keyboard by blurring
  // #kbd with nothing else taking focus; from then on the keyboard is not
  // wanted until the button asks again. A blur that moves focus into a
  // panel is that panel's, and the flag stays for when it closes.
  document.getElementById('kbd')?.addEventListener('blur', () => {
    if (!mobile.on) return;
    setTimeout(() => {
      if (document.activeElement === document.body || document.activeElement === null) {
        mobile.keyboard = false;
        paintKeyboard();
      }
    }, 0);
  });
}

/** Put the left drawer out, or away. */
export function openSide(open: boolean) {
  if (mobile.side === open) return;
  mobile.side = open;
  // The soft keyboard would cover most of the drawer, so it goes away while
  // the drawer is out; the keyboard button asks for it back.
  if (open) {
    mobile.keyboard = false;
    paintKeyboard();
    (document.getElementById('kbd') as HTMLTextAreaElement | null)?.blur();
  }
}
