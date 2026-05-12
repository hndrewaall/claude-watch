#!/usr/bin/env bash
# Example claude-event check script.
#
# Runs `df` on a mount and emits a claude-event when used-space crosses a
# threshold. The pattern is generic — swap the check + tag for any periodic
# condition you want surfaced to the Claude Code session.
#
# Wire this into cron via examples/cron/example.crontab; the cron line
# already runs it every 10 minutes. Customise via env vars:
#
#     MOUNT=/var THRESHOLD_PERCENT=90 ./disk-space-check.sh
#
# Exit code is always 0 — cron's mail-on-nonzero behaviour is noisy, and the
# claude-event emission is the real signal channel here.

set -uo pipefail

THRESHOLD_PERCENT="${THRESHOLD_PERCENT:-85}"
MOUNT="${MOUNT:-/}"

# df -P forces POSIX output format; column 5 is "Capacity" (NN%).
USED_PCT=$(df -P "$MOUNT" 2>/dev/null | awk 'NR==2 {gsub("%",""); print $5}')

if [ -z "$USED_PCT" ]; then
    # df failed (bad mount, permissions, etc.). Emit a soft error so the
    # operator notices the check itself is broken.
    claude-event "disk-space-check: df failed on $MOUNT" \
        --tag disk-space-check-failed \
        --source cron \
        --data mount="$MOUNT"
    exit 0
fi

if [ "$USED_PCT" -ge "$THRESHOLD_PERCENT" ]; then
    claude-event "disk usage on $MOUNT is ${USED_PCT}% (threshold ${THRESHOLD_PERCENT}%)" \
        --tag disk-space-low \
        --priority high \
        --source cron \
        --data mount="$MOUNT" \
        --data used_percent="$USED_PCT" \
        --data threshold="$THRESHOLD_PERCENT"
fi

exit 0
