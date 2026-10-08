// The icons heading the tabs (thinkterm-web/src/tab_icons.rs): the cards
// this page's server sent over the relay, drawn here as one stylesheet --
// each card's colours and its mark, as a mask cut out of its SVG -- and
// the class that picks a card by its id. Which card a tab gets is the
// wasm's to say; every client is handed the same cards, other machines'
// tabs included, as the desktop dresses them with its own.

import { everyClient, type Client } from './client';

type WireCard = { id: string; circle: string; glyph: string; svg: string };

export const icons = $state({
  /** Each card's class, by its id. */
  classes: {} as Record<string, string>,
});

/** The cards as the server sent them, for a client that starts later. */
let sent: string | null = null;

const HEX = /^#[0-9a-fA-F]{6}$/;

/** A card's mark as a mask. An SVG drawn as an image runs nothing, which
    is what makes an imported one safe to show. A card sent without one
    (its file unreadable there) shows its circle alone, as the desktop's
    does: a mask that hides the whole glyph. */
function mask(svg: string): string {
  if (svg === '') return 'linear-gradient(transparent, transparent)';
  return `url("data:image/svg+xml,${encodeURIComponent(svg)}")`;
}

/** Take the server's cards: draw them, and hand them to every client. A
    card whose colours are not colours is left out, and draws as the
    terminal's mark would without icons. */
export function setTabIcons(catalog: unknown) {
  const cards = (catalog as { cards?: unknown })?.cards;
  if (!Array.isArray(cards)) return;
  const rules: string[] = [];
  const classes: Record<string, string> = {};
  cards.forEach((card: WireCard, i) => {
    if (typeof card?.id !== 'string' || typeof card.svg !== 'string' || !HEX.test(card.circle) || !HEX.test(card.glyph)) return;
    classes[card.id] = `k${i}`;
    rules.push(`.ticon.k${i} { --c: ${card.circle}; --g: ${card.glyph}; --m: ${mask(card.svg)}; }`);
  });
  let style = document.getElementById('tab-icons');
  if (!(style instanceof HTMLStyleElement)) {
    style = document.createElement('style');
    style.id = 'tab-icons';
    document.head.append(style);
  }
  style.textContent = rules.join('\n');
  icons.classes = classes;
  sent = JSON.stringify(catalog);
  for (const client of everyClient()) dress(client);
}

/** A client that starts after the cards came gets them too. */
export function dress(client: Client) {
  if (sent === null) return;
  const refused = client.set_tab_icons(sent);
  if (refused !== '') console.warn('tab icons: ' + refused);
}

/** The class drawing card `id`; empty for none, or one the page has no
    picture of. */
export function iconClass(id: string | null | undefined): string {
  return id ? (icons.classes[id] ?? '') : '';
}

// For probes, beside `window.thinkterm.client`.
(window as unknown as { thinkterm: Record<string, unknown> }).thinkterm.tabIcons = icons;
