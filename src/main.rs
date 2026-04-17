//! claude-watch: persistent daemon that monitors Claude Code health via tmux.
//!
//! Replaces the Python cron-based predecessor with a continuously-running
//! Rust daemon. Observes tmux pane state, Claude Code status bar, heartbeat
//! files, and watcher processes to detect and recover from stuck states.
//!
//! Features:
//!   - Dead process detection (Claude Code crashed -> restart)
//!   - Fresh /clear detection (inject resume prompt)
//!   - Heartbeat stale detection (zombie -> alert + inject)
//!   - Token-stall detection (tokens unchanged + bashes declining)
//!   - Foreground blocking detection (too long in foreground ops)
//!   - Individual watcher health monitoring
//!   - Exponential backoff on alerts
//!   - State persistence across restarts
//!   - Structured JSON logging
//!
//! Usage:
//!   claude-watch          # run the daemon (default)
//!   claude-watch status   # show Claude Code status and exit

mod agent;
mod alert;
mod cmd;
mod config;
mod hook_fire;
mod logging;
mod metrics;
mod policy;
mod proc_util;
mod reminders;
mod session_event;
mod state;
mod status;
mod task_filters;
mod task_watch;
mod tmux;
mod watcher;
mod workload;

use clap::{Parser, Subcommand};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::sleep;
use tracing::info;

use config::load_config;
use logging::{write_jsonl_log, write_legacy_log};
use state::{load_state, save_state};

#[derive(Parser)]
#[command(name = "claude-watch", about = "Monitor Claude Code health via tmux")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Show Claude Code status and exit
    Status {
        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Print just the token count
        #[arg(long)]
        tokens: bool,

        /// Print just the bash/background task count
        #[arg(long)]
        bashes: bool,
    },
    /// Trigger a version update immediately (bypass schedule)
    Update {
        /// Force update even if versions match
        #[arg(long)]
        force: bool,
    },
    /// Log session lifecycle events and show statistics
    Event {
        #[command(subcommand)]
        action: EventAction,
    },
    /// List and kill Claude Code subagents
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
    /// Manage task-watch (background task tmux panes)
    Task {
        #[command(subcommand)]
        action: TaskAction,
    },
    /// Manage watchers (supervision, enable/disable, restart)
    Watcher {
        #[command(subcommand)]
        action: WatcherAction,
    },
    /// Launch long-running workloads in the tasks tmux session
    Workload {
        #[command(subcommand)]
        action: WorkloadAction,
    },
    /// Write Prometheus textfile metrics from claude-watch state
    Metrics,
    /// Fire a hybrid-model hook reminder (invoked by Claude Code hooks).
    ///
    /// Writes a marker to `~/.cache/claude-watch/reminders/<type>.json` so
    /// the daemon knows to defer its fallback injection, and emits a hook
    /// JSON response on stdout that injects reminder text into the
    /// conversation.
    #[command(name = "hook-fire")]
    HookFire {
        /// Reminder type: context_high | version_update | pre_compact
        kind: String,
        /// Override the hookEventName echoed in the JSON response. Defaults
        /// to a sensible per-type value (Stop / SessionStart / PreCompact).
        #[arg(long)]
        hook_event: Option<String>,
    },
}

#[derive(Subcommand)]
enum WorkloadAction {
    /// Start a workload in tmux
    Run {
        /// Short label for the workload
        label: String,
        /// Command to run (after --)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        cmd: Vec<String>,
    },
    /// Show running workloads
    #[command(alias = "ls")]
    List,
    /// Block until workload completes, print final output
    Wait {
        /// Workload label
        label: String,
        /// Number of output lines to show (default: 20)
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
    },
    /// Show/tail workload output
    Log {
        /// Workload label
        label: String,
        /// Follow output (tail -f)
        #[arg(short, long)]
        follow: bool,
        /// Number of lines (default: 20)
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
    },
    /// Kill a running workload
    Kill {
        /// Workload label
        label: String,
    },
}

#[derive(Subcommand)]
enum EventAction {
    /// Log a session event (boot, compaction, restart, exit, checklist, compact-prep)
    Log {
        /// Event type
        #[arg(value_parser = ["boot", "compaction", "restart", "exit", "checklist", "compact-prep"])]
        event_type: String,
        /// Optional context note
        #[arg(long, short)]
        note: Option<String>,
        /// Override token count (default: auto-detect from tmux)
        #[arg(long)]
        tokens: Option<u64>,
    },
    /// Show summary of session events
    Stats {
        /// Time window (e.g. 1h, 2d, 30m)
        #[arg(long)]
        since: Option<String>,
    },
    /// Show compaction frequency and interval analysis
    CompactionStats {
        /// Time window (e.g. 1h, 2d, 30m)
        #[arg(long)]
        since: Option<String>,
        /// Check if daily stats DM is due (exit 0 = due, exit 1 = not due)
        #[arg(long)]
        check: bool,
    },
    /// Show recent completed tasks from session-task
    History {
        /// Time window (e.g. 1h, 2d, 30m)
        #[arg(long)]
        since: Option<String>,
        /// Number of tasks to show
        #[arg(short, long, default_value = "10")]
        count: usize,
    },
}

#[derive(Subcommand)]
enum TaskAction {
    /// Create/reinit the tasks tmux session
    Init {
        /// Show all tasks including persistent watchers
        #[arg(long, short)]
        all: bool,
        /// Detach after creating (for systemd)
        #[arg(long)]
        detach: bool,
        /// Destroy and recreate session (kills workload panes)
        #[arg(long)]
        recreate: bool,
        /// Allow --recreate even with running workloads
        #[arg(long)]
        force: bool,
    },
    /// List tracked tasks
    #[command(alias = "ls")]
    List {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Add a tail -f pane for a task
    Add {
        /// Task ID
        id: String,
        /// Display label for the pane
        #[arg(long, short)]
        label: Option<String>,
    },
    /// Kill a task pane
    Remove {
        /// Task ID
        id: String,
        /// Show what would be removed
        #[arg(long, short = 'n')]
        dry_run: bool,
    },
    /// Garbage-collect dead/orphaned panes
    Gc,
    /// Read-only attach to the tasks session (fatfinger-proof)
    Monitor {
        /// Use tmux -CC (iTerm2 control mode)
        #[arg(long)]
        cc: bool,
    },
    /// Read-write multi-client attach
    Attach {
        /// Use tmux -CC (iTerm2 control mode)
        #[arg(long)]
        cc: bool,
    },
    /// Prefix each stdin line with [HH:MM:SS]
    #[command(name = "timestamp-lines")]
    TimestampLines,
    /// Pretty-print agent JSONL from stdin
    #[command(name = "format-jsonl")]
    FormatJsonl,
    /// Compat shim: the daemon loop runs in-process in claude-watch now.
    /// Accepts and ignores --all / --poll-interval / --done-delay / --min-display.
    #[command(hide = true)]
    Daemon {
        #[arg(long, short)]
        all: bool,
        #[arg(long)]
        poll_interval: Option<u64>,
        #[arg(long)]
        done_delay: Option<u64>,
        #[arg(long)]
        min_display: Option<u64>,
    },
}

#[derive(Subcommand)]
enum AgentAction {
    /// List agents and their processes
    #[command(alias = "ls")]
    List {
        /// Also show watcher processes
        #[arg(long, short)]
        all: bool,
    },
    /// Kill a specific agent (by ID, ID prefix, or PID)
    Kill {
        /// Agent ID (or prefix) or PID
        target: String,
        /// Show what would be killed
        #[arg(short, long)]
        dry_run: bool,
    },
    /// Kill all agent processes (not watchers)
    KillAll {
        /// Show what would be killed
        #[arg(short, long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum WatcherAction {
    /// Run a watcher by name (exec start_cmd, wait for exit)
    Run {
        /// Watcher name
        name: String,
    },
    /// List configured watchers
    #[command(alias = "ls")]
    List {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show running status of all watchers
    Status {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Enable a watcher (toggle + start)
    Enable {
        /// Watcher name
        name: String,
    },
    /// Disable a watcher (toggle + kill)
    Disable {
        /// Watcher name
        name: String,
    },
    /// Kill all enabled watcher processes and clean PID files
    Restart,
}

async fn run_status(json: bool, tokens_only: bool, bashes_only: bool) {
    let config = load_config();
    let max_tokens = config.claude.max_context_tokens;
    let cs = status::get_claude_status().await;

    // If no pane found, try version info only
    let cs = match cs {
        Some(cs) => cs,
        None => {
            let version_info = tokio::task::spawn_blocking(status::get_version_info)
                .await
                .unwrap_or_default();

            if version_info.running.is_none() && version_info.installed.is_none() {
                eprintln!("Could not find claude tmux pane or process.");
                std::process::exit(1);
            }

            // Minimal status with version info only
            status::ClaudeStatus {
                pane: String::new(),
                tokens: 0,
                bashes: 0,
                compact_remaining: None,
                version: version_info.running,
                latest: version_info.installed,
            }
        }
    };

    if json {
        let mut map = serde_json::Map::new();
        if !cs.pane.is_empty() {
            map.insert(
                "pane".to_string(),
                serde_json::Value::String(cs.pane.clone()),
            );
        }
        if cs.tokens > 0 || !cs.pane.is_empty() {
            map.insert(
                "tokens".to_string(),
                serde_json::Value::Number(cs.tokens.into()),
            );
        }
        if cs.bashes > 0 || !cs.pane.is_empty() {
            map.insert(
                "bashes".to_string(),
                serde_json::Value::Number(cs.bashes.into()),
            );
        }
        if let Some(cr) = cs.compact_remaining {
            map.insert(
                "compact_remaining".to_string(),
                serde_json::Value::Number(cr.into()),
            );
        }
        if let Some(ref v) = cs.version {
            map.insert("version".to_string(), serde_json::Value::String(v.clone()));
        }
        if let Some(ref v) = cs.latest {
            map.insert("latest".to_string(), serde_json::Value::String(v.clone()));
        }
        let json_obj = serde_json::Value::Object(map);
        println!("{}", serde_json::to_string_pretty(&json_obj).unwrap());
        return;
    }

    if tokens_only {
        if cs.tokens > 0 || !cs.pane.is_empty() {
            println!("{}", cs.tokens);
        } else {
            println!("?");
        }
        return;
    }

    if bashes_only {
        if cs.bashes > 0 || !cs.pane.is_empty() {
            println!("{}", cs.bashes);
        } else {
            println!("?");
        }
        return;
    }

    // Human-readable output
    if cs.tokens > 0 || !cs.pane.is_empty() {
        let pct = cs.tokens as f64 / max_tokens as f64 * 100.0;
        println!(
            "Tokens:   {:>7} / {} ({:.0}%)",
            format_number(cs.tokens),
            format_number(max_tokens),
            pct
        );
    }
    if let Some(cr) = cs.compact_remaining {
        println!("Compact:  {}% remaining", cr);
    }
    if cs.bashes > 0 || !cs.pane.is_empty() {
        println!("Bashes:   {}", cs.bashes);
    }

    let version = cs.version.as_deref().unwrap_or("?");
    let latest = cs.latest.as_deref().unwrap_or("?");
    if version == latest {
        println!("Version:  {} (up to date)", version);
    } else {
        println!("Version:  {} (latest: {})", version, latest);
    }

    if !cs.pane.is_empty() {
        println!("Pane:     {}", cs.pane);
    }
}

/// Format a number with comma separators.
fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// Trigger file path for signaling the daemon to run an update.
const UPDATE_TRIGGER_FILE: &str = "/tmp/claude-watch-update-trigger";

async fn run_update(force: bool) {
    // Write a trigger file for the daemon to pick up.
    // The daemon runs the actual update sequence (it survives Claude Code's exit).
    let content = if force { "force" } else { "" };
    match std::fs::write(UPDATE_TRIGGER_FILE, content) {
        Ok(_) => {
            println!(
                "Update trigger written. The daemon will pick it up within ~10s and perform the restart.{}",
                if force { " (forced)" } else { "" }
            );
        }
        Err(e) => {
            eprintln!("Failed to write trigger file: {}", e);
            std::process::exit(1);
        }
    }
}

async fn run_daemon() {
    // Set up tracing (stderr for human-readable, jsonl file handled separately)
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    info!("claude-watch starting");

    let config = load_config();
    let mut state = load_state(&config.general.state_file);

    // Ensure log directory exists
    for path in [&config.general.log_file, &config.general.legacy_log_file] {
        if let Some(parent) = Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
    }

    write_jsonl_log(
        &config.general.log_file,
        "daemon_start",
        serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "check_interval": config.general.check_interval,
        }),
    );
    write_legacy_log(
        &config.general.legacy_log_file,
        &format!(
            "daemon started (v{}, interval={}s)",
            env!("CARGO_PKG_VERSION"),
            config.general.check_interval
        ),
    );

    // Signal handling
    let shutdown = Arc::new(AtomicBool::new(false));
    let reload = Arc::new(AtomicBool::new(false));

    // SIGTERM / SIGINT -> graceful shutdown
    let shutdown_flag = shutdown.clone();
    tokio::spawn(async move {
        let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM");
        let mut sigint = signal(SignalKind::interrupt()).expect("failed to register SIGINT");
        tokio::select! {
            _ = sigterm.recv() => {
                info!("received SIGTERM, shutting down");
            }
            _ = sigint.recv() => {
                info!("received SIGINT, shutting down");
            }
        }
        shutdown_flag.store(true, Ordering::Relaxed);
    });

    // SIGHUP -> reload config
    let reload_flag = reload.clone();
    tokio::spawn(async move {
        let mut sighup = signal(SignalKind::hangup()).expect("failed to register SIGHUP");
        loop {
            sighup.recv().await;
            info!("received SIGHUP, will reload config on next cycle");
            reload_flag.store(true, Ordering::Relaxed);
        }
    });

    // Spawn task-watch loop if enabled
    if config.task_watch.enabled {
        let tw_config = config.task_watch.clone();
        let tw_shutdown = shutdown.clone();
        tokio::spawn(async move {
            task_watch::run_task_watch_loop(tw_config, tw_shutdown).await;
        });
        info!("task-watch loop spawned");
    }

    // Main loop
    //
    // The loop sleeps for the foreground_monitor.check_interval (or general.check_interval
    // if foreground monitoring is disabled). Full check cycles run at general.check_interval,
    // while lightweight foreground checks run on every iteration.
    let mut current_config = config;
    let general_interval = Duration::from_secs(current_config.general.check_interval);
    let fg_interval = if current_config.foreground_monitor.enabled {
        Duration::from_secs(current_config.foreground_monitor.check_interval)
    } else {
        general_interval
    };
    let loop_interval = fg_interval.min(general_interval);
    info!(
        loop_interval_ms = loop_interval.as_millis() as u64,
        general_interval_ms = general_interval.as_millis() as u64,
        fg_interval_ms = fg_interval.as_millis() as u64,
        "main loop intervals"
    );
    let mut last_full_check = std::time::Instant::now() - general_interval; // run immediately

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        // Reload config if SIGHUP received
        if reload.load(Ordering::Relaxed) {
            reload.store(false, Ordering::Relaxed);
            info!("reloading config");
            current_config = load_config();
            write_jsonl_log(
                &current_config.general.log_file,
                "config_reload",
                serde_json::json!({}),
            );
        }

        let now = std::time::Instant::now();

        // Full check cycle at general.check_interval
        if now.duration_since(last_full_check) >= general_interval {
            policy::check_cycle(&current_config, &mut state).await;
            last_full_check = now;
        } else if current_config.foreground_monitor.enabled {
            // Lightweight foreground-only check between full cycles
            let pane = state.last_known_pane.clone();
            let tokens = state.last_known_tokens;
            let bashes = state.last_known_bashes;
            tracing::debug!("foreground-only check (between full cycles)");
            policy::check_foreground(&current_config, &mut state, &pane, tokens, bashes).await;
        }

        sleep(loop_interval).await;
    }

    // Graceful shutdown
    info!("claude-watch shutting down");
    write_jsonl_log(
        &current_config.general.log_file,
        "daemon_stop",
        serde_json::json!({}),
    );
    write_legacy_log(&current_config.general.legacy_log_file, "daemon stopped");
    save_state(&current_config.general.state_file, &state);
}

async fn run_event(action: EventAction) {
    match action {
        EventAction::Log {
            event_type,
            note,
            tokens,
        } => {
            let events_file = session_event::events_file_path();
            match session_event::log_event(&event_type, note.as_deref(), tokens, &events_file).await
            {
                Ok(entry) => {
                    let token_str = entry
                        .tokens
                        .map(|t| format!("  [{} tokens]", session_event::format_number(t)))
                        .unwrap_or_default();
                    let note_str = entry
                        .note
                        .as_ref()
                        .map(|n| format!(" ({})", n))
                        .unwrap_or_default();
                    println!(
                        "Logged: {} at {}{}{}",
                        entry.event, entry.timestamp, note_str, token_str
                    );
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        EventAction::Stats { since } => {
            let since_secs = since
                .as_deref()
                .map(session_event::parse_duration_secs)
                .transpose()
                .unwrap_or_else(|e| {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                });
            let events_file = session_event::events_file_path();
            let events = session_event::read_events(&events_file, since_secs);
            print!("{}", session_event::format_stats(&events));
        }
        EventAction::CompactionStats { since, check } => {
            if check {
                let due = session_event::check_compaction_stats_due();
                match due {
                    session_event::CompactionStatsDue::Due(hours) => {
                        println!("DUE ({}h since last post)", hours);
                        std::process::exit(0);
                    }
                    session_event::CompactionStatsDue::NotDue(hours) => {
                        println!(
                            "Not due ({}h since last post, next in ~{}h)",
                            hours,
                            24 - hours
                        );
                        std::process::exit(1);
                    }
                    session_event::CompactionStatsDue::NeverPosted => {
                        println!("DUE (never posted)");
                        std::process::exit(0);
                    }
                    session_event::CompactionStatsDue::Error(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(2);
                    }
                }
            }
            let since_secs = since
                .as_deref()
                .map(session_event::parse_duration_secs)
                .transpose()
                .unwrap_or_else(|e| {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                });
            let events_file = session_event::events_file_path();
            let events = session_event::read_events(&events_file, since_secs);
            print!("{}", session_event::format_compaction_stats(&events));
        }
        EventAction::History { since: _, count } => {
            let tasks_file = session_event::completed_tasks_path();
            let tasks = session_event::read_completed_tasks(&tasks_file, count);
            print!("{}", session_event::format_history(&tasks));
        }
    }
}

async fn run_task(action: TaskAction) -> i32 {
    let config = load_config();
    let session = &config.task_watch.session;
    match action {
        TaskAction::Init {
            all,
            detach,
            recreate,
            force,
        } => {
            task_watch::cmd_task_init(session, all, detach, recreate, force).await;
            0
        }
        TaskAction::List { json } => {
            task_watch::cmd_task_list(session, json).await;
            0
        }
        TaskAction::Add { id, label } => {
            task_watch::cmd_task_add(session, &id, label.as_deref()).await
        }
        TaskAction::Remove { id, dry_run } => {
            task_watch::cmd_task_remove(session, &id, dry_run).await
        }
        TaskAction::Gc => task_watch::cmd_task_gc(session).await,
        TaskAction::Monitor { cc } => task_watch::cmd_task_monitor(session, true, cc).await,
        TaskAction::Attach { cc } => task_watch::cmd_task_monitor(session, false, cc).await,
        TaskAction::TimestampLines => task_filters::cmd_timestamp_lines(),
        TaskAction::FormatJsonl => task_filters::cmd_format_jsonl(),
        TaskAction::Daemon { .. } => {
            // Compat shim for old callers (e.g. the init pane 0 command).
            // The real daemon loop runs inside claude-watch as a tokio task
            // when `[task_watch] enabled = true` in the config.
            println!("task-watch daemon handled by claude-watch");
            // Sleep indefinitely so this behaves like the Python daemon
            // would from a process-lifetime perspective.
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        }
    }
}

async fn run_watcher(action: WatcherAction) {
    let cfg = watcher::config_path();
    let exit_code = match action {
        WatcherAction::Run { name } => watcher::cmd_run(&cfg, &name).await,
        WatcherAction::List { json } => {
            watcher::cmd_list(&cfg, json);
            0
        }
        WatcherAction::Status { json } => {
            watcher::cmd_status(&cfg, json).await;
            0
        }
        WatcherAction::Enable { name } => watcher::cmd_toggle(&cfg, &name, true).await,
        WatcherAction::Disable { name } => watcher::cmd_toggle(&cfg, &name, false).await,
        WatcherAction::Restart => {
            watcher::cmd_restart(&cfg).await;
            0
        }
    };
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

fn run_workload(action: WorkloadAction) -> i32 {
    match action {
        WorkloadAction::Run { label, mut cmd } => {
            // Strip leading '--' from command remainder (shell passes it through)
            if cmd.first().map(|s| s.as_str()) == Some("--") {
                cmd.remove(0);
            }
            workload::cmd_run(&label, &cmd)
        }
        WorkloadAction::List => workload::cmd_list(),
        WorkloadAction::Wait { label, lines } => workload::cmd_wait(&label, lines),
        WorkloadAction::Log {
            label,
            follow,
            lines,
        } => workload::cmd_log(&label, lines, follow),
        WorkloadAction::Kill { label } => workload::cmd_kill(&label),
    }
}

fn run_agent(action: AgentAction) {
    let exit_code = match action {
        AgentAction::List { all } => agent::cmd_list(all),
        AgentAction::Kill { target, dry_run } => agent::cmd_kill(&target, dry_run),
        AgentAction::KillAll { dry_run } => agent::cmd_kill_all(dry_run),
    };
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

/// Multicall dispatch: detect argv[0] symlink name and rewrite args to the
/// appropriate subcommand. This is like busybox — `agent-ctl list` becomes
/// `claude-watch agent list`, `session-event boot` becomes
/// `claude-watch event log boot`, etc.
fn multicall_rewrite_args() -> Vec<String> {
    let args: Vec<String> = std::env::args().collect();
    let binary_name = args
        .first()
        .and_then(|a| Path::new(a).file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("claude-watch");

    match binary_name {
        "agent-ctl" => {
            // agent-ctl <args...> → claude-watch agent <args...>
            let mut new_args = vec!["claude-watch".to_string(), "agent".to_string()];
            new_args.extend_from_slice(&args[1..]);
            new_args
        }
        "task-watch-ctl" | "task-watch" => {
            // task-watch <args...> → claude-watch task <args...>
            let mut new_args = vec!["claude-watch".to_string(), "task".to_string()];
            new_args.extend_from_slice(&args[1..]);
            new_args
        }
        "watcher-ctl" => {
            // watcher-ctl <args...> → claude-watch watcher <args...>
            let mut new_args = vec!["claude-watch".to_string(), "watcher".to_string()];
            new_args.extend_from_slice(&args[1..]);
            new_args
        }
        "watcher-status" => {
            // watcher-status → claude-watch watcher status
            let mut new_args = vec![
                "claude-watch".to_string(),
                "watcher".to_string(),
                "status".to_string(),
            ];
            new_args.extend_from_slice(&args[1..]);
            new_args
        }
        "workload" => {
            // workload <args...> → claude-watch workload <args...>
            let mut new_args = vec!["claude-watch".to_string(), "workload".to_string()];
            new_args.extend_from_slice(&args[1..]);
            new_args
        }
        "claude-watch-metrics" => {
            // claude-watch-metrics → claude-watch metrics
            let mut new_args = vec!["claude-watch".to_string(), "metrics".to_string()];
            new_args.extend_from_slice(&args[1..]);
            new_args
        }
        "watcher-restart" => {
            // watcher-restart → claude-watch watcher restart
            let mut new_args = vec![
                "claude-watch".to_string(),
                "watcher".to_string(),
                "restart".to_string(),
            ];
            new_args.extend_from_slice(&args[1..]);
            new_args
        }
        "session-event" => {
            // Backward compat: if first arg is a known event type, insert "log"
            // session-event boot --note X → claude-watch event log boot --note X
            // session-event stats → claude-watch event stats
            let mut new_args = vec!["claude-watch".to_string(), "event".to_string()];
            let first_arg = args.get(1).map(|s| s.as_str());
            match first_arg {
                Some("boot" | "compaction" | "restart" | "exit" | "checklist" | "compact-prep") => {
                    new_args.push("log".to_string());
                    new_args.extend_from_slice(&args[1..]);
                }
                _ => {
                    new_args.extend_from_slice(&args[1..]);
                }
            }
            new_args
        }
        _ => args,
    }
}

#[tokio::main]
async fn main() {
    // Restore default SIGPIPE handling so piping to `head` etc. exits
    // cleanly instead of panicking on println! with a broken pipe.
    // Safe: we only call this once at startup before any async work.
    // See https://github.com/rust-lang/rust/issues/46016
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let args = multicall_rewrite_args();
    let cli = Cli::parse_from(args);

    match cli.command {
        Some(Commands::Status {
            json,
            tokens,
            bashes,
        }) => {
            run_status(json, tokens, bashes).await;
        }
        Some(Commands::Update { force }) => {
            run_update(force).await;
        }
        Some(Commands::Event { action }) => {
            run_event(action).await;
        }
        Some(Commands::Agent { action }) => {
            run_agent(action);
        }
        Some(Commands::Task { action }) => {
            let code = run_task(action).await;
            if code != 0 {
                std::process::exit(code);
            }
        }
        Some(Commands::Watcher { action }) => {
            run_watcher(action).await;
        }
        Some(Commands::Workload { action }) => {
            let code = run_workload(action);
            if code != 0 {
                std::process::exit(code);
            }
        }
        Some(Commands::Metrics) => {
            let code = metrics::cmd_metrics();
            if code != 0 {
                std::process::exit(code);
            }
        }
        Some(Commands::HookFire { kind, hook_event }) => {
            let code = hook_fire::cmd_hook_fire(&kind, hook_event.as_deref()).await;
            if code != 0 {
                std::process::exit(code);
            }
        }
        None => {
            run_daemon().await;
        }
    }
}
