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
agent-tail <agent_id> --verbose        # include per-record metadata
agent-tail <agent_id> --path           # print the resolved path
agent-tail --list                      # all agents under ~/.claude/projects
agent-tail --test                      # run the embedded test suite
```

Truncation knobs (defaults aim at 'readable in a terminal'):

```
--truncate-tool-result LINES   # cap each tool-result preview (default 20)
--truncate-chars CHARS         # cap each rendered text block (default 2000)
--no-ts                        # drop the leading [timestamp] prefix
-v / --verbose                 # emit per-record metadata after each
                               # headline (model, usage tokens,
                               # stop_reason, requestId,
                               # attributionAgent, agentId, slug, uuid,
                               # parentUuid, sourceToolAssistantUUID,
                               # cwd, gitBranch, etc.).
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
[<ts>] USER_IMAGE [<media-type> / <source-type>] (<N> base64 chars)
[<ts>] ASSISTANT: <text>
[<ts>] TOOL_CALL <Name>(<one-line-args>)  [toolu_...]  via=<direct|subagent>
[<ts>] TOOL_RESULT [toolu_...]
         <body line 1>
         <body line 2>
         ... [N more line(s) truncated]
         toolUseResult:                  # surfaced when it diverges from `content`
           <side-channel summary>
[<ts>] TOOL_RESULT [error] [toolu_...]
         <body>
[<ts>] THINKING: <text>
[<ts>] PROGRESS: type=<hook_progress> hookEvent=... hookName=... toolUseID=...
[<ts>] SYSTEM: subtype=<...> duration=<N>s level=<...>
         <content body, when present>
[<ts>] ATTACHMENT [<type>]
         added (N): Foo, Bar, ...        # for deferred_tools_delta
         <content>                       # for skill_listing / hook_additional_context
[<ts>] EVENT: <repr>                     # unknown record types
```

`--json` skips all of that and just emits the original JSONL line.

`--verbose` appends a metadata block after each headline:

```
[<ts>] ASSISTANT: <text>
  model: claude-opus-4-7
  usage: in=5 out=3 cache_create=1234 cache_read=5678 tier=standard
  cache_creation: 5m=1234 1h=0
  stop_reason: end_turn
  requestId: req_011C...
  attributionAgent: torrent-process       # which subagent type owns this turn
  agentId: a194312241cc25f9c
  slug: validated-finding-waterfall       # claude-watch's friendly label
  uuid: ...
  parentUuid: ...
  sourceToolAssistantUUID: ...            # on tool_result rows
  sessionId: ...
  gitBranch: HEAD
  cwd: /path
  version: 2.1.139
  userType: external
  entrypoint: cli
  isSidechain: True                       # only when non-default
```

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
make test-agent-tail            # 36 cases, all in-process
agent-tail --test               # same, direct invocation
```

Coverage: pure helpers (duration parsing, ISO-ts parsing, line truncation,
arg flattening), `format_record` dispatch (every record type, error path,
thinking, unknown, tool_use caller, tool_result side-channel,
user image blocks, progress events, system subtypes, attachment shapes,
verbose metadata block on/off), resolution under a fake projects tree
(no candidates, exact, prefix, prefix-with-tie-break-on-mtime),
`iter_lines_with_ts` (`--since` filter, torn-line tolerance), and
follow-mode (line append, truncation handling).

## Why it lives in claude-watch

`active_agents.rs` already walks the same `~/.claude/projects/<slug>/<sid>/subagents/`
tree to enumerate live agents — this CLI is the "drill in on one of them"
companion to that subcommand. Both tools share the same path convention
and resolution heuristics. `claude-watch active-agents --json` is the
canonical "what agents exist" enumeration; `agent-tail <id>` is the
canonical "what is this one doing" view.
