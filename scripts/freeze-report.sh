#!/bin/sh
# freeze-report.sh: capture evidence WHILE ThinkTerm feels frozen or laggy.
# Run it from another terminal (Terminal.app, iTerm, or a working ThinkTerm
# window) the moment the problem is visible; everything lands in one
# timestamped directory so a single occurrence is enough to diagnose.
set -u

STAMP=$(date +%Y%m%d-%H%M%S)
OUT="$HOME/.local/share/thinkterm/freeze-reports/$STAMP"
mkdir -p "$OUT"

PID=$(pgrep -x thinkterm-gui | head -1)
if [ -z "$PID" ]; then
    echo "no thinkterm-gui process found" >&2
    exit 1
fi
echo "gui pid $PID -> $OUT"

# What was the user seeing (fill in by hand afterwards if possible).
cat > "$OUT/README.txt" <<EOF
captured: $STAMP
gui pid:  $PID
symptom:  (edit me: which window/pane was stuck? did clicking revive it?
           was the pointer beachballing? local shell or Lab Server?)
EOF

# Per-thread CPU twice, 2s apart: shows whether the main thread is pegged
# or idle during the stall, and which worker threads are burning.
ps -M -p "$PID" > "$OUT/threads-1.txt" 2>&1
sleep 2
ps -M -p "$PID" > "$OUT/threads-2.txt" 2>&1

# 5s call-graph sample: if the main thread is busy, this names the code;
# if it is idle in mach_msg, the stall is a lost wakeup, not CPU.
sample "$PID" 5 -file "$OUT/sample-gui.txt" >/dev/null 2>&1 &
SAMPLE_PID=$!

# Unix-socket queue depths: a non-zero Send-Q/Recv-Q on the gui socketpair
# means the in-process mux link is backed up (push path wedged), which
# no CPU sample can show.
netstat -f unix > "$OUT/netstat-unix.txt" 2>&1
lsof -p "$PID" > "$OUT/lsof.txt" 2>&1

# Memory footprint at the moment of the stall.
footprint "$PID" > "$OUT/footprint.txt" 2>&1

# The live log (watchdog warnings, client/server errors land here).
cp "$HOME/.local/share/thinkterm/thinkterm-gui-log-$PID.txt" "$OUT/gui-log.txt" 2>/dev/null

wait "$SAMPLE_PID"
echo "done: $OUT"
