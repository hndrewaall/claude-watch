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
