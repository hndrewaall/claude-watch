//! Persistent state: serialization, deserialization, load/save.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tracing::error;

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct State {
    pub last_check: Option<String>,
    pub consecutive_failures: u32,
    pub consecutive_dead_checks: u32,
    pub consecutive_fast_detections: u32,
    pub alert_count: u32,
    pub last_alert: Option<String>,
    pub last_fast_path_alert: Option<String>,
    pub last_restart: Option<String>,
    pub restart_count: u32,
    pub pending_resume_inject: bool,
    pub token_history: Vec<u64>,
    pub bash_history: Vec<u64>,
    pub last_failure: Option<String>,
    pub last_failure_detail: Option<FailureDetail>,
    pub last_status: Option<StatusSnapshot>,
    // Foreground monitor
    pub foreground_start: Option<String>,
    pub foreground_alerted: bool,
    // Thinking duration monitor
    #[serde(default)]
    pub thinking_start: Option<String>,
    #[serde(default)]
    pub thinking_alerted: bool,
    /// Count of consecutive thinking interrupts (for exponential backoff)
    #[serde(default)]
    pub thinking_interrupt_count: u32,
    // Last known pane/status for foreground polling (not persisted meaningfully)
    #[serde(default)]
    pub last_known_pane: String,
    #[serde(default)]
    pub last_known_tokens: u64,
    #[serde(default)]
    pub last_known_bashes: u64,
    // Context monitoring
    #[serde(default)]
    pub context_clear_triggered: bool,
    #[serde(default)]
    pub last_context_clear: Option<String>,
    #[serde(default)]
    pub context_clear_child_pid: Option<u32>,
    // Watcher health
    pub watcher_health: HashMap<String, WatcherState>,
    #[serde(default)]
    pub last_watcher_inject: Option<String>,
    /// Count of watcher inject events (for metrics)
    #[serde(default)]
    pub watcher_inject_count: u32,
    /// Count of auto-update events (for metrics)
    #[serde(default)]
    pub auto_update_count: u32,
    /// Count of heartbeat stale alert events (for metrics)
    #[serde(default)]
    pub heartbeat_stale_count: u32,
    // Auto-update tracking
    #[serde(default)]
    pub last_update_check: Option<String>,
    #[serde(default)]
    pub last_update_attempt: Option<String>,
    #[serde(default)]
    pub update_in_progress: bool,
    // Reauth detection
    #[serde(default)]
    pub reauth_detected: bool,
    #[serde(default)]
    pub last_reauth_alert: Option<String>,
    #[serde(default)]
    pub login_injected: bool,
    /// Tracks whether we've already injected "resume" for a fresh external session
    /// (tokens=0 with Claude idle prompt visible). Reset when tokens become non-zero.
    #[serde(default)]
    pub fresh_session_injected: bool,
    /// Tracks whether Claude was ever alive (tokens > 0) since the last fresh inject.
    /// Prevents the inject loop: inject → startup (tokens=0) → "dead" reset → re-inject.
    /// Only set to true when tokens > 0 while fresh_session_injected is true.
    #[serde(default)]
    pub was_alive_since_inject: bool,
    /// Timestamp of the last fresh session inject. Used as a fallback timeout: if Claude
    /// never becomes active within N minutes after inject, allow resetting the flag.
    #[serde(default)]
    pub last_fresh_inject: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FailureDetail {
    pub bashes: u64,
    pub watchmen: u32,
    pub stuck_reason: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StatusSnapshot {
    pub bashes: u64,
    pub watchmen: u32,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WatcherState {
    pub last_seen_running: Option<String>,
    pub consecutive_missing: u32,
    pub enabled: bool,
}

pub fn load_state(path: &str) -> State {
    match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => State::default(),
    }
}

pub fn save_state(path: &str, state: &State) {
    if let Some(parent) = Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(state) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path, json) {
                error!(error = %e, "failed to save state");
            }
        }
        Err(e) => error!(error = %e, "failed to serialize state"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_state() {
        let state = State::default();
        assert_eq!(state.consecutive_failures, 0);
        assert_eq!(state.consecutive_dead_checks, 0);
        assert_eq!(state.alert_count, 0);
        assert_eq!(state.restart_count, 0);
        assert!(!state.pending_resume_inject);
        assert!(state.token_history.is_empty());
        assert!(state.bash_history.is_empty());
        assert!(state.last_check.is_none());
        assert!(state.watcher_health.is_empty());
    }

    #[test]
    fn test_serialize_deserialize_roundtrip() {
        let mut state = State::default();
        state.consecutive_failures = 5;
        state.alert_count = 2;
        state.last_check = Some("2026-03-16T12:00:00-05:00".to_string());
        state.token_history = vec![100000, 100050, 100100];
        state.bash_history = vec![50, 48, 45];
        state.pending_resume_inject = true;
        state.last_failure_detail = Some(FailureDetail {
            bashes: 45,
            watchmen: 3,
            stuck_reason: "heartbeat stale".to_string(),
        });
        state.last_status = Some(StatusSnapshot {
            bashes: 45,
            watchmen: 3,
        });
        state.watcher_health.insert(
            "signal-wait".to_string(),
            WatcherState {
                last_seen_running: Some("2026-03-16T12:00:00-05:00".to_string()),
                consecutive_missing: 0,
                enabled: true,
            },
        );

        let json = serde_json::to_string_pretty(&state).expect("serialize");
        let restored: State = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.consecutive_failures, 5);
        assert_eq!(restored.alert_count, 2);
        assert_eq!(restored.last_check, state.last_check);
        assert_eq!(restored.token_history, vec![100000, 100050, 100100]);
        assert_eq!(restored.bash_history, vec![50, 48, 45]);
        assert!(restored.pending_resume_inject);
        assert!(restored.last_failure_detail.is_some());
        assert!(restored.last_status.is_some());
        assert_eq!(restored.watcher_health.len(), 1);
        assert!(restored.watcher_health.contains_key("signal-wait"));
    }

    #[test]
    fn test_load_state_missing_file() {
        let state = load_state("/tmp/nonexistent-claude-watch-test-state.json");
        assert_eq!(state.consecutive_failures, 0);
    }

    #[test]
    fn test_load_state_invalid_json() {
        let path = "/tmp/claude-watch-test-invalid-state.json";
        std::fs::write(path, "not json").unwrap();
        let state = load_state(path);
        assert_eq!(state.consecutive_failures, 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let path = "/tmp/claude-watch-test-state-roundtrip.json";
        let mut state = State::default();
        state.alert_count = 7;
        state.restart_count = 2;
        save_state(path, &state);

        let loaded = load_state(path);
        assert_eq!(loaded.alert_count, 7);
        assert_eq!(loaded.restart_count, 2);
        let _ = std::fs::remove_file(path);
    }
}
