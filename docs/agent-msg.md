# agent-msg — async messaging for running Claude Code agents

`agent-msg` delivers asynchronous messages to spawned subagents by writing to
a per-agent inbox file and surfacing the inbox via the obligations gate. The
subagent's next non-exempt tool call DENIES with the message body in the deny
banner; the agent reads the banner, runs `agent-msg ack`, and proceeds.

This is a workaround layer for the upstream `SendMessage` gap (documented in
the `Agent` tool docs for Agent Teams but not implemented at the CLI level —
[anthropics/claude-code#47021](https://github.com/anthropics/claude-code/issues/47021)).
We do NOT inject into the agent's context window; we surface messages via the
PreToolUse hook stderr banner, which is impossible to ignore because the
agent's tool call literally fails until they ack.

## Architecture

```
                                    ┌────────────────────────────────┐
main loop          ──── send ───▶  │ ~/.config/claude/agent-inbox/  │
                                    │ <agent_id>.json (0600)         │
                                    └─────────────┬──────────────────┘
                                                  │ read on every
                                                  │ obligations check
                                                  ▼
                              ┌────────────────────────────────────┐
spawned subagent ─── tool ─▶  │ PreToolUse: pre-tool-obligations-  │
                              │   gate-hook                        │
                              │   evaluates `agent_inbox_empty`    │
                              │   predicate against the inbox      │
                              └────────────┬───────────────────────┘
                                           │ inbox not empty?
                                           ▼
                                    DENY the tool call with the
                                    message body in the banner.
                                    Agent reads it + runs:
                                      agent-msg ack <id>
                                    inbox flips empty → gate clears.
```

## Protocol

1. **arm**: at agent-spawn time, the main loop calls `agent-msg arm <id>` which:
   - Initialises the inbox to `{"schema_version": 2, "messages": []}` (0600).
   - Registers a gate-mode obligation against `tool_pattern: '*'`,
     scoped via `all_of [is_main_loop {negate: true}, agent_inbox_empty]`,
     default TTL 4 hours.
   - Adds exempt patterns for `agent-msg ack/inbox/gc/disarm` so the
     agent can always clear its own inbox.
2. **send**: `agent-msg send <id> <text>` atomically appends a message:
   `{id, from, ts, queue_item, text, read=false, read_at=null}`.
3. **gate fires**: on the subagent's next non-exempt tool call, the
   PreToolUse hook DENIES with the message body in the deny banner.
4. **ack**: subagent runs `agent-msg ack <id>` (exempt — always passes).
   Flips every UNREAD entry to `read=true` + stamps `read_at`.
5. **gc**: every `agent-msg` invocation runs an implicit GC pass that drops
   `read_at` older than `AGENT_MSG_TTL_HOURS` (default 24h). Unread
   messages are NEVER GC'd regardless of `ts` age.
6. **disarm**: at agent-completion, the main loop calls `agent-msg disarm <id>`,
   which removes the inbox file AND removes the per-agent obligation.

## Subcommand surface

| Command | Purpose |
|---------|---------|
| `agent-msg arm <id>` | initialise inbox + register gate-mode obligation |
| `agent-msg disarm <id>` | clear inbox + remove obligation |
| `agent-msg send <id> <text>` | atomic-append message to inbox |
| `agent-msg inbox <id>` | read inbox (default UNREAD only; `--all` = full) |
| `agent-msg ack <id>` | flip unread → read; KEEPS bodies on disk |
| `agent-msg gc <id>` | drop already-read messages older than the TTL |
| `agent-msg list` | show agent index |
| `agent-msg show <id>` | show one agent's index entry |
| `agent-msg index register <id>` | add to index |
| `agent-msg index done <id>` | drop from index |
| `agent-msg --test` | embedded test suite (38 cases) |

All commands accept `--inbox-dir <path>` for sandboxed testing.

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

Legacy v1 (a bare list of message dicts with no `read`/`id`) is auto-migrated
on first read — every entry gets `read=false`, a synthetic id, and missing
fields default. The migration is purely additive; we never drop content.

## Files

- `~/.config/claude/active-agents.json` — agent index (0600)
- `~/.config/claude/agent-inbox/<id>.json` — per-agent inbox (0600)
- `~/.config/claude/obligations.json` — obligation registry (managed by
  `obligations`)

Override the inbox directory via `--inbox-dir <path>` or
`$AGENT_MSG_INBOX_DIR`.

## Operational pseudocode

```python
# Main loop, agent spawn:
agent_id = spawn_agent(prompt=...)
agent_msg.arm(agent_id)                       # 1. arm before first tool call

# ... agent runs ...
agent_msg.send(agent_id, "ctx update text")   # 2. nudge whenever needed

# ... agent processes message + calls ack on its own ...

agent_msg.disarm(agent_id)                    # 3. on agent completion
```

Subagent prompts should mention:

> If you see an `[obligations:gate] ... agent-msg/inbox:<your-id>` banner
> on stderr, read the message body, act on it, then run
> `agent-msg ack <your-id>` before continuing.

## Limitations

- Does NOT pause the agent. The agent only sees the banner when its NEXT
  tool call fires; if the agent is mid-think with no tool call pending,
  the message sits in the inbox until the next call.
- Does NOT survive `/clear` of the main loop. The obligation persists in
  the registry, but the agent process itself is orphaned. On resume, the
  main loop should `disarm` orphaned agents.
- Does NOT inject into the agent's context window. We surface messages
  as harness-emitted stderr banners in the tool-result stream.

## Tests

```
make test-agent-msg        # 38 cases via embedded --test
```

Coverage includes: schema-v2 envelope writes, v1 auto-migration, dry-run,
multi-append, ack flips read=true keeping bodies, ack by message id, inbox
default-unread + `--all` history, GC drops aged read entries, GC keeps
unread regardless of age, implicit GC on every invocation, predicate v2
envelope, atomic concurrency, send-ack-send cycle, index ops, Unicode,
gate-mode end-to-end with real ack clearing the gate.
