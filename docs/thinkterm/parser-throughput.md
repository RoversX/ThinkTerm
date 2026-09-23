# Parser throughput: ASCII and CSI batching

Measured on 2026-09-23 with an Apple M1 Pro (16 GiB RAM), macOS, and kitten
0.49.0. The baseline is commit `06bdaa4`, before the parser optimizations. Both
binaries were built locally with the same Cargo release profile. The measured
binary hashes and individual runs are in [the results JSON](parser-throughput-results.json).

## What changed

CSI erase and REP now batch consecutive single-width writes within a row. The
first write still performs the original wide-cell boundary handling, hyperlink
invalidation, zone invalidation, and sequence-number update. REP keeps the
existing wrapping, scrolling, image-placement behavior, and oversized-count cap.
Wide cells and compact append storage retain the scalar path.

Printable ASCII runs now write a row at a time and append compact text and
attribute runs in bulk. `PrintString` also appends to the existing print/title
buffer directly. Mixed Unicode, DEC/UK character sets, insertion mode, and custom
character-width mappings retain the grapheme path. No new cache, persistent
buffer, unsafe code, or protocol change was introduced. Image placements are
preserved when writing text; erase operations still clear them.

## Method and results

An isolated foreground mux server hosted a real PTY at 120 columns by 32 rows,
with 3,500 scrollback lines. Each server had a temporary socket and a macOS
sandbox denying writes to the existing settings/session files. No GUI client was
attached. Each baseline/candidate pair ran sequentially, three pairs in total,
with 100 repetitions per workload. Builds and test suites did not run during the
benchmarks. Kitten generates random ASCII/CSI data, so runs are comparable
workloads, not byte-identical replays.

Median throughput, in MiB/s (kitten labels these values MB/s):

| Workload | Baseline | Optimized | Change |
| --- | ---: | ---: | ---: |
| ASCII | 24.4 | 43.4 | +77.9% |
| Unicode | 46.9 | 48.0 | +2.3% |
| Unique multi-codepoint cells | 63.1 | 69.7 | +10.5% |
| CSI with few characters | 15.6 | 20.3 | +30.1% |
| Long escape codes | 201.6 | 200.7 | -0.4% |

The mux process's median peak RSS, recorded with `wait4`, was 443.0 MiB before and
440.8 MiB after. A separate 200-repetition ASCII/CSI check with scrollback enabled
measured 427.2/427.5 MiB and 24.5/43.6 MiB/s ASCII, 15.6/20.3 MiB/s CSI
(baseline/optimized). These observations show no material peak-memory regression
in these workloads; they are not a long-duration leak test or GUI/GPU memory measurement.

**Images is excluded from the performance conclusion.** The baseline and
optimized binaries both logged `Invalid padding` while decoding the benchmark
images. Their roughly 278 MiB/s scores measure a rejected transfer, not successful
image processing. This pre-existing compatibility issue is separate from the
text/CSI changes. A subsequent [Kitty Base64 fix](kitty-base64-compatibility.md)
accepts unpadded payloads; the historical rejected-transfer scores above remain
excluded.

The benchmark waits for terminal status replies after parsing. These results do
not establish foreground frame rate, input latency, or the cost of synchronizing
rendered content through a mux client. The installed application was not replaced.
For the benchmark's timing contract, see [Kitty's benchmark source](https://github.com/kovidgoyal/kitty/blob/master/tools/cmd/benchmark/main.go).

## Reproduce and validate

Save the baseline release binary before changing revisions, then build the
candidate with the same compiler/profile:

```sh
cargo build --offline --release -p wezterm-gui -p wezterm-mux-server
```

Within a PTY hosted by each respective binary, use identical dimensions and
configuration and alternate at least three runs:

```sh
kitten __benchmark__ --repetitions 100
kitten __benchmark__ --repetitions 200 --with-scrollback ascii csi
```

Use separate temporary mux sockets for automated runs, prevent writes to existing
settings/session data, capture the server's stderr, and terminate only the test
processes. Do not accept a workload score when its data was rejected. `--render`
can be tested separately in a visible GUI, but its elapsed time is still not a
frame-rate measurement.

Differential tests compare the batched writes with the original scalar behavior,
including storage representation, shape hashes, semantic zones, hyperlinks,
image placements, compact attribute runs crossing the 65,535-cell boundary,
terminal snapshots, scrollback, cursor/margins, ConPTY wrapping, Unicode
normalization, custom widths, and fragmented input. Invalid ASCII runs are
rejected without mutation; very large REP counts retain their existing cap.

```sh
cargo test --offline -p wezterm-gui -p wezterm-term -p wezterm-surface --features wezterm-term/use_serde
cargo test --offline -p wezterm-escape-parser --all-features
cargo check --offline -p wezterm-surface --no-default-features --target wasm32-unknown-unknown
```

Validation passed: 901 GUI tests (one existing ignored test), 126 terminal tests,
47 surface tests, and 78 escape-parser tests, plus the release build and WASM
check.

The shared-memory cleanup tests require native shared-memory access; a sandbox
that denies `shm_open` produces an environment failure rather than exercising
that cleanup path.

Native Windows execution and foreground GUI visual QA were not performed for
these changes. ConPTY behavior is covered by model-level differential tests.
