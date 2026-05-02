#!/bin/bash
# dashboard-lib.sh — INI parser helpers for the `dashboard` script.
#
# Sourced by `dashboard` (and by the test harness in
# `tools/dashboard/tests/dashboard-parser.test`). Defines two pure-parsing
# functions that read `$CONF` (set by the caller) and emit values to stdout.
# No side effects, no tmux, no claude — just awk over an INI file.
#
# Functions:
#   conf_get  <key> <default> [section]  — print value or default
#   conf_windows                         — print "name<TAB>command" per
#                                           [windows] entry
#
# `$CONF` is the path to the layout file (typically
# `${XDG_CONFIG_HOME:-$HOME/.config}/dashboard/layout.conf`). When the file
# is absent, `conf_get` prints the default and `conf_windows` prints
# nothing.

# Parse config file (INI-style, simple key = value).
# Reads global $CONF.
conf_get() {
    local key="$1" default="$2" section="${3:-main}"
    if [ -f "$CONF" ]; then
        val=$(awk -v k="$key" -v sect="$section" '
            /^\[/ { gsub(/[\[\]]/, ""); current = $0; gsub(/^[[:space:]]+|[[:space:]]+$/, "", current); next }
            /^#/ { next }
            /^$/ { next }
            current == sect {
              idx = index($0, "=")
              if (idx == 0) next
              key_part = substr($0, 1, idx - 1)
              gsub(/^[[:space:]]+|[[:space:]]+$/, "", key_part)
              if (key_part == k) {
                  val = substr($0, idx + 1)
                  gsub(/^[[:space:]]+|[[:space:]]+$/, "", val)
                  print val
                  exit
              }
            }' "$CONF")
        [ -n "$val" ] && echo "$val" || echo "$default"
    else
        echo "$default"
    fi
}

# Parse [windows] section: emit "name<TAB>command" per entry.
# Commands may embed /// to request multiple panes; the caller splits.
# Reads global $CONF.
conf_windows() {
    [ -f "$CONF" ] || return
    awk '
        /^\[/ { gsub(/[\[\]]/, ""); section = $0; gsub(/^[[:space:]]+|[[:space:]]+$/, "", section); next }
        /^#/ { next }
        /^$/ { next }
        section == "windows" {
            idx = index($0, "=")
            if (idx == 0) next
            key = substr($0, 1, idx - 1)
            val = substr($0, idx + 1)
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", key)
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", val)
            if (key != "" && val != "") print key "\t" val
        }
    ' "$CONF"
}

# Decide whether window 0 needs a top split (any [main] command keys present).
# Reads global $TOP_LEFT, $TOP_RIGHT.
has_split() { [ -n "$TOP_LEFT" ] || [ -n "$TOP_RIGHT" ]; }

# Compute the expected pane count for window 0 ("main"):
#   1 (claude) + 1 if top_left is set + 1 if top_right is set.
# Reads global $TOP_LEFT, $TOP_RIGHT.
expected_panes() {
    local n=1
    [ -n "$TOP_LEFT" ] && n=$((n+1))
    [ -n "$TOP_RIGHT" ] && n=$((n+1))
    echo "$n"
}
