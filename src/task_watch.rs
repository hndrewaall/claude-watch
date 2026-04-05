//! Task-watch daemon loop — auto-manages tmux panes for Claude Code background tasks.
//!
//! Ports the Python `task-watch` daemon into the claude-watch Rust binary.
//! Monitors /tmp/claude-1000/ for task output files, creates tmux panes with
//! `tail -f` for active tasks, and removes them after tasks complete.

use crate::cmd::run_cmd;
use crate::config::TaskWatchConfig;
use crate::proc_util;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use notify::{Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Base directory for Claude Code task output files.
const TASKS_BASE: &str = "/tmp/claude-1000/-home-hndrewaall";

/// Mtime threshold for considering an agent "active" (seconds).
const AGENT_MTIME_THRESHOLD: u64 = 600;

/// A tracked task pane in the tmux session.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TrackedTask {
    pub pane_id: String,
    pub label: String,
    pub added_at: Instant,
    pub is_agent: bool,
}

/// Main task-watch state.
pub struct TaskWatchState {
    pub tracked: HashMap<String, TrackedTask>,
    pub pending_removal: HashMap<String, Instant>,
    pub tasks_dir: PathBuf,
    pub session: String,
    pub max_panes: usize,
}

/// Find the active Claude Code tasks directory.
///
/// Claude Code uses UUID-scoped directories:
///   /tmp/claude-1000/-home-hndrewaall/<uuid>/tasks/
/// Pick the one with the most recently modified .output file.
pub fn find_tasks_dir() -> Option<PathBuf> {
    find_tasks_dir_in(TASKS_BASE)
}

/// Testable version with configurable base path.
pub fn find_tasks_dir_in(base: &str) -> Option<PathBuf> {
    let base_path = Path::new(base);
    let mut best_dir: Option<PathBuf> = None;
    let mut best_mtime: f64 = 0.0;

    let entries = match std::fs::read_dir(base_path) {
        Ok(e) => e,
        Err(_) => return None,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Session IDs are UUIDs (36 chars with dashes)
        if name_str.len() != 36 || !name_str.contains('-') {
            continue;
        }
        if !entry.path().is_dir() {
            continue;
        }

        let tasks_path = entry.path().join("tasks");
        if !tasks_path.is_dir() {
            continue;
        }

        if let Ok(task_entries) = std::fs::read_dir(&tasks_path) {
            for f in task_entries.flatten() {
                if f.file_name().to_string_lossy().ends_with(".output") {
                    if let Ok(meta) = f.metadata() {
                        if let Ok(mtime) = meta.modified() {
                            let mt = mtime
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs_f64();
                            if mt > best_mtime {
                                best_mtime = mt;
                                best_dir = Some(tasks_path.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    best_dir
}

/// Check if a task output file is an agent JSONL file (symlinked).
pub fn is_agent_output(tasks_dir: &Path, task_id: &str) -> bool {
    let output_file = tasks_dir.join(format!("{}.output", task_id));
    if let Ok(meta) = std::fs::symlink_metadata(&output_file) {
        if meta.file_type().is_symlink() {
            if let Ok(target) = std::fs::read_link(&output_file) {
                return target.to_string_lossy().ends_with(".jsonl");
            }
        }
    }
    false
}

/// Check if a task's output file has any content.
pub fn has_output(tasks_dir: &Path, task_id: &str) -> bool {
    let output_file = tasks_dir.join(format!("{}.output", task_id));
    match std::fs::metadata(&output_file) {
        Ok(meta) => meta.len() > 0,
        Err(_) => false,
    }
}

/// Infer a human-readable label from the first line of a task output file.
pub fn infer_label(tasks_dir: &Path, task_id: &str) -> String {
    let output_file = tasks_dir.join(format!("{}.output", task_id));
    match std::fs::read_to_string(&output_file) {
        Ok(content) => infer_label_from_content(task_id, &content),
        Err(_) => task_id.chars().take(12).collect(),
    }
}

/// Pure function: extract label from file content.
pub fn infer_label_from_content(task_id: &str, content: &str) -> String {
    let first_line = match content.lines().next() {
        Some(line) => line.trim(),
        None => return task_id.chars().take(12).collect(),
    };

    if first_line.is_empty() {
        return task_id.chars().take(12).collect();
    }

    // Agent JSONL: try to extract slug or agentId
    if first_line.starts_with('{') {
        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(first_line) {
            if let Some(slug) = obj.get("slug").and_then(|s| s.as_str()) {
                if !slug.is_empty() {
                    return format!("agent:{}", slug);
                }
            }
            if let Some(agent_id) = obj.get("agentId").and_then(|s| s.as_str()) {
                if !agent_id.is_empty() {
                    let short: String = agent_id.chars().take(12).collect();
                    return format!("agent:{}", short);
                }
            }
        }
    }

    // Trim long lines
    if first_line.len() > 40 {
        let truncated: String = first_line.chars().take(37).collect();
        format!("{}...", truncated)
    } else {
        first_line.to_string()
    }
}

/// Scan /proc for processes with writable fds pointing to .output files in tasks_dir.
///
/// Returns a map of task_id -> is_service for active writers.
/// This is the core /proc scanning logic ported from the Python `get_open_output_files`.
pub fn scan_active_writers(tasks_dir: &Path, include_watchers: bool) -> HashMap<String, bool> {
    let prefix = match tasks_dir.canonicalize() {
        Ok(p) => format!("{}/", p.display()),
        Err(_) => format!("{}/", tasks_dir.display()),
    };

    let mut active: HashMap<String, bool> = HashMap::new();

    // Build reverse map: realpath -> task_id for symlinked output files (agents)
    let mut realpath_map: HashMap<String, String> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(tasks_dir) {
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if !fname.ends_with(".output") {
                continue;
            }
            let fpath = entry.path();
            if let Ok(meta) = std::fs::symlink_metadata(&fpath) {
                if meta.file_type().is_symlink() {
                    if let Ok(real) = std::fs::canonicalize(&fpath) {
                        let tid = fname.strip_suffix(".output").unwrap_or(&fname).to_string();
                        realpath_map.insert(real.to_string_lossy().to_string(), tid);
                    }
                }
            }
        }
    }

    // Scan /proc for writers
    let proc_dir = match std::fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return active,
    };

    for proc_entry in proc_dir.flatten() {
        let pid_name = proc_entry.file_name();
        let pid_str = pid_name.to_string_lossy();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let fd_dir = format!("/proc/{}/fd", pid_str);
        let fd_entries = match std::fs::read_dir(&fd_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for fd_entry in fd_entries.flatten() {
            let fd_name = fd_entry.file_name();
            let fd_str = fd_name.to_string_lossy().to_string();

            let link_path = format!("/proc/{}/fd/{}", pid_str, fd_str);
            let target = match std::fs::read_link(&link_path) {
                Ok(t) => t.to_string_lossy().to_string(),
                Err(_) => continue,
            };

            let mut tid: Option<String> = None;

            // Direct match: fd points to tasks dir .output file
            if target.starts_with(&prefix) && target.ends_with(".output") {
                let rest = &target[prefix.len()..];
                if let Some(id) = rest.strip_suffix(".output") {
                    tid = Some(id.to_string());
                }
            }
            // Symlink match: fd points to real file behind a symlink
            else if let Some(id) = realpath_map.get(&target) {
                tid = Some(id.clone());
            }

            if let Some(task_id) = tid {
                if proc_util::fd_is_writable(&pid_str, &fd_str) && !active.contains_key(&task_id) {
                    let is_service = proc_util::is_service_process(&pid_str)
                        || proc_util::is_service_output(tasks_dir, &task_id);
                    active.insert(task_id, is_service);
                }
            }
        }
    }

    if !include_watchers {
        active.retain(|_, is_svc| !*is_svc);
    }
    active
}

/// Check if an agent's JSONL conversation is complete.
///
/// A completed conversation has a final assistant message with only text
/// content (no tool_use blocks).
pub fn agent_conversation_complete(tasks_dir: &Path, task_id: &str) -> bool {
    let output_file = tasks_dir.join(format!("{}.output", task_id));
    let real_path = match std::fs::canonicalize(&output_file) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let content = match std::fs::read_to_string(&real_path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    agent_conversation_complete_from_str(&content)
}

/// Pure function: check if JSONL content represents a completed agent conversation.
pub fn agent_conversation_complete_from_str(content: &str) -> bool {
    let mut last_role: Option<String> = None;
    let mut last_has_tool_use = false;

    for line in content.lines() {
        let entry: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if let Some(role) = entry
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
        {
            last_role = Some(role.to_string());
            last_has_tool_use = false;
            if let Some(content_arr) = entry
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for block in content_arr {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        last_has_tool_use = true;
                    }
                }
            }
        }
    }

    last_role.as_deref() == Some("assistant") && !last_has_tool_use
}

/// Check if an agent output file is still active.
///
/// Uses conversation completion, mtime, and child process detection.
pub fn agent_is_active(tasks_dir: &Path, task_id: &str) -> bool {
    // Fast check: if conversation is complete, agent is dead
    if agent_conversation_complete(tasks_dir, task_id) {
        return false;
    }

    let output_file = tasks_dir.join(format!("{}.output", task_id));
    let real_path = match std::fs::canonicalize(&output_file) {
        Ok(p) => p,
        Err(_) => return false,
    };

    if let Ok(meta) = std::fs::metadata(&real_path) {
        if let Ok(mtime) = meta.modified() {
            let elapsed = SystemTime::now()
                .duration_since(mtime)
                .unwrap_or_default()
                .as_secs();
            if elapsed < AGENT_MTIME_THRESHOLD {
                return true;
            }
        }
    }

    // Mtime stale — check for running child processes
    agent_has_child_process(tasks_dir, task_id)
}

/// Check if an agent has a running child process under the Claude PID.
fn agent_has_child_process(tasks_dir: &Path, task_id: &str) -> bool {
    let claude_pid = match crate::agent::find_claude_pid() {
        Some(pid) => pid,
        None => return false,
    };

    // Get the agent's last bash command from JSONL
    let output_file = tasks_dir.join(format!("{}.output", task_id));
    let real_path = match std::fs::canonicalize(&output_file) {
        Ok(p) => p,
        Err(_) => return false,
    };

    let content = match std::fs::read_to_string(&real_path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    let last_bash_cmd = match crate::agent::extract_last_bash_cmd_from_str(&content) {
        Some(cmd) => cmd,
        None => return false,
    };

    // Check children of claude PID
    let children = crate::agent::get_children(claude_pid);
    children.iter().any(|c| c.cmd.contains(&last_bash_cmd))
}

/// Get the set of all alive pane IDs in a tmux session.
async fn get_alive_panes(session: &str) -> std::collections::HashSet<String> {
    let args: Vec<&str> = vec!["tmux", "list-panes", "-t", session, "-F", "#{pane_id}"];
    match run_cmd(&args, 5).await {
        Some(output) => output.lines().map(|l| l.trim().to_string()).collect(),
        None => std::collections::HashSet::new(),
    }
}

/// Info about an existing pane in the tmux session.
#[derive(Debug)]
struct ExistingPane {
    pane_index: usize,
    pane_id: String,
    task_id: Option<String>,
}

/// List all panes in the session with their index, id, and parsed task_id.
///
/// Pane start commands follow the pattern:
///   echo '=== LABEL [TASK_ID] ==='; tail -f PATH
/// We extract TASK_ID from the `[...]` part.
async fn list_existing_panes(session: &str) -> Vec<ExistingPane> {
    let args: Vec<&str> = vec![
        "tmux",
        "list-panes",
        "-t",
        session,
        "-F",
        "#{pane_index}\t#{pane_id}\t#{pane_start_command}",
    ];
    let output = match run_cmd(&args, 5).await {
        Some(o) => o,
        None => return Vec::new(),
    };

    output
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(3, '\t').collect();
            if parts.len() < 3 {
                return None;
            }
            let pane_index: usize = parts[0].trim().parse().ok()?;
            let pane_id = parts[1].trim().to_string();
            let start_cmd = parts[2];

            // Extract task_id from [TASK_ID] in the start command
            let task_id = extract_task_id_from_pane_cmd(start_cmd);

            Some(ExistingPane {
                pane_index,
                pane_id,
                task_id,
            })
        })
        .collect()
}

/// Extract a task_id from a pane start command.
///
/// Looks for `[TASK_ID]` pattern in strings like:
///   echo '=== Some Label [abc123def] ==='; tail -f /path/to/file
fn extract_task_id_from_pane_cmd(cmd: &str) -> Option<String> {
    // Find the last occurrence of [...] in the command
    // (to avoid matching other brackets that might appear)
    let mut last_open = None;
    let mut last_close = None;
    for (i, c) in cmd.char_indices() {
        if c == '[' {
            last_open = Some(i);
            last_close = None; // reset close for this open
        }
        if c == ']' && last_open.is_some() {
            last_close = Some(i);
        }
    }

    if let (Some(open), Some(close)) = (last_open, last_close) {
        if close > open + 1 {
            let tid = &cmd[open + 1..close];
            // Validate: task IDs are alphanumeric hex-ish strings
            if !tid.is_empty() && tid.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
                return Some(tid.to_string());
            }
        }
    }
    None
}

/// Check if the tmux session exists.
async fn session_exists(session: &str) -> bool {
    run_cmd(&["tmux", "has-session", "-t", session], 5)
        .await
        .is_some()
}

/// Add a tail -f pane for a task. Returns the new pane ID.
async fn add_pane(
    state: &mut TaskWatchState,
    task_id: &str,
    label: &str,
) -> Option<String> {
    // Already tracked and alive?
    if let Some(tracked) = state.tracked.get(task_id) {
        let alive = get_alive_panes(&state.session).await;
        if alive.contains(&tracked.pane_id) {
            return Some(tracked.pane_id.clone());
        }
    }

    // Check pane count
    let alive = get_alive_panes(&state.session).await;
    if alive.len() >= state.max_panes {
        warn!(
            "Max panes ({}) reached, skipping {}",
            state.max_panes, task_id
        );
        return None;
    }

    let output_file = state.tasks_dir.join(format!("{}.output", task_id));
    let output_path = output_file.to_string_lossy();
    let display_label = label.replace('\'', "'\\''");
    let is_agent = is_agent_output(&state.tasks_dir, task_id);

    // Build the tail command — use JSONL formatter for agent output
    let tail_cmd = if is_agent {
        format!(
            "tail -f {} | task-watch format-jsonl | task-watch timestamp-lines",
            output_path
        )
    } else {
        format!("tail -f {} | task-watch timestamp-lines", output_path)
    };

    let pane_cmd = format!(
        "echo '=== {} [{}] ==='; {}",
        display_label, task_id, tail_cmd
    );

    // Create pane with tail
    let result = run_cmd(
        &[
            "tmux",
            "split-window",
            "-t",
            &state.session,
            "-v",
            "-P",
            "-F",
            "#{pane_id}",
            &pane_cmd,
        ],
        5,
    )
    .await;

    let pane_id = match result {
        Some(id) if !id.is_empty() => id.trim().to_string(),
        _ => return None,
    };

    // Rebalance
    let _ = run_cmd(
        &[
            "tmux",
            "select-layout",
            "-t",
            &state.session,
            "even-vertical",
        ],
        5,
    )
    .await;

    state.tracked.insert(
        task_id.to_string(),
        TrackedTask {
            pane_id: pane_id.clone(),
            label: label.to_string(),
            added_at: Instant::now(),
            is_agent,
        },
    );

    Some(pane_id)
}

/// Remove a task pane.
async fn remove_pane(state: &mut TaskWatchState, task_id: &str) {
    if let Some(tracked) = state.tracked.remove(task_id) {
        let alive = get_alive_panes(&state.session).await;
        if alive.contains(&tracked.pane_id) {
            let _ = run_cmd(
                &["tmux", "kill-pane", "-t", &tracked.pane_id],
                5,
            )
            .await;
        }
        // Rebalance
        let _ = run_cmd(
            &[
                "tmux",
                "select-layout",
                "-t",
                &state.session,
                "even-vertical",
            ],
            5,
        )
        .await;
    }
}

/// Garbage-collect panes that are dead (tmux pane exited but still tracked).
async fn gc_dead_panes(state: &mut TaskWatchState) -> usize {
    let alive = get_alive_panes(&state.session).await;
    let dead: Vec<String> = state
        .tracked
        .iter()
        .filter(|(_, info)| !alive.contains(&info.pane_id))
        .map(|(tid, _)| tid.clone())
        .collect();

    let count = dead.len();
    for tid in &dead {
        if let Some(info) = state.tracked.remove(tid) {
            info!(task_id = %tid, label = %info.label, "GC: pane dead");
        }
    }

    // Also clean pending_removal for dead panes
    for tid in &dead {
        state.pending_removal.remove(tid);
    }

    count
}

/// Main task-watch daemon loop. Runs as a tokio task.
pub async fn run_task_watch_loop(config: TaskWatchConfig, shutdown: Arc<AtomicBool>) {
    let session = config.session.clone();
    let poll_interval = config.poll_interval;
    let done_delay = config.done_delay;
    let agent_done_delay = config.agent_done_delay;
    let show_all = config.show_all;
    let max_panes = config.max_panes;

    let mode = if show_all {
        "all tasks"
    } else {
        "workloads only"
    };
    info!(
        poll_interval,
        done_delay,
        agent_done_delay,
        mode,
        "task-watch loop started"
    );

    // Find initial tasks directory (use override if provided, e.g. for testing)
    let tasks_dir = if let Some(ref override_dir) = config.tasks_dir_override {
        override_dir.clone()
    } else {
        match find_tasks_dir() {
            Some(d) => d,
            None => {
                warn!("No Claude Code tasks directory found, waiting...");
                // Wait and retry
                loop {
                    if shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(poll_interval)).await;
                    if let Some(d) = find_tasks_dir() {
                        break d;
                    }
                }
            }
        }
    };

    info!(tasks_dir = %tasks_dir.display(), "found tasks directory");

    let mut state = TaskWatchState {
        tracked: HashMap::new(),
        pending_removal: HashMap::new(),
        tasks_dir: tasks_dir.clone(),
        session: session.clone(),
        max_panes,
    };

    // Wait for session to exist
    if !session_exists(&session).await {
        info!(session = %session, "waiting for tmux session...");
        loop {
            if shutdown.load(Ordering::Relaxed) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if session_exists(&session).await {
                break;
            }
        }
    }

    // Initial scan: find active writers
    info!("scanning for active tasks...");
    let active_tids =
        tokio::task::spawn_blocking({
            let td = state.tasks_dir.clone();
            move || scan_active_writers(&td, show_all)
        })
        .await
        .unwrap_or_default();

    // Also discover active agents via mtime
    let mut all_active = active_tids;
    {
        let td = state.tasks_dir.clone();
        let extra_agents = tokio::task::spawn_blocking(move || {
            let mut agents = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&td) {
                for entry in entries.flatten() {
                    let fname = entry.file_name().to_string_lossy().to_string();
                    if fname.ends_with(".output") {
                        let tid = fname.strip_suffix(".output").unwrap_or(&fname).to_string();
                        if is_agent_output(&td, &tid) && agent_is_active(&td, &tid) {
                            agents.push(tid);
                        }
                    }
                }
            }
            agents
        })
        .await
        .unwrap_or_default();

        for tid in extra_agents {
            all_active.entry(tid).or_insert(false);
        }
    }

    for tid in all_active.keys() {
        if has_output(&state.tasks_dir, tid) {
            let label = infer_label(&state.tasks_dir, tid);
            if let Some(pane_id) = add_pane(&mut state, tid, &label).await {
                info!(task_id = %tid, label = %label, pane_id = %pane_id, "initial task");
            }
        }
    }

    let active_count = state.tracked.len();
    info!(active_count, "initial sync complete");

    // Orphan pane cleanup: scan existing panes in the session and kill any
    // that aren't tracked (leftover from a previous daemon instance).
    // Pane 0 is the daemon/status pane — never kill it.
    let existing_panes = list_existing_panes(&session).await;
    let tracked_pane_ids: std::collections::HashSet<String> = state
        .tracked
        .values()
        .map(|t| t.pane_id.clone())
        .collect();
    let mut orphan_count = 0;
    for pane in &existing_panes {
        // Skip pane 0 (daemon pane)
        if pane.pane_index == 0 {
            continue;
        }
        // Skip panes we just adopted during initial sync
        if tracked_pane_ids.contains(&pane.pane_id) {
            continue;
        }
        // This pane is untracked — it's an orphan from a previous daemon instance.
        // Kill it.
        info!(
            pane_id = %pane.pane_id,
            pane_index = pane.pane_index,
            task_id = ?pane.task_id,
            "killing orphan pane from previous daemon"
        );
        let _ = run_cmd(
            &["tmux", "kill-pane", "-t", &pane.pane_id],
            5,
        )
        .await;
        orphan_count += 1;
    }
    if orphan_count > 0 {
        info!(orphan_count, "orphan pane cleanup complete");
        // Rebalance after killing orphans
        let _ = run_cmd(
            &[
                "tmux",
                "select-layout",
                "-t",
                &session,
                "even-vertical",
            ],
            5,
        )
        .await;
    }

    // Set up file watcher for new .output files
    let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<String>();

    let mut _watcher = setup_notify_watcher(&state.tasks_dir, notify_tx.clone());

    // Main daemon loop
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        tokio::time::sleep(std::time::Duration::from_secs(poll_interval)).await;

        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        debug!(tracked = state.tracked.len(), pending = state.pending_removal.len(), "task-watch poll cycle");

        // Detect UUID directory change (new Claude Code session)
        if let Some(new_dir) = find_tasks_dir() {
            if new_dir != state.tasks_dir {
                info!(
                    old = %state.tasks_dir.display(),
                    new = %new_dir.display(),
                    "UUID change detected"
                );
                state.tasks_dir = new_dir.clone();
                _watcher = setup_notify_watcher(&new_dir, notify_tx.clone());
            }
        }

        // Process notify events (new files)
        while let Ok(fname) = notify_rx.try_recv() {
            if !fname.ends_with(".output") {
                continue;
            }
            let tid = fname.strip_suffix(".output").unwrap_or(&fname).to_string();
            if state.tracked.contains_key(&tid) {
                continue;
            }

            // Brief delay for file to get initial content
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;

            if is_agent_output(&state.tasks_dir, &tid) {
                if !has_output(&state.tasks_dir, &tid) {
                    continue;
                }
            } else {
                // Check for writer
                let has_writer = {
                    let td = state.tasks_dir.clone();
                    let tid_c = tid.clone();
                    tokio::task::spawn_blocking(move || {
                        file_has_writer(&td.join(format!("{}.output", tid_c)))
                    })
                    .await
                    .unwrap_or(false)
                };

                if !has_writer {
                    continue;
                }

                if !show_all {
                    let is_svc = {
                        let td = state.tasks_dir.clone();
                        let tid_c = tid.clone();
                        tokio::task::spawn_blocking(move || {
                            // Check if any writer pid is a service
                            check_is_service_for_task(&td, &tid_c)
                        })
                        .await
                        .unwrap_or(false)
                    };
                    if is_svc {
                        continue;
                    }
                }

                if !has_output(&state.tasks_dir, &tid) {
                    continue;
                }
            }

            let label = infer_label(&state.tasks_dir, &tid);
            if let Some(pane_id) = add_pane(&mut state, &tid, &label).await {
                info!(task_id = %tid, label = %label, pane_id = %pane_id, "new task");
                state.pending_removal.remove(&tid);
            }
        }

        // GC dead panes
        let gc_count = gc_dead_panes(&mut state).await;
        if gc_count > 0 {
            debug!(gc_count, "GC cleaned dead panes");
        }

        // Full /proc scan for active writers
        let active_tids = {
            let td = state.tasks_dir.clone();
            tokio::task::spawn_blocking(move || scan_active_writers(&td, show_all))
                .await
                .unwrap_or_default()
        };
        debug!(active_count = active_tids.len(), "proc scan results");

        // Detect completed tasks
        let tracked_ids: Vec<String> = state.tracked.keys().cloned().collect();
        for tid in &tracked_ids {
            if state.pending_removal.contains_key(tid) {
                continue;
            }
            if !active_tids.contains_key(tid) {
                let is_agent = is_agent_output(&state.tasks_dir, &tid);
                if is_agent {
                    let td = state.tasks_dir.clone();
                    let tid_c = tid.clone();
                    let still_active = tokio::task::spawn_blocking(move || {
                        agent_is_active(&td, &tid_c)
                    })
                    .await
                    .unwrap_or(false);
                    if still_active {
                        continue;
                    }
                }

                let label = state
                    .tracked
                    .get(tid)
                    .map(|t| t.label.clone())
                    .unwrap_or_default();
                let effective_delay = if is_agent {
                    agent_done_delay
                } else {
                    done_delay
                };

                info!(
                    task_id = %tid,
                    label = %label,
                    delay = effective_delay,
                    "writer gone, scheduling removal"
                );
                state
                    .pending_removal
                    .insert(tid.clone(), Instant::now() + std::time::Duration::from_secs(effective_delay));
            }
        }

        // Pick up untracked active tasks (catch missed inotify events)
        // Also check for new agents via mtime
        {
            let td = state.tasks_dir.clone();
            let extra_agents = tokio::task::spawn_blocking(move || {
                let mut agents = Vec::new();
                if let Ok(entries) = std::fs::read_dir(&td) {
                    for entry in entries.flatten() {
                        let fname = entry.file_name().to_string_lossy().to_string();
                        if fname.ends_with(".output") {
                            let tid = fname.strip_suffix(".output").unwrap_or(&fname).to_string();
                            if is_agent_output(&td, &tid) && agent_is_active(&td, &tid) {
                                agents.push(tid);
                            }
                        }
                    }
                }
                agents
            })
            .await
            .unwrap_or_default();

            let mut all_active_now = active_tids;
            for tid in extra_agents {
                all_active_now.entry(tid).or_insert(false);
            }

            for tid in all_active_now.keys() {
                if !state.tracked.contains_key(tid)
                    && !state.pending_removal.contains_key(tid)
                    && has_output(&state.tasks_dir, tid)
                {
                    let label = infer_label(&state.tasks_dir, tid);
                    if let Some(pane_id) = add_pane(&mut state, tid, &label).await {
                        info!(
                            task_id = %tid,
                            label = %label,
                            pane_id = %pane_id,
                            "discovered task"
                        );
                    }
                }
            }
        }

        // Process due removals
        let now = Instant::now();
        let due: Vec<String> = state
            .pending_removal
            .iter()
            .filter(|(_, removal_time)| now >= **removal_time)
            .map(|(tid, _)| tid.clone())
            .collect();

        for tid in due {
            // Re-check agents before removing
            if is_agent_output(&state.tasks_dir, &tid) {
                let td = state.tasks_dir.clone();
                let tid_c = tid.clone();
                let still_active = tokio::task::spawn_blocking(move || {
                    agent_is_active(&td, &tid_c)
                })
                .await
                .unwrap_or(false);
                if still_active {
                    state.pending_removal.remove(&tid);
                    info!(task_id = %tid, "agent resumed, cancelling removal");
                    continue;
                }
            }

            state.pending_removal.remove(&tid);
            remove_pane(&mut state, &tid).await;
            info!(task_id = %tid, "removed");
        }

        // Self-heal: if session disappeared, wait for it to come back
        if !session_exists(&session).await {
            warn!("tasks session disappeared, waiting for it to return...");
            // Clear tracked state since panes are gone
            state.tracked.clear();
            state.pending_removal.clear();

            loop {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                if session_exists(&session).await {
                    info!("tasks session is back, resuming");
                    // Re-scan for active tasks (both /proc writers and agents)
                    let td = state.tasks_dir.clone();
                    let mut active: HashMap<String, bool> = tokio::task::spawn_blocking(move || scan_active_writers(&td, show_all))
                        .await
                        .unwrap_or_default();

                    // Also discover active agents via mtime
                    {
                        let td = state.tasks_dir.clone();
                        let extra_agents = tokio::task::spawn_blocking(move || {
                            let mut agents = Vec::new();
                            if let Ok(entries) = std::fs::read_dir(&td) {
                                for entry in entries.flatten() {
                                    let fname = entry.file_name().to_string_lossy().to_string();
                                    if fname.ends_with(".output") {
                                        let tid = fname.strip_suffix(".output").unwrap_or(&fname).to_string();
                                        if is_agent_output(&td, &tid) && agent_is_active(&td, &tid) {
                                            agents.push(tid);
                                        }
                                    }
                                }
                            }
                            agents
                        })
                        .await
                        .unwrap_or_default();

                        for tid in extra_agents {
                            active.entry(tid).or_insert(false);
                        }
                    }

                    for (tid, _) in &active {
                        if has_output(&state.tasks_dir, tid) {
                            let label = infer_label(&state.tasks_dir, tid);
                            if let Some(pane_id) = add_pane(&mut state, tid, &label).await {
                                info!(task_id = %tid, label = %label, pane_id = %pane_id, "reconnected task");
                            }
                        }
                    }
                    break;
                }
            }
        }
    }

    info!("task-watch loop shutting down");
}

/// Check if a specific task's writer process is a service.
fn check_is_service_for_task(tasks_dir: &Path, task_id: &str) -> bool {
    let output_file = tasks_dir.join(format!("{}.output", task_id));
    let real_path = match std::fs::canonicalize(&output_file) {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(_) => output_file.to_string_lossy().to_string(),
    };

    // Find writer PID
    let proc_dir = match std::fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return false,
    };

    for proc_entry in proc_dir.flatten() {
        let pid_name = proc_entry.file_name();
        let pid_str = pid_name.to_string_lossy().to_string();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let fd_dir = format!("/proc/{}/fd", pid_str);
        let fd_entries = match std::fs::read_dir(&fd_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for fd_entry in fd_entries.flatten() {
            let fd_name = fd_entry.file_name();
            let fd_str = fd_name.to_string_lossy().to_string();

            let link_path = format!("/proc/{}/fd/{}", pid_str, fd_str);
            let target = match std::fs::read_link(&link_path) {
                Ok(t) => t.to_string_lossy().to_string(),
                Err(_) => continue,
            };

            if target == real_path
                && proc_util::fd_is_writable(&pid_str, &fd_str)
            {
                return proc_util::is_service_process(&pid_str)
                    || proc_util::is_service_output(tasks_dir, task_id);
            }
        }
    }
    false
}

/// Check if a file has an active writer process.
fn file_has_writer(filepath: &Path) -> bool {
    let real_path = match std::fs::canonicalize(filepath) {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(_) => return false,
    };

    let proc_dir = match std::fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return false,
    };

    for proc_entry in proc_dir.flatten() {
        let pid_name = proc_entry.file_name();
        let pid_str = pid_name.to_string_lossy().to_string();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let fd_dir = format!("/proc/{}/fd", pid_str);
        let fd_entries = match std::fs::read_dir(&fd_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for fd_entry in fd_entries.flatten() {
            let fd_name = fd_entry.file_name();
            let fd_str = fd_name.to_string_lossy().to_string();

            let link_path = format!("/proc/{}/fd/{}", pid_str, fd_str);
            let target = match std::fs::read_link(&link_path) {
                Ok(t) => t.to_string_lossy().to_string(),
                Err(_) => continue,
            };

            if target == real_path && proc_util::fd_is_writable(&pid_str, &fd_str) {
                return true;
            }
        }
    }
    false
}

/// Set up a notify file watcher on the tasks directory.
/// Returns the watcher (must be kept alive) or None on failure.
fn setup_notify_watcher(
    tasks_dir: &Path,
    tx: mpsc::UnboundedSender<String>,
) -> Option<RecommendedWatcher> {
    let mut watcher = match RecommendedWatcher::new(
        move |result: Result<Event, notify::Error>| {
            if let Ok(event) = result {
                if matches!(event.kind, EventKind::Create(_)) {
                    for path in &event.paths {
                        if let Some(fname) = path.file_name() {
                            let _ = tx.send(fname.to_string_lossy().to_string());
                        }
                    }
                }
            }
        },
        NotifyConfig::default(),
    ) {
        Ok(w) => w,
        Err(e) => {
            warn!(error = %e, "failed to create file watcher");
            return None;
        }
    };

    if let Err(e) = watcher.watch(tasks_dir, RecursiveMode::NonRecursive) {
        warn!(
            error = %e,
            path = %tasks_dir.display(),
            "failed to watch tasks directory"
        );
        return None;
    }

    info!(path = %tasks_dir.display(), "file watcher active");
    Some(watcher)
}

/// CLI handler: create/reinit the tasks tmux session.
pub async fn cmd_task_init(session: &str, show_all: bool) {
    let all_flag = if show_all { " --all" } else { "" };

    // Check if session exists
    if session_exists(session).await {
        // Respawn daemon pane without killing workloads
        let daemon_cmd = format!("task-watch daemon{}", all_flag);
        let _ = run_cmd(
            &[
                "tmux",
                "respawn-pane",
                "-k",
                "-t",
                &format!("{}:0.0", session),
                &daemon_cmd,
            ],
            5,
        )
        .await;

        // Reset window-size
        let _ = run_cmd(
            &[
                "tmux",
                "set-option",
                "-t",
                session,
                "window-size",
                "latest",
            ],
            5,
        )
        .await;

        println!(
            "Session '{}' reinited (daemon restarted, workload panes preserved)",
            session
        );
    } else {
        // Create new session with daemon pane
        let daemon_cmd = format!("task-watch daemon{}", all_flag);
        let _ = run_cmd(
            &["tmux", "new-session", "-d", "-s", session, "-n", "watch", &daemon_cmd],
            5,
        )
        .await;

        let _ = run_cmd(
            &[
                "tmux",
                "set-option",
                "-t",
                session,
                "window-size",
                "latest",
            ],
            5,
        )
        .await;

        println!(
            "Session '{}' created, daemon running in pane 0",
            session
        );
    }
}

/// CLI handler: list tracked tasks.
pub async fn cmd_task_list(_session: &str, json: bool) {
    // Find tasks dir
    let tasks_dir = match find_tasks_dir() {
        Some(d) => d,
        None => {
            if json {
                println!("[]");
            } else {
                println!("No tasks directory found");
            }
            return;
        }
    };

    // List .output files with mtime
    let mut entries = Vec::new();
    if let Ok(dir) = std::fs::read_dir(&tasks_dir) {
        for entry in dir.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if !fname.ends_with(".output") {
                continue;
            }
            let tid = fname.strip_suffix(".output").unwrap_or(&fname).to_string();
            let label = infer_label(&tasks_dir, &tid);
            let is_agent = is_agent_output(&tasks_dir, &tid);
            let has_content = has_output(&tasks_dir, &tid);

            entries.push(serde_json::json!({
                "task_id": tid,
                "label": label,
                "is_agent": is_agent,
                "has_output": has_content,
            }));
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".to_string()));
    } else if entries.is_empty() {
        println!("No tracked tasks");
    } else {
        for e in &entries {
            let tid = e["task_id"].as_str().unwrap_or("?");
            let label = e["label"].as_str().unwrap_or("?");
            let agent = if e["is_agent"].as_bool().unwrap_or(false) {
                " [agent]"
            } else {
                ""
            };
            println!("  {:24}  {}{}", label, tid, agent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_infer_label_from_content_empty() {
        assert_eq!(infer_label_from_content("abcdef123456", ""), "abcdef123456");
    }

    #[test]
    fn test_infer_label_from_content_short_text() {
        assert_eq!(
            infer_label_from_content("tid", "Hello world"),
            "Hello world"
        );
    }

    #[test]
    fn test_infer_label_from_content_long_text() {
        let long = "A".repeat(50);
        let result = infer_label_from_content("tid", &long);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 43); // 37 + "..."
    }

    #[test]
    fn test_infer_label_from_content_agent_slug() {
        let json = r#"{"slug":"my-agent","agentId":"abc123"}"#;
        assert_eq!(
            infer_label_from_content("tid", json),
            "agent:my-agent"
        );
    }

    #[test]
    fn test_infer_label_from_content_agent_id_only() {
        let json = r#"{"agentId":"abcdef123456789"}"#;
        assert_eq!(
            infer_label_from_content("tid", json),
            "agent:abcdef123456"
        );
    }

    #[test]
    fn test_infer_label_from_content_invalid_json() {
        let content = "{not valid json}";
        // Falls through to text handling
        let result = infer_label_from_content("tid", content);
        assert_eq!(result, "{not valid json}");
    }

    #[test]
    fn test_agent_conversation_complete_from_str_complete() {
        let content = r#"{"message":{"role":"user","content":"do something"}}
{"message":{"role":"assistant","content":[{"type":"text","text":"Done!"}]}}"#;
        assert!(agent_conversation_complete_from_str(content));
    }

    #[test]
    fn test_agent_conversation_complete_from_str_still_working() {
        let content = r#"{"message":{"role":"user","content":"do something"}}
{"message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#;
        assert!(!agent_conversation_complete_from_str(content));
    }

    #[test]
    fn test_agent_conversation_complete_from_str_empty() {
        assert!(!agent_conversation_complete_from_str(""));
    }

    #[test]
    fn test_agent_conversation_complete_from_str_user_only() {
        let content = r#"{"message":{"role":"user","content":"do something"}}"#;
        assert!(!agent_conversation_complete_from_str(content));
    }

    #[test]
    fn test_find_tasks_dir_in_nonexistent() {
        assert!(find_tasks_dir_in("/tmp/nonexistent-claude-watch-test-dir").is_none());
    }

    #[test]
    fn test_is_agent_output_no_file() {
        let dir = Path::new("/tmp/nonexistent-task-watch-test");
        assert!(!is_agent_output(dir, "fake-id"));
    }

    #[test]
    fn test_has_output_no_file() {
        let dir = Path::new("/tmp/nonexistent-task-watch-test");
        assert!(!has_output(dir, "fake-id"));
    }

    #[test]
    fn test_extract_task_id_from_pane_cmd_standard() {
        let cmd = "echo '=== Some Label [abc123def] ==='; tail -f /tmp/tasks/abc123def.output";
        assert_eq!(extract_task_id_from_pane_cmd(cmd), Some("abc123def".to_string()));
    }

    #[test]
    fn test_extract_task_id_from_pane_cmd_agent() {
        let cmd = "echo '=== agent:tracker-search [a644bc543a7a9e8a6] ==='; tail -f /tmp/tasks/a644bc543a7a9e8a6.output | task-watch format-jsonl | task-watch timestamp-lines";
        assert_eq!(extract_task_id_from_pane_cmd(cmd), Some("a644bc543a7a9e8a6".to_string()));
    }

    #[test]
    fn test_extract_task_id_from_pane_cmd_no_brackets() {
        let cmd = "echo 'task-watch daemon handled by claude-watch'; sleep infinity";
        assert_eq!(extract_task_id_from_pane_cmd(cmd), None);
    }

    #[test]
    fn test_extract_task_id_from_pane_cmd_empty_brackets() {
        let cmd = "echo '=== [] ==='; tail -f /dev/null";
        assert_eq!(extract_task_id_from_pane_cmd(cmd), None);
    }

    #[test]
    fn test_extract_task_id_from_pane_cmd_with_hyphens() {
        let cmd = "echo '=== Build Task [my-task-123] ==='; tail -f /tmp/out";
        assert_eq!(extract_task_id_from_pane_cmd(cmd), Some("my-task-123".to_string()));
    }
}
