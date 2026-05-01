//! `claude-watch active-agents` — read-only enumeration of live agents.
//!
//! Emits a minimal fact set about who is running RIGHT NOW:
//!
//!   - `subagents`: live child PIDs of the Claude Code main process that
//!     are NOT watchers and NOT our own introspection commands. Reuses the
//!     same heuristic as `agent::cmd_list` and `respawn::count_active_subagents`.
//!     We emit raw PIDs (not agent IDs) deliberately — agent IDs would
//!     require reading the per-session subagents JSONL directory, which is
//!     a path-leak we explicitly want to avoid in the public-repo source.
//!
//!   - `workloads`: labels of currently-running workloads, read from
//!     claude-watch's own internal workload state. `running` here means the
//!     tmux pane is still alive; we exclude completed/dead workloads.
//!
//! Design intent: claude-watch emits FACTS about live processes. Whoever
//! consumes this output (e.g. a private cron shim that cross-references
//! against `session-task queue list --json`) decides what is "expected"
//! vs "orphaned" on its own side. claude-watch deliberately does NOT
//! consume the queue.json schema, scope semantics, or any local-path
//! convention. This is the abstraction line the repo failed to hold in
//! the first iteration of #53.

use serde::Serialize;

use crate::agent::{find_claude_pid, get_children, is_own_command, is_watcher, ChildProcess};
use crate::workload::{load_state, WorkloadState};

/// Output shape for `claude-watch active-agents [--json]`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ActiveAgents {
    /// Live PIDs of Claude Code subagent processes (children of the main
    /// Claude PID, minus watchers + own commands). Sorted ascending for a
    /// stable diff.
    pub subagents: Vec<u32>,

    /// Labels of currently-running workloads. Sorted ascending.
    pub workloads: Vec<String>,
}

/// Pure: filter `children` down to subagent PIDs and sort.
pub fn filter_subagent_pids(children: &[ChildProcess]) -> Vec<u32> {
    let mut pids: Vec<u32> = children
        .iter()
        .filter(|c| !is_watcher(&c.cmd) && !is_own_command(&c.cmd))
        .map(|c| c.pid)
        .collect();
    pids.sort_unstable();
    pids
}

/// Pure: extract running workload labels from a `WorkloadState`,
/// using a caller-supplied liveness predicate (so tests can avoid tmux).
///
/// "Running" = tmux pane is still alive. We do NOT consult `<label>.exit`:
/// a workload that has exited but whose pane is in the 30-second sleep
/// tail is conceptually "winding down", not "orphaned"; the consumer
/// shim doesn't need to see it. The pane-alive check is exactly what
/// `workload list` already uses for its "running" status.
pub fn running_workload_labels<F>(state: &WorkloadState, mut is_pane_alive: F) -> Vec<String>
where
    F: FnMut(&str) -> bool,
{
    let mut labels: Vec<String> = state
        .iter()
        .filter(|(_, info)| is_pane_alive(&info.pane_id))
        .map(|(label, _)| label.clone())
        .collect();
    labels.sort();
    labels
}

/// Production helper: ask tmux whether a pane id is alive.
///
/// Mirrors the private `pane_alive` in `workload.rs` — duplicating here
/// (instead of exposing the workload helper) keeps `active_agents` tests
/// completely independent of tmux. The function is short and the
/// duplication cost is one tmux invocation's worth of code.
fn pane_alive(pane_id: &str) -> bool {
    if pane_id.is_empty() {
        return false;
    }
    let out = std::process::Command::new("tmux")
        .args(["list-panes", "-t", "tasks", "-F", "#{pane_id}"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            stdout.lines().any(|l| l.trim() == pane_id)
        }
        _ => false,
    }
}

/// Collect the live agents fact-set. Production entry point.
///
/// Fail-open: if no Claude PID is detected (`find_claude_pid` returns None),
/// `subagents` is an empty Vec. Workloads still report normally — they
/// don't depend on the Claude PID. This matches the semantics of
/// `respawn::count_active_subagents`: when /proc isn't readable, we can't
/// see subagents, but we also can't auto-respawn, so emitting an empty
/// list is the safe default.
pub fn collect() -> ActiveAgents {
    let subagents = match find_claude_pid() {
        Some(pid) => filter_subagent_pids(&get_children(pid)),
        None => Vec::new(),
    };
    let workloads = running_workload_labels(&load_state(), pane_alive);
    ActiveAgents {
        subagents,
        workloads,
    }
}

/// CLI entry point. Returns exit code.
pub fn cmd_active_agents(json: bool) -> i32 {
    let agents = collect();
    if json {
        // Pretty-print so it composes cleanly with `jq` and is readable
        // when invoked manually for debugging.
        let s = serde_json::to_string_pretty(&agents).unwrap_or_else(|_| "{}".to_string());
        println!("{}", s);
    } else {
        // Human-readable, scannable in a terminal.
        if agents.subagents.is_empty() {
            println!("Subagents: (none)");
        } else {
            let pids: Vec<String> = agents.subagents.iter().map(|p| p.to_string()).collect();
            println!("Subagents: {}", pids.join(", "));
        }
        if agents.workloads.is_empty() {
            println!("Workloads: (none)");
        } else {
            println!("Workloads: {}", agents.workloads.join(", "));
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workload::WorkloadEntry;

    fn cp(pid: u32, cmd: &str) -> ChildProcess {
        ChildProcess {
            pid,
            cmd: cmd.to_string(),
        }
    }

    #[test]
    fn filter_subagent_pids_excludes_watchers() {
        let children = vec![
            cp(100, "zsh -c eval 'signal-wait --dm' < /dev/null"),
            cp(200, "python3 /home/user/.claude/sidechain-agent.py"),
            cp(300, "zsh -c eval 'torrent-wait' < /dev/null"),
        ];
        // 100 + 300 are watchers; only 200 should remain.
        assert_eq!(filter_subagent_pids(&children), vec![200]);
    }

    #[test]
    fn filter_subagent_pids_excludes_own_commands() {
        let children = vec![
            cp(100, "agent-ctl list"),
            cp(200, "claude-watch agent list"),
            cp(300, "ps --ppid 1 -o pid=,cmd="),
            cp(400, "python3 /some/agent-script.py"),
        ];
        assert_eq!(filter_subagent_pids(&children), vec![400]);
    }

    #[test]
    fn filter_subagent_pids_sorts_ascending() {
        let children = vec![
            cp(500, "python3 /tmp/a.py"),
            cp(100, "ruby /tmp/b.rb"),
            cp(300, "node /tmp/c.js"),
        ];
        assert_eq!(filter_subagent_pids(&children), vec![100, 300, 500]);
    }

    #[test]
    fn filter_subagent_pids_empty_input() {
        assert!(filter_subagent_pids(&[]).is_empty());
    }

    #[test]
    fn filter_subagent_pids_all_watchers() {
        let children = vec![
            cp(1, "zsh -c eval 'signal-wait' < /dev/null"),
            cp(2, "zsh -c eval 'memory-remind' < /dev/null"),
            cp(3, "zsh -c eval 'context-watch' < /dev/null"),
        ];
        assert!(filter_subagent_pids(&children).is_empty());
    }

    #[test]
    fn running_workload_labels_filters_dead_panes() {
        let mut state = WorkloadState::new();
        state.insert(
            "alive-1".to_string(),
            WorkloadEntry {
                pane_id: "%5".to_string(),
                command: "sleep 100".to_string(),
                output: "/tmp/a.output".to_string(),
                started_at: "2026-05-01T00:00:00".to_string(),
            },
        );
        state.insert(
            "dead-1".to_string(),
            WorkloadEntry {
                pane_id: "%6".to_string(),
                command: "echo hi".to_string(),
                output: "/tmp/b.output".to_string(),
                started_at: "2026-05-01T00:00:01".to_string(),
            },
        );

        // Predicate: only %5 is alive.
        let alive_only = |pid: &str| pid == "%5";
        let labels = running_workload_labels(&state, alive_only);
        assert_eq!(labels, vec!["alive-1".to_string()]);
    }

    #[test]
    fn running_workload_labels_sorts_alphabetically() {
        let mut state = WorkloadState::new();
        for label in ["zeta", "alpha", "mu"] {
            state.insert(
                label.to_string(),
                WorkloadEntry {
                    pane_id: format!("%{label}"),
                    command: String::new(),
                    output: String::new(),
                    started_at: String::new(),
                },
            );
        }
        let labels = running_workload_labels(&state, |_| true);
        assert_eq!(labels, vec!["alpha", "mu", "zeta"]);
    }

    #[test]
    fn running_workload_labels_empty_state() {
        let state = WorkloadState::new();
        assert!(running_workload_labels(&state, |_| true).is_empty());
    }

    #[test]
    fn running_workload_labels_all_dead() {
        let mut state = WorkloadState::new();
        state.insert(
            "x".to_string(),
            WorkloadEntry {
                pane_id: "%1".to_string(),
                command: String::new(),
                output: String::new(),
                started_at: String::new(),
            },
        );
        assert!(running_workload_labels(&state, |_| false).is_empty());
    }

    #[test]
    fn active_agents_serializes_to_expected_json_shape() {
        let agents = ActiveAgents {
            subagents: vec![1234, 5678],
            workloads: vec!["promote-foo".to_string(), "scan-bar".to_string()],
        };
        let json = serde_json::to_value(&agents).expect("serialize");
        assert_eq!(json["subagents"], serde_json::json!([1234, 5678]));
        assert_eq!(
            json["workloads"],
            serde_json::json!(["promote-foo", "scan-bar"])
        );
        // No extra keys leak through (e.g. internal scope tokens, paths,
        // queue ids). The whole point of this surface is the minimum
        // possible fact-set.
        let obj = json.as_object().expect("object");
        assert_eq!(obj.len(), 2);
    }

    #[test]
    fn active_agents_empty_serializes_to_empty_arrays() {
        let agents = ActiveAgents {
            subagents: vec![],
            workloads: vec![],
        };
        let json = serde_json::to_string(&agents).expect("serialize");
        assert_eq!(json, r#"{"subagents":[],"workloads":[]}"#);
    }
}
