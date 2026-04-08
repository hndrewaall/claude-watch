//! E2e tests for dead process detection and restart.
//!
//! Verifies that when claude-status reports tokens=0 and bashes=0 (Claude Code
//! has crashed), the daemon detects this after the configured number of checks
//! and initiates a restart by injecting a relaunch command into the tmux pane.

mod common;

use common::{MockStatus, TestEnv, TestEnvOptions};

/// Dead process should be detected after consecutive zero-token checks.
/// When a shell prompt is visible in the pane, the daemon should write a
/// relaunch script and inject it.
#[test]
fn dead_process_detected_and_restart_triggered() {
    let env = TestEnv::new(
        "dead-detect",
        TestEnvOptions {
            check_interval: 1,
            dead_checks_required: 2,
            show_shell_prompt: true,
            ..Default::default()
        },
    );

    // Set status to dead (tokens=0, bashes=0, empty pane)
    // The daemon will fall back to dashboard_pane from config since pane is empty
    env.set_status(&MockStatus {
        pane: String::new(),
        tokens: 0,
        bashes: 0,
        compact_remaining: None,
        version: None,
    });

    // Run daemon for enough cycles: need dead_checks_required (2) + 1 extra for action
    let run = env.run_daemon_cycles(4, 2000);

    // Check that dead process was detected in logs
    let legacy_log = env.read_legacy_log();
    assert!(
        legacy_log.contains("Dead process detected"),
        "should log dead process detection. Log: {}",
        legacy_log
    );

    // Check state file
    let state = env.read_state();
    assert!(
        state["restart_count"].as_u64().unwrap_or(0) >= 1
            || legacy_log.contains("restarting Claude Code"),
        "should have attempted restart. State: {:?}, Stderr: {}",
        state,
        run.stderr
    );

    // Check that pingme was called (mock logs the call)
    let pingme_calls = env.read_pingme_log();
    if !pingme_calls.is_empty() {
        assert!(
            pingme_calls.iter().any(|c| c.contains("auto-restarting")),
            "pingme should mention auto-restarting. Calls: {:?}",
            pingme_calls
        );
    }
}

/// Dead process detection should NOT trigger if tokens > 0.
#[test]
fn healthy_process_not_flagged_as_dead() {
    let env = TestEnv::new(
        "dead-healthy",
        TestEnvOptions {
            check_interval: 1,
            dead_checks_required: 2,
            ..Default::default()
        },
    );

    // Set healthy status
    env.set_status(&MockStatus::healthy(&env.tmux_pane));

    let _run = env.run_daemon_cycles(3, 1000);

    // Check that no dead process was detected
    let legacy_log = env.read_legacy_log();
    assert!(
        !legacy_log.contains("Dead process detected"),
        "should NOT log dead process for healthy instance. Log: {}",
        legacy_log
    );

    // State should show no restarts
    let state = env.read_state();
    assert_eq!(
        state["restart_count"].as_u64().unwrap_or(0),
        0,
        "should have zero restarts"
    );
}

/// Dead process detection requires consecutive checks -- a single zero-token
/// check followed by a healthy check should NOT trigger restart.
#[test]
fn transient_zero_tokens_no_restart() {
    let env = TestEnv::new(
        "dead-transient",
        TestEnvOptions {
            check_interval: 1,
            dead_checks_required: 3,
            show_shell_prompt: true,
            ..Default::default()
        },
    );

    // Start with dead status
    env.set_status(&MockStatus::dead());

    // Run 1 cycle with dead status
    let _run = env.run_daemon_cycles(1, 500);

    // Switch to healthy before enough consecutive checks
    env.set_status(&MockStatus::healthy(&env.tmux_pane));

    // Run 2 more cycles
    let _run = env.run_daemon_cycles(2, 1000);

    // Should NOT have restarted
    let state = env.read_state();
    assert_eq!(
        state["restart_count"].as_u64().unwrap_or(0),
        0,
        "transient dead should not trigger restart"
    );
}
