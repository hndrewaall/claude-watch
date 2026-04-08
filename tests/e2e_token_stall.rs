//! E2e tests for token stall detection.
//!
//! Verifies that when tokens remain unchanged (within max_range) while bashes
//! are declining and context usage is above the minimum fraction, the daemon
//! detects a token stall and alerts.

mod common;

use common::{MockStatus, TestEnv, TestEnvOptions};
use std::thread;
use std::time::Duration;

/// Token stall should be detected when tokens plateau at high usage while
/// bashes decline over consecutive checks.
#[test]
fn token_stall_detected_with_declining_bashes() {
    let env = TestEnv::new(
        "stall-detect",
        TestEnvOptions {
            check_interval: 1,
            token_stall_checks: 3,
            show_idle_prompt: true,
            ..Default::default()
        },
    );

    // We need to feed declining bashes over multiple checks.
    // Strategy: start the daemon, then update status mid-run.
    // Since check_interval=1s and we need 3 checks, schedule updates.

    // Start with high tokens, high bashes
    env.set_status(&MockStatus::high_context(&env.tmux_pane, 180000, 50));

    // Spawn a thread to update status after each cycle
    let data_path = env.mock_status_data.clone();
    let pane = env.tmux_pane.clone();
    let updater = thread::spawn(move || {
        // The daemon checks every 1s. We update status to simulate declining bashes.
        let statuses = [
            MockStatus::high_context(&pane, 180010, 48),
            MockStatus::high_context(&pane, 180020, 46),
            MockStatus::high_context(&pane, 180030, 44),
            MockStatus::high_context(&pane, 180040, 42),
        ];
        for status in &statuses {
            thread::sleep(Duration::from_millis(1200));
            std::fs::write(&data_path, status.to_json()).ok();
        }
    });

    // Run for enough cycles: initial + 4 updates + margin
    let _run = env.run_daemon_cycles(6, 3000);
    updater.join().unwrap();

    // Check for stall detection in logs
    let legacy_log = env.read_legacy_log();
    let log_entries = env.read_log_entries();

    // Look for stall indicators in check entries
    let stuck_checks: Vec<_> = log_entries
        .iter()
        .filter(|e| e["event"].as_str() == Some("check") && e["stuck"].as_bool() == Some(true))
        .collect();

    let has_stall_in_legacy = legacy_log.contains("token stall");
    let has_stuck_checks = !stuck_checks.is_empty();
    let has_alert = log_entries
        .iter()
        .any(|e| e["event"].as_str() == Some("alert"));

    assert!(
        has_stall_in_legacy || has_stuck_checks || has_alert,
        "should detect token stall. Legacy log: {}\nStuck checks: {}\nAll entries: {:?}\nStderr: {}",
        legacy_log,
        stuck_checks.len(),
        log_entries,
        _run.stderr
    );
}

/// No stall should be detected when tokens are actively changing.
#[test]
fn changing_tokens_no_stall() {
    let env = TestEnv::new(
        "stall-changing",
        TestEnvOptions {
            check_interval: 1,
            token_stall_checks: 3,
            ..Default::default()
        },
    );

    // Tokens increasing significantly each cycle (well beyond max_range=500)
    let data_path = env.mock_status_data.clone();
    let pane = env.tmux_pane.clone();
    let updater = thread::spawn(move || {
        let statuses = [
            MockStatus::high_context(&pane, 100000, 50),
            MockStatus::high_context(&pane, 120000, 48),
            MockStatus::high_context(&pane, 140000, 46),
            MockStatus::high_context(&pane, 160000, 44),
        ];
        for status in &statuses {
            thread::sleep(Duration::from_millis(1200));
            std::fs::write(&data_path, status.to_json()).ok();
        }
    });

    env.set_status(&MockStatus::high_context(&env.tmux_pane, 80000, 52));
    let _run = env.run_daemon_cycles(6, 2000);
    updater.join().unwrap();

    // No stall should be detected
    let log_entries = env.read_log_entries();
    let stuck_checks: Vec<_> = log_entries
        .iter()
        .filter(|e| {
            e["event"].as_str() == Some("check")
                && e["stuck"].as_bool() == Some(true)
                && e["stuck_reason"]
                    .as_str()
                    .map(|r| r.contains("token stall"))
                    .unwrap_or(false)
        })
        .collect();

    assert!(
        stuck_checks.is_empty(),
        "should NOT detect stall when tokens are changing. Stuck: {:?}",
        stuck_checks
    );
}

/// No stall when token usage is below the minimum fraction.
#[test]
fn low_usage_no_stall() {
    let env = TestEnv::new(
        "stall-low",
        TestEnvOptions {
            check_interval: 1,
            token_stall_checks: 3,
            ..Default::default()
        },
    );

    // Tokens plateau at low values (below 70% of 200000 = 140000)
    env.set_status(&MockStatus::high_context(&env.tmux_pane, 50000, 50));

    let data_path = env.mock_status_data.clone();
    let pane = env.tmux_pane.clone();
    let updater = thread::spawn(move || {
        let statuses = [
            MockStatus::high_context(&pane, 50010, 48),
            MockStatus::high_context(&pane, 50020, 46),
            MockStatus::high_context(&pane, 50030, 44),
        ];
        for status in &statuses {
            thread::sleep(Duration::from_millis(1200));
            std::fs::write(&data_path, status.to_json()).ok();
        }
    });

    let _run = env.run_daemon_cycles(5, 2000);
    updater.join().unwrap();

    let log_entries = env.read_log_entries();
    let stuck_checks: Vec<_> = log_entries
        .iter()
        .filter(|e| {
            e["event"].as_str() == Some("check")
                && e["stuck"].as_bool() == Some(true)
                && e["stuck_reason"]
                    .as_str()
                    .map(|r| r.contains("token stall"))
                    .unwrap_or(false)
        })
        .collect();

    assert!(
        stuck_checks.is_empty(),
        "should NOT detect stall at low token usage. Stuck: {:?}",
        stuck_checks
    );
}
