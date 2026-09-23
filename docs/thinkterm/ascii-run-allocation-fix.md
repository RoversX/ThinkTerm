# Borrowed ASCII runs: allocation regression fix

Owned ASCII runs introduced a temporary `String` for each segment before copying it into the terminal print buffer or mux action accumulator. Short ASCII/Unicode mixtures could therefore allocate thousands of temporary strings per input chunk and regress despite the improved long-ASCII benchmark.

## Implementation and ownership

`Parser::parse_with_borrowed_text` emits synchronous `ParsedAction` events. ASCII spans borrow their input only during the callback. The terminal appends them to its existing print buffer; the mux coalesces them directly into owned queued actions. Adjacent Unicode characters and ASCII spans remain together for grapheme segmentation.

The existing scalar, owned-run, and first-action parser APIs keep their observable action granularity. Control actions still take the existing synchronized-output and flush paths. No wire format, cell layout, payload limit, persistent cache, or shared attribute ownership changes. The application adds no unsafe code; the standalone counting-allocator example delegates allocation operations to `System`.

## Comparison setup

Three release binaries were tested: `e791e31` (before the three throughput optimizations), `23b052a` (owned runs, before this fix), and the borrowed-run fix (`96dc67c`; measured source matches its production patch). All runs used macOS arm64, a 120x32 PTY, 3,500 scrollback lines, private sockets, and a sandbox protecting existing user settings/sessions. No GUI client was attached. Builds and tests were finished before timing. Each suite used three rounds in the orders baseline/before/fixed, fixed/before/baseline, before/baseline/fixed.

The short-run fixtures are deterministic: `unicode-N` repeats N ASCII x characters followed by 中; `sgr-N` alternates ANSI red and green text of N characters each. Each case sends approximately 32 MiB in approximately 64 KiB writes after a 256 KiB warm-up. A terminal status query waits for preceding output to be applied. Kitten 0.49.0 used `__benchmark__ --repetitions 100` for all six workloads; kitten randomizes its inputs.

Rates below are medians in MiB/s (kitten labels them MB/s). The [JSON evidence](ascii-run-allocation-fix-results.json) retains all runs, binary hashes, allocator measurements, and RSS samples. These are synthetic throughput observations, not foreground rendering or input-latency measurements.

## Short-run throughput

| Case | Before optimizations | Before fix | Fixed | Fixed vs before fix |
| --- | ---: | ---: | ---: | ---: |
| sgr-2 | 29.44 | 31.56 | 31.92 | +1.1% |
| unicode-2 | 38.89 | 34.13 | 40.89 | +19.8% |
| sgr-4 | 26.63 | 28.41 | 29.12 | +2.5% |
| unicode-4 | 33.27 | 33.26 | 37.94 | +14.1% |
| sgr-8 | 23.55 | 25.75 | 26.25 | +1.9% |
| unicode-8 | 32.51 | 35.26 | 39.16 | +11.1% |
| sgr-16 | 20.97 | 24.71 | 24.81 | +0.4% |
| unicode-16 | 32.54 | 37.15 | 38.49 | +3.6% |
| sgr-64 | 19.61 | 24.30 | 24.01 | -1.2% |
| unicode-64 | 32.16 | 39.80 | 40.35 | +1.4% |

The `xx中` regression is removed: the fixed median exceeds the pre-optimization baseline. The 64-character SGR median is 1.2% lower than before the fix, but the three paired directions differ and the ranges overlap (before: 23.20–24.42; fixed: 23.66–25.28 MiB/s). This series does not establish a stable regression there.

## Full kitten throughput

| Case | Before optimizations | Before fix | Fixed | Fixed vs before fix |
| --- | ---: | ---: | ---: | ---: |
| Only ASCII chars | 42.6 | 68.9 | 68.6 | -0.4% |
| Unicode chars | 47.0 | 45.9 | 47.6 | +3.7% |
| Unique multi-codepoint Unicode cells | 66.0 | 66.2 | 68.4 | +3.3% |
| CSI codes with few chars | 20.0 | 23.2 | 24.9 | +7.3% |
| Long escape codes | 188.9 | 195.8 | 206.7 | +5.6% |
| Images | 260.1 | 258.4 | 269.8 | +4.4% |

ASCII differs by -0.4% from before the fix: the first paired run was slower, the second faster, and the third equal at kitten's reported precision. Its substantial gain over the pre-optimization baseline is retained. Several first-round measurements varied considerably, so small median differences and the image increase must not be treated as proven changes. No unexpected decoding/parser errors were logged. The existing sandbox SSH-agent and live-daemon handoff refusals occurred at startup, and saturation watchdog warnings occurred under sustained output in all three binary variants.

## Allocations and memory

For one 65,535-byte `xx中` input into the actual mux coalescer, scalar/owned/borrowed modes made 15 / 13,122 / 16 allocating calls (`alloc` + `realloc`). The direct print-buffer model made 14 / 13,121 / 14. The remaining borrowed-mode calls grow the owned output buffer; the per-run temporary strings are eliminated. These are cumulative allocation counts, not simultaneously live allocations.

The current parser's three API modes are compared in this probe; this is not a replay of three historical parser implementations. Timing and allocation counting run separately. The direct buffer model does not include screen updates. To reproduce:

```sh
cargo run --locked --offline --release -p wezterm-escape-parser \
  --example print-runs --features std,use_serde,use_image,tmux_cc,kitty-shm -- stats
# Omit "-- stats" for the isolated timing CSV.
```

Median peak RSS in the full kitten series was 442.27 / 440.38 / 442.55 MiB (baseline / before fix / fixed). The fixed value is 2.17 MiB above before-fix, with overlapping ranges; eliminating temporary allocations does not guarantee a lower process RSS in every workload.

A separate 60-second sustained-output run alternated short ASCII/Unicode, frequent SGR, long ASCII lines, and combining characters. RSS was sampled every two seconds. For samples after the first 20 seconds:

| Version | Minimum RSS | Maximum RSS | Final sampled RSS |
| --- | ---: | ---: | ---: |
| Before fix | 34.00 MiB | 34.03 MiB | 34.03 MiB |
| Fixed | 33.78 MiB | 33.91 MiB | 33.89 MiB |

One minute of bounded observation cannot rule out long-duration leaks or describe GUI/GPU memory. The soak was separately timed and did not overlap any other benchmark, build, or test.

## Correctness and build checks

- 84 escape-parser, 129 terminal, and 202 mux tests passed. Borrowed events are checked against scalar actions across all 256 next-byte values in multiple parser states and input chunk sizes. Input-buffer mutation after parsing leaves queued text intact.
- Direct and queued terminal consumers match scalar screen snapshots across widths, normalization, custom character widths, charsets, styled text, hyperlinks, scrolling/wrapping, title accumulation, and chunk boundaries. The existing image/control tests also pass.
- The callback lifetime compile-fail documentation test passed: a borrowed run cannot escape its callback.
- GUI tests passed serially: 909 passed, one pre-existing ignored test. The initial parallel run stalled in `pasted_image_staging_runs_off_the_calling_thread`; only that owned test process was stopped, and the full serial rerun passed. No image-staging or executor code was changed.
- Release GUI/mux builds and the WASM core check passed. The standalone allocation example was built and checked against the recorded allocator totals.

Foreground GUI visual QA, input latency under load, native Windows execution, and long-duration memory behavior remain unmeasured. No installed application was replaced or restarted. Raw local harnesses and full logs are retained in `/private/tmp/thinkterm-borrowed-runs/`.
