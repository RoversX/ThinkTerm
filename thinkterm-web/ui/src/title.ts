// Titles are shown short; the full one is the tooltip. What a title says
// is the wasm's rule (`navbar::display_title`); how much of it fits a
// capsule is the page's.
export function short(title: string): string {
  const MAX = 28;
  const chars = Array.from(title);
  return chars.length <= MAX ? title : chars.slice(0, MAX - 1).join('') + '…';
}
