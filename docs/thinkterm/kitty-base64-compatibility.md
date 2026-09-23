# Kitty Base64 compatibility

Kitten 0.49.0's Images benchmark used to log `base64_decode: Invalid padding`
and discard each image in ThinkTerm. Kitty's [Go graphics sender](https://github.com/kovidgoyal/kitty/blob/master/tools/tui/graphics/command.go)
uses `RawStdEncoding`, which omits trailing `=` characters. ThinkTerm's decoder
required them when the raw byte count was not divisible by three.

## Change

Kitty pixel payloads, file paths, temporary-file paths, and shared-memory names
now accept either canonical padding or no padding. The decoder checks the last
byte to choose the mode before allocating its output. Each chunk is decoded
once, without copying the input, appending padding, or concatenating/retrying
the transfer. Path/name decoding also drops the previous temporary input copy.

Malformed padding, invalid characters, and impossible Base64 lengths still
fail. Existing tolerance for unused trailing bits is preserved. OSC clipboard,
user-variable, and iTerm decoding retain their original policy. The chunk/byte
limits, image-store budget, decompression cap, external-file restrictions,
cleanup, and rendering paths are unchanged. No dependency, cache, or unsafe
code was added.

## Validation on 2026-09-23

The new small-image regression failed before the fix with `Invalid padding`.
After the fix, the escape-parser and terminal suites passed (81 + 118 tests).
The new coverage checks padded/unpadded payload lengths, external-source names,
malformed inputs, unchanged OSC policy, exact decoded pixels, placement, a bad
transfer followed by a good one, and image memory accounting after deletion.
The large regression sends a 1024x1024 RGBA image in 128 KiB Base64 chunks and
splits those chunks across PTY-sized reads, then compares all 4 MiB of pixels.

On an M1 Pro, an isolated release mux received 100 uncompressed 4 MiB images per
run, with three warmups. Each transfer waited for its own successful Kitty ACK
before deletion and the next transfer. The byte content was identical between
the padded and unpadded cases. Three runs per case, with baseline/candidate
order varied, gave these medians:

| Case | Encoded wire throughput (decimal MB/s) | Peak mux RSS (MiB) |
| --- | ---: | ---: |
| Before, padded | 95.49 | 41.11 |
| Fixed, padded | 95.48 | 41.13 |
| Fixed, unpadded | 97.96 | 42.61 |

All transfers were acknowledged. The baseline binary is the parser-optimized
binary already recorded in `parser-throughput-results.json`; intervening GUI
recording-mask changes do not affect this mux workload. Both binaries used the
repository's Cargo release profile. Each mux used a temporary config/socket and
a macOS sandbox denying writes to existing user settings/session files. Builds
and test suites were stopped during measurement. These short runs show no
material regression for previously working padded input; they are not a leak
test or a GUI/GPU memory measurement.

A separate allocator-counting harness used the old and new decoder functions
extracted from the source. For raw payloads of 16 B, 4 KiB, 96 KiB, and 4 MiB,
each decode performed one allocation, zero reallocations, identical peak output
allocation sizes, and released all measured bytes. The fixed decoder's padded
input times stayed within about 1.2% of the old decoder over seven interleaved
rounds. This is a codec microbenchmark, separate from mux throughput. The sample
external path went from two allocations to one.

The original `kitten __benchmark__ --repetitions 100 images` also completed on
the fixed isolated mux: 2.08 s, 256.9 MiB/s (kitten labels this MB/s), zero image
decode errors, and 47.36 MiB peak RSS. It runs asynchronously without a per-image
ACK, so its number is not comparable to the ACK-paced table. Earlier scores
from rejected images cannot be used as the performance baseline for this fix.
Individual measurements and binary hashes are in [the results JSON](kitty-base64-results.json).

```sh
cargo test --offline -p wezterm-term -p wezterm-escape-parser --features wezterm-escape-parser/kitty-shm
cargo check --offline -p wezterm-escape-parser --no-default-features --target wasm32-unknown-unknown
cargo build --offline --release -p wezterm-gui -p wezterm-mux-server
```

The native release builds for GUI and mux, and the WASM parser check above,
passed. Shared-memory tests need permission to create their test POSIX
shared-memory objects. The installed application and active user sessions were not replaced
or restarted. Foreground GUI visual QA and native Windows execution were not
performed; pixel content and placement were checked in the terminal model.
