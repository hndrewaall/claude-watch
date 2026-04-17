---
name: setup-claude-watch-hooks
description: Install or uninstall the three Claude Code hooks that let claude-watch fire conversational reminders before the daemon falls back to tmux injection.
---

# setup-claude-watch-hooks

Install / uninstall the hybrid-model hooks that pair with the `claude-watch`
daemon. Each hook shells out to `claude-watch hook-fire <type>`, which:

1. Reads current Claude Code status (tokens, version).
2. Emits hook-response JSON on stdout that injects a reminder into the
   conversation (or blocks auto-compaction).
3. Writes a timestamped marker to `~/.cache/claude-watch/reminders/<type>.json`
   so the daemon knows to defer its heavy-handed fallback injection for a
   configurable grace window.

## Usage

```
/setup-claude-watch-hooks install     # idempotent — adds the three hooks
/setup-claude-watch-hooks uninstall   # removes the three hooks
/setup-claude-watch-hooks --scope project install   # .claude/settings.json
```

Default `--scope` is `global` (writes `~/.claude/settings.json`). Pass
`--scope project` to write `.claude/settings.json` inside the current repo.

## What gets installed

Three hook entries, each calling `claude-watch hook-fire` with a specific
reminder type:

| Hook event | Matcher | Fires when | Injects |
|---|---|---|---|
| `SessionStart` | `startup\|resume` | Installed Claude Code is newer than running | "Version X → Y available, run `/restart`" |
| `Stop` | (none) | Context usage > 80% | "Context at N%, consider `/clear`" |
| `PreCompact` | `auto` | Auto-compaction is about to run | Blocks and suggests `/clear` instead |

All three run with `timeout: 10` seconds and are resilient — any
failure emits an empty JSON object and exits 0, so a broken hook can
never break a Claude Code session.

### Exact JSON written

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup|resume",
        "hooks": [
          {
            "type": "command",
            "command": "claude-watch hook-fire version_update",
            "timeout": 10
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "claude-watch hook-fire context_high",
            "timeout": 10
          }
        ]
      }
    ],
    "PreCompact": [
      {
        "matcher": "auto",
        "hooks": [
          {
            "type": "command",
            "command": "claude-watch hook-fire pre_compact",
            "timeout": 10
          }
        ]
      }
    ]
  }
}
```

## How install works

1. **Read the target settings file** (`~/.claude/settings.json` by
   default). Create an empty `{}` if missing.
2. For each of the three events (`SessionStart`, `Stop`, `PreCompact`):
   - Find or create the corresponding array at `.hooks.<event>`.
   - Check whether a hook whose command contains `claude-watch hook-fire`
     already exists. If yes, leave it alone (idempotent). If no, append
     the hook entry with the correct matcher.
3. **Atomically rewrite** the JSON (temp file + rename), preserving
   formatting and non-hook settings.
4. Print a summary: `installed=3`, `already_present=0`, etc.

## How uninstall works

1. Read the target settings file.
2. For each of `SessionStart`, `Stop`, `PreCompact`:
   - Remove any hook entry whose command contains
     `claude-watch hook-fire`.
   - If the event's array becomes empty, remove the event key.
3. If `.hooks` becomes empty, remove that key too.
4. Atomically rewrite.

## Implementation outline

A small shell helper (e.g. `scripts/install-hooks.sh`, or inline `jq`) is
sufficient. Reference `jq` expression for the install transform:

```bash
jq --argjson add '{
  "SessionStart": [{
    "matcher": "startup|resume",
    "hooks": [{"type": "command", "command": "claude-watch hook-fire version_update", "timeout": 10}]
  }],
  "Stop": [{
    "hooks": [{"type": "command", "command": "claude-watch hook-fire context_high", "timeout": 10}]
  }],
  "PreCompact": [{
    "matcher": "auto",
    "hooks": [{"type": "command", "command": "claude-watch hook-fire pre_compact", "timeout": 10}]
  }]
}' '
  . as $existing
  | .hooks //= {}
  | reduce ($add | to_entries[]) as $evt (
      .;
      .hooks[$evt.key] as $cur
      | if ($cur // [] | any(.hooks[]?.command // "" | contains("claude-watch hook-fire")))
        then .
        else .hooks[$evt.key] = (($cur // []) + $evt.value)
        end
    )
' ~/.claude/settings.json > ~/.claude/settings.json.tmp \
  && mv ~/.claude/settings.json.tmp ~/.claude/settings.json
```

Equivalent for `uninstall`:

```bash
jq '
  if .hooks then
    .hooks |= with_entries(
      .value |= map(
        .hooks |= map(select(
          (.command // "") | contains("claude-watch hook-fire") | not
        ))
      )
      | .value |= map(select(.hooks | length > 0))
    )
    | if (.hooks | length == 0) then del(.hooks) else . end
  else .
  end
' ~/.claude/settings.json > ~/.claude/settings.json.tmp \
  && mv ~/.claude/settings.json.tmp ~/.claude/settings.json
```

## Daemon coordination (how the hybrid model works)

Once hooks are installed, the daemon behavior changes:

- **Context clear path** — before spawning `self-clear` via tmux, the
  daemon calls `should_defer_to_hook(context_high, 300s)`. If the hook
  fired within that window, it skips injection and logs
  `context_threshold_hook_deferred`. Otherwise it proceeds with the
  existing deferred-clear + tmux-inject flow and bumps
  `fallback_clear_count`.
- **Version update path** — same gate on `version_update` with a 900s
  window. Gated path logs `auto_update_hook_deferred`; fallback path
  logs `auto_update_start{"hybrid_fallback": true}` and bumps
  `fallback_update_count`.
- **Reminder-to-action latency** — when the expected action lands
  (context drops below 30k tokens, version mismatch clears), the daemon
  samples `seconds_since_fire(kind)` and accumulates sum + count in
  the state file. Prometheus exports:
  `claude_watch_reminder_to_action_latency_seconds_{sum,count}{type=...}`.

## Prometheus metrics

`claude-watch metrics` exports (in addition to existing gauges):

```
# HELP claude_watch_reminder_fires_total Total hybrid-hook reminder fires by type
# TYPE claude_watch_reminder_fires_total counter
claude_watch_reminder_fires_total{type="context_high"} N
claude_watch_reminder_fires_total{type="version_update"} N
claude_watch_reminder_fires_total{type="pre_compact"} N

# HELP claude_watch_fallback_injections_total Total daemon fallback injections when hook reminder went unheeded
# TYPE claude_watch_fallback_injections_total counter
claude_watch_fallback_injections_total{type="clear"} N
claude_watch_fallback_injections_total{type="update"} N

# HELP claude_watch_reminder_to_action_latency_seconds_sum ...
# HELP claude_watch_reminder_to_action_latency_seconds_count ...
```

The ratio `fallback_injections_total / reminder_fires_total` tells you
how often Claude ignored the hint.

## Tuning

The fallback grace windows are daemon-side config in
`~/.config/claude-watch/config.toml`:

```toml
[hybrid]
enabled = true                   # master switch
context_fallback_secs = 300      # 5 min after context_high fire before /clear fallback
version_fallback_secs = 900      # 15 min after version_update fire before claude update fallback
```

Set `enabled = false` to fall back to the old always-inject behaviour.

## Verification

After `install`, verify the hooks are live:

```bash
jq -e '.hooks.Stop[] | select(.hooks[0].command == "claude-watch hook-fire context_high")' ~/.claude/settings.json
claude-watch hook-fire pre_compact   # should print JSON with "continue": false
ls ~/.cache/claude-watch/reminders/  # marker files appear after fires
claude-watch metrics                 # prom file includes reminder counters
```
