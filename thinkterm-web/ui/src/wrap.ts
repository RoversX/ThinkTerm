// A plugin panel's text broken into lines (thinkterm-plugin-panel
// `Text::wrap`), by the rules every client breaks them by
// (thinkterm-plugin-panel/src/wrap.rs, which the desktop calls and this
// follows), measured with whatever the caller measures in.

// Punctuation that closes a phrase of a script written without spaces: a
// line does not start with it.
const CLOSING = new Set(['、', '。', '，', '．', '：', '；', '！', '？', '）', '」', '』', '】', '〉', '》', '〕', 'ー', '…']);

// Chinese, Japanese, Korean, and their punctuation: scripts written
// without spaces between their words.
const UNSPACED: [number, number][] = [
  [0x2e80, 0x2fff],
  [0x3000, 0x303f],
  [0x3040, 0x30ff],
  [0x3100, 0x31ff],
  [0x3400, 0x4dbf],
  [0x4e00, 0x9fff],
  [0xac00, 0xd7af],
  [0xf900, 0xfaff],
  [0xff00, 0xffef],
  [0x20000, 0x3ffff],
];

// What a line may not be broken inside: a letter and its accents, an
// emoji's parts. Code points where the browser cannot say.
const segmenter = typeof Intl !== 'undefined' && 'Segmenter' in Intl
  ? new Intl.Segmenter(undefined, { granularity: 'grapheme' })
  : null;

function graphemes(text: string): string[] {
  return segmenter ? Array.from(segmenter.segment(text), (part) => part.segment) : Array.from(text);
}

function unspaced(ch: string): boolean {
  const code = ch.codePointAt(0) ?? 0;
  return UNSPACED.some(([from, to]) => code >= from && code <= to);
}

/** Whether a line may break before character `at` of `chars` -- whole
    characters, as `graphemes` makes them: after a space, or where the
    characters on either side are of a script written without spaces --
    but not before the punctuation that closes one. */
function breaksBefore(chars: string[], at: number): boolean {
  const before = chars[at - 1];
  const after = chars[at];
  if (before === undefined || after === undefined) return false;
  if (/\s/u.test(before)) return true;
  return (unspaced(before) || unspaced(after)) && !/\s/u.test(after) && !CLOSING.has(after);
}

/** `text` as at most `rows` lines no wider than `room` -- broken at the
    last place that keeps one within it, or else where it overflows, and
    at its own line breaks -- and whether text is left over. */
export function breakLines(
  text: string,
  room: number,
  rows: number,
  measure: (text: string) => number,
): { lines: string[]; leftOver: boolean } {
  const lines: string[] = [];
  const paragraphs = text.split(/\r?\n/);
  if (paragraphs.length > 1 && paragraphs[paragraphs.length - 1] === '') paragraphs.pop();
  for (const paragraph of paragraphs) {
    if (lines.length === rows) return { lines, leftOver: true };
    const chars = graphemes(paragraph);
    if (chars.length === 0) {
      lines.push('');
      continue;
    }
    let start = 0;
    while (start < chars.length) {
      if (lines.length === rows) return { lines, leftOver: true };
      // The most characters from `start` that fit, and at least one, the
      // spaces after them hanging past the edge; then back to the last
      // place the line may break, if it has one.
      let low = start + 1;
      let high = chars.length;
      while (low < high) {
        const mid = Math.ceil((low + high) / 2);
        if (measure(chars.slice(start, mid).join('')) <= room) low = mid;
        else high = mid - 1;
      }
      let end = low;
      while (end < chars.length && /\s/u.test(chars[end])) end++;
      if (end < chars.length) {
        for (let at = end; at > start; at--) {
          if (breaksBefore(chars, at)) {
            end = at;
            break;
          }
        }
      }
      lines.push(chars.slice(start, end).join('').trimEnd());
      start = end;
    }
  }
  return { lines, leftOver: false };
}
