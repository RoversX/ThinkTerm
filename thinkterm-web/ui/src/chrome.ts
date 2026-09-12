// What a click on the chrome meant, as `chrome::TabStrip::click_target`
// read it: the nearest element that carries one of the three marks, and
// the bar it sits in for the buttons that act on a bar's pane.

import { handle } from './client';
import { chromeClick, views } from './client.svelte';
import { focusTerminal, mobile, openSide } from './mobile.svelte';
import { openPalette } from './palette.svelte';
import { openSettings, setSetting } from './settings.svelte';

/** Show or hide the sidebar. The page's own: nothing the wasm shows
    depends on it, and the canvas's observer resizes the terminal when its
    box moves. Also what the palette's Toggle Sidebar comes to.

    On a phone there is no room beside the canvas, so the panel is a drawer
    over it and this is what puts it out and away; nothing is resized, and
    `body[data-side]` -- which is what makes room -- stays off it. */
export function toggleSidebar() {
  if (mobile.on) {
    openSide(!mobile.side);
    return;
  }
  const off = document.body.dataset.side === 'off';
  if (off) delete document.body.dataset.side; else document.body.dataset.side = 'off';
  // Refit in this task, before the browser paints the moved box with the
  // old bitmap stretched across it.
  handle.client?.resize();
  focusTerminal();
}

function id(el: Element | null, attr: string): number | null {
  const raw = el?.getAttribute(attr);
  if (raw === null || raw === undefined) return null;
  const n = Number(raw);
  return Number.isSafeInteger(n) && n >= 0 ? n : null;
}

export function onChromeClick(ev: MouseEvent) {
  const target = ev.target;
  if (!(target instanceof Element)) return;
  const hit = target.closest('[data-pane],[data-follow],[data-action]');
  if (!hit) return;
  let handled = false;
  const action = hit.hasAttribute('data-follow') ? 'follow' : hit.getAttribute('data-action');
  if (action === null) {
    const pane = id(hit, 'data-pane');
    handled = pane !== null && chromeClick('pane', pane);
  } else if (action === 'close-pane') {
    const pane = id(hit, 'data-pane');
    handled = pane !== null && chromeClick('close-pane', pane);
  } else if (action === 'close-tab') {
    const tab = id(hit, 'data-tab');
    handled = tab !== null && chromeClick('close-tab', null, tab);
  } else if (action === 'sidebar') {
    toggleSidebar();
    handled = true;
  } else if (action === 'search') {
    // The page's own, as the sidebar toggle is: the palette and the
    // settings panel are drawn here, not by the wasm.
    openPalette();
    handled = true;
  } else if (action === 'settings') {
    openSettings();
    handled = true;
  } else if (action === 'agents') {
    // Also the page's own: the panel is drawn here, and whether it is on
    // is a preference the wasm keeps.
    setSetting('agents-panel', !views.settings['agents-panel']);
    handled = true;
  } else if (action === 'split-right' || action === 'split-below' || action === 'zoom' || action === 'new-pane') {
    // A bar's buttons act on the bar's pane, whichever is focused.
    handled = chromeClick(action, id(hit.closest('[data-nav]'), 'data-nav'));
  } else {
    handled = chromeClick(action);
  }
  if (handled) ev.preventDefault();
}
