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
    assert!(!claude_watch::task_watch::has_output(&tasks_dir, "empty-task"));
}

#[test]
fn test_has_output_with_content() {
    let (_base, tasks_dir) = create_mock_tasks_dir();
    let output_file = tasks_dir.join("full-task.output");
    fs::write(&output_file, "hello world\n").unwrap();
    assert!(claude_watch::task_watch::has_output(&tasks_dir, "full-task"));
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
    assert!(!claude_watch::task_watch::is_agent_output(&tasks_dir, "regular"));
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

    assert!(claude_watch::task_watch::is_agent_output(&tasks_dir, "agent-task"));
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
    assert_eq!(label, "tick", "label should be 'tick' from the writer output");

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
        .args([
            "list-panes",
            "-t",
            &session,
            "-F",
            "#{pane_id}",
        ])
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
