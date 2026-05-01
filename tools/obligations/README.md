# obligations

Generic obligations gate — enforces "must do X before Y" rules at the Claude
Code tool layer. Used together with the PreToolUse / PostToolUse hooks under
`../hooks/`.

This is the **canonical implementation**. Other repos that previously
shipped a copy of this script now reference the binary installed from
here (default `~/bin/obligations`).

## What it does

An obligation is a row of (tool_pattern, predicate, enforcement,
deny_message). The PreToolUse hook (`../hooks/pre-tool-obligations-gate-hook`)
calls `obligations check` on every tool invocation. If a gate-mode
obligation matches the tool but its predicate is unsatisfied, the call is
denied with a banner.

The PostToolUse hook (`../hooks/post-tool-obligations-update-hook`) calls
`obligations post-tool` after every tool, which:

  - auto-removes obligations whose `satisfied_by` pattern matches the tool
    that just ran (e.g. `watcher-restart` clears a watcher-restart
    obligation), and
  - evaluates `inform`-mode obligations and prints a banner for any whose
    predicate is currently failing (non-blocking).

## Subcommands

```
obligations add | list | show | satisfy | override | prune
                 check | post-satisfy | inform-check | post-tool
```

The first six are operator-facing. The last four are the hook hot-path —
they're called by the PreToolUse / PostToolUse hooks.

## Predicate vocabulary

Bounded set, NOT Turing-complete:

  - `file_mtime_within {path, max_age_secs}` — path is fresher than N seconds
  - `file_exists {path, negate?}` — file present (or absent if negate)
  - `env_present {var, value?}` — env var set, optionally to a specific value
  - `queue_status {id, status}` — `session-task queue show <id>` reports given status
  - `no_pipe_pattern {regex}` — Bash command does NOT match regex
  - `marker_file_present {path, negate?}` — alias of `file_exists`
  - `process_alive {pid_file}` — pid_file contains a live PID
  - `process_in_pgrep {pattern}` — `pgrep -f <pattern>` returns a match
  - `watchers_healthy {}` — `watcher-status --unhealthy-only` produces no output
  - `no_pending_watcher_outputs {}` — no captured-but-unread watcher output sidecars
  - `agent_inbox_empty {path}` — agent-msg inbox has no UNREAD messages
  - `is_main_loop {negate?}` — caller is the main session loop (no agent_id)
  - `all_of {predicates: [...]}` — meta-predicate; logical AND with
    `is_main_loop` short-circuit semantics (a failing `is_main_loop`
    inside `all_of` returns satisfied=True, i.e. "this rule does not
    apply in the current context").

## Enforcement modes

  - `gate` (default): PreToolUse hook DENIES the matching tool call when
    the predicate is unsatisfied. Classic "must do X before Y."
  - `inform`: PreToolUse never denies. PostToolUse prints a single-line
    advisory banner if the predicate is currently failing.

## Bypass / overrides

Two flavors:

  1. Per-obligation `exempt_patterns` — list of tool_pattern strings; if
     any match, the obligation auto-allows even when tool_pattern matches.
     Used to encode "this gate exists but the recovery path (e.g.
     `watcher-ctl run X`) must always be allowed."
  2. Per-call audited overrides — `obligations override <reason>
     --duration <60|5m|1h>` registers a short-TTL override that bypasses
     ALL gate-mode obligations. Audited to
     `~/.config/claude/obligations-bypass.log`. 24h cap.

Legacy env-var bypass: `OBLIGATIONS_BYPASS=1` (also audited).

## State files

  - `~/.config/claude/obligations.json` (0600) — persistent state.
    Schema: `{"obligations": [...], "overrides": [...]}`. Lock-protected
    via `fcntl.flock` on every read-modify-write.
  - `~/.config/claude/obligations-bypass.log` — audit log for overrides
    and env-var bypass invocations.
  - `~/.config/claude/obligations-hook-errors.log` — hook diagnostics
    (default-open events, missing CLI, bad JSON, etc.).
  - `/tmp/claude-watcher-output-pending/<task_id>.json` — sidecar files
    used by the `no_pending_watcher_outputs` predicate.

## Tests

The hook test suite (which exercises the CLI as well) lives at
`../hooks/tests/pre-tool-obligations-gate-hook.test`. Run from the
repo root with `make test-hooks`.

## Implementation note

Python 3, no third-party runtime deps. Default-open on every internal
error: a broken hook must NEVER blackhole the loop.
