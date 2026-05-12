#!/usr/bin/env bash
# claude-container entrypoint — sets up a 2-pane tmux session.
#
# Pane 0 (main, left): runs `claude` interactively.
# Pane 1 (right, ~25%): runs the in-container `claude-watch` daemon, observing
#   pane 0 via in-container `tmux capture-pane`. Replaces the Phase 0
#   placeholder bash banner (q-2026-05-11-8a96, Phase 1e).
#
# Pane 1 invocation (bare `claude-watch` = daemon, same call as a host-side
# `claude-watch.service` systemd unit). Daemon picks up config from the env
# var below; the config pins it to this very tmux session
# (`claude-container:0.0`) so it doesn't auto-detect across other sessions.
#
# Env passthrough: CLAUDE_CODE_SSE_PORT, ANTHROPIC_API_KEY, plus any CLAUDE_*
# and ANTHROPIC_* vars are already in the process env (docker -e or compose
# environment:); tmux inherits them, so panes see them.
#
# Debug escape hatch: if argv[1] is set, exec it instead of launching tmux.
# Lets `docker run claude-container bash` (etc.) work for inspection. Also
# keeps the `claude-tmux bash -c "..."` validation path working unchanged.
#
# Inspect the running session from another shell on the host:
#   sudo docker exec -it <container> tmux attach -t claude-container
#   sudo docker exec <container> tmux capture-pane -t claude-container:0.1 -p
# Or peek at the structured log:
#   sudo docker exec <container> cat /tmp/claude-watch.jsonl

set -euo pipefail

SESSION="claude-container"

# Tell the in-container claude-watch where its config lives. The host's
# ~/.config/ is NOT bind-mounted (only ~/.claude, ~/repos, $PWD per the
# wrapper's "blast radius" header), so we ship a container-tailored config
# at /etc/claude-watch/config.toml via the Dockerfile and pin it explicitly
# here. The daemon also reads CLAUDE_WATCH_CONFIG by design (see
# claude-watch/src/config.rs `try_load_config`).
export CLAUDE_WATCH_CONFIG="${CLAUDE_WATCH_CONFIG:-/etc/claude-watch/config.toml}"

# Make sure the directories claude-watch wants to write to exist + are
# writable by uid 1000. State dir is under ~/.cache; logs are in /tmp.
mkdir -p "${HOME:-/home/hndrewaall}/.cache/claude-watch"

cleanup() {
    # Killing the tmux session terminates both panes' child processes
    # (pane 0's claude, pane 1's claude-watch daemon) via SIGHUP from the
    # tmux server. This is the right shape for SIGTERM/SIGINT delivered to
    # PID 1 (this script) from `docker stop` / wrapper signal forwarding.
    if tmux has-session -t "$SESSION" 2>/dev/null; then
        tmux kill-session -t "$SESSION" || true
    fi
    exit 0
}
trap cleanup TERM INT

# Debug / one-shot exec path. Skips tmux entirely — `claude-tmux bash`,
# `claude-tmux bash -c "..."`, and any other argv-passed command path
# bypasses both panes. Important for the Phase 0e validation pattern
# (non-interactive `claude --print` via `claude-tmux bash -c "..."`).
if [ "$#" -gt 0 ]; then
    exec "$@"
fi

# Build the tmux session. Start detached so we can configure panes before
# attaching, then attach at the end (blocks until session ends).
#
# Pane 0 uses `exec claude` (Phase 1.5 fix #2, q-2026-05-11-d7c0): bash replaces
# itself with the claude binary, so tmux's `#{pane_current_command}` reports
# `claude` rather than `bash`. claude-watch's status command uses
# `pane_current_command == "claude"` as its primary pane-discovery filter
# (claude-watch/src/status.rs); the prior wrapper `claude; echo ...; read`
# kept bash as PID 1 of the pane, so the filter never matched and pane
# discovery silently no-op'd. Trade-off: the pane now closes immediately when
# claude exits (no "press Enter to close pane" UX). Acceptable for Phase 1.5;
# Phase 2+ can revisit if users need post-exit inspection. See Phase 1f §8
# "Bug #2" in the project doc.
tmux new-session -d -s "$SESSION" -x 200 -y 50 \
    "exec claude"

# Pane 1 (right, ~25%): the in-container claude-watch daemon. Bare
# `claude-watch` with no subcommand runs the daemon (same invocation as
# the host's systemd unit). If the binary fails to start (config parse
# error, missing tmux server, etc.) we surface stderr inline AND keep the
# pane open with a shell prompt so Andrew can see the failure on
# `tmux attach` instead of finding a closed pane and an empty session.
tmux split-window -h -t "$SESSION:0" -p 25 \
    "echo '[pane 1] starting claude-watch (config=$CLAUDE_WATCH_CONFIG)'; \
     claude-watch 2>&1 || { ec=\$?; \
        echo; echo '[pane 1] claude-watch exited with code '\$ec; \
        echo '[pane 1] dropping to shell so you can inspect; exit to close'; \
        exec bash; }"

# Focus the main claude pane.
tmux select-pane -t "$SESSION:0.0"

# Attach. Blocks until the session exits.
exec tmux attach-session -t "$SESSION"
