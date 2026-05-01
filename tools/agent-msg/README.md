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
agent-msg --test                  # run embedded test suite (38 cases)
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

## Tests

```
python3 tools/agent-msg/agent-msg --test
# or:
make test-agent-msg
```

The embedded `--test` flag runs the full suite (38 cases) in-process against
isolated tmpdirs.
