// The one context menu the page has open: what the wasm offered for what
// was clicked (thinkterm-web/src/menu.rs), where it was clicked, and how
// far into its submenus the pointer or the keyboard has gone. The wasm
// owns what a menu offers and what a row does; this owns only the fact
// that one is open.

import { handle } from './client';
import { refreshViews } from './client.svelte';
import { hidePanel, runPageItem, spaceMenu } from './machines.svelte';
import type { MenuItem, MenuOutcome } from './model';
import { focusTerminal, openSide } from './mobile.svelte';

export const menu = $state({
  /** The root list; empty when no menu is open. */
  items: [] as MenuItem[],
  /** Where it was asked for, in client px. */
  x: 0,
  y: 0,
  /** What it was asked for: the wasm's kind and id. */
  kind: '',
  id: '',
  /** The open submenus, as the row's index within each panel. */
  path: [] as number[],
  /** The row the keyboard is on, within the innermost panel; -1 for none. */
  selected: -1,
});

/** The button a menu was opened from, if it was one: a press on it is
    not a press elsewhere, but the same button again (`toggleMenu`). */
let trigger: Element | null = null;

export function menuTrigger(): Element | null {
  return trigger;
}

/** A button's menu: opened by the button, closed by the same button. */
export function toggleMenu(kind: string, id: string, clientX: number, clientY: number, from: Element) {
  if (menu.items.length > 0 && trigger === from) {
    closeMenu();
    return;
  }
  openMenu(kind, id, clientX, clientY, from);
}

/** The menu for `kind`/`id` at a point, if the wasm has one to offer. */
export function openMenu(kind: string, id: string, clientX: number, clientY: number, from: Element | null = null) {
  if (!handle.client) return;
  let items: MenuItem[];
  try {
    items = JSON.parse(handle.client.context_menu(kind, id)) as MenuItem[];
  } catch {
    return;
  }
  // Nothing to offer is not an empty menu: the page shows none at all.
  if (!Array.isArray(items) || items.length === 0) return;
  // The Space menu lists every machine's Spaces, as the desktop's does.
  if (kind === 'space') items = spaceMenu(items);
  menu.items = items;
  trigger = from;
  menu.kind = kind;
  menu.id = id;
  menu.x = clientX;
  menu.y = clientY;
  menu.path = [];
  menu.selected = -1;
}

/** Close the whole thing; the keyboard goes back to the terminal. */
export function closeMenu() {
  if (menu.items.length === 0) return;
  trigger = null;
  menu.items = [];
  menu.path = [];
  menu.selected = -1;
  // The menu took focus off the field the terminal types through, so
  // closing it has to hand focus back or the next key goes nowhere.
  focusTerminal();
}

/** Close the innermost submenu; the whole menu when none is open. */
export function closeInnermost() {
  if (menu.path.length === 0) {
    closeMenu();
    return;
  }
  menu.selected = menu.path[menu.path.length - 1];
  menu.path = menu.path.slice(0, -1);
}

/** Do what a row asks, then close. A row that only opens a submenu, one
    that is disabled, and a header or separator do nothing. */
export function runItem(item: MenuItem) {
  if (item.kind !== 'item' || !item.enabled || item.submenu.length > 0 || item.id === '') return;
  if (runPageItem(item.id)) {
    closeMenu();
    return;
  }
  const client = handle.client;
  if (!client) return;
  let outcome: MenuOutcome;
  try {
    outcome = JSON.parse(client.menu_action(item.id)) as MenuOutcome;
  } catch {
    closeMenu();
    return;
  }
  closeMenu();
  if (!outcome.handled) return;
  // Another Space, or a new one, puts the Remote Hosts page behind its
  // terminal, as switching Spaces does on the desktop -- and on a phone,
  // the drawer the menu was opened from.
  if (item.id === 'new-space' || item.id.startsWith('space:')) {
    openSide(false);
    hidePanel();
  }
  // What the row changed is on show before this returns, as it is for a
  // click on the chrome: the client's own notice is a frame away.
  refreshViews();
  if (typeof outcome.copy === 'string') {
    // A clipboard the browser refuses is not worth a remark: the
    // terminal's own Cmd+C still works.
    navigator.clipboard?.writeText(outcome.copy).catch(() => {});
  }
  if (outcome.paste) {
    navigator.clipboard
      ?.readText()
      .then((text) => {
        if (text) client.paste(text);
      })
      .catch(() => {});
  }
}
