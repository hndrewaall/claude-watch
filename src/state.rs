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
    /// Timestamp of the last interrupt fired (across all fire paths:
    /// prolonged-thinking, watcher-down, context-warning). Used as the
    /// global post-interrupt cooldown gate so any one interrupt suppresses
    /// re-fires from the other paths for a short window.
    #[serde(default)]
    pub last_interrupt_at: Option<String>,
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
    /// Last observed token count (for detecting external clears)
    #[serde(default)]
    pub last_seen_tokens: Option<u64>,
    /// Number of consecutive check cycles where the pane has shown a "wedged"
    /// pattern (context limit reached / persistent rate limit). When this
    /// reaches `context_monitor.wedged_consecutive`, claude-watch runs
    /// `self-clear` itself rather than waiting for the agent to do it.
    #[serde(default)]
    pub wedged_consecutive: u32,
    /// Timestamp of the last wedged-triggered self-clear (cooldown gate).
    #[serde(default)]
    pub last_wedged_clear: Option<String>,
    /// Total wedged-triggered self-clears (for metrics).
    #[serde(default)]
    pub wedged_clear_count: u32,
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
    /// Cumulative count of prolonged-thinking interrupts (for metrics).
    /// Separate from `thinking_interrupt_count` which is a per-episode
    /// backoff index that resets when Claude exits the thinking state.
    #[serde(default)]
    pub prolonged_thinking_interrupts_total: u64,
    /// Cumulative count of foreground-blocking interrupts (for metrics).
    #[serde(default)]
    pub foreground_blocking_interrupts_total: u64,
    /// Cumulative count of context-warning interrupts (for metrics).
    /// The `fallback_clear_count` field shares the same fire site; this
    /// field is the canonical per-interrupt counter name.
    #[serde(default)]
    pub context_warning_interrupts_total: u64,
    /// Cumulative count of watcher-down interrupts (for metrics).
    /// The `watcher_inject_count` field shares the same fire site; this
    /// field is the canonical per-interrupt counter name.
    #[serde(default)]
    pub watcher_down_interrupts_total: u64,
    /// Cumulative count of wedged-pane self-clear interrupts (for metrics).
    #[serde(default)]
    pub wedged_clear_interrupts_total: u64,
    /// Cumulative count of auto-update interrupts (for metrics).
    /// The `auto_update_count` field shares the same fire site; this
    /// field is the canonical per-interrupt counter name.
    #[serde(default)]
    pub auto_update_interrupts_total: u64,
    /// Cumulative count of reauth `/login` injections (for metrics).
    #[serde(default)]
    pub reauth_inject_interrupts_total: u64,
    /// Cumulative count of post-restart resume injections (for metrics).
    #[serde(default)]
    pub post_restart_resume_inject_interrupts_total: u64,
    /// Cumulative count of fresh-external-session resume injections.
    #[serde(default)]
    pub fresh_session_inject_interrupts_total: u64,
    /// Cumulative count of fresh-/clear resume injections.
    #[serde(default)]
    pub fresh_clear_resume_inject_interrupts_total: u64,
    /// Cumulative count of restart-claude events (for metrics).
    /// The `restart_count` field shares the same fire site; this is the
    /// canonical per-interrupt counter name.
    #[serde(default)]
    pub restart_claude_interrupts_total: u64,
    /// Count of context-clear fallback injections (daemon injected `/clear`
    /// because the context_high hook fire was stale or absent).
    #[serde(default)]
    pub fallback_clear_count: u32,
    /// Count of version-update fallback injections (daemon ran `claude update`
    /// because the version_update hook fire was stale or absent).
    #[serde(default)]
    pub fallback_update_count: u32,
    /// Sum of reminder-to-action latency samples (seconds) for the context_high
    /// reminder. Used to emit a histogram-style rate via Prometheus counters.
    #[serde(default)]
    pub reminder_to_clear_latency_secs_sum: f64,
    /// Number of reminder-to-action latency samples collected for context_high.
    #[serde(default)]
    pub reminder_to_clear_latency_count: u64,
    /// Sum of reminder-to-action latency samples (seconds) for the version_update
    /// reminder.
    #[serde(default)]
    pub reminder_to_update_latency_secs_sum: f64,
    /// Number of reminder-to-action latency samples collected for version_update.
    #[serde(default)]
    pub reminder_to_update_latency_count: u64,
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
    let mut state: State = match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => State::default(),
    };
    // Transient timers are meaningless across daemon restarts — daemon
    // downtime makes the elapsed measurement unreliable and can trigger
    // spurious "prolonged thinking" interrupts within seconds of startup.
    // Clear them on load so tracking starts fresh.
    state.thinking_start = None;
    state.thinking_alerted = false;
    state.thinking_interrupt_count = 0;
    // last_interrupt_at is a short-lived global cooldown gate — daemon
    // downtime makes any persisted value meaningless (either stale or
    // indefinitely-suppressive). Clear on load so the next interrupt is
    // allowed to fire immediately.
    state.last_interrupt_at = None;
    state.foreground_start = None;
    state.foreground_alerted = false;
    // wedged_consecutive is transient — daemon downtime breaks the
    // "consecutive" semantics. Reset on load. (last_wedged_clear and
    // wedged_clear_count persist for cooldown + metrics.)
    state.wedged_consecutive = 0;
    state
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
        assert!(state.last_check.is_none());
        assert!(state.watcher_health.is_empty());
    }

    #[test]
    fn test_serialize_deserialize_roundtrip() {
        let mut state = State::default();
        state.consecutive_failures = 5;
        state.alert_count = 2;
        state.last_check = Some("2026-03-16T12:00:00-05:00".to_string());
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

    #[test]
    fn test_interrupt_counters_roundtrip() {
        let path = "/tmp/claude-watch-test-interrupt-counters.json";
        let mut state = State::default();
        state.prolonged_thinking_interrupts_total = 7;
        state.foreground_blocking_interrupts_total = 3;
        state.context_warning_interrupts_total = 11;
        state.watcher_down_interrupts_total = 42;
        state.wedged_clear_interrupts_total = 2;
        state.auto_update_interrupts_total = 19;
        state.reauth_inject_interrupts_total = 1;
        state.post_restart_resume_inject_interrupts_total = 4;
        state.fresh_session_inject_interrupts_total = 5;
        state.fresh_clear_resume_inject_interrupts_total = 6;
        state.restart_claude_interrupts_total = 8;
        save_state(path, &state);

        let loaded = load_state(path);
        assert_eq!(loaded.prolonged_thinking_interrupts_total, 7);
        assert_eq!(loaded.foreground_blocking_interrupts_total, 3);
        assert_eq!(loaded.context_warning_interrupts_total, 11);
        assert_eq!(loaded.watcher_down_interrupts_total, 42);
        assert_eq!(loaded.wedged_clear_interrupts_total, 2);
        assert_eq!(loaded.auto_update_interrupts_total, 19);
        assert_eq!(loaded.reauth_inject_interrupts_total, 1);
        assert_eq!(loaded.post_restart_resume_inject_interrupts_total, 4);
        assert_eq!(loaded.fresh_session_inject_interrupts_total, 5);
        assert_eq!(loaded.fresh_clear_resume_inject_interrupts_total, 6);
        assert_eq!(loaded.restart_claude_interrupts_total, 8);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_interrupt_counters_default_to_zero_on_missing_fields() {
        // State files written before these fields existed should still
        // deserialize — counters default to 0 (serde default).
        let path = "/tmp/claude-watch-test-interrupt-counters-default.json";
        std::fs::write(path, "{}").unwrap();
        let loaded = load_state(path);
        assert_eq!(loaded.prolonged_thinking_interrupts_total, 0);
        assert_eq!(loaded.foreground_blocking_interrupts_total, 0);
        assert_eq!(loaded.context_warning_interrupts_total, 0);
        assert_eq!(loaded.watcher_down_interrupts_total, 0);
        assert_eq!(loaded.wedged_clear_interrupts_total, 0);
        assert_eq!(loaded.auto_update_interrupts_total, 0);
        assert_eq!(loaded.reauth_inject_interrupts_total, 0);
        assert_eq!(loaded.post_restart_resume_inject_interrupts_total, 0);
        assert_eq!(loaded.fresh_session_inject_interrupts_total, 0);
        assert_eq!(loaded.fresh_clear_resume_inject_interrupts_total, 0);
        assert_eq!(loaded.restart_claude_interrupts_total, 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_interrupt_counters_preserved_across_load() {
        // load_state() explicitly resets some transient fields (thinking_start,
        // last_interrupt_at, etc.) but must NOT reset cumulative counters.
        let path = "/tmp/claude-watch-test-interrupt-counters-preserve.json";
        let mut state = State::default();
        state.prolonged_thinking_interrupts_total = 100;
        state.watcher_down_interrupts_total = 200;
        state.thinking_interrupt_count = 5; // transient (gets cleared on load)
        state.last_interrupt_at = Some("2026-01-01T00:00:00+00:00".to_string()); // transient
        save_state(path, &state);

        let loaded = load_state(path);
        // Cumulative counters preserved
        assert_eq!(loaded.prolonged_thinking_interrupts_total, 100);
        assert_eq!(loaded.watcher_down_interrupts_total, 200);
        // Transient state cleared (guarded by existing behavior in load_state)
        assert_eq!(loaded.thinking_interrupt_count, 0);
        assert!(loaded.last_interrupt_at.is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_hybrid_fallback_counters_roundtrip() {
        let path = "/tmp/claude-watch-test-hybrid-roundtrip.json";
        let mut state = State::default();
        state.fallback_clear_count = 11;
        state.fallback_update_count = 3;
        state.reminder_to_clear_latency_secs_sum = 123.45;
        state.reminder_to_clear_latency_count = 5;
        state.reminder_to_update_latency_secs_sum = 600.0;
        state.reminder_to_update_latency_count = 2;
        save_state(path, &state);

        let loaded = load_state(path);
        assert_eq!(loaded.fallback_clear_count, 11);
        assert_eq!(loaded.fallback_update_count, 3);
        assert!((loaded.reminder_to_clear_latency_secs_sum - 123.45).abs() < 1e-6);
        assert_eq!(loaded.reminder_to_clear_latency_count, 5);
        assert!((loaded.reminder_to_update_latency_secs_sum - 600.0).abs() < 1e-6);
        assert_eq!(loaded.reminder_to_update_latency_count, 2);
        let _ = std::fs::remove_file(path);
    }
}
