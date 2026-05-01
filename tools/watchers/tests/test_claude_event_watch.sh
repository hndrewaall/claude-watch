#!/bin/bash
# Smoke test for tools/watchers/claude-event-watch.
#
# Exercises the fast-path drain only: pre-load events into the queue dir,
# run the watcher in fast-path-fast-exit mode (events already pending),
# and verify (a) the one-liner stdout shape, (b) that the queue file is
# deleted, and (c) that the consumed-log JSONL line is appended.
#
# We intentionally do NOT test the inotify-blocking path here — that would
# require a tmux/timeout dance and is best left to the live integration.
# The fast path is the bulk of the script's logic and the only piece that
# matters for the "still works after migration" smoke gate.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
WATCHER="$REPO/tools/watchers/claude-event-watch"

if [[ ! -x "$WATCHER" ]]; then
    echo "FAIL: $WATCHER missing or not executable" >&2
    exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

QUEUE="$TMP/queue"
LOG_DIR="$TMP/log"
mkdir -p "$QUEUE" "$LOG_DIR"

# Pre-load one event into the queue
python3 - "$QUEUE" <<'PYEOF'
import json, sys, time
queue = sys.argv[1]
ev = {
    "timestamp": time.time(),
    "source": "manual",
    "tag": "smoke",
    "message": "hello from test",
    "data": {},
}
with open(f"{queue}/100_smoke.json", "w") as f:
    json.dump(ev, f)
PYEOF

# Run the watcher — fast path should drain immediately (no inotify wait)
out=$(CLAUDE_EVENT_QUEUE="$QUEUE" CLAUDE_EVENT_LOG_DIR="$LOG_DIR" "$WATCHER" 2>&1)

# Verify stdout has the one-liner shape
if ! grep -q '^EVENT\[manual/smoke\] hello from test' <<<"$out"; then
    echo "FAIL: stdout missing one-liner" >&2
    echo "$out"
    exit 1
fi

# Verify the restart banner is present
if ! grep -q 'WATCHER EXITED' <<<"$out"; then
    echo "FAIL: restart banner missing" >&2
    echo "$out"
    exit 1
fi

# Verify the queue file was deleted
if [[ -f "$QUEUE/100_smoke.json" ]]; then
    echo "FAIL: queue file not deleted" >&2
    exit 1
fi

# Verify the consumed-log line was appended
if [[ ! -s "$LOG_DIR/consumed.jsonl" ]]; then
    echo "FAIL: consumed.jsonl not written" >&2
    exit 1
fi
if ! python3 -c "
import json, sys
line = open('$LOG_DIR/consumed.jsonl').read().strip().splitlines()[0]
ev = json.loads(line)
assert ev['tag'] == 'smoke', ev
assert ev['source'] == 'manual', ev
assert ev['message'] == 'hello from test', ev
print('  log entry OK')
"; then
    echo "FAIL: consumed.jsonl content mismatch" >&2
    exit 1
fi

# Test malformed event: should print a placeholder one-liner, NOT crash
echo "not valid json" >"$QUEUE/200_bad.json"
out=$(CLAUDE_EVENT_QUEUE="$QUEUE" CLAUDE_EVENT_LOG_DIR="$LOG_DIR" "$WATCHER" 2>&1)
if ! grep -q 'EVENT\[malformed/unknown\]' <<<"$out"; then
    echo "FAIL: malformed event not handled gracefully" >&2
    echo "$out"
    exit 1
fi

# Test debounce flag validation (non-numeric input should fail with rc=2)
set +e
CLAUDE_EVENT_QUEUE="$QUEUE" "$WATCHER" --debounce abc >/dev/null 2>&1
rc=$?
set -e
if (( rc != 2 )); then
    echo "FAIL: --debounce abc returned rc=$rc, expected 2" >&2
    exit 1
fi

# Test --help works
"$WATCHER" --help >/dev/null

echo "PASS: all claude-event-watch fast-path checks"
