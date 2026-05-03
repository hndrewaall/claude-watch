# agent-msg

CLI for delivering async messages to running Claude Code agents.

This is the **canonical implementation**. Other repos that previously shipped
a copy of this script now contain a thin wrapper that invokes the binary
installed from here.

## What it does

When the main loop spawns a subagent via the `Agent` tool, that subagent has
no inbound channel — its prompt is fixed at spawn time. `agent-msg send`
fills the gap by:

1. Writing the message to the agent's inbox file at
   `~/.config/claude/agent-inbox/<agent_id>.json` (0600).
2. Registering a **gate-mode** obligation against tool-pattern `*`, scoped to
   subagents only via `all_of [is_main_loop {negate: true},
   agent_inbox_empty]`.
3. The agent's next non-exempt tool call hits the obligations gate, which
   DENIES the call with the message body in the deny banner.
4. The agent reads the banner, runs `agent-msg ack <id>` (which is on the
   exempt list), the inbox flips empty, and the gate stops firing.

The persistence model is "messages stay on disk after ack" so the agent can
re-read them later via `agent-msg inbox --all`. Garbage collection drops
messages whose `read_at` is older than `AGENT_MSG_TTL_HOURS` (default 24h).

### Auto-disarm leak + `gc-dead` sweeper

The auto-disarm path runs in a `PostToolUse` hook on the `Agent` tool. The
hook expects `tool_response.status == "completed"` to fire `agent-msg
disarm <id>`. **Claude Code never emits that status for async-launched
subagents** — the only status seen on `Agent` PostToolUse is
`async_launched` (verified across all transcripts: 1292
`async_launched`, 0 `completed` for async agents). The actual subagent
termination only surfaces via a separate `SubagentStop` lifecycle hook,
which is not currently wired and is not the same event as PostToolUse.

Result: every armed agent's inbox obligation leaks into the obligations
DB until its 4-hour TTL expires. q-0162 (PR #82) made the leaks
*harmless* by scoping the `agent_inbox_empty` predicate leaf to its
owning agent_id (so a dead agent's banner no longer cross-contaminates
new agents), but the leaked rows still bloat the DB.

`agent-msg gc-dead` is the backstop. It:

1. Calls `claude-watch active-agents --json` to learn which agent_ids
   are still alive.
2. Walks every `agent-msg/inbox:*` obligation; extracts the owning
   agent_id from `deny_message` (or the predicate-leaf params).
3. For each obligation whose owner is NOT alive: calls `obligations
   satisfy <ob_id>` and removes the inbox file.

Default-open contract: if `claude-watch` is unavailable or returns
malformed JSON, `gc-dead` is a no-op (`reason="claude-watch-unavailable"`).
The 4h TTL safety belt remains the ultimate backstop.

Two ways to run it:

- **Implicit, rate-limited** (default): every agent-msg CLI invocation
  opportunistically calls `gc-dead` if more than
  `AGENT_MSG_GC_DEAD_INTERVAL_SECS` (default 300s) has passed since the
  last sweep. Stamp file lives at
  `~/.config/claude/agent-inbox/.gc-dead.stamp`. Set the interval to 0
  to disable.
- **Explicit**: cron / manual `agent-msg gc-dead [--dry-run]`. Use
  `--dry-run` to see what would be reaped without satisfying anything.

## Subcommands

```
agent-msg list                    # show currently tracked agents
agent-msg show <id>               # metadata for one agent
agent-msg index register <id>     # add to index (no inbox)
agent-msg index done <id>         # drop from index
agent-msg arm <id>                # initialise inbox + register gate
agent-msg disarm <id>             # clear inbox + remove gate
agent-msg send <id> <text>        # append message to inbox
agent-msg inbox <id>              # read inbox (default: unread only)
agent-msg ack <id>                # mark unread messages as read
agent-msg gc <id>                 # drop read messages older than TTL
agent-msg gc-dead [--dry-run]     # satisfy inbox obligations whose owner is
                                  #   no longer alive (sweeper for the broken
                                  #   PostToolUse disarm path; see below)
agent-msg --test                  # run embedded test suite
```

## Inbox schema (v2)

```json
{
  "schema_version": 2,
  "messages": [
    {
      "id":         "m-XXXXXXXX",
      "from":       "...",
      "ts":         "ISO8601 UTC",
      "queue_item": "q-... | null",
      "text":       "body",
      "read":       false,
      "read_at":    null
    }
  ]
}
```

Legacy (pre-v2) inboxes are bare JSON arrays of messages with no read/id
fields; they are auto-migrated on first read (every message gets `read=false`
+ a synthetic id + missing-field defaults). The migration is purely additive.

## Files

- `~/.config/claude/active-agents.json` — agent index (0600)
- `~/.config/claude/agent-inbox/<id>.json` — per-agent inbox (0600)
- `~/.config/claude/obligations.json` — obligations registry (managed by
  the `obligations` CLI in `../obligations/`)

## Dependency

The `arm` / `disarm` paths shell out to the `obligations` CLI. The script
prefers a sibling executable in the same directory; failing that, it falls
back to `$PATH`. Install both via `make install` from the repo root.

`gc-dead` shells out to `claude-watch active-agents --json` to enumerate
live subagents. Override the binary via `AGENT_MSG_CLAUDE_WATCH_CLI`
(used by the embedded test suite to inject a fake).

## Environment variables

| Var | Default | Effect |
|-----|---------|--------|
| `AGENT_MSG_TTL_HOURS` | `24` | Inbox-message TTL (drops messages whose `read_at` is older). `<= 0` disables the message GC. |
| `AGENT_MSG_INBOX_DIR` | `~/.config/claude/agent-inbox` | Override the inbox directory (used by tests). |
| `AGENT_MSG_GC_DEAD_INTERVAL_SECS` | `300` | Min seconds between implicit `gc-dead` sweeps at the top of each CLI call. `<= 0` disables the implicit sweep entirely. |
| `AGENT_MSG_CLAUDE_WATCH_CLI` | (PATH lookup) | Override the `claude-watch` binary used by `gc-dead` (test injection). |

## Tests

```
python3 tools/agent-msg/agent-msg --test
# or:
make test-agent-msg
```

The embedded `--test` flag runs the full suite in-process against
isolated tmpdirs.
