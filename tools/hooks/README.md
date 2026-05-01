# hooks

PreToolUse / PostToolUse hook scripts that wire Claude Code's tool layer
into the obligations gate (`../obligations/`) and the session-task queue
(`../session-task/`). These are the canonical implementations — install
via `make install` from the repo root.

Each hook is a self-contained Python 3 script (no third-party deps). All
hooks default-open on any internal error: a broken hook must NEVER
blackhole the loop.

## Scripts

| Script | Hook event | Matcher | Purpose |
|--------|------------|---------|---------|
| `pre-agent-queue-gate-hook` | PreToolUse | `Agent` | Refuses Agent spawns missing `Queue item: q-XXXX` markers, or whose marker isn't `running`. |
| `pre-tool-obligations-gate-hook` | PreToolUse | `*` | Calls `obligations check`; denies when a gate-mode obligation's predicate is unsatisfied. Also enforces the bare-`watcher-ctl run` cardinal rule. |
| `post-tool-obligations-update-hook` | PostToolUse | `*` | Runs `obligations post-tool` (satisfy-by-completion + inform-mode advisories), and manages the `no_pending_watcher_outputs` sidecar registry. |
| `post-tool-mark-attachment-read-hook` | PostToolUse | `Read` | Auto-marks Signal attachments as read via `signal-mark-read` when Claude opens a file under `~/signal-queue/attachments/`. |

## Wiring example

In `~/.claude/settings.json` (after `make install` puts the binaries in
`~/bin/`):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Agent",
        "hooks": [
          {"type": "command", "command": "/home/USER/bin/pre-agent-queue-gate-hook", "timeout": 5}
        ]
      },
      {
        "matcher": "*",
        "hooks": [
          {"type": "command", "command": "/home/USER/bin/pre-tool-obligations-gate-hook", "timeout": 5}
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "Read",
        "hooks": [
          {"type": "command", "command": "/home/USER/bin/post-tool-mark-attachment-read-hook", "timeout": 5}
        ]
      },
      {
        "matcher": "*",
        "hooks": [
          {"type": "command", "command": "/home/USER/bin/post-tool-obligations-update-hook", "timeout": 5}
        ]
      }
    ]
  }
}
```

## Emergency bypass

  - `OBLIGATIONS_BYPASS=1` env var — pre-tool-obligations-gate-hook treats
    as allow + audits to `~/.config/claude/obligations-bypass.log`.
  - `QUEUE_GATE_BYPASS=1` env var — pre-agent-queue-gate-hook treats as
    allow + audits to `~/.config/claude/queue-gate-bypass.log`.
  - `obligations override "<reason>" --duration <60|5m|1h>` — preferred
    audited bypass; self-clears via TTL (24h cap).

## Tests

Two test scripts live under `tests/`:

  - `tests/pre-agent-queue-gate-hook.test` — exercises the queue gate
    against the real `session-task` CLI, using disjoint scopes from the
    live queue. ~8 cases.
  - `tests/pre-tool-obligations-gate-hook.test` — exercises the
    obligations gate (PreToolUse + PostToolUse) end-to-end against an
    isolated `HOME=$tmpdir` sandbox. 70+ cases covering every predicate,
    enforcement mode, exempt-patterns, overrides, and the watcher-ctl
    cardinal-rule gate.

Run from the repo root with `make test-hooks`.
