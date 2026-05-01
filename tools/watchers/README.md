# watchers

Watcher scripts and the `self-clear` helper that the main loop spawns as
background tasks. These are the **canonical implementations**.

## Scripts

| Script | Type | Purpose |
|--------|------|---------|
| `claude-event-watch` | bash watcher | Block on `$CLAUDE_EVENT_QUEUE` (default `~/claude-events/`); print one-liner per pending event; append full JSON to `$CLAUDE_EVENT_LOG_DIR/consumed.jsonl`; exit. The main loop re-invokes it after each delivery. |
| `self-clear` | one-shot | Inject `/clear` + a configurable resume-prompt into the Claude Code tmux pane. Final step of a compact-prep procedure; eliminates the wait for the daemon's resume-injection path to fire on its own. |

## Watcher lifecycle (cardinal rule)

> **Watchers can ONLY ever be started by Claude Code's main loop**, via the
> Bash tool's `run_in_background: true`. Never via systemd-run, never via
> nohup, never by the daemon. The daemon's only emergency action is
> tmux-injecting a `watcher-ctl run <name>` line into the main loop's pane,
> so the main loop re-spawns the watcher itself.

`watcher-ctl`, `watcher-restart`, and `watcher-status` are dispatched by
the `claude-watch` Rust binary (multicall symlinks — see
`scripts/git-hooks/pre-commit` and `src/main.rs::multicall_rewrite_args`).

## `claude-event-watch`

```
claude-event-watch [--debounce SECONDS]
```

- Fast path: drain anything already pending (no debounce).
- Slow path: `inotifywait -e create -e moved_to --include '\.json$'`,
  then sleep `$DEBOUNCE_SECONDS` (default 30) to batch any related events,
  then drain.
- Output shape: `EVENT[<source>/<tag>] <first-60-chars-of-message>…`
- Restart banner: `WATCHER EXITED. RESTART NOW: watcher-ctl run claude-event-watch`

Environment:

- `$CLAUDE_EVENT_QUEUE` — queue dir (default `~/claude-events/`)
- `$CLAUDE_EVENT_LOG_DIR` — log dir (default `~/.config/claude-events/`)
- `$CLAUDE_EVENT_LOG_MAX_LINES` — ring-buffer rotation threshold
  (default 10000)
- `$EVENT_WATCH_DEBOUNCE_SECONDS` — equivalent of `--debounce`

## `self-clear`

```
self-clear [--delay SECONDS] [--no-resume] [--timeout SECONDS]
           [--log-file PATH] [--lock-file PATH] [--resume-prompt TEXT]
```

Forks immediately so the calling tool call can complete; the child polls
the tmux pane via `claude-watch status --json`, injects `/clear` (vim-mode
sequence: `Escape, dd, i, /clear, Enter`), waits for tokens to drop below
`FRESH_SESSION_MAX_TOKENS` (30000), dismisses the post-/clear feedback
prompt, then injects the resume prompt.

Environment defaults (all overridable via flag):

- `$CLAUDE_SELF_CLEAR_LOG` — log path (default
  `$XDG_STATE_HOME/claude-watch/self-clear.log`, falling back to
  `~/.local/state/claude-watch/self-clear.log`)
- `$CLAUDE_SELF_CLEAR_LOCK` — lock path (default
  `$XDG_RUNTIME_DIR/claude-self-clear.lock`, falling back to
  `/tmp/claude-self-clear.lock`)
- `$CLAUDE_SELF_CLEAR_RESUME_PROMPT` — resume-prompt text (default is a
  generic placeholder; override to point at a host-specific
  resume-checklist).

## What's NOT here

`session-resume` is intentionally NOT migrated — it's a host-specific
resume-checklist driver that calls site-local CLIs (request tracker,
system health-check, Signal history, etc.). The portable equivalent is the
`claude-watch hook-fire` system + the resume-prompt that `self-clear`
injects, plus whatever per-host resume-checklist the operator writes.

## Tests

```
make test-watchers      # runs both:
python3 tools/watchers/tests/test_self_clear_config.py
tools/watchers/tests/test_claude_event_watch.sh
```

`self-clear`'s end-to-end inject flow needs a live Claude Code tmux pane,
so the unit tests cover only the portable config-resolution path. The
event-watch test covers the fast-path drain + log append + malformed-event
handling.
