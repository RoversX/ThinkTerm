# Browser client: performance baseline

Measured 2026-09-09 on `web-client` after 6473846, on an Apple-silicon Mac,
headless Chrome (`--headless=new --enable-unsafe-webgpu`), release server,
release bundle through `ci/build-web.sh` with `wasm-opt -Oz` (binaryen 132).
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
| attached to the pane | 121-142 ms |

Loopback; the wasm fetch itself is ~22 ms.

## Idle

3 s after attaching: 0-2 frames asked for, no long tasks, JS heap 7.7 MB,
renderer RSS ~250 MB (the whole headless renderer, before any terminal
content). The page does not repaint on its own.

## A screen of fresh CJK (30 rows of Chinese, Korean, Japanese and symbols)

| | first screen | a second screen of *different* fresh CJK |
|---|---|---|
| frames | 2-3 | 2-3 |
| worst gap between frames | 71-79 ms | 30-44 ms |
| long tasks (>50 ms) | one, 70-73 ms | none |
| CPU profile, top self time | `clearRect` 44 ms, glue 24 ms | `fillText` 4 ms, `getImageData` 1 ms |

The long task is a one-time cost of the 2D canvas's first use (the first
`clearRect` waits for the context and its readback pipeline to exist), not
a per-glyph cost: the second cold screen has none. The 8 ms per-frame
fallback budget holds once the canvas exists. A warm-up during font loading
would move those ~45 ms off the first CJK frame.

Repainting the same CJK from the cache: 3 frames, 19-31 ms apart, no long
task.

## Sustained output (300 lines, one every 10 ms)

251-253 frames in 5 s; gaps median 17 ms, p95 18 ms, max 23-38 ms. That is
vsync pacing with nothing dropped.

## A burst (`seq 1 20000`)

3 frames. The client fetches what is on screen when it asks, not every
intermediate state, so a burst that finishes between two fetches is one
repaint; this is not a stress test.

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
