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

/// CLI: `workload run <label> -- <command...>`
pub fn cmd_run(label: &str, cmd_args: &[String]) -> i32 {
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
         {exe_q} workload emit-done --label {label_q} --exit-code \"$EC\" --log-path {out_q} >/dev/null 2>&1 || true\n\
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

/// CLI (hidden): `workload emit-done --label X --exit-code N --log-path P [--killed]`.
/// Invoked by the wrapper script after the workload exits. Keeps the
/// emit logic in Rust (testable, dep-free) instead of in bash.
pub fn cmd_emit_done(label: &str, exit_code: i32, log_path: &str, killed: bool) -> i32 {
    emit_workload_done(&WorkloadDoneEvent {
        label,
        exit_code,
        killed,
        log_path,
    });
    0
}

#[cfg(test)]
mod tests {
    use super::*;

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
            },
        );
        let j = serde_json::to_string(&s).unwrap();
        let parsed: WorkloadState = serde_json::from_str(&j).unwrap();
        assert_eq!(parsed["foo"].pane_id, "%3");
        assert_eq!(parsed["foo"].command, "sleep 10");
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
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        // SAFETY: same justification as the event_bus tests — these are
        // not concurrently mutating the env var with peers.
        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
        }

        let rc = cmd_emit_done("test-task", 0, "/tmp/foo.output", false);
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
    }

    #[test]
    fn cmd_emit_done_killed_marker() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var("CLAUDE_EVENT_QUEUE").ok();
        unsafe {
            std::env::set_var("CLAUDE_EVENT_QUEUE", tmp.path());
        }
        let rc = cmd_emit_done("killed-task", -15, "/tmp/k.output", true);
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
}
