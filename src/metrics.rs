//! metrics — write Prometheus textfile metrics for node-exporter.
//!
//! Rust port of `claude-watch-metrics` (Python). Reads
//! `~/.config/claude-watch/state.json` and writes
//! `/var/lib/node-exporter/textfile/claude_watch.prom` atomically.
//!
//! Run from cron every minute:
//!     * * * * * /home/hndrewaall/bin/claude-watch metrics

use chrono::DateTime;
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const PROM_FILE: &str = "/var/lib/node-exporter/textfile/claude_watch.prom";

fn default_state_file() -> PathBuf {
    if let Ok(s) = std::env::var("CLAUDE_WATCH_STATE") {
        return PathBuf::from(s);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(home).join(".config/claude-watch/state.json")
}

/// Parse an ISO 8601 timestamp into epoch seconds (float).
/// Returns 0.0 on failure — matches Python behavior.
fn parse_iso_timestamp(ts: &str) -> f64 {
    let ts = ts.trim();
    if ts.is_empty() {
        return 0.0;
    }
    // Try RFC3339 first (covers +HH:MM offsets that Rust's chrono handles)
    if let Ok(dt) = DateTime::parse_from_rfc3339(ts) {
        return dt.timestamp() as f64 + (dt.timestamp_subsec_nanos() as f64 / 1e9);
    }
    // Fallback: naive / other format
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S%.f") {
        return dt.and_utc().timestamp() as f64;
    }
    0.0
}

fn num(v: &Value, key: &str) -> u64 {
    v.get(key)
        .and_then(|x| x.as_u64().or_else(|| x.as_i64().map(|n| n.max(0) as u64)))
        .unwrap_or(0)
}

fn build_metrics(state: &Value, current_version: &str, latest_version: &str) -> Vec<String> {
    let last_check = state
        .get("last_check")
        .and_then(|v| v.as_str())
        .map(parse_iso_timestamp)
        .unwrap_or(0.0);
    let last_context_clear = state
        .get("last_context_clear")
        .and_then(|v| v.as_str())
        .map(parse_iso_timestamp)
        .unwrap_or(0.0);

    let last_known_tokens = num(state, "last_known_tokens");
    let last_known_bashes = num(state, "last_known_bashes");
    let consecutive_failures = num(state, "consecutive_failures");
    let consecutive_dead = num(state, "consecutive_dead_checks");
    let alert_count = num(state, "alert_count");
    let restart_count = num(state, "restart_count");

    // Watcher health
    let (watchers_missing, watchers_total) = match state.get("watcher_health") {
        Some(Value::Object(map)) => {
            let mut missing = 0u64;
            let mut total = 0u64;
            for (_k, w) in map {
                let enabled = w.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false);
                if enabled {
                    total += 1;
                    let cm = w
                        .get("consecutive_missing")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    if cm > 3 {
                        missing += 1;
                    }
                }
            }
            (missing, total)
        }
        _ => (0, 0),
    };

    let watcher_inject = num(state, "watcher_inject_count");
    let thinking_interrupt = num(state, "thinking_interrupt_count");
    let auto_update = num(state, "auto_update_count");
    let heartbeat_stale = num(state, "heartbeat_stale_count");

    vec![
        "# HELP claude_watch_up Whether claude-watch state file is readable".to_string(),
        "# TYPE claude_watch_up gauge".to_string(),
        "claude_watch_up 1".to_string(),
        "".to_string(),
        "# HELP claude_heartbeat_timestamp_seconds Epoch of last successful claude-watch check"
            .to_string(),
        "# TYPE claude_heartbeat_timestamp_seconds gauge".to_string(),
        format!("claude_heartbeat_timestamp_seconds {:.3}", last_check),
        "".to_string(),
        "# HELP claude_context_tokens Current context token count".to_string(),
        "# TYPE claude_context_tokens gauge".to_string(),
        format!("claude_context_tokens {}", last_known_tokens),
        "".to_string(),
        "# HELP claude_bash_count Number of bash calls in current context".to_string(),
        "# TYPE claude_bash_count gauge".to_string(),
        format!("claude_bash_count {}", last_known_bashes),
        "".to_string(),
        "# HELP claude_consecutive_failures Number of consecutive check failures".to_string(),
        "# TYPE claude_consecutive_failures gauge".to_string(),
        format!("claude_consecutive_failures {}", consecutive_failures),
        "".to_string(),
        "# HELP claude_consecutive_dead_checks Number of consecutive dead-process checks"
            .to_string(),
        "# TYPE claude_consecutive_dead_checks gauge".to_string(),
        format!("claude_consecutive_dead_checks {}", consecutive_dead),
        "".to_string(),
        "# HELP claude_alert_count Total alerts fired this session".to_string(),
        "# TYPE claude_alert_count gauge".to_string(),
        format!("claude_alert_count {}", alert_count),
        "".to_string(),
        "# HELP claude_restart_count Total restarts performed".to_string(),
        "# TYPE claude_restart_count gauge".to_string(),
        format!("claude_restart_count {}", restart_count),
        "".to_string(),
        "# HELP claude_watchers_missing Number of enabled watchers currently missing".to_string(),
        "# TYPE claude_watchers_missing gauge".to_string(),
        format!("claude_watchers_missing {}", watchers_missing),
        "".to_string(),
        "# HELP claude_watchers_total Total number of enabled watchers".to_string(),
        "# TYPE claude_watchers_total gauge".to_string(),
        format!("claude_watchers_total {}", watchers_total),
        "".to_string(),
        "# HELP claude_last_context_clear_timestamp_seconds Epoch of last context clear"
            .to_string(),
        "# TYPE claude_last_context_clear_timestamp_seconds gauge".to_string(),
        format!(
            "claude_last_context_clear_timestamp_seconds {:.3}",
            last_context_clear
        ),
        "".to_string(),
        "# HELP claude_version_info Claude Code version info".to_string(),
        "# TYPE claude_version_info gauge".to_string(),
        format!(
            "claude_version_info{{current=\"{}\",latest=\"{}\"}} 1",
            current_version, latest_version
        ),
        "".to_string(),
        "# HELP claude_watcher_inject_total Total watcher inject events".to_string(),
        "# TYPE claude_watcher_inject_total counter".to_string(),
        format!("claude_watcher_inject_total {}", watcher_inject),
        "".to_string(),
        "# HELP claude_thinking_interrupt_total Total thinking interrupt events".to_string(),
        "# TYPE claude_thinking_interrupt_total counter".to_string(),
        format!("claude_thinking_interrupt_total {}", thinking_interrupt),
        "".to_string(),
        "# HELP claude_auto_update_total Total auto-update events".to_string(),
        "# TYPE claude_auto_update_total counter".to_string(),
        format!("claude_auto_update_total {}", auto_update),
        "".to_string(),
        "# HELP claude_heartbeat_stale_total Total heartbeat stale events".to_string(),
        "# TYPE claude_heartbeat_stale_total counter".to_string(),
        format!("claude_heartbeat_stale_total {}", heartbeat_stale),
    ]
}

fn down_metrics() -> Vec<String> {
    vec![
        "# HELP claude_watch_up Whether claude-watch state file is readable".to_string(),
        "# TYPE claude_watch_up gauge".to_string(),
        "claude_watch_up 0".to_string(),
    ]
}

/// Atomic write: temp file in same dir + rename.
fn write_prom(lines: &[String], path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = format!("{}\n", lines.join("\n"));
    let tmp_path = path.with_extension("prom.tmp");
    {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o644))?;
    }
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Fetch version info via `claude-watch status --json` (best-effort).
fn fetch_version_info() -> (String, String) {
    let out = std::process::Command::new("claude-watch")
        .args(["status", "--json"])
        .output();
    if let Ok(o) = out {
        if o.status.success() {
            if let Ok(v) = serde_json::from_slice::<Value>(&o.stdout) {
                let cur = v
                    .get("version")
                    .and_then(|x| x.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let latest = v
                    .get("latest")
                    .and_then(|x| x.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                return (cur, latest);
            }
        }
    }
    ("unknown".to_string(), "unknown".to_string())
}

/// CLI entry point: `claude-watch metrics`.
pub fn cmd_metrics() -> i32 {
    let state_path = default_state_file();
    let prom_path = PathBuf::from(PROM_FILE);

    let state_str = match fs::read_to_string(&state_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error reading state file: {e}");
            let _ = write_prom(&down_metrics(), &prom_path);
            return 1;
        }
    };
    let state: Value = match serde_json::from_str(&state_str) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error parsing state file: {e}");
            let _ = write_prom(&down_metrics(), &prom_path);
            return 1;
        }
    };

    let (cur, latest) = fetch_version_info();
    let lines = build_metrics(&state, &cur, &latest);
    if let Err(e) = write_prom(&lines, &prom_path) {
        eprintln!("Error writing prom file: {e}");
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_iso_rfc3339() {
        let v = parse_iso_timestamp("2026-01-01T00:00:00-05:00");
        assert!(v > 0.0);
    }

    #[test]
    fn parse_iso_empty_is_zero() {
        assert_eq!(parse_iso_timestamp(""), 0.0);
    }

    #[test]
    fn parse_iso_garbage_is_zero() {
        assert_eq!(parse_iso_timestamp("not a date"), 0.0);
    }

    #[test]
    fn build_metrics_minimal() {
        let state = json!({});
        let lines = build_metrics(&state, "1.2.3", "1.2.4");
        // Key lines present
        assert!(lines.iter().any(|l| l == "claude_watch_up 1"));
        assert!(lines.iter().any(|l| l == "claude_context_tokens 0"));
        assert!(lines
            .iter()
            .any(|l| l.contains("claude_version_info{current=\"1.2.3\",latest=\"1.2.4\"} 1")));
    }

    #[test]
    fn build_metrics_watcher_health() {
        let state = json!({
            "watcher_health": {
                "signal-wait": {"enabled": true, "consecutive_missing": 0},
                "torrent-wait": {"enabled": true, "consecutive_missing": 5},
                "dead-one": {"enabled": false, "consecutive_missing": 10},
            },
            "last_known_tokens": 42,
            "alert_count": 3,
        });
        let lines = build_metrics(&state, "x", "y");
        assert!(lines.iter().any(|l| l == "claude_watchers_total 2"));
        assert!(lines.iter().any(|l| l == "claude_watchers_missing 1"));
        assert!(lines.iter().any(|l| l == "claude_context_tokens 42"));
        assert!(lines.iter().any(|l| l == "claude_alert_count 3"));
    }

    #[test]
    fn down_metrics_format() {
        let lines = down_metrics();
        assert!(lines.iter().any(|l| l == "claude_watch_up 0"));
    }

    #[test]
    fn write_and_read_prom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.prom");
        let lines = vec![
            "a".to_string(),
            "b".to_string(),
            "".to_string(),
            "c".to_string(),
        ];
        write_prom(&lines, &path).unwrap();
        let read = std::fs::read_to_string(&path).unwrap();
        assert_eq!(read, "a\nb\n\nc\n");
    }
}
