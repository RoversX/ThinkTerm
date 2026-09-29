# Browser client: performance baseline

Measured 2026-09-09 (the four-pane run after the mirror landed), on an
Apple-silicon Mac, headless Chrome (`--headless=new --enable-unsafe-webgpu`),
release server, release bundle through `ci/build-web.sh` with `wasm-opt -Oz` (binaryen 132).
Three runs of `thinkterm-web/smoke/bench.sh`; the spread between runs is
given where it matters. Nothing here is a target: it is the number to beat,
and the number a change must not make worse.

## Bundle

| file | raw | over the wire (gzip) |
|---|---|---|
| `thinkterm_web_bg.wasm` | 3.63 MB (4.60 MB before `wasm-opt`) | 1.30 MB |
| `thinkterm_web.js` | 83 KB | 15 KB |
| JetBrains Mono Regular | 274 KB | 128 KB |
| Symbols Nerd Font Mono | 2.33 MB | 1.42 MB |

The symbols face is larger than the wasm on the wire.

## Load

| | |
|---|---|
| DOMContentLoaded | 28-37 ms |
| first status line | 15-35 ms |
| attached to the pane | 121-197 ms |

Loopback; the wasm fetch itself is ~22 ms. Attach varies by ±40 ms from run
to run with nothing changed, so treat a difference below that as noise.

## Idle

3 s after attaching: 0-2 frames asked for, no long tasks, JS heap 7.7 MB,
renderer RSS ~250 MB (the whole headless renderer, before any terminal
content). The page does not repaint on its own.

## A screen of fresh CJK (30 rows of Chinese, Korean, Japanese and symbols)

| | first screen | a second screen of *different* fresh CJK |
|---|---|---|
| frames | 3 | 2-3 |
| worst gap between frames | 23-28 ms | 16 ms |
| long tasks (>50 ms) | none | none |
| CPU profile, top self time | `fillText` 21-25 ms in total | `fillText` ~4 ms |

Before the page warmed the canvas (`GlyphCache::warm`, run 250 ms after the
first frame) the first screen carried one 70-79 ms task, 44 ms of it in the
first `clearRect`: the 2D context and its readback path coming into being.
That was a one-time cost, not per glyph -- a second cold screen never had
it -- and the warm-up moves it to a moment nobody is waiting on. From then
on the 8 ms per-frame fallback budget holds.

Repainting the same CJK from the cache: 2 frames, 11 ms apart.

## Sustained output (300 lines, one every 10 ms)

219-253 frames in 5 s; gaps median 17 ms, p95 18 ms. That is vsync pacing
with nothing dropped. The largest gap (23-207 ms) is the python interpreter
starting between the echoed command and its first line, not a paint.

## A burst (`seq 1 20000`)

3 frames. The client fetches what is on screen when it asks, not every
intermediate state, so a burst that finishes between two fetches is one
repaint; this is not a stress test.

## Four panes, a screen of fresh CJK in each

Three splits, then the CJK screen sent to all four at once: 4 frames, gaps
17 ms, no long task, RSS +10 MB over the one-pane run. Four panes' quads
go through the same allocator and draw call as one pane's; the fallback
budget is shared, so the last pane's glyphs may arrive a frame later.

## After all of it

Renderer RSS 273-285 MB (+25-35 MB over idle: atlas, scrollback, caches);
JS heap 2.6-3.2 MB. wasm linear memory is not in the JS heap number; RSS is
the one to watch.

## Reading a new run

`thinkterm-web/smoke/bench.sh` prints one JSON line. `frames` and `raf_asked` count the page's
own `requestAnimationFrame` calls (wrapped before the page loads); `gap_ms`
is between those frames; `long_tasks` come from a `PerformanceObserver`;
`renderer_rss_mb` is the largest `--type=renderer` child of the Chrome that
was launched. Compare like with like: headless Chrome's canvas and GPU
paths are not the same as a windowed browser's, and the first-use cost
above may differ there.
