#!/bin/sh
# Snapshot the memory of a running thinkterm-gui so before/after numbers for
# memory work come from the same probe every time.
#
# Usage:   scripts/mem-baseline.sh [label]
# Env:     MEM_BASELINE_LOG  append target (default: ./mem-baseline.log)
#          MEM_BASELINE_PID  pid override (default: newest thinkterm-gui)
#
# Suggested scenario, one invocation per step so runs stay comparable:
#   1. fresh-start        (launch, one window, no output yet)
#   2. windows-open       (open the usual number of windows/panes)
#   3. after-scroll       (seq 1 100000 in one pane)
#   4. overview-closed    (open Live Overview, close it again)
#   5. all-occluded       (cover/minimize every window, wait ~2 min)
#
# The in-app view of the same story (per-cache byte totals, atlas size,
# quad_capacity) lives in Settings -> memory; capture it alongside if the
# per-window split matters for the step.

set -eu

label="${1:-snapshot}"
log="${MEM_BASELINE_LOG:-mem-baseline.log}"
pid="${MEM_BASELINE_PID:-$(pgrep -nx thinkterm-gui || true)}"

if [ -z "$pid" ]; then
    echo "mem-baseline: no running thinkterm-gui" >&2
    exit 1
fi

{
    echo "=== ${label} | pid ${pid} | $(date '+%Y-%m-%d %H:%M:%S') ==="

    footprint "$pid" 2>/dev/null \
        | grep 'phys_footprint' \
        | sed 's/^ */  /'

    echo "--- footprint dirty, top categories ---"
    footprint "$pid" 2>/dev/null \
        | sed -n '/^  Dirty/,/^ *TOTAL/p' \
        | grep -v '^ *---' \
        | head -12 \
        | sed 's/^/  /'

    echo "--- vmmap key regions (virtual/resident/dirty/swapped) ---"
    vmmap --summary "$pid" 2>/dev/null \
        | sed '/^MALLOC ZONE/,$d' \
        | grep -E '^(MALLOC_(TINY|SMALL|LARGE) |IOSurface|IOAccelerator \(graphics\)|owned unmapped \(graphics\)|TOTAL)' \
        | sed 's/^/  /'

    echo "--- malloc zones (allocated vs dirty+swap gap = fragmentation) ---"
    vmmap --summary "$pid" 2>/dev/null \
        | sed -n '/^MALLOC ZONE/,$p' \
        | sed 's/^/  /'

    echo
} | tee -a "$log"
