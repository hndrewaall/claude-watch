# claude-watch

A Rust daemon that monitors [Claude Code](https://claude.ai/code) sessions running in tmux. Detects activity states, recovers from stalls, and manages the tmux layout.

## What it does

claude-watch captures the Claude Code tmux pane every few seconds and parses it to determine what Claude is doing:

- **Activity detection**: Thinking, Writing, ToolRunning, Idle, ForegroundBash, ShellPrompt
- **Health monitoring**: Detects zombie sessions (no heartbeat), token stalls (context exhaustion), prolonged thinking, and foreground blocks
- **Recovery actions**: Injects prompts to resume stalled sessions, triggers context clears, sends alerts via Pushover
- **Fresh session detection**: Detects when Claude Code starts fresh (via `dashboard --recreate --fresh`) and injects a resume prompt
- **Task monitoring**: Watches Claude Code's background task output files, tracks agent lifecycle, cleans up orphaned tmux panes

## Architecture

```
claude-watch (systemd service)
    |
    +-- main loop (3s interval)
    |       Captures tmux pane -> detect_activity() -> policy decisions
    |       Tracks: tokens, bashes, dead checks, thinking duration
    |
    +-- task-watch loop (5s interval)
    |       Monitors /tmp/claude-1000/.../tasks/ via inotify
    |       Tracks task lifecycle, cleans up done tasks
    |
    +-- dashboard / dashboard-refit (shell scripts)
            Creates and manages the tmux session layout
```

### Key modules

| Module | Purpose |
|--------|---------|
| `tmux.rs` | Pane capture, `detect_activity()`, key injection |
| `policy.rs` | Decision engine: when to alert, inject, recover |
| `state.rs` | Persistent state (JSON): dead checks, inject flags, history |
| `status.rs` | Status bar parsing (tokens, bashes, compact %) |
| `task_watch.rs` | Background task and agent lifecycle monitoring |
| `alert.rs` | Pushover notifications |
| `config.rs` | TOML configuration |

### Dashboard scripts

The `dashboard` script creates a tmux session with Claude Code and optional companion panes. Layout is configured via `~/.config/dashboard/layout.conf`:

```ini
[main]
top_right = sidebar        # fixed-width right pane
sidebar_width = 25
claude_percent = 45        # claude pane height %

[windows]
monitor = glances /// htop   # extra window, panes split by ///
logs = journalctl -f         # single-pane window
```

## What it doesn't do

claude-watch monitors the session and recovers from failures, but it has no memory of what Claude was working on. It can detect "Claude is idle" or "Claude is stuck," but it can't tell Claude *what to resume*.

**You need a separate memory/session continuity system** to make recovery useful. In our setup, this is a set of session tools (`session-resume`, `session-task`, `session-event`, `session-log`) that:

- Save resume actions before context clears (`session-task set "what to do next"`)
- Gather full session state on startup (`session-resume boot`)
- Track completed work across clears (`session-task complete`)
- Log session events for debugging (`session-event`)

Without something like this, claude-watch can inject "resume" into a fresh session, but Claude won't know what it was doing. The memory system is the bridge between "session is alive" (claude-watch's job) and "session knows what to do" (the memory system's job).

## Build & run

```bash
make test       # run all tests (~300 tests, <1s unit, ~10s e2e)
make build      # release build
make deploy     # build + systemctl restart
```

## Configuration

`~/.config/claude-watch/config.toml`:

```toml
[tmux]
dashboard_session = "dashboard"
dashboard_pane = ""   # auto-detected from /var/run/claude/pane-id

[thresholds]
dead_process_checks = 5        # consecutive dead checks before action
thinking_interrupt_secs = 180  # prolonged thinking threshold
fg_block_secs = 15             # foreground bash block threshold

[alerts]
pushover_user = "..."
pushover_token = "..."
```

## License

Private repository. Not open source.
