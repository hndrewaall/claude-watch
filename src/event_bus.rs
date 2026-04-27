//! Claude-event bus emitter.
//!
//! Writes structured JSON events into `~/claude-events/` so that
//! `claude-event-watch` surfaces them to the main loop. This is an
//! ADDITIVE third alert sink alongside Pushover (`pingme`) and the
//! tmux-inject prompt — those paths must keep firing whether or not
//! event emission succeeds.
//!
//! Field shape mirrors what `~/bin/claude-event` (Python helper) writes
//! so the consumer (`claude-event-watch`) needs no special-case logic.
//! The full event JSON is:
//!
//! ```json
//! {
//!   "timestamp": <unix float>,
//!   "timestamp_iso": "<RFC 3339 local>",
//!   "hostname": "...",
//!   "source": "claude-watch",
//!   "source_name": "claude-watch",
//!   "tag": "claude-watch-alert",
//!   "priority": "low|normal|high|urgent",
//!   "message": "<full human-readable, same as Pushover body>",
//!   "data": {
//!       "alert_type":        "<heartbeat-stale|prolonged-thinking|...>",
//!       "stuck_reason":      "<short human-readable>",
//!       "stale_minutes":     <int|null>,
//!       "affected_watchers": ["<name>", ...],
//!       "severity":          "<low|medium|high|critical>"
//!   },
//!   "pid":  <int>,
//!   "user": "..."
//! }
//! ```
//!
//! Note: `source` is **`claude-watch`**, which is outside the canonical
//! source enum used by the Python helper (`cron|alertmanager|queue|...`).
//! `claude-event-watch` itself doesn't validate `source` — it dispatches
//! by `tag`. The new tag is `claude-watch-alert`; see
//! `~/.claude/projects/-home-hndrewaall/memory/claude-event-routing.md`.
//!
//! Writes are atomic (tmp file in same dir + rename). Filename:
//! `<unix_ns>_claude-watch-alert.json` (matches Python helper convention).
//!
//! On any error the function logs and returns — never panics, never
//! propagates failure to the caller. Same default-open principle as the
//! obligations PreToolUse hook: a broken alert sink must not blackhole
//! Pushover or tmux-inject.

use serde::Serialize;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Severity levels that map cleanly onto Pushover priority and downstream
/// triage decisions in the routing table.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    /// Map severity → claude-event `priority` field. Mirrors what the
    /// Python `claude-event` helper accepts (`low|normal|high|urgent`).
    pub fn as_priority(self) -> &'static str {
        match self {
            Severity::Low => "low",
            Severity::Medium => "normal",
            Severity::High => "high",
            Severity::Critical => "urgent",
        }
    }
}

/// One claude-watch alert. Caller fills the fields it knows; missing
/// optional fields default to None / empty.
#[derive(Debug, Clone)]
pub struct ClaudeWatchAlert<'a> {
    /// Discriminator string matching the codebase alert paths:
    ///   `heartbeat-stale`, `prolonged-thinking`, `watcher-down`,
    ///   `fresh-clear-stuck`, `claude-crashed`, `auto-update-failed`,
    ///   `auto-update-complete`, `reauth-needed`, `wedged-pane`.
    pub alert_type: &'a str,
    /// Short human-readable reason. For heartbeat-path alerts this is
    /// the same `stuck_reason` already threaded through `policy.rs`.
    pub stuck_reason: &'a str,
    /// Heartbeat staleness in minutes (only meaningful for
    /// `heartbeat-stale`; None elsewhere).
    pub stale_minutes: Option<u64>,
    /// Names of watchers known to be missing (only meaningful for
    /// `watcher-down`; empty elsewhere).
    pub affected_watchers: Vec<String>,
    /// Severity tier driving Pushover priority + dispatch routing.
    pub severity: Severity,
    /// Full human-readable message, byte-for-byte the same string sent
    /// to Pushover so log/event/push all agree.
    pub message: &'a str,
}

/// Build the JSON event body. Public for testability — production
/// callers should use `emit()` which also performs the atomic write.
pub fn build_event_json(alert: &ClaudeWatchAlert<'_>) -> serde_json::Value {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let now_iso = chrono::Local::now().to_rfc3339();
    let hostname = hostname_string();
    let user = std::env::var("USER").unwrap_or_default();
    let pid = std::process::id();

    serde_json::json!({
        "timestamp": now,
        "timestamp_iso": now_iso,
        "hostname": hostname,
        "source": "claude-watch",
        "source_name": "claude-watch",
        "tag": "claude-watch-alert",
        "priority": alert.severity.as_priority(),
        "message": alert.message,
        "data": {
            "alert_type": alert.alert_type,
            "stuck_reason": alert.stuck_reason,
            "stale_minutes": alert.stale_minutes,
            "affected_watchers": alert.affected_watchers,
            "severity": alert.severity,
        },
        "pid": pid,
        "user": user,
    })
}

/// Resolve the queue dir. Honors `CLAUDE_EVENT_QUEUE` (preferred) and
/// the legacy `CRON_EVENT_QUEUE`, matching the Python helper. Falls
/// back to `~/claude-events/`.
pub fn queue_dir() -> PathBuf {
    if let Ok(p) = std::env::var("CLAUDE_EVENT_QUEUE") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Ok(p) = std::env::var("CRON_EVENT_QUEUE") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join("claude-events")
}

/// Emit a claude-event JSON file into the queue dir. Default-open: any
/// I/O failure is logged at warn level and swallowed. The caller's
/// Pushover + tmux-inject paths must remain unaffected.
pub fn emit(alert: &ClaudeWatchAlert<'_>) {
    let dir = queue_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, dir = %dir.display(),
            "claude-event emit: failed to create queue dir, skipping");
        return;
    }

    let event = build_event_json(alert);
    let body = match serde_json::to_string_pretty(&event) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "claude-event emit: failed to serialize event");
            return;
        }
    };

    // Atomic write: tmp file in same dir + rename. Filename matches
    // the Python helper's <ts_ns>_<safe_tag>.json convention so any
    // tooling that parses filenames stays compatible.
    let ts_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let final_name = format!("{}_claude-watch-alert.json", ts_ns);
    let final_path = dir.join(&final_name);
    let tmp_path = dir.join(format!(".{}.tmp", final_name));

    if let Err(e) = std::fs::write(&tmp_path, body.as_bytes()) {
        tracing::warn!(error = %e, path = %tmp_path.display(),
            "claude-event emit: failed to write tmp file");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp_path, &final_path) {
        tracing::warn!(error = %e, src = %tmp_path.display(), dst = %final_path.display(),
            "claude-event emit: failed to rename tmp into place");
        // best-effort cleanup
        let _ = std::fs::remove_file(&tmp_path);
        return;
    }

    tracing::info!(
        path = %final_path.display(),
        alert_type = %alert.alert_type,
        severity = ?alert.severity,
        "claude-event emitted"
    );
}

/// Cheap, no-deps hostname lookup. Falls back to `gethostname`'s
/// failure mode (empty string) — the event still emits, the field is
/// just blank.
fn hostname_string() -> String {
    // Try /etc/hostname first (cheap, no syscall), then `uname -n`,
    // then env. nix could supply this but we already pull libc; keep
    // this dep-free.
    if let Ok(s) = std::fs::read_to_string("/etc/hostname") {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(s) = std::env::var("HOSTNAME") {
        if !s.is_empty() {
            return s;
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_maps_to_priority() {
        assert_eq!(Severity::Low.as_priority(), "low");
        assert_eq!(Severity::Medium.as_priority(), "normal");
        assert_eq!(Severity::High.as_priority(), "high");
        assert_eq!(Severity::Critical.as_priority(), "urgent");
    }

    #[test]
    fn build_event_json_has_required_fields() {
        let alert = ClaudeWatchAlert {
            alert_type: "heartbeat-stale",
            stuck_reason: "heartbeat stale (574min, threshold=10min, watchmen=8)",
            stale_minutes: Some(574),
            affected_watchers: vec![],
            severity: Severity::High,
            message: "Claude stuck: heartbeat stale (574min, threshold=10min, watchmen=8). 2 consecutive checks failed.",
        };

        let v = build_event_json(&alert);

        assert_eq!(v["tag"], "claude-watch-alert");
        assert_eq!(v["source"], "claude-watch");
        assert_eq!(v["source_name"], "claude-watch");
        assert_eq!(v["priority"], "high");
        assert_eq!(v["message"], alert.message);
        assert!(v["timestamp"].is_number());
        assert!(v["timestamp_iso"].is_string());
        assert!(v["pid"].is_number());

        let data = &v["data"];
        assert_eq!(data["alert_type"], "heartbeat-stale");
        assert_eq!(data["stuck_reason"], alert.stuck_reason);
        assert_eq!(data["stale_minutes"], 574);
        assert_eq!(data["severity"], "high");
        assert!(data["affected_watchers"].is_array());
        assert_eq!(data["affected_watchers"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn build_event_json_handles_watcher_down() {
        let alert = ClaudeWatchAlert {
            alert_type: "watcher-down",
            stuck_reason: "2 watcher(s) missing: signal-wait, torrent-wait",
            stale_minutes: None,
            affected_watchers: vec!["signal-wait".to_string(), "torrent-wait".to_string()],
            severity: Severity::Medium,
            message: "watchers down: signal-wait, torrent-wait",
        };

        let v = build_event_json(&alert);

        assert_eq!(v["data"]["alert_type"], "watcher-down");
        assert!(v["data"]["stale_minutes"].is_null());
        let watchers = v["data"]["affected_watchers"].as_array().unwrap();
        assert_eq!(watchers.len(), 2);
        assert_eq!(watchers[0], "signal-wait");
        assert_eq!(watchers[1], "torrent-wait");
        assert_eq!(v["priority"], "normal");
    }

    #[test]
    fn build_event_json_handles_minimal_alert() {
        // Pushover-only paths (e.g. auto-update-complete) carry no
        // structured stale/watcher data — verify the optional fields
        // serialise cleanly as null/empty.
        let alert = ClaudeWatchAlert {
            alert_type: "auto-update-complete",
            stuck_reason: "claude-watch: auto-update complete (1.0.0 → 1.0.1)",
            stale_minutes: None,
            affected_watchers: vec![],
            severity: Severity::Low,
            message: "claude-watch: auto-update complete (1.0.0 → 1.0.1)",
        };

        let v = build_event_json(&alert);
        assert_eq!(v["data"]["alert_type"], "auto-update-complete");
        assert!(v["data"]["stale_minutes"].is_null());
        assert!(v["data"]["affected_watchers"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(v["priority"], "low");
    }

    #[test]
    fn emit_writes_a_file_in_temp_queue_dir() {
        // Point CLAUDE_EVENT_QUEUE at a tempdir, emit, verify file lands
        // with valid JSON. Doesn't touch ~/claude-events/.
        let tmp = tempfile::tempdir().expect("tempdir");
        // Save & restore env; tests in this binary run in parallel
        // so be careful — we set + read + clean up in one shot.
        let prev = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        // Safety: tests in this module are not racy w.r.t. each other
        // because each call to emit() reads queue_dir() up-front before
        // any sub-task spawns. Other tests in this module don't touch
        // the env var.
        // SAFETY: setting an env var in a test module — Rust 1.85+ flags
        // this unsafe but our test threads do not concurrently mutate
        // CLAUDE_EVENT_QUEUE.
        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
        }

        let alert = ClaudeWatchAlert {
            alert_type: "prolonged-thinking",
            stuck_reason: "prolonged thinking (>300s)",
            stale_minutes: None,
            affected_watchers: vec![],
            severity: Severity::Medium,
            message: "prolonged thinking interrupt #1 fired",
        };
        emit(&alert);

        // Restore env first so a panic below doesn't leak state.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
            }
        }

        // Verify exactly one file was created with the right tag and
        // that it parses as valid JSON with our expected fields.
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read tempdir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with("_claude-watch-alert.json")
            })
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one event file");

        let content = std::fs::read_to_string(entries[0].path()).expect("read event");
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("event is valid JSON");
        assert_eq!(parsed["tag"], "claude-watch-alert");
        assert_eq!(parsed["data"]["alert_type"], "prolonged-thinking");
    }
}
