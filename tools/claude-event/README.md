# claude-event

Two CLIs for the **claude-event** bus that the `tools/watchers/claude-event-watch`
script consumes:

- `claude-event` — emit a JSON event into the queue directory (atomic rename).
- `claude-event-tail` — read-only viewer for the consumed-event ring buffer.

These are the canonical implementations.

## Architecture

```
producer (cron, alertmanager, queue, torrent, security)
  └── claude-event "msg" --tag X --source SRC --data K=V
        └── atomic write to $CLAUDE_EVENT_QUEUE/<ns_ts>_<safe_tag>.json

consumer
  └── claude-event-watch (running as a watcher)
        ├── prints "EVENT[<source>/<tag>] <msg…>" one-liner per event
        ├── appends compact JSON to $CLAUDE_EVENT_LOG_DIR/consumed.jsonl
        └── deletes the queue file

operator
  └── claude-event-tail [--n N] [--tag T] [--source S] [--since DUR] [--json]
```

## `claude-event` — emit

```
claude-event "message" \
    [--tag TAG] \
    [--priority low|normal|high|urgent] \
    [--source cron|alertmanager|queue|torrent|security|claude-watch|manual] \
    [--source-name NAME] \
    [--data KEY=VAL ...]
```

Validates `--source` against an allowlist; rejects unknown sources with rc=2.
Generates a filename of the shape `<ns_ts>_<safe_tag>.json` (slashes and other
non-alnum characters in the tag are sanitised to `_`).

Environment:

- `$CLAUDE_EVENT_QUEUE` — queue directory (default: `~/claude-events/`).
  Legacy `$CRON_EVENT_QUEUE` is honored as fallback.
- `$CLAUDE_EVENT_SOURCE_NAME` / `$CRON_COMMAND` — fallback for `--source-name`.

## `claude-event-tail` — read

```
claude-event-tail [-n N] [--tag PAT] [--source SRC] [--since DUR] [--json]
```

Reads the consumed-event ring buffer (`consumed.jsonl` + rotations
`.1`..`.N`). Default: pretty table, last 20 entries, newest-first.

Filters:

- `--tag PAT` — case-insensitive substring match
- `--source SRC` — exact source match
- `--since DUR` — only entries within `5m | 1h | 1d | 7d` (suffix `s/m/h/d/w`)

Environment:

- `$CLAUDE_EVENT_LOG_DIR` — log directory (default `~/.config/claude-events/`).

This is read-only; it never deletes or modifies the log.

## Schema

Every event is a single JSON object:

```json
{
  "timestamp":     1735776000.123,
  "timestamp_iso": "2026-01-01T12:00:00-05:00",
  "hostname":      "...",
  "source":        "cron",
  "source_name":   "stronglifts-sync",
  "tag":           "stronglifts-sync",
  "priority":      "normal",
  "message":       "...",
  "data":          {"key": "val"},
  "pid":           12345,
  "user":          "..."
}
```

The same JSON gets compacted onto a single line in `consumed.jsonl` once the
watcher consumes it.

## Tests

```
python3 tools/claude-event/tests/test_claude_event.py
# or:
make test-claude-event
```

Exercises the emit + tail round trip (11 cases) under per-test tempdirs so
the live `~/claude-events/` is never touched.
