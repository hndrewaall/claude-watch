# dashboard layout config

The `dashboard` script (in this repo) creates and manages a tmux session
holding Claude Code (and, optionally, side panes / extra windows for
monitoring). It reads layout overrides from
`~/.config/dashboard/layout.conf` (override path with `$DASHBOARD_CONF`).

This document is the source of truth for the layout file's schema.

## Default behavior — no config file

If the config file does not exist, the default layout is the simplest
possible:

- one tmux session named `dashboard`,
- one window named `main`,
- one pane running `claude --continue` (or `claude` with `--fresh`).

Nothing else. `claude-watch` only needs that pane to exist in order to
monitor Claude Code health. Anything beyond that is opt-in.

## Format

The config is a small INI dialect parsed by `dashboard-lib.sh`. Two
sections are recognized:

| Section    | Purpose                                                       |
|------------|---------------------------------------------------------------|
| `[main]`   | Pane composition of window 0 ("main")                         |
| `[windows]`| Extra windows (each entry creates one window)                 |

Lines starting with `#` are comments. Blank lines are ignored. Whitespace
around `=` is stripped. Unknown keys are ignored.

## `[main]` keys

| Key              | Default | Effect                                                       |
|------------------|---------|--------------------------------------------------------------|
| `top_left`       | (unset) | Command for an extra pane top-left of the claude pane        |
| `top_right`      | (unset) | Command for an extra pane to the right of the claude pane    |
| `sidebar_width`  | `25`    | Width in columns of the right-side pane (when `top_right` set) |
| `claude_percent` | `45`    | Height % of the claude pane when both `top_left` + `top_right` are set |

Layout dispatch table (which combinations produce which arrangement):

| `top_left` | `top_right` | Result                                                          |
|------------|-------------|-----------------------------------------------------------------|
| unset      | unset       | Claude only (single full-screen pane). **The default.**         |
| unset      | set         | Side-by-side — claude (left, full height) + sidebar (right).    |
| set        | unset       | Top pane (`top_left` cmd, full width) + claude below.           |
| set        | set         | Top split (`top_left` left, `top_right` right) + claude below.  |

## `[windows]` entries

Each key under `[windows]` becomes one extra tmux window. The window's
name is the key. The value is the shell command for the first pane.

To put multiple panes inside one window, separate commands with `///`
(three slashes). Each pane is created via a vertical split.

```ini
[windows]
monitor = glances /// sudo htop
logs = journalctl -u claude-watch -f
```

This creates:

- window 1 `monitor` with two panes (top: `glances`, bottom: `sudo htop`)
- window 2 `logs` with one pane (`journalctl -u claude-watch -f`)

Window indices follow file order.

## Example: simple default (no config)

No file. `dashboard --recreate` produces:

```
0: main* (1 panes)
```

## Example: full local layout

```ini
[main]
top_right = sidebar
sidebar_width = 25
claude_percent = 45

[windows]
monitor = glances /// sudo htop
logs = journalctl -u claude-watch -f --no-hostname
```

Produces:

```
0: main*    (2 panes)  ← claude (left, full height) + sidebar (right, 25 cols)
1: monitor  (2 panes)  ← glances (top) + sudo htop (bottom)
2: logs     (1 panes)  ← journalctl
```

## Migration

If you were running the old hardcoded layout (claude + sidebar + monitor +
logs), drop a `layout.conf` matching the "full local layout" example
above and you keep the previous behavior. With no file present, you get
the new simple default.

## Testing

Unit tests for the parser live at
`tools/dashboard/tests/dashboard-parser.test` (Python, sources
`dashboard-lib.sh` in a bash subshell). Run them directly:

```bash
./tools/dashboard/tests/dashboard-parser.test
```

End-to-end verification — build a layout against a throwaway tmux session
(use `DASHBOARD_SESSION=dashboard-test`, `--no-attach`, and a stubbed
`claude` in `$PATH`), inspect with `tmux list-windows`, then
`tmux kill-session`.
