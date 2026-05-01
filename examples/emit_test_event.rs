//! Smoke-test helper: emit one claude-watch-alert event into a target
//! queue dir using the production `event_bus::emit` code path.
//!
//! Usage:
//!     CLAUDE_EVENT_QUEUE=/tmp/cw-test cargo run --release --example emit_test_event
//!
//! Prints the path of the resulting file. Used to validate the wire
//! format end-to-end without forging a real heartbeat-stale condition
//! against the running daemon.

use claude_watch::event_bus::{emit, ClaudeWatchAlert, Severity};

fn main() {
    let alert = ClaudeWatchAlert {
        alert_type: "heartbeat-stale",
        stuck_reason: "heartbeat stale (574min, threshold=10min, watchmen=8) [SMOKE TEST]",
        stale_minutes: Some(574),
        affected_watchers: vec![],
        severity: Severity::Critical,
        message:
            "Claude stuck: heartbeat stale (574min, threshold=10min, watchmen=8). 12 consecutive checks failed. [SMOKE TEST — emit_test_event example]",
    };
    emit(&alert);
    eprintln!(
        "wrote test event to {}",
        std::env::var("CLAUDE_EVENT_QUEUE").unwrap_or_else(|_| "~/claude-events".to_string())
    );
}
