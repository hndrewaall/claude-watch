# agent-tail

Stream a Claude Code subagent's JSONL transcript.

## What it does

Each spawned subagent writes its full transcript (user prompt, assistant
text, tool calls, tool results, system events) as JSONL to:

```
~/.claude/projects/<project-slug>/<session-uuid>/subagents/agent-<id>.jsonl
```

`agent-tail` resolves an agent id (full or short prefix) to that path, then
either dumps the file once or follows it like `tail -f`. It pretty-prints
each record by default; `--json` is a passthrough mode for downstream
consumers.

## Usage

```
agent-tail <agent_id>                  # one-shot dump (pretty)
agent-tail <agent_id> --follow         # tail -f equivalent
agent-tail <agent_id> --since 5m       # only events from the last 5 min
agent-tail <agent_id> --json           # raw JSONL passthrough
agent-tail <agent_id> --path           # print the resolved path
agent-tail --list                      # all agents under ~/.claude/projects
agent-tail --test                      # run the embedded test suite
```

Truncation knobs (defaults aim at 'readable in a terminal'):

```
--truncate-tool-result LINES   # cap each tool-result preview (default 20)
--truncate-chars CHARS         # cap each rendered text block (default 2000)
--no-ts                        # drop the leading [timestamp] prefix
```

## Resolution

The agent id arg is matched against `agent-*.jsonl` under
`~/.claude/projects/`. Both full ids and short prefixes work:

```
agent-tail a194312241cc25f9c          # exact
agent-tail agent-a194312241cc25f9c    # exact (with the 'agent-' prefix)
agent-tail a1943                      # shortest unique prefix
```

When several files match a prefix, the most recently modified one wins.

## Output format

Pretty mode formats each record as one of:

```
[<ts>] USER: <prompt-text>
[<ts>] ASSISTANT: <text>
[<ts>] TOOL_CALL <Name>(<one-line-args>)  [toolu_...]
[<ts>] TOOL_RESULT [toolu_...]
         <body line 1>
         <body line 2>
         ... [N more line(s) truncated]
[<ts>] TOOL_RESULT [error] [toolu_...]
         <body>
[<ts>] THINKING: <text>
[<ts>] EVENT: <repr>            # system / unknown record types
```

`--json` skips all of that and just emits the original JSONL line.

## Follow-mode behaviour

`--follow` polls `stat()` every 0.5s by default (`--poll-interval`). It
handles three transcript-mutation cases:

- inode changes (atomic-write replacement): reset to offset 0.
- size shrinks (in-place truncation): reset to offset 0.
- size stays >= our offset but the byte at offset-1 isn't `\n`: same — we
  lost the line boundary, reset to 0. Catches `open(w) + immediate write
  past our resume offset`.

## Tests

```
make test-agent-tail            # 28 cases, all in-process
agent-tail --test               # same, direct invocation
```

Coverage: pure helpers (duration parsing, ISO-ts parsing, line truncation,
arg flattening), `format_record` dispatch (every record type, error path,
thinking, unknown), resolution under a fake projects tree (no candidates,
exact, prefix, prefix-with-tie-break-on-mtime), `iter_lines_with_ts`
(`--since` filter, torn-line tolerance), and follow-mode (line append,
truncation handling).

## Why it lives in claude-watch

`active_agents.rs` already walks the same `~/.claude/projects/<slug>/<sid>/subagents/`
tree to enumerate live agents — this CLI is the "drill in on one of them"
companion to that subcommand. Both tools share the same path convention
and resolution heuristics. `claude-watch active-agents --json` is the
canonical "what agents exist" enumeration; `agent-tail <id>` is the
canonical "what is this one doing" view.
