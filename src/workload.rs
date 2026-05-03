//! workload — launch long-running tasks in the `tasks` tmux session that
//! survive Claude Code /clear and compaction.
//!
//! Straight Rust port of the Python `workload` script. State lives under
//! `/tmp/claude-workloads/` (state.json, <label>.output, <label>.exit,
//! <label>.sh) for compatibility with the existing layout so in-flight
//! workloads from the old script keep working during the transition.
//!
//! On workload completion (natural or via `workload kill`), an event of
//! `tag=workload-done`, `source=workload` is emitted into
//! `~/claude-events/` so `claude-event-watch` surfaces the completion to
//! the main loop without needing a separate `workload wait` background
//! task. Idempotency: the wrapper script writes an exit-code marker file
//! BEFORE invoking the emitter; `cmd_kill` consults that marker and
//! skips its own emit if the wrapper already finished naturally.

use crate::event_bus::{emit_workload_done, WorkloadDoneEvent};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const SESSION: &str = "tasks";
const WORKLOAD_DIR: &str = "/tmp/claude-workloads";

fn state_file() -> PathBuf {
    PathBuf::from(WORKLOAD_DIR).join("state.json")
}

fn output_file(label: &str) -> PathBuf {
    PathBuf::from(WORKLOAD_DIR).join(format!("{label}.output"))
}

fn exit_file(label: &str) -> PathBuf {
    PathBuf::from(WORKLOAD_DIR).join(format!("{label}.exit"))
}

fn script_file(label: &str) -> PathBuf {
    PathBuf::from(WORKLOAD_DIR).join(format!("{label}.sh"))
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkloadEntry {
    #[serde(default)]
    pub pane_id: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub started_at: String,
    /// Queue id this workload is bound to (`workload run --queue-id
    /// q-X`). When set, the wrapper-side `emit_done` carries the qid
    /// into the `workload-done` event AND transitions the queue item
    /// to done/abandoned via `session-task` — first-class workload
    /// model (Andrew DM 2026-05-03 05:23 ET). Backward compatible:
    /// existing state.json entries without the field deserialize as
    /// None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_id: Option<String>,
}

pub type WorkloadState = BTreeMap<String, WorkloadEntry>;

pub fn load_state() -> WorkloadState {
    let path = state_file();
    let data = match fs::read_to_string(&path) {
        Ok(d) => d,
        Err(_) => return WorkloadState::new(),
    };
    serde_json::from_str(&data).unwrap_or_default()
}

pub fn save_state(state: &WorkloadState) -> std::io::Result<()> {
    fs::create_dir_all(WORKLOAD_DIR)?;
    let json = serde_json::to_string_pretty(state).unwrap_or_else(|_| "{}".to_string());
    fs::write(state_file(), json)
}

/// POSIX single-quote shell escape.
fn shell_quote(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

fn session_exists() -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", SESSION])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn pane_alive(pane_id: &str) -> bool {
    if pane_id.is_empty() {
        return false;
    }
    let out = Command::new("tmux")
        .args(["list-panes", "-t", SESSION, "-F", "#{pane_id}"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            stdout.lines().any(|l| l.trim() == pane_id)
        }
        _ => false,
    }
}

fn rebalance() {
    let _ = Command::new("tmux")
        .args(["select-layout", "-t", SESSION, "even-vertical"])
        .output();
}

/// Best-effort PATH walk for the `session-task` CLI. Used by
/// `transition_queue_item_for_workload` to mark the queue item
/// done/abandoned after a workload-bound (`--queue-id`) workload
/// exits. Honours an explicit override via the `SESSION_TASK_CLI`
/// env var (used by tests to point at a per-test stub).
fn find_session_task_cli() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SESSION_TASK_CLI") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("session-task");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let home = std::env::var("HOME").ok()?;
    let candidate = PathBuf::from(home).join("bin/session-task");
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

fn read_exit_code(label: &str) -> Option<i32> {
    let path = exit_file(label);
    let s = fs::read_to_string(path).ok()?;
    s.trim().parse::<i32>().ok()
}

fn print_tail(path: &Path, n: usize) {
    if let Ok(data) = fs::read_to_string(path) {
        let lines: Vec<&str> = data.lines().collect();
        let start = lines.len().saturating_sub(n);
        for line in &lines[start..] {
            println!("{line}");
        }
    }
}

/// Kill only the setsid child process group of a pane — never the wrapper
/// shell's PGID (which may be shared with the tmux session). Mirrors the
/// Python `_kill_pane_tree`.
fn kill_pane_tree(pane_id: &str) {
    // Pane shell PID
    let out = Command::new("tmux")
        .args(["list-panes", "-t", pane_id, "-F", "#{pane_pid}"])
        .output();
    let shell_pid = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => return,
    };
    if shell_pid.is_empty() {
        return;
    }

    // Shell's own PGID — skip this one
    let shell_pgid = Command::new("ps")
        .args(["-o", "pgid=", "-p", &shell_pid])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    // Direct children
    let children: Vec<String> = Command::new("pgrep")
        .args(["-P", &shell_pid])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let mut killed_pgids = std::collections::HashSet::new();
    for pid in &children {
        let pgid = Command::new("ps")
            .args(["-o", "pgid=", "-p", pid])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if pgid.is_empty() || pgid == "1" || pgid == shell_pgid {
            // Kill the PID directly (not the pgroup)
            let _ = Command::new("kill").args(["-9", pid]).output();
            continue;
        }
        if killed_pgids.insert(pgid.clone()) {
            // setsid group — safe to kill entirely
            let _ = Command::new("kill")
                .args(["-9", "--", &format!("-{pgid}")])
                .output();
        }
    }

    // Kill any remaining descendants by PID
    let remaining = get_descendants(&shell_pid);
    if !remaining.is_empty() {
        let mut args = vec!["-9".to_string()];
        args.extend(remaining);
        let _ = Command::new("kill").args(&args).output();
    }
}

fn get_descendants(pid: &str) -> Vec<String> {
    let mut out = Vec::new();
    let children: Vec<String> = Command::new("pgrep")
        .args(["-P", pid])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default();
    for c in children {
        let sub = get_descendants(&c);
        out.push(c);
        out.extend(sub);
    }
    out
}

/// CLI: `workload run <label> [--queue-id q-X] -- <command...>`
///
/// When `queue_id` is `Some(...)` the workload is bound to a queue
/// item: on completion the `workload-done` event carries the qid AND
/// the queue item is transitioned to done/abandoned via `session-task`.
/// Workloads-as-first-class-queue-items model — Andrew DM 2026-05-03
/// 05:23 ET. Resolves the q-2026-05-03-1e7d orphaned-workload bug by
/// making workload exit equivalent to queue completion (no agent
/// respawn dance).
pub fn cmd_run(label: &str, cmd_args: &[String], queue_id: Option<&str>) -> i32 {
    if cmd_args.is_empty() {
        eprintln!("No command specified");
        return 1;
    }
    let command: String = cmd_args
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");

    if !session_exists() {
        eprintln!("No '{SESSION}' tmux session. Run: claude-watch task init");
        return 1;
    }

    if let Err(e) = fs::create_dir_all(WORKLOAD_DIR) {
        eprintln!("Failed to create {WORKLOAD_DIR}: {e}");
        return 1;
    }

    let out_path = output_file(label);
    let exit_path = exit_file(label);
    let script_path = script_file(label);

    // Clean up previous run's exit marker + output
    let _ = fs::remove_file(&exit_path);
    let _ = fs::remove_file(&out_path);

    // Kill existing workload with same label
    let mut state = load_state();
    if let Some(entry) = state.get(label) {
        if pane_alive(&entry.pane_id) {
            let _ = Command::new("tmux")
                .args(["kill-pane", "-t", &entry.pane_id])
                .output();
        }
        state.remove(label);
        let _ = save_state(&state);
    }

    // Wrapper script — identical layout to Python version, plus a
    // claude-event emit step after the exit-code is written so the main
    // loop's `claude-event-watch` learns about the completion without
    // needing a separate `workload wait` background task.
    //
    // The emit invokes the claude-watch binary itself (this process's
    // current_exe path baked in at run time) via the hidden `workload
    // emit-done` subcommand. We embed the absolute path so the wrapper
    // doesn't depend on PATH discovery inside tmux.
    let out_q = shell_quote(&out_path.to_string_lossy());
    let exit_q = shell_quote(&exit_path.to_string_lossy());
    let cmd_q = shell_quote(&command);
    let label_q = shell_quote(label);
    let exe_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "claude-watch".to_string());
    let exe_q = shell_quote(&exe_path);
    // When --queue-id is set, append it to the emit-done call so the
    // wrapper-side emit carries the qid into the workload-done event
    // AND triggers the queue done/abandon transition. The flag stays
    // optional — bare workloads emit the legacy event with no qid and
    // no queue side effect (regression safety).
    let queue_id_emit_arg = match queue_id {
        Some(qid) => format!(" --queue-id {}", shell_quote(qid)),
        None => String::new(),
    };
    let script = format!(
        "#!/bin/bash\n\
         # Trap SIGINT/SIGTERM — fatfinger-proof against accidental Ctrl-C\n\
         trap '' INT TERM\n\
         exec > >(tee -a {out_q}) 2>&1\n\
         echo '=== workload: {label} ==='\n\
         echo 'Started: '$(date -Iseconds)\n\
         echo 'Command: {command_escaped}'\n\
         echo '---'\n\
         setsid --wait bash -c {cmd_q}\n\
         EC=$?\n\
         echo ''\n\
         echo \"=== DONE (exit $EC) at $(date -Iseconds) ===\"\n\
         echo $EC > {exit_q}\n\
         # Emit claude-event for the main loop. Default-open: any failure\n\
         # here is silently swallowed — the exit-file write above is the\n\
         # source of truth for `workload wait`.\n\
         {exe_q} workload emit-done --label {label_q} --exit-code \"$EC\" --log-path {out_q}{queue_id_emit_arg} >/dev/null 2>&1 || true\n\
         sleep 30\n",
        // The "Command: " line gets the unquoted version for readability;
        // escape single quotes for the heredoc context.
        command_escaped = command.replace('\'', "'\\''"),
    );

    if let Err(e) = fs::write(&script_path, script) {
        eprintln!("Failed to write script: {e}");
        return 1;
    }
    let _ = fs::set_permissions(&script_path, fs::Permissions::from_mode(0o700));

    // Create pane running the script
    let out = Command::new("tmux")
        .args([
            "split-window",
            "-t",
            SESSION,
            "-v",
            "-P",
            "-F",
            "#{pane_id}",
            &script_path.to_string_lossy(),
        ])
        .output();
    let pane_id = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Ok(o) => {
            eprintln!(
                "Failed to create pane: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            return 1;
        }
        Err(e) => {
            eprintln!("Failed to create pane: {e}");
            return 1;
        }
    };

    rebalance();

    let started_at = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    state.insert(
        label.to_string(),
        WorkloadEntry {
            pane_id: pane_id.clone(),
            command: command.clone(),
            output: out_path.to_string_lossy().to_string(),
            started_at,
            queue_id: queue_id.map(str::to_string),
        },
    );
    let _ = save_state(&state);

    println!("Started workload '{label}' in pane {pane_id}");
    println!("Output: {}", out_path.display());
    println!(
        "Watch for the `workload-done` claude-event in the next \
         UserPromptSubmit context (fire-and-forget). Do NOT spawn \
         `workload wait` as a background task."
    );
    0
}

/// CLI: `workload list`
pub fn cmd_list() -> i32 {
    let state = load_state();
    if state.is_empty() {
        println!("No workloads");
        return 0;
    }
    for (label, info) in &state {
        let alive = pane_alive(&info.pane_id);
        let exit_code = read_exit_code(label);
        let status = if alive {
            "running".to_string()
        } else if let Some(ec) = exit_code {
            format!("done (exit {ec})")
        } else {
            "dead".to_string()
        };
        println!(
            "  {:24}  {:6}  [{}]  started {}",
            label, info.pane_id, status, info.started_at
        );
        println!("    {}", info.command);
    }
    0
}

/// CLI: `workload wait <label> [--force-i-acknowledge-events-are-better]`
///
/// Disabled by default. Workloads emit a `workload-done` claude-event when
/// they exit; that event arrives in the main loop's next UserPromptSubmit
/// context via the claude-event hook chain, so blocking polling via
/// `workload wait` is fully redundant and ties up a Claude Code background
/// task slot. Returns exit code 2 with an explanatory error unless the
/// user has explicitly opted in via the long flag.
pub fn cmd_wait(label: &str, lines: usize, force_acknowledged: bool) -> i32 {
    if !force_acknowledged {
        eprintln!(
            "ERROR: `workload wait` is disabled by default.\n\
             \n\
             Workloads emit a `workload-done` claude-event when they exit.\n\
             That event surfaces in the main loop's next UserPromptSubmit\n\
             context, so blocking polling via `workload wait` is redundant\n\
             and only clutters the Claude Code background task list.\n\
             \n\
             Recommended pattern: fire-and-forget the workload\n\
             (`workload run <label> -- <cmd>`) and watch for the\n\
             `workload-done` claude-event on the next turn.\n\
             \n\
             If you genuinely need the blocking-poll behavior, opt in:\n\
             \n\
             \tworkload wait {label} --force-i-acknowledge-events-are-better\n\
             \n\
             See feedback_no-explicit-task-watchers.md for the full rule."
        );
        return 2;
    }

    let state = load_state();
    let info = match state.get(label) {
        Some(i) => i.clone(),
        None => {
            eprintln!("No workload '{label}'");
            return 1;
        }
    };

    let exit_path = exit_file(label);
    if exit_path.exists() {
        let ec = read_exit_code(label).unwrap_or(1);
        println!("Workload '{label}' already completed (exit {ec})");
        print_tail(Path::new(&info.output), lines);
        return ec;
    }

    println!("Waiting for workload '{label}' to complete...");

    loop {
        if exit_path.exists() {
            break;
        }
        if !pane_alive(&info.pane_id) {
            // Give a moment for the exit file to appear
            std::thread::sleep(Duration::from_secs(1));
            break;
        }
        std::thread::sleep(Duration::from_secs(5));
    }

    if exit_path.exists() {
        let ec = read_exit_code(label).unwrap_or(1);
        println!("\n=== Workload '{label}' completed (exit {ec}) ===");
        ec
    } else {
        println!("\n=== Workload '{label}' pane died without exit code ===");
        1
    }
}

/// CLI: `workload log <label>`
pub fn cmd_log(label: &str, lines: usize, follow: bool) -> i32 {
    let state = load_state();
    let info = match state.get(label) {
        Some(i) => i.clone(),
        None => {
            eprintln!("No workload '{label}'");
            return 1;
        }
    };
    let path = PathBuf::from(&info.output);
    if !path.exists() {
        eprintln!("No output file: {}", path.display());
        return 1;
    }
    if follow {
        // exec tail -f
        use std::os::unix::process::CommandExt;
        let err = Command::new("tail")
            .args(["-f", "-n", &lines.to_string()])
            .arg(&path)
            .exec();
        eprintln!("exec tail failed: {err}");
        1
    } else {
        print_tail(&path, lines);
        0
    }
}

/// CLI: `workload kill <label>`
pub fn cmd_kill(label: &str) -> i32 {
    let mut state = load_state();
    let info = match state.get(label) {
        Some(i) => i.clone(),
        None => {
            eprintln!("No workload '{label}'");
            return 1;
        }
    };

    // If the wrapper script already wrote its exit file, it also
    // already emitted (or will emit before its 30s sleep ends). Skip
    // our kill-event emit to keep the contract "exactly one event per
    // workload run". Only synthesise a kill event when we're racing
    // ahead of a still-alive wrapper.
    let exit_path = exit_file(label);
    let already_exited = exit_path.exists();

    if pane_alive(&info.pane_id) {
        if !already_exited {
            // Synthesise the exit marker so subsequent `workload wait`
            // calls return cleanly with the kill code, and emit the
            // claude-event before tearing down the pane.
            let _ = fs::write(&exit_path, "-15\n");
            emit_workload_done(&WorkloadDoneEvent {
                label,
                exit_code: -15,
                killed: true,
                log_path: &info.output,
                queue_id: info.queue_id.as_deref(),
            });
        }
        kill_pane_tree(&info.pane_id);
        let _ = Command::new("tmux")
            .args(["kill-pane", "-t", &info.pane_id])
            .output();
        println!("Killed workload '{label}' (pane {})", info.pane_id);
    } else {
        println!("Workload '{label}' already dead");
    }
    state.remove(label);
    let _ = save_state(&state);
    rebalance();
    0
}

/// CLI (hidden): `workload emit-done --label X --exit-code N --log-path P [--killed] [--queue-id q-X]`.
/// Invoked by the wrapper script after the workload exits. Keeps the
/// emit logic in Rust (testable, dep-free) instead of in bash.
///
/// When `queue_id` is set, the workload is treated as a FIRST-CLASS
/// queue item (Andrew DM 2026-05-03 05:23 ET). On exit:
///   * the `workload-done` event carries the qid in `data.queue_id`;
///   * the queue item is transitioned to `done` (rc==0 + not killed)
///     or `abandoned` (non-zero rc OR killed) via `session-task`.
///
/// No respawn-event or mandatory-obligation: workload completion IS
/// queue completion. The main loop sees the canonical `queue-done` /
/// `queue-abandoned` claude-event when `session-task` performs the
/// transition.
///
/// The queue-transition step is best-effort: failure (CLI not on
/// PATH, session-task non-zero) is logged at warn level and
/// swallowed. The `workload-done` event is emitted regardless.
/// Suppression knob: `WORKLOAD_QUEUE_TRANSITION=0` (env) skips the
/// queue call entirely (used by tests).
pub fn cmd_emit_done(
    label: &str,
    exit_code: i32,
    log_path: &str,
    killed: bool,
    queue_id: Option<&str>,
) -> i32 {
    emit_workload_done(&WorkloadDoneEvent {
        label,
        exit_code,
        killed,
        log_path,
        queue_id,
    });
    if let Some(qid) = queue_id {
        transition_queue_item_for_workload(qid, label, exit_code, killed, log_path);
    }
    0
}

/// Mark the queue item bound to this workload as `done` (clean exit)
/// or `abandoned` (non-zero rc / killed). Best-effort; never fails the
/// caller. Suppression knob: `WORKLOAD_QUEUE_TRANSITION=0`. CLI
/// override: `SESSION_TASK_CLI` (used by tests to point at a stub).
///
/// Mapping rationale:
///   * rc==0 && !killed → `session-task queue done <qid>` (success)
///   * killed           → `session-task queue abandon <qid> --reason ...`
///   * other rc != 0    → `session-task queue abandon <qid> --reason ...`
///
/// `session-task queue done` already emits the `queue-done` claude-
/// event; `queue abandon` emits `queue-abandoned`. Either way the main
/// loop sees the canonical lifecycle event without us inventing a new
/// tag. First-class workload model — Andrew DM 2026-05-03 05:23 ET.
fn transition_queue_item_for_workload(
    queue_id: &str,
    label: &str,
    exit_code: i32,
    killed: bool,
    log_path: &str,
) {
    if std::env::var("WORKLOAD_QUEUE_TRANSITION")
        .ok()
        .as_deref()
        == Some("0")
    {
        return;
    }
    let cli = match find_session_task_cli() {
        Some(p) => p,
        None => {
            tracing::warn!(
                queue_id = %queue_id,
                label = %label,
                "workload queue transition: session-task CLI not found, skipping"
            );
            return;
        }
    };

    let args: Vec<String> = if exit_code == 0 && !killed {
        vec![
            "queue".to_string(),
            "done".to_string(),
            queue_id.to_string(),
            "--silent".to_string(),
        ]
    } else {
        let reason = if killed {
            format!("workload {label} killed (rc={exit_code}, log={log_path})")
        } else {
            format!(
                "workload {label} exited non-zero rc={exit_code} (log={log_path})"
            )
        };
        vec![
            "queue".to_string(),
            "abandon".to_string(),
            queue_id.to_string(),
            "--reason".to_string(),
            reason,
            "--silent".to_string(),
        ]
    };

    let result = std::process::Command::new(&cli)
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::time::{Duration, Instant};
            // 15s timeout — session-task queue done/abandon is
            // normally <500ms but file-locking under load can stretch.
            // A wedged CLI must not stall the wrapper.
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => return Ok(status),
                    Ok(None) => {
                        if Instant::now() >= deadline {
                            let _ = child.kill();
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "session-task queue transition timed out (15s)",
                            ));
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => return Err(e),
                }
            }
        });

    match result {
        Ok(status) if status.success() => {
            tracing::info!(
                queue_id = %queue_id,
                label = %label,
                exit_code = exit_code,
                killed = killed,
                "workload queue transition succeeded"
            );
        }
        Ok(status) => {
            tracing::warn!(
                queue_id = %queue_id,
                label = %label,
                rc = ?status.code(),
                "workload queue transition exited non-zero"
            );
        }
        Err(e) => {
            tracing::warn!(
                queue_id = %queue_id,
                label = %label,
                error = %e,
                "workload queue transition failed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Serializes tests that mutate process-global env vars (CLAUDE_EVENT_QUEUE,
    // SESSION_TASK_CLI, WORKLOAD_QUEUE_TRANSITION). Without it, parallel test
    // execution interleaves env-var sets and the wrong tempdir / stub path is
    // observed by the function under test. Same pattern as `task_watch::tests`'
    // WORKLOAD_ENV_LOCK. Acquire BEFORE setting any env var; hold for the
    // entire body so the restore-on-drop window is exclusive.
    use std::sync::Mutex;
    static WORKLOAD_TEST_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn shell_quote_plain() {
        assert_eq!(shell_quote("hello"), "'hello'");
    }

    #[test]
    fn shell_quote_with_apostrophe() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn state_roundtrip() {
        let mut s = WorkloadState::new();
        s.insert(
            "foo".to_string(),
            WorkloadEntry {
                pane_id: "%3".to_string(),
                command: "sleep 10".to_string(),
                output: "/tmp/claude-workloads/foo.output".to_string(),
                started_at: "2026-01-01T00:00:00".to_string(),
                queue_id: None,
            },
        );
        let j = serde_json::to_string(&s).unwrap();
        let parsed: WorkloadState = serde_json::from_str(&j).unwrap();
        assert_eq!(parsed["foo"].pane_id, "%3");
        assert_eq!(parsed["foo"].command, "sleep 10");
        assert_eq!(parsed["foo"].queue_id, None);
    }

    #[test]
    fn state_roundtrip_with_queue_id() {
        // First-class workload model: when `workload run --queue-id`
        // is used, the qid is persisted in state.json so `cmd_kill`'s
        // synthesised event also carries it.
        let mut s = WorkloadState::new();
        s.insert(
            "scoped".to_string(),
            WorkloadEntry {
                pane_id: "%4".to_string(),
                command: "sleep 99".to_string(),
                output: "/tmp/claude-workloads/scoped.output".to_string(),
                started_at: "2026-05-03T05:00:00".to_string(),
                queue_id: Some("q-2026-05-03-test".to_string()),
            },
        );
        let j = serde_json::to_string(&s).unwrap();
        let parsed: WorkloadState = serde_json::from_str(&j).unwrap();
        assert_eq!(
            parsed["scoped"].queue_id.as_deref(),
            Some("q-2026-05-03-test")
        );
    }

    #[test]
    fn state_loads_legacy_entry_without_queue_id_field() {
        // Existing /tmp/claude-workloads/state.json files predate the
        // queue_id field — must deserialize cleanly with queue_id=None.
        let raw = r#"{"foo":{"pane_id":"%5","command":"x","output":"/tmp/x","started_at":"2026"}}"#;
        let parsed: WorkloadState = serde_json::from_str(raw).expect("legacy parse");
        assert_eq!(parsed["foo"].queue_id, None);
    }

    #[test]
    fn state_loads_missing_file_as_empty() {
        // load_state uses WORKLOAD_DIR which may not exist in CI — should return empty.
        let s = load_state();
        // Just verify no panic and is a BTreeMap
        let _ = s.len();
    }

    #[test]
    fn cmd_wait_without_force_flag_exits_with_code_2() {
        // Bare `workload wait <label>` must short-circuit BEFORE touching
        // any state — Andrew's rule (2026-05-01): the `workload-done`
        // claude-event is the canonical completion signal, polling is
        // redundant. The flag has to be hard to type accidentally.
        let rc = cmd_wait("nonexistent-label", 20, false);
        assert_eq!(
            rc, 2,
            "bare `workload wait` must exit 2 (opt-in required), got {rc}"
        );
    }

    #[test]
    fn cmd_wait_with_force_flag_proceeds_to_state_lookup() {
        // With the opt-in flag set, the gate is bypassed and we fall
        // through to the existing state-lookup code path. For a missing
        // label that yields exit code 1 ("No workload 'X'"), proving the
        // flag actually unblocked the function (versus the gate's exit 2).
        let rc = cmd_wait("definitely-not-a-real-workload-xyz", 20, true);
        assert_eq!(
            rc, 1,
            "opt-in `workload wait` should reach state lookup and exit 1 \
             for missing label, got {rc}"
        );
    }

    #[test]
    fn cmd_emit_done_writes_event_file() {
        // Point CLAUDE_EVENT_QUEUE at a tempdir; cmd_emit_done should
        // produce exactly one workload-done event with the right shape.
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        // SAFETY: lock above serializes against peer tests touching
        // the same process-global env vars.
        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
        }

        let rc = cmd_emit_done("test-task", 0, "/tmp/foo.output", false, None);
        assert_eq!(rc, 0);

        // Restore env first so any panic below doesn't leak.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
            }
        }

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read tempdir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with("_workload-done.json")
            })
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one event");

        let body = std::fs::read_to_string(entries[0].path()).expect("read");
        let v: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(v["tag"], "workload-done");
        assert_eq!(v["data"]["label"], "test-task");
        assert_eq!(v["data"]["exit_code"], 0);
        assert_eq!(v["data"]["killed"], false);
        assert_eq!(v["data"]["log_path"], "/tmp/foo.output");
        // Without --queue-id, no queue_id field in event data.
        assert!(v["data"].get("queue_id").is_none());
    }

    #[test]
    fn cmd_emit_done_killed_marker() {
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
        }
        let rc = cmd_emit_done("killed-task", -15, "/tmp/k.output", true, None);
        assert_eq!(rc, 0);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
            }
        }

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read tempdir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with("_workload-done.json")
            })
            .collect();
        assert_eq!(entries.len(), 1);
        let body = std::fs::read_to_string(entries[0].path()).expect("read");
        let v: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(v["data"]["killed"], true);
        assert_eq!(v["data"]["exit_code"], -15);
        assert!(v["message"]
            .as_str()
            .unwrap()
            .contains("workload killed-task killed"));
    }

    #[test]
    fn cmd_emit_done_with_queue_id_carries_qid_in_event() {
        // First-class workload model: --queue-id puts data.queue_id in
        // the event. Suppress the queue transition (no session-task on
        // PATH in CI) — that path is exercised in the stub-CLI test.
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_q = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        let prev_t = std::env::var("WORKLOAD_QUEUE_TRANSITION").ok();
        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
            std::env::set_var("WORKLOAD_QUEUE_TRANSITION", "0");
        }

        let rc = cmd_emit_done(
            "qa-task",
            0,
            "/tmp/qa.output",
            false,
            Some("q-2026-05-03-test"),
        );
        assert_eq!(rc, 0);

        unsafe {
            match prev_q {
                Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
            }
            match prev_t {
                Some(v) => std::env::set_var("WORKLOAD_QUEUE_TRANSITION", v),
                None => std::env::remove_var("WORKLOAD_QUEUE_TRANSITION"),
            }
        }

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read tempdir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with("_workload-done.json")
            })
            .collect();
        assert_eq!(entries.len(), 1, "expected one workload-done event");
        let body = std::fs::read_to_string(entries[0].path()).expect("read");
        let v: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(v["tag"], "workload-done");
        assert_eq!(v["data"]["queue_id"], "q-2026-05-03-test");
    }

    #[test]
    fn cmd_emit_done_calls_session_task_queue_done_on_clean_exit() {
        // Stub session-task as a recording bash script. Verify it was
        // invoked with `queue done <qid>` when exit_code=0 and
        // killed=false. SESSION_TASK_CLI overrides PATH lookup.
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_q = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        let prev_cli = std::env::var("SESSION_TASK_CLI").ok();
        let prev_t = std::env::var("WORKLOAD_QUEUE_TRANSITION").ok();

        let recording = tmp.path().join("session-task.recording");
        let stub_path = tmp.path().join("session-task-stub");
        let stub = format!(
            "#!/bin/bash\nprintf '%s\\n' \"$@\" > {rec}\nexit 0\n",
            rec = shell_quote(&recording.to_string_lossy()),
        );
        std::fs::write(&stub_path, stub).expect("write stub");
        let _ = std::fs::set_permissions(
            &stub_path,
            std::fs::Permissions::from_mode(0o755),
        );

        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
            std::env::set_var("SESSION_TASK_CLI", &stub_path);
            std::env::remove_var("WORKLOAD_QUEUE_TRANSITION");
        }

        let rc = cmd_emit_done(
            "stub-task",
            0,
            "/tmp/stub.output",
            false,
            Some("q-2026-05-03-stub"),
        );

        unsafe {
            match prev_q {
                Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
            }
            match prev_cli {
                Some(v) => std::env::set_var("SESSION_TASK_CLI", v),
                None => std::env::remove_var("SESSION_TASK_CLI"),
            }
            match prev_t {
                Some(v) => std::env::set_var("WORKLOAD_QUEUE_TRANSITION", v),
                None => std::env::remove_var("WORKLOAD_QUEUE_TRANSITION"),
            }
        }

        assert_eq!(rc, 0);
        assert!(recording.exists(), "stub session-task should have been invoked");
        let recorded = std::fs::read_to_string(&recording).expect("read recording");
        assert!(
            recorded.contains("queue\ndone\nq-2026-05-03-stub"),
            "expected `queue done <qid>` invocation in {recorded}"
        );
        assert!(
            recorded.contains("--silent"),
            "expected --silent flag in {recorded}"
        );
    }

    #[test]
    fn cmd_emit_done_calls_queue_abandon_on_failure() {
        // Stub session-task. Verify `queue abandon <qid> --reason ...`
        // when the workload exits non-zero. Reason should mention the
        // exit code so post-mortem inspection is straightforward.
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_q = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        let prev_cli = std::env::var("SESSION_TASK_CLI").ok();
        let prev_t = std::env::var("WORKLOAD_QUEUE_TRANSITION").ok();

        let recording = tmp.path().join("session-task.recording");
        let stub_path = tmp.path().join("session-task-stub");
        let stub = format!(
            "#!/bin/bash\nprintf '%s\\n' \"$@\" > {rec}\nexit 0\n",
            rec = shell_quote(&recording.to_string_lossy()),
        );
        std::fs::write(&stub_path, stub).expect("write stub");
        let _ = std::fs::set_permissions(
            &stub_path,
            std::fs::Permissions::from_mode(0o755),
        );

        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
            std::env::set_var("SESSION_TASK_CLI", &stub_path);
            std::env::remove_var("WORKLOAD_QUEUE_TRANSITION");
        }

        let rc = cmd_emit_done(
            "fail-task",
            7,
            "/tmp/fail.output",
            false,
            Some("q-2026-05-03-fail"),
        );

        unsafe {
            match prev_q {
                Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
            }
            match prev_cli {
                Some(v) => std::env::set_var("SESSION_TASK_CLI", v),
                None => std::env::remove_var("SESSION_TASK_CLI"),
            }
            match prev_t {
                Some(v) => std::env::set_var("WORKLOAD_QUEUE_TRANSITION", v),
                None => std::env::remove_var("WORKLOAD_QUEUE_TRANSITION"),
            }
        }

        assert_eq!(rc, 0);
        assert!(recording.exists(), "stub should have been invoked");
        let recorded = std::fs::read_to_string(&recording).expect("read recording");
        assert!(
            recorded.contains("queue\nabandon\nq-2026-05-03-fail"),
            "expected `queue abandon <qid>` in {recorded}"
        );
        assert!(recorded.contains("--reason"));
        assert!(
            recorded.contains("rc=7"),
            "abandon reason must mention exit code: {recorded}"
        );
    }

    #[test]
    fn cmd_emit_done_calls_queue_abandon_on_kill() {
        // Killed workloads transition to abandoned with a reason
        // mentioning the kill — symmetric with non-zero exit handling.
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_q = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        let prev_cli = std::env::var("SESSION_TASK_CLI").ok();
        let prev_t = std::env::var("WORKLOAD_QUEUE_TRANSITION").ok();

        let recording = tmp.path().join("session-task.recording");
        let stub_path = tmp.path().join("session-task-stub");
        let stub = format!(
            "#!/bin/bash\nprintf '%s\\n' \"$@\" > {rec}\nexit 0\n",
            rec = shell_quote(&recording.to_string_lossy()),
        );
        std::fs::write(&stub_path, stub).expect("write stub");
        let _ = std::fs::set_permissions(
            &stub_path,
            std::fs::Permissions::from_mode(0o755),
        );

        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
            std::env::set_var("SESSION_TASK_CLI", &stub_path);
            std::env::remove_var("WORKLOAD_QUEUE_TRANSITION");
        }

        let rc = cmd_emit_done(
            "killed-task",
            -15,
            "/tmp/killed.output",
            true,
            Some("q-2026-05-03-killed"),
        );

        unsafe {
            match prev_q {
                Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
            }
            match prev_cli {
                Some(v) => std::env::set_var("SESSION_TASK_CLI", v),
                None => std::env::remove_var("SESSION_TASK_CLI"),
            }
            match prev_t {
                Some(v) => std::env::set_var("WORKLOAD_QUEUE_TRANSITION", v),
                None => std::env::remove_var("WORKLOAD_QUEUE_TRANSITION"),
            }
        }

        assert_eq!(rc, 0);
        let recorded = std::fs::read_to_string(&recording).expect("read recording");
        assert!(
            recorded.contains("queue\nabandon\nq-2026-05-03-killed"),
            "killed → abandon expected: {recorded}"
        );
        assert!(
            recorded.contains("killed"),
            "reason must mention kill: {recorded}"
        );
    }

    #[test]
    fn cmd_emit_done_queue_transition_skipped_by_env() {
        // WORKLOAD_QUEUE_TRANSITION=0 must skip the session-task call
        // entirely (regression safety + test-harness escape hatch).
        let _lock = WORKLOAD_TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_q = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        let prev_cli = std::env::var("SESSION_TASK_CLI").ok();
        let prev_t = std::env::var("WORKLOAD_QUEUE_TRANSITION").ok();

        let recording = tmp.path().join("session-task.recording");
        let stub_path = tmp.path().join("session-task-stub");
        let stub = format!(
            "#!/bin/bash\nprintf '%s\\n' \"$@\" > {rec}\nexit 0\n",
            rec = shell_quote(&recording.to_string_lossy()),
        );
        std::fs::write(&stub_path, stub).expect("write stub");
        let _ = std::fs::set_permissions(
            &stub_path,
            std::fs::Permissions::from_mode(0o755),
        );

        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
            std::env::set_var("SESSION_TASK_CLI", &stub_path);
            std::env::set_var("WORKLOAD_QUEUE_TRANSITION", "0");
        }

        let rc = cmd_emit_done(
            "skip-task",
            0,
            "/tmp/skip.output",
            false,
            Some("q-2026-05-03-skip"),
        );

        unsafe {
            match prev_q {
                Some(v) => std::env::set_var("CLAUDE_EVENT_QUEUE", v),
                None => std::env::remove_var("CLAUDE_EVENT_QUEUE"),
            }
            match prev_cli {
                Some(v) => std::env::set_var("SESSION_TASK_CLI", v),
                None => std::env::remove_var("SESSION_TASK_CLI"),
            }
            match prev_t {
                Some(v) => std::env::set_var("WORKLOAD_QUEUE_TRANSITION", v),
                None => std::env::remove_var("WORKLOAD_QUEUE_TRANSITION"),
            }
        }

        assert_eq!(rc, 0);
        assert!(
            !recording.exists(),
            "stub must NOT be invoked when WORKLOAD_QUEUE_TRANSITION=0"
        );
    }
}
