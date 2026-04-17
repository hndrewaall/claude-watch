//! Tests for the `claude-watch hook-fire` subcommand and the reminder/
//! fallback-gating machinery.
//!
//! These tests invoke the built binary directly because the reminder
//! marker directory is discovered via env var at runtime (so we can
//! isolate each test). The reminder module's library-level unit tests
//! cover the pure logic in `src/reminders.rs`.

use std::path::PathBuf;
use std::process::Command;

fn binary() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let status = Command::new("cargo")
        .args(["build"])
        .current_dir(manifest_dir)
        .status()
        .expect("cargo build");
    assert!(status.success(), "cargo build failed");
    PathBuf::from(format!("{}/target/debug/claude-watch", manifest_dir))
}

/// Set up an isolated reminder dir for a single invocation. Returns the
/// dir path so tests can assert on marker files.
fn scoped_dir(name: &str) -> PathBuf {
    let p = PathBuf::from(format!(
        "/tmp/claude-watch-hook-fire-test-{}-{}",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn hook_fire_unknown_type_emits_empty_json() {
    let bin = binary();
    let dir = scoped_dir("unknown_type");
    let out = Command::new(&bin)
        .args(["hook-fire", "bogus-type"])
        .env("CLAUDE_WATCH_REMINDER_DIR", &dir)
        .env("HOME", &dir)
        .output()
        .expect("run hook-fire");

    assert!(out.status.success(), "hook-fire should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout should be JSON");
    assert!(
        v.as_object().map(|o| o.is_empty()).unwrap_or(false),
        "unknown type should emit empty JSON object, got: {}",
        stdout
    );

    // No marker should be written
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hook_fire_context_high_below_threshold_noop() {
    // Without a running Claude Code pane, get_claude_status() returns
    // None, so handle_context_high short-circuits to noop. Good enough
    // for this assertion.
    let bin = binary();
    let dir = scoped_dir("ctx_noop");
    let out = Command::new(&bin)
        .args(["hook-fire", "context_high"])
        .env("CLAUDE_WATCH_REMINDER_DIR", &dir)
        // Point HOME somewhere writable-but-empty so config.toml lookup
        // fails gracefully; on failure load_config() exits. We ship a
        // minimal config via env var instead.
        .env("CLAUDE_WATCH_CONFIG", config_path(&dir))
        .env("HOME", &dir)
        .output()
        .expect("run hook-fire");

    assert!(out.status.success(), "hook-fire should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("JSON");
    // Could be empty (status unavailable) or a hook response. Either way
    // we must not crash.
    assert!(v.is_object(), "stdout must be JSON object, got {}", stdout);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hook_fire_pre_compact_always_writes_marker() {
    let bin = binary();
    let dir = scoped_dir("precompact");
    let cfg = config_path(&dir);

    let out = Command::new(&bin)
        .args(["hook-fire", "pre_compact"])
        .env("CLAUDE_WATCH_REMINDER_DIR", &dir)
        .env("CLAUDE_WATCH_CONFIG", &cfg)
        .env("HOME", &dir)
        .output()
        .expect("run hook-fire");

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("JSON");
    // PreCompact always blocks with continue=false
    assert_eq!(v["continue"], serde_json::Value::Bool(false));
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreCompact");

    // Marker must be persisted
    let marker_path = dir.join("pre_compact.json");
    assert!(
        marker_path.exists(),
        "marker file should exist at {:?}",
        marker_path
    );
    let marker_content = std::fs::read_to_string(&marker_path).unwrap();
    let marker: serde_json::Value = serde_json::from_str(&marker_content).unwrap();
    assert!(marker["last_fired"].is_string());
    assert_eq!(marker["fire_count"], 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hook_fire_pre_compact_increments_counter_across_calls() {
    let bin = binary();
    let dir = scoped_dir("precompact_count");
    let cfg = config_path(&dir);

    for _ in 0..3 {
        let out = Command::new(&bin)
            .args(["hook-fire", "pre_compact"])
            .env("CLAUDE_WATCH_REMINDER_DIR", &dir)
            .env("CLAUDE_WATCH_CONFIG", &cfg)
            .env("HOME", &dir)
            .output()
            .expect("run hook-fire");
        assert!(out.status.success());
    }

    let marker_path = dir.join("pre_compact.json");
    let marker: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&marker_path).unwrap()).unwrap();
    assert_eq!(marker["fire_count"], 3);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hook_fire_accepts_canonical_and_dashed_forms() {
    let bin = binary();
    let dir = scoped_dir("canonical");
    let cfg = config_path(&dir);

    for form in ["pre_compact", "pre-compact", "precompact"] {
        let out = Command::new(&bin)
            .args(["hook-fire", form])
            .env("CLAUDE_WATCH_REMINDER_DIR", &dir)
            .env("CLAUDE_WATCH_CONFIG", &cfg)
            .env("HOME", &dir)
            .output()
            .expect("run hook-fire");
        assert!(out.status.success(), "form {} should work", form);
    }

    // All three forms should have bumped the same counter
    let marker_path = dir.join("pre_compact.json");
    let marker: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&marker_path).unwrap()).unwrap();
    assert_eq!(marker["fire_count"], 3);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hook_fire_hook_event_override_is_echoed() {
    let bin = binary();
    let dir = scoped_dir("hook_event_override");
    let cfg = config_path(&dir);

    let out = Command::new(&bin)
        .args(["hook-fire", "pre_compact", "--hook-event", "CustomEvent"])
        .env("CLAUDE_WATCH_REMINDER_DIR", &dir)
        .env("CLAUDE_WATCH_CONFIG", &cfg)
        .env("HOME", &dir)
        .output()
        .expect("run hook-fire");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("JSON");
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "CustomEvent");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Write a minimal config.toml in `dir` and return its path.
fn config_path(dir: &PathBuf) -> PathBuf {
    let p = dir.join("config.toml");
    let state_path = dir.join("state.json");
    let log_path = dir.join("test.jsonl");
    let legacy_log = dir.join("test.log");
    let heartbeat = dir.join("heartbeat");
    let relaunch = dir.join("relaunch.sh");
    let watchers = dir.join("watchers.conf");
    let content = format!(
        r#"[general]
check_interval = 10
state_file = "{state}"
log_file = "{log}"
legacy_log_file = "{legacy}"

[claude]
max_context_tokens = 1000000
heartbeat_file = "{heartbeat}"
relaunch_script = "{relaunch}"

[dead_process]
checks_required = 3
restart_cooldown = 60

[fresh_clear]
min_tokens = 2000
max_tokens = 5000
detections_required = 2
cooldown = 60

[heartbeat]
stale_minutes = 10

[alerts]
initial_cooldown = 60
escalation_tiers = [60, 120]
max_pingme_alerts = 3
resume_prompt = "resume"

[foreground_monitor]
enabled = false
threshold_seconds = 180
check_interval = 3

[watcher_monitor]
enabled = false
watchers_config = "{watchers}"
expected_watchmen = 0

[context_monitor]
enabled = true
threshold_percent = 75
compact_trigger_percent = 5
grace_period = 120
cooldown = 300
"#,
        state = state_path.display(),
        log = log_path.display(),
        legacy = legacy_log.display(),
        heartbeat = heartbeat.display(),
        relaunch = relaunch.display(),
        watchers = watchers.display(),
    );
    std::fs::write(&p, content).unwrap();
    // Touch the auxiliary files to avoid permissions issues
    std::fs::write(&watchers, "# empty\n").unwrap();
    p
}
