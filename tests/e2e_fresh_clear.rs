//! E2e tests for fresh /clear detection and resume injection.
//!
//! Verifies that when Claude Code shows low tokens (2000-5000) with zero bashes
//! and an idle prompt, the daemon detects this as a fresh /clear and injects
//! the resume prompt.

mod common;

use common::{MockStatus, TestEnv, TestEnvOptions};

/// Fresh /clear should be detected when tokens are in the low range,
/// bashes are zero, and Claude is idle.
#[test]
fn fresh_clear_detected_and_resume_injected() {
    let env = TestEnv::new(
        "clear-detect",
        TestEnvOptions {
            check_interval: 1,
            fresh_clear_detections: 2,
            show_idle_prompt: true,
            ..Default::default()
        },
    );

    // Set status to fresh /clear pattern
    env.set_status(&MockStatus::fresh_clear(&env.tmux_pane));

    // Run daemon for enough cycles to trigger (detections_required=2 + margin)
    // The daemon returns early on fresh /clear detection (before the main "check" log),
    // so we check legacy log and pingme for evidence of detection.
    let _run = env.run_daemon_cycles(5, 2000);

    // Check legacy log for fresh /clear detection
    let legacy_log = env.read_legacy_log();

    // Check that pingme was called for fresh /clear
    let pingme_calls = env.read_pingme_log();

    // Check state for evidence of detection
    let state = env.read_state();
    let had_fast_alert = state["last_fast_path_alert"].is_string();

    // At least one of these indicators should be present
    let detected = legacy_log.contains("fresh /clear")
        || legacy_log.contains("Fresh /clear")
        || pingme_calls.iter().any(|c| c.contains("Fresh /clear"))
        || pingme_calls.iter().any(|c| c.contains("Injecting resume"))
        || had_fast_alert;

    assert!(
        detected,
        "should detect fresh /clear. Legacy log: {}\nPingme: {:?}\nState: {:?}\nStderr: {}",
        legacy_log,
        pingme_calls,
        state,
        _run.stderr
    );
}

/// Fresh /clear should NOT trigger when tokens are above the max threshold.
#[test]
fn high_tokens_not_detected_as_fresh_clear() {
    let env = TestEnv::new(
        "clear-high",
        TestEnvOptions {
            check_interval: 1,
            fresh_clear_detections: 2,
            show_idle_prompt: true,
            ..Default::default()
        },
    );

    // Set tokens above max_tokens (5000) -- not a fresh /clear
    env.set_status(&MockStatus {
        pane: env.tmux_pane.clone(),
        tokens: 50000,
        bashes: 0,
        compact_remaining: None,
        version: Some("1.0.0".to_string()),
    });

    let _run = env.run_daemon_cycles(4, 1000);

    let legacy_log = env.read_legacy_log();
    assert!(
        !legacy_log.contains("fresh /clear") && !legacy_log.contains("Fresh /clear"),
        "high tokens should NOT be detected as fresh /clear. Log: {}",
        legacy_log
    );
}

/// Fresh /clear should NOT trigger when bashes > 0 (Claude is actively working).
#[test]
fn active_bashes_not_detected_as_fresh_clear() {
    let env = TestEnv::new(
        "clear-active",
        TestEnvOptions {
            check_interval: 1,
            fresh_clear_detections: 2,
            show_idle_prompt: true,
            ..Default::default()
        },
    );

    // Low tokens but bashes > 0 -- Claude is working, not a fresh /clear
    env.set_status(&MockStatus {
        pane: env.tmux_pane.clone(),
        tokens: 3000,
        bashes: 5,
        compact_remaining: None,
        version: Some("1.0.0".to_string()),
    });

    let _run = env.run_daemon_cycles(4, 1000);

    let legacy_log = env.read_legacy_log();
    assert!(
        !legacy_log.contains("fresh /clear") && !legacy_log.contains("Fresh /clear"),
        "should NOT detect fresh /clear when bashes > 0. Log: {}",
        legacy_log
    );
}
