//! End-to-end tests for the task-watch module.
//!
//! Tests task discovery, label inference, and agent conversation completion
//! using temporary directories and mock files.

use std::fs;
use tempfile::TempDir;

/// Helper: create a mock tasks directory structure under a UUID-like subdir.
fn create_mock_tasks_dir() -> (TempDir, std::path::PathBuf) {
    let base = TempDir::new().unwrap();
    let uuid_dir = base.path().join("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    let tasks_dir = uuid_dir.join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    (base, tasks_dir)
}

#[test]
fn test_find_tasks_dir_finds_uuid_dir() {
    let (base, tasks_dir) = create_mock_tasks_dir();

    // Create a mock .output file
    let output_file = tasks_dir.join("test-task-id.output");
    fs::write(&output_file, "some output\n").unwrap();

    // find_tasks_dir_in should find this
    let found = claude_watch::task_watch::find_tasks_dir_in(base.path().to_str().unwrap());
    assert!(found.is_some());
    assert_eq!(found.unwrap(), tasks_dir);
}

#[test]
fn test_find_tasks_dir_empty_base() {
    let base = TempDir::new().unwrap();
    let found = claude_watch::task_watch::find_tasks_dir_in(base.path().to_str().unwrap());
    assert!(found.is_none());
}

#[test]
fn test_find_tasks_dir_picks_newest() {
    let base = TempDir::new().unwrap();

    // Create two UUID dirs
    let uuid1 = "aaaaaaaa-1111-2222-3333-444444444444";
    let uuid2 = "bbbbbbbb-1111-2222-3333-444444444444";
    let tasks1 = base.path().join(uuid1).join("tasks");
    let tasks2 = base.path().join(uuid2).join("tasks");
    fs::create_dir_all(&tasks1).unwrap();
    fs::create_dir_all(&tasks2).unwrap();

    // Write older file in tasks1
    let f1 = tasks1.join("old.output");
    fs::write(&f1, "old").unwrap();

    // Set old mtime
    let old_time = filetime::FileTime::from_unix_time(1000000, 0);
    filetime::set_file_mtime(&f1, old_time).unwrap();

    // Write newer file in tasks2
    let f2 = tasks2.join("new.output");
    fs::write(&f2, "new").unwrap();

    let found = claude_watch::task_watch::find_tasks_dir_in(base.path().to_str().unwrap());
    assert_eq!(found.unwrap(), tasks2);
}

#[test]
fn test_has_output_empty_file() {
    let (_base, tasks_dir) = create_mock_tasks_dir();
    let output_file = tasks_dir.join("empty-task.output");
    fs::write(&output_file, "").unwrap();
    assert!(!claude_watch::task_watch::has_output(
        &tasks_dir,
        "empty-task"
    ));
}

#[test]
fn test_has_output_with_content() {
    let (_base, tasks_dir) = create_mock_tasks_dir();
    let output_file = tasks_dir.join("full-task.output");
    fs::write(&output_file, "hello world\n").unwrap();
    assert!(claude_watch::task_watch::has_output(
        &tasks_dir,
        "full-task"
    ));
}

#[test]
fn test_infer_label_regular_output() {
    let (_base, tasks_dir) = create_mock_tasks_dir();
    let output_file = tasks_dir.join("my-task.output");
    fs::write(&output_file, "Running cargo build...\nsecond line\n").unwrap();
    let label = claude_watch::task_watch::infer_label(&tasks_dir, "my-task");
    assert_eq!(label, "Running cargo build...");
}

#[test]
fn test_infer_label_agent_jsonl() {
    let (_base, tasks_dir) = create_mock_tasks_dir();
    let output_file = tasks_dir.join("agent-task.output");
    fs::write(
        &output_file,
        r#"{"slug":"tracker-search","agentId":"abc123"}"#,
    )
    .unwrap();
    let label = claude_watch::task_watch::infer_label(&tasks_dir, "agent-task");
    assert_eq!(label, "agent:tracker-search");
}

#[test]
fn test_is_agent_output_regular_file() {
    let (_base, tasks_dir) = create_mock_tasks_dir();
    let output_file = tasks_dir.join("regular.output");
    fs::write(&output_file, "output").unwrap();
    assert!(!claude_watch::task_watch::is_agent_output(
        &tasks_dir, "regular"
    ));
}

#[cfg(unix)]
#[test]
fn test_is_agent_output_symlink_to_jsonl() {
    let (_base, tasks_dir) = create_mock_tasks_dir();

    // Create a target JSONL file
    let jsonl_file = tasks_dir.join("agent.jsonl");
    fs::write(&jsonl_file, "{}").unwrap();

    // Create symlink
    let output_file = tasks_dir.join("agent-task.output");
    std::os::unix::fs::symlink(&jsonl_file, &output_file).unwrap();

    assert!(claude_watch::task_watch::is_agent_output(
        &tasks_dir,
        "agent-task"
    ));
}

#[test]
fn test_agent_conversation_complete_from_str() {
    // Complete: last message is assistant with text only
    let complete = r#"{"message":{"role":"user","content":"do task"}}
{"message":{"role":"assistant","content":[{"type":"text","text":"All done."}]}}"#;
    assert!(claude_watch::task_watch::agent_conversation_complete_from_str(complete));

    // Incomplete: last message has tool_use
    let incomplete = r#"{"message":{"role":"user","content":"do task"}}
{"message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#;
    assert!(!claude_watch::task_watch::agent_conversation_complete_from_str(incomplete));

    // Empty
    assert!(!claude_watch::task_watch::agent_conversation_complete_from_str(""));
}

#[test]
fn test_scan_active_writers_no_proc_match() {
    // Scanning a temp dir with no real writers should return empty
    let (_base, tasks_dir) = create_mock_tasks_dir();
    let output_file = tasks_dir.join("orphan.output");
    fs::write(&output_file, "data").unwrap();

    let active = claude_watch::task_watch::scan_active_writers(&tasks_dir, true);
    // No process should have this file open for writing
    assert!(active.is_empty());
}

/// End-to-end test for session reconnect behavior.
///
/// Simulates the scenario where the tmux "tasks" session disappears and comes
/// back. Verifies:
///   1. session_exists detection (via tmux has-session)
///   2. State clearing when session disappears
///   3. scan_active_writers finds tasks with active writer processes
///   4. add_pane equivalent (tmux split-window) works after reconnect
#[test]
fn session_reconnect_after_disappearance() {
    use std::process::Command;

    let pid = std::process::id();
    let session = format!("tw-reconnect-{}", pid);

    // Helper: check if tmux session exists (mirrors the private session_exists fn)
    let session_exists = |name: &str| -> bool {
        Command::new("tmux")
            .args(["has-session", "-t", name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };

    // Cleanup guard: kill the test session on drop
    struct SessionGuard {
        name: String,
    }
    impl Drop for SessionGuard {
        fn drop(&mut self) {
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", &self.name])
                .output();
        }
    }
    let _guard = SessionGuard {
        name: session.clone(),
    };

    // --- Phase 1: Create session, verify it exists ---
    let create = Command::new("tmux")
        .args(["new-session", "-d", "-s", &session, "-x", "120", "-y", "40"])
        .output()
        .expect("failed to create tmux session");
    assert!(
        create.status.success(),
        "tmux new-session failed: {}",
        String::from_utf8_lossy(&create.stderr)
    );
    assert!(
        session_exists(&session),
        "session should exist after creation"
    );

    // --- Phase 2: Kill session, verify it's gone ---
    let kill = Command::new("tmux")
        .args(["kill-session", "-t", &session])
        .output()
        .expect("failed to kill tmux session");
    assert!(
        kill.status.success(),
        "tmux kill-session failed: {}",
        String::from_utf8_lossy(&kill.stderr)
    );
    assert!(
        !session_exists(&session),
        "session should not exist after kill"
    );

    // Simulate what the daemon does: clear tracked state when session disappears
    let mut tracked: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    tracked.insert("task-1".to_string(), "%0".to_string());
    tracked.insert("task-2".to_string(), "%1".to_string());
    // On session disappearance, daemon clears tracked and pending_removal
    tracked.clear();
    assert!(tracked.is_empty(), "tracked state should be cleared");

    // --- Phase 3: Recreate session, verify reconnect ---
    let recreate = Command::new("tmux")
        .args(["new-session", "-d", "-s", &session, "-x", "120", "-y", "40"])
        .output()
        .expect("failed to recreate tmux session");
    assert!(
        recreate.status.success(),
        "tmux new-session (recreate) failed: {}",
        String::from_utf8_lossy(&recreate.stderr)
    );
    assert!(
        session_exists(&session),
        "session should exist after recreation"
    );

    // --- Phase 4: Verify scan_active_writers finds tasks with active writers ---
    let (_base, tasks_dir) = create_mock_tasks_dir();
    let output_file = tasks_dir.join("reconnect-task.output");

    // Start a background process that holds the file open for writing
    // (simulating an active Claude Code task writing output)
    let mut writer = Command::new("bash")
        .args([
            "-c",
            &format!(
                "exec 3>>'{}'; while true; do echo tick >&3; sleep 1; done",
                output_file.display()
            ),
        ])
        .spawn()
        .expect("failed to spawn writer process");

    // Give the writer a moment to open the file
    std::thread::sleep(std::time::Duration::from_millis(200));

    // scan_active_writers should find our task
    let active = claude_watch::task_watch::scan_active_writers(&tasks_dir, true);
    assert!(
        active.contains_key("reconnect-task"),
        "scan_active_writers should find the task with an active writer, got: {:?}",
        active.keys().collect::<Vec<_>>()
    );

    // has_output should return true (writer has written at least one "tick")
    assert!(
        claude_watch::task_watch::has_output(&tasks_dir, "reconnect-task"),
        "has_output should be true for file with content"
    );

    // infer_label should return the first line of output
    let label = claude_watch::task_watch::infer_label(&tasks_dir, "reconnect-task");
    assert_eq!(
        label, "tick",
        "label should be 'tick' from the writer output"
    );

    // --- Phase 5: Verify add_pane equivalent (split-window into the session) ---
    let tail_cmd = format!("tail -f {}", output_file.display());
    let split = Command::new("tmux")
        .args([
            "split-window",
            "-t",
            &session,
            "-v",
            "-P",
            "-F",
            "#{pane_id}",
            &tail_cmd,
        ])
        .output()
        .expect("failed to split-window");
    assert!(
        split.status.success(),
        "tmux split-window should succeed after session recreation: {}",
        String::from_utf8_lossy(&split.stderr)
    );
    let pane_id = String::from_utf8_lossy(&split.stdout).trim().to_string();
    assert!(
        pane_id.starts_with('%'),
        "split-window should return a pane id (got '{}')",
        pane_id
    );

    // Verify the pane is alive
    let list = Command::new("tmux")
        .args(["list-panes", "-t", &session, "-F", "#{pane_id}"])
        .output()
        .expect("failed to list panes");
    let panes = String::from_utf8_lossy(&list.stdout);
    assert!(
        panes.contains(&pane_id),
        "new pane {} should appear in session pane list: {}",
        pane_id,
        panes
    );

    // --- Cleanup ---
    writer.kill().expect("failed to kill writer process");
    let _ = writer.wait();
    // Session cleanup handled by SessionGuard drop
}

/// E2E test for the actual `run_task_watch_loop` function's session reconnect behavior.
///
/// This test verifies that the daemon loop SURVIVES tmux session disappearance
/// and reconnects when the session comes back. With the old `break` code, the
/// loop would exit when the session disappeared. With the fix (wait + reconnect),
/// the loop stays alive and re-adds task panes when the session returns.
///
/// Steps:
///   1. Create a unique tmux session
///   2. Set up a tasks_dir with an .output file held open by a writer process
///   3. Spawn run_task_watch_loop as a tokio task
///   4. Wait for the initial pane to appear (proves loop is running)
///   5. Kill the tmux session (session disappears)
///   6. Wait a few seconds, assert the loop is NOT finished (would have exited with old `break`)
///   7. Recreate the tmux session
///   8. Wait for a new pane to appear (proves reconnect worked)
///   9. Shut down via the AtomicBool flag
#[tokio::test]
async fn test_run_task_watch_loop_survives_session_disappearance() {
    use claude_watch::config::TaskWatchConfig;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let pid = std::process::id();
    let session = format!("tw-loop-reconnect-{}", pid);

    // Cleanup guard for the tmux session
    struct TestGuard {
        session: String,
    }
    impl Drop for TestGuard {
        fn drop(&mut self) {
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", &self.session])
                .output();
        }
    }

    let _guard = TestGuard {
        session: session.clone(),
    };

    // --- Setup: create tmux session ---
    eprintln!("[test] creating tmux session: {}", session);
    let create = Command::new("tmux")
        .args(["new-session", "-d", "-s", &session, "-x", "120", "-y", "40"])
        .output()
        .expect("failed to create tmux session");
    assert!(
        create.status.success(),
        "tmux new-session failed: {}",
        String::from_utf8_lossy(&create.stderr)
    );

    // --- Setup: create tasks_dir with a mock agent .output file ---
    // Use agent-style detection (symlink to .jsonl + mtime) instead of /proc fd
    // scanning, which is unreliable in CI containers.
    let (_base, tasks_dir) = create_mock_tasks_dir();
    let jsonl_file = tasks_dir.join("reconnect-loop-task.jsonl");
    let output_file = tasks_dir.join("reconnect-loop-task.output");

    // Write an incomplete agent conversation (last role = "user" means not complete)
    let jsonl_content =
        r#"{"message": {"role": "user", "content": [{"type": "text", "text": "test task"}]}}"#;
    fs::write(&jsonl_file, format!("{}\n", jsonl_content)).unwrap();

    // Create symlink: .output -> .jsonl (marks it as an agent output)
    #[cfg(unix)]
    std::os::unix::fs::symlink(&jsonl_file, &output_file)
        .expect("failed to create .output symlink");

    // Create a mock `task-watch` script and inject it into the tmux session's PATH.
    // The real task-watch binary is a separate tool not available in CI.
    // The daemon's add_pane pipes through `task-watch format-jsonl` and
    // `task-watch timestamp-lines`, so we need a shim that passes through stdin.
    let mock_bin_dir = _base.path().join("mock-bin");
    fs::create_dir_all(&mock_bin_dir).unwrap();
    let mock_script = mock_bin_dir.join("task-watch");
    fs::write(&mock_script, "#!/bin/sh\nexec cat\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&mock_script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    // Update the tmux session's PATH so split-window subshells find the mock.
    // Also set remain-on-exit so panes survive even if the command exits.
    let new_path = format!(
        "{}:{}",
        mock_bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let set_env = Command::new("tmux")
        .args(["set-environment", "-t", &session, "PATH", &new_path])
        .output()
        .expect("failed to set tmux PATH");
    assert!(set_env.status.success(), "tmux set-environment failed");
    let _ = Command::new("tmux")
        .args(["set-option", "-t", &session, "remain-on-exit", "on"])
        .output();

    // --- Spawn the actual daemon loop ---
    let shutdown = Arc::new(AtomicBool::new(false));
    let config = TaskWatchConfig {
        enabled: true,
        session: session.clone(),
        poll_interval: 1,
        done_delay: 60,
        agent_done_delay: 60,
        max_panes: 10,
        show_all: true, // show all tasks (not just workloads)
        tasks_dir_override: Some(tasks_dir.clone()),
    };

    let shutdown_clone = shutdown.clone();
    let loop_handle = tokio::spawn(async move {
        claude_watch::task_watch::run_task_watch_loop(config, shutdown_clone).await;
    });

    // --- Phase 1: Wait for the loop to create an initial pane ---
    eprintln!("[test] waiting for initial pane to appear...");
    let pane_appeared = wait_for_pane_count(&session, 2, 10).await;
    assert!(
        pane_appeared,
        "daemon loop should have created a pane for the active task within 10s"
    );
    eprintln!("[test] initial pane appeared");

    // --- Phase 2: Kill the tmux session ---
    eprintln!("[test] killing tmux session to simulate disappearance...");
    let kill = Command::new("tmux")
        .args(["kill-session", "-t", &session])
        .output()
        .expect("failed to kill tmux session");
    assert!(kill.status.success(), "tmux kill-session failed");

    // --- Phase 3: Wait a few seconds, verify loop is still running ---
    // With the old `break` code, the loop would exit here.
    // With the fix, it waits for the session to come back.
    eprintln!("[test] waiting 4s to verify loop doesn't exit...");
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    assert!(
        !loop_handle.is_finished(),
        "BUG: run_task_watch_loop exited when session disappeared — \
         it should wait for the session to return (the old `break` behavior)"
    );
    eprintln!("[test] loop is still alive (session reconnect wait is working)");

    // --- Phase 4: Recreate the tmux session ---
    eprintln!("[test] recreating tmux session...");
    let recreate = Command::new("tmux")
        .args(["new-session", "-d", "-s", &session, "-x", "120", "-y", "40"])
        .output()
        .expect("failed to recreate tmux session");
    assert!(
        recreate.status.success(),
        "tmux new-session (recreate) failed: {}",
        String::from_utf8_lossy(&recreate.stderr)
    );
    // Re-inject PATH and remain-on-exit for the new session
    let set_env2 = Command::new("tmux")
        .args(["set-environment", "-t", &session, "PATH", &new_path])
        .output()
        .expect("failed to set tmux PATH on recreated session");
    assert!(set_env2.status.success());
    let _ = Command::new("tmux")
        .args(["set-option", "-t", &session, "remain-on-exit", "on"])
        .output();

    // Touch the JSONL file to refresh mtime (agent_is_active checks mtime threshold)
    {
        use std::io::Write;
        if let Ok(mut f) = fs::OpenOptions::new().append(true).open(&jsonl_file) {
            let _ = writeln!(
                f,
                r#"{{"message": {{"role": "user", "content": [{{"type": "text", "text": "still active"}}]}}}}"#
            );
        }
    }
    eprintln!("[test] touched JSONL to refresh mtime");

    // --- Phase 5: Wait for the loop to reconnect and add a pane ---
    eprintln!("[test] waiting for pane to reappear after reconnect...");
    let pane_reappeared = wait_for_pane_count(&session, 2, 20).await;
    assert!(
        pane_reappeared,
        "daemon loop should have reconnected and re-added a pane after session recreation"
    );
    eprintln!("[test] pane reappeared — reconnect successful");

    // --- Cleanup ---
    shutdown.store(true, Ordering::Relaxed);
    // Give the loop a moment to notice the shutdown flag
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    // Guard drop handles tmux session cleanup
}

/// E2E test for orphan pane cleanup on daemon startup.
///
/// After a daemon restart, existing panes in the "tasks" tmux session from the
/// old daemon instance aren't tracked by the new daemon. These orphan panes
/// persist forever because:
///   1. GC only removes panes that ARE tracked but dead
///   2. The initial scan only picks up tasks with active writers
///   3. Orphan panes (tail -f on completed task output) have no writer,
///      aren't tracked, so they're invisible to both GC and removal logic
///
/// This test:
///   1. Creates a tmux session with a manually-created orphan pane
///      (simulating a leftover from a previous daemon instance)
///   2. Spawns run_task_watch_loop
///   3. Verifies the orphan pane gets cleaned up during initial sync
#[tokio::test]
async fn test_orphan_panes_cleaned_on_startup() {
    use claude_watch::config::TaskWatchConfig;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let session = format!("tw-orphan-{}-{}", pid, n);

    // Cleanup guard for the tmux session
    struct TestGuard {
        session: String,
    }
    impl Drop for TestGuard {
        fn drop(&mut self) {
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", &self.session])
                .output();
        }
    }

    let _guard = TestGuard {
        session: session.clone(),
    };

    // --- Setup: create tmux session ---
    eprintln!("[test-orphan] creating tmux session: {}", session);
    let create = Command::new("tmux")
        .args(["new-session", "-d", "-s", &session, "-x", "120", "-y", "40"])
        .output()
        .expect("failed to create tmux session");
    assert!(
        create.status.success(),
        "tmux new-session failed: {}",
        String::from_utf8_lossy(&create.stderr)
    );

    // --- Setup: create tasks_dir (empty — no active tasks) ---
    let (_base, tasks_dir) = create_mock_tasks_dir();

    // Create a .output file for the orphan task (so the pane command references a real file)
    let orphan_tid = "orphan123abc";
    let orphan_output = tasks_dir.join(format!("{}.output", orphan_tid));
    fs::write(&orphan_output, "old task output from previous daemon\n").unwrap();

    // --- Create an orphan pane manually (simulating leftover from old daemon) ---
    // This mimics what add_pane() creates: echo header with [task_id], then tail -f
    let orphan_cmd = format!(
        "echo '=== Old Task [{}] ==='; tail -f {}",
        orphan_tid,
        orphan_output.display()
    );
    let split = Command::new("tmux")
        .args([
            "split-window",
            "-t",
            &session,
            "-v",
            "-P",
            "-F",
            "#{pane_id}",
            &orphan_cmd,
        ])
        .output()
        .expect("failed to create orphan pane");
    assert!(
        split.status.success(),
        "tmux split-window for orphan failed: {}",
        String::from_utf8_lossy(&split.stderr)
    );
    let orphan_pane_id = String::from_utf8_lossy(&split.stdout).trim().to_string();
    eprintln!(
        "[test-orphan] created orphan pane {} for task {}",
        orphan_pane_id, orphan_tid
    );

    // Verify: session has 2 panes (pane 0 + orphan)
    let initial_count = count_panes(&session);
    assert_eq!(
        initial_count, 2,
        "session should have 2 panes before daemon starts (pane 0 + orphan)"
    );

    // --- Spawn the daemon loop ---
    let shutdown = Arc::new(AtomicBool::new(false));
    let config = TaskWatchConfig {
        enabled: true,
        session: session.clone(),
        poll_interval: 1,
        done_delay: 5,
        agent_done_delay: 5,
        max_panes: 10,
        show_all: true,
        tasks_dir_override: Some(tasks_dir.clone()),
    };

    let shutdown_clone = shutdown.clone();
    let _loop_handle = tokio::spawn(async move {
        claude_watch::task_watch::run_task_watch_loop(config, shutdown_clone).await;
    });

    // --- Wait for initial sync + orphan cleanup ---
    // The daemon should detect the orphan pane (no active writer for orphan123abc)
    // and kill it during initial sync, leaving only pane 0.
    eprintln!("[test-orphan] waiting for orphan pane to be cleaned up...");
    let orphan_cleaned = wait_for_pane_count_exact(&session, 1, 10).await;
    assert!(
        orphan_cleaned,
        "BUG: orphan pane was NOT cleaned up during initial sync — \
         session still has {} panes (expected 1). The orphan pane [{}] persists \
         because the daemon doesn't scan existing panes for adoption/cleanup.",
        count_panes(&session),
        orphan_tid
    );
    eprintln!("[test-orphan] orphan pane cleaned up successfully");

    // --- Cleanup ---
    shutdown.store(true, Ordering::Relaxed);
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
}

/// E2E test: workload panes (registered in `/tmp/claude-workloads/state.json`)
/// must NOT be killed during orphan-pane cleanup on daemon startup.
///
/// Bug context (2026-04-30 incident, pane_id=%1832, label=promote-layl-s01):
///   Prior to this fix, the orphan-pane cleanup at the start of `run_task_watch_loop`
///   killed every non-pane-0 pane that wasn't in `state.tracked`. `state.tracked`
///   is populated from agent .output files only, so workload panes (which live
///   in a separate registry under `/tmp/claude-workloads/state.json`) were
///   misidentified as orphans and killed. The underlying long-running process
///   often survived (reparented to init), but the live tmux pane disappeared.
///
/// This test:
///   1. Creates a tmux session with pane 0 + a "fake workload" pane.
///   2. Writes a workload state.json mock that registers the fake pane.
///   3. Points `CLAUDE_WATCH_WORKLOAD_STATE` at the mock.
///   4. Spawns `run_task_watch_loop`.
///   5. Asserts the workload pane SURVIVES (still in tmux list-panes).
#[tokio::test]
async fn test_workload_pane_preserved_on_startup() {
    use claude_watch::config::TaskWatchConfig;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let session = format!("tw-workload-{}-{}", pid, n);

    // Cleanup guard for the tmux session
    struct TestGuard {
        session: String,
    }
    impl Drop for TestGuard {
        fn drop(&mut self) {
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", &self.session])
                .output();
        }
    }

    let _guard = TestGuard {
        session: session.clone(),
    };

    // --- Setup: tmux session ---
    let create = Command::new("tmux")
        .args(["new-session", "-d", "-s", &session, "-x", "120", "-y", "40"])
        .output()
        .expect("failed to create tmux session");
    assert!(
        create.status.success(),
        "tmux new-session failed: {}",
        String::from_utf8_lossy(&create.stderr)
    );

    // --- Setup: empty tasks_dir (no agent tasks — only workload pane should exist) ---
    let (_base, tasks_dir) = create_mock_tasks_dir();

    // --- Create the "fake workload" pane (long-running tail -f on a file) ---
    let workload_file = _base.path().join("workload.log");
    fs::write(&workload_file, "workload running\n").unwrap();
    let workload_cmd = format!("tail -f {}", workload_file.display());
    let split = Command::new("tmux")
        .args([
            "split-window",
            "-t",
            &session,
            "-v",
            "-P",
            "-F",
            "#{pane_id}",
            &workload_cmd,
        ])
        .output()
        .expect("failed to create workload pane");
    assert!(
        split.status.success(),
        "tmux split-window for workload failed: {}",
        String::from_utf8_lossy(&split.stderr)
    );
    let workload_pane_id = String::from_utf8_lossy(&split.stdout).trim().to_string();
    eprintln!(
        "[test-workload] created workload pane {} for fake long-running task",
        workload_pane_id
    );

    // Verify: session has 2 panes (pane 0 + workload)
    assert_eq!(
        count_panes(&session),
        2,
        "session should have 2 panes before daemon starts (pane 0 + workload)"
    );

    // --- Write workload state.json mock ---
    let workload_state_path = _base.path().join("workload-state.json");
    let workload_state_json = format!(
        r#"{{
            "fake-promote-task": {{
                "pane_id": "{}",
                "command": "fake long-running command",
                "output": "/tmp/claude-workloads/fake-promote-task.output",
                "started_at": "2026-04-30T16:00:00"
            }}
        }}"#,
        workload_pane_id
    );
    fs::write(&workload_state_path, workload_state_json).unwrap();

    // Point claude-watch at our mock state file (not /tmp/claude-workloads/state.json)
    std::env::set_var("CLAUDE_WATCH_WORKLOAD_STATE", &workload_state_path);

    // --- Spawn the daemon loop ---
    let shutdown = Arc::new(AtomicBool::new(false));
    let config = TaskWatchConfig {
        enabled: true,
        session: session.clone(),
        poll_interval: 1,
        done_delay: 5,
        agent_done_delay: 5,
        max_panes: 10,
        show_all: true,
        tasks_dir_override: Some(tasks_dir.clone()),
    };

    let shutdown_clone = shutdown.clone();
    let _loop_handle = tokio::spawn(async move {
        claude_watch::task_watch::run_task_watch_loop(config, shutdown_clone).await;
    });

    // --- Wait for the daemon to complete its initial sync ---
    // It will scan existing panes and decide whether to kill the workload pane.
    // With the fix: 2 panes remain (pane 0 + workload).
    // Without the fix: 1 pane remains (workload pane killed as "orphan").
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;

    let final_count = count_panes(&session);
    let panes_remaining: Vec<String> = {
        let out = Command::new("tmux")
            .args(["list-panes", "-t", &session, "-F", "#{pane_id}"])
            .output()
            .expect("list-panes failed");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };

    // --- Cleanup BEFORE assertion (env var, shutdown) so we don't poison other tests ---
    std::env::remove_var("CLAUDE_WATCH_WORKLOAD_STATE");
    shutdown.store(true, Ordering::Relaxed);

    assert_eq!(
        final_count, 2,
        "BUG: workload pane was killed during orphan-pane cleanup. \
         Expected 2 panes (pane 0 + workload pane {}), got {}. \
         Remaining panes: {:?}",
        workload_pane_id, final_count, panes_remaining
    );
    assert!(
        panes_remaining.contains(&workload_pane_id),
        "workload pane {} should still be alive in session, got panes: {:?}",
        workload_pane_id,
        panes_remaining
    );

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
}

/// Count panes in a tmux session.
fn count_panes(session: &str) -> usize {
    use std::process::Command;
    let output = Command::new("tmux")
        .args(["list-panes", "-t", session, "-F", "#{pane_id}"])
        .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count(),
        _ => 0,
    }
}

/// Poll tmux list-panes until the session has exactly `target` panes,
/// or timeout after `timeout_secs` seconds.
async fn wait_for_pane_count_exact(session: &str, target: usize, timeout_secs: u64) -> bool {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        let count = count_panes(session);
        if count == target {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// Poll tmux list-panes until the session has at least `min_panes` panes,
/// or timeout after `timeout_secs` seconds.
async fn wait_for_pane_count(session: &str, min_panes: usize, timeout_secs: u64) -> bool {
    use std::process::Command;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        let output = Command::new("tmux")
            .args(["list-panes", "-t", session, "-F", "#{pane_id}"])
            .output();
        if let Ok(out) = output {
            if out.status.success() {
                let count = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .count();
                if count >= min_panes {
                    return true;
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}
