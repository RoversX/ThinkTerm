// The one context menu the page has open: what the wasm offered for what
// was clicked (thinkterm-web/src/menu.rs), where it was clicked, and how
// far into its submenus the pointer or the keyboard has gone. The wasm
// owns what a menu offers and what a row does; this owns only the fact
// that one is open.

import { handle } from './client';
import { refreshViews } from './client.svelte';
import type { MenuItem, MenuOutcome } from './model';
import { focusTerminal } from './mobile.svelte';

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

/** The menu for `kind`/`id` at a point, if the wasm has one to offer. */
export function openMenu(kind: string, id: string, clientX: number, clientY: number) {
  if (!handle.client) return;
  let items: MenuItem[];
  try {
    items = JSON.parse(handle.client.context_menu(kind, id)) as MenuItem[];
  } catch {
    return;
  }
  // Nothing to offer is not an empty menu: the page shows none at all.
  if (!Array.isArray(items) || items.length === 0) return;
  menu.items = items;
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
