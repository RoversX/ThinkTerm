# Parser throughput follow-up

This follow-up starts at `e791e31`, after the earlier screen-write batching and
Kitty Base64 compatibility fixes. It evaluates the parser boundary, CSI row
updates, and repeated extended-attribute allocation separately from rendering.

## Implementation

`VTActor::print_ascii` has a scalar default. The terminal and mux opt into
`Parser::parse_print_runs`; existing `parse`, `parse_first`, and
`parse_first_as_vec` keep their action granularity. Adjacent `Print` and
`PrintString` actions coalesce before grapheme segmentation, including an ASCII
base followed by a Unicode combining sequence. Runs are owned strings, so they
cannot retain a PTY input buffer. UTF-8, control codes, and character-set handling
keep their original state-machine semantics.

OSC, APC, and DCS string payloads finish the current input chunk in the original
byte loop. This avoids adding a run-detection check to every image/payload byte.
Text following a terminator in that same chunk uses ordinary print actions;
batching resumes on a later input chunk. The payload limits and decoding logic
are unchanged.

Default-blank tail erasure can truncate a vector-backed row after resolving the
left wide-character boundary and invalidating implicit links and semantic zones.
All-blank vector rows keep their historical length; compact rows, styled blanks,
and unsupported boundaries keep their existing behavior. Bulk erasure uses slice
fill. REP can reuse destination cells when there are no image attachments, and
keeps the previous image-placement merge path otherwise.

`Cell::clone_from` and `CellAttributes::clone_from` reuse an existing extended
attribute allocation. Attributes remain independently owned, with the same
16-byte attribute and 24-byte cell layouts on this 64-bit target. No interning
table, new persistent cache, unsafe production code, or serialization change is
introduced.

## Why shared attributes were not retained

An `Arc<FatAttributes>` copy-on-write prototype passed isolation tests and helped
repeated styles. However, 100,000 independently allocated RGB attributes increased
from 11,200,096 to 12,800,112 requested live heap bytes. The 16-byte reference-count
header is a real cost for each independent allocation, including decoded styles
that have no sharing opportunity. The final implementation keeps `Box` ownership
and instead reuses allocations during overwrites.

An isolated counting-allocator probe overwrote 100,000 already styled RGB cells
four times using slice fill. Allocations fell from 400,000 to 4. The ordinary
attribute construction/clone cases retained the baseline allocation counts and
requested live bytes; all allocations in each construction case were released
after dropping the values. This is an allocation microbenchmark, not a
long-running process leak test. A serialized attribute fixture was byte-identical
to the baseline and round-tripped successfully.

## PTY/mux measurements

Measured on macOS arm64 with kitten 0.49.0, release builds, a 120x32 PTY, and
3,500 scrollback lines. Three pairs used 100 repetitions per workload, in order
baseline/candidate, candidate/baseline, baseline/candidate. Each mux server used
a private temporary socket and a sandbox preventing writes to existing settings
and sessions. Builds and tests did not run during the measured pairs. Kitten
randomizes input, so these are comparable workloads, not byte-identical replays.

Median throughput in MiB/s (kitten labels the rates MB/s):

| Workload | Baseline | Candidate | Change |
| --- | ---: | ---: | ---: |
| ASCII | 43.2 | 69.3 | +60.4% |
| Unicode | 47.6 | 47.2 | -0.8% |
| Unique multi-codepoint cells | 69.1 | 68.7 | -0.6% |
| CSI with few characters | 20.2 | 25.0 | +23.8% |
| Long escape codes | 203.2 | 209.3 | +3.0% |
| Images | 241.0 | 239.7 | -0.5% |

Median peak mux RSS was 438.2 MiB versus 439.9 MiB (+0.4%); the individual ranges
overlapped. An additional 200-repetition, scrollback-enabled pair measured
43.3/69.7 MiB/s ASCII and 20.3/25.2 MiB/s CSI, with peak RSS 430.5/430.8 MiB
(baseline/candidate). These are bounded observations, not a guarantee of equal
memory use for every workload.

The [individual runs and binary hashes](parser-throughput-followup-results.json)
include the full results. In particular, Images varied from 239.2 to 261.9 MiB/s
in the baseline and 229.5 to 244.1 MiB/s in the candidate; the median alone does
not establish a small image speed change. There were no unexpected parser or
image-decoding errors. Expected sandbox SSH-agent and existing-daemon handoff
refusal messages occurred at startup. Successful decoding here does not test
foreground image rendering: no GUI client was attached.

To reproduce the workloads inside each binary's own isolated PTY:

```sh
kitten __benchmark__ --repetitions 100
kitten __benchmark__ --repetitions 200 --with-scrollback ascii csi
```

## Validation boundaries

Differential tests cover fragmented and invalid UTF-8, control/parser states,
Unicode combining sequences, title accumulation, DEC/UK character sets, custom
widths, wide-cell boundaries, semantic zones, hyperlinks, image placements,
compact storage, snapshots, wrapping, scrolling, and the existing oversized REP
cap. Clone tests check allocation reuse and independently mutable values.

The native parser tests need shared-memory access; denying `shm_open` in a test
sandbox prevents the existing cleanup test from running. The integration feature
set enables serialization through `wezterm-term/use_serde`.

Integration suites passed 202 mux tests and 909 GUI tests (one existing ignored
test). After the final UTF-8 and clone-inlining adjustments, the affected suites
passed again: 26 VT parser, 82 escape parser, 14 cell, 48 surface, and 129 terminal
tests. Release GUI/mux builds and the `wasm32-unknown-unknown` core check passed.

Foreground GUI visual QA, input latency under load, native Windows execution,
and long-duration memory behavior remain unmeasured. No installed application
was replaced or restarted.
