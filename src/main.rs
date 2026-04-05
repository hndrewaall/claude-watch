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
mod logging;
mod policy;
mod proc_util;
mod session_event;
mod state;
mod status;
mod task_watch;
mod tmux;

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
    },
    /// List tracked tasks
    #[command(alias = "ls")]
    List {
        /// Output as JSON
        #[arg(long)]
        json: bool,
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
            map.insert("pane".to_string(), serde_json::Value::String(cs.pane.clone()));
        }
        if cs.tokens > 0 || !cs.pane.is_empty() {
            map.insert("tokens".to_string(), serde_json::Value::Number(cs.tokens.into()));
        }
        if cs.bashes > 0 || !cs.pane.is_empty() {
            map.insert("bashes".to_string(), serde_json::Value::Number(cs.bashes.into()));
        }
        if let Some(cr) = cs.compact_remaining {
            map.insert("compact_remaining".to_string(), serde_json::Value::Number(cr.into()));
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
        println!("Tokens:   {:>7} / {} ({:.0}%)", format_number(cs.tokens), format_number(max_tokens), pct);
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
    info!(loop_interval_ms = loop_interval.as_millis() as u64, general_interval_ms = general_interval.as_millis() as u64, fg_interval_ms = fg_interval.as_millis() as u64, "main loop intervals");
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
            match session_event::log_event(
                &event_type,
                note.as_deref(),
                tokens,
                &events_file,
            )
            .await
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
                        println!("Not due ({}h since last post, next in ~{}h)", hours, 24 - hours);
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

async fn run_task(action: TaskAction) {
    let config = load_config();
    match action {
        TaskAction::Init { all } => {
            task_watch::cmd_task_init(&config.task_watch.session, all).await;
        }
        TaskAction::List { json } => {
            task_watch::cmd_task_list(&config.task_watch.session, json).await;
        }
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
        "task-watch-ctl" => {
            // task-watch-ctl <args...> → claude-watch task <args...>
            let mut new_args = vec!["claude-watch".to_string(), "task".to_string()];
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
    let args = multicall_rewrite_args();
    let cli = Cli::parse_from(args);

    match cli.command {
        Some(Commands::Status { json, tokens, bashes }) => {
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
            run_task(action).await;
        }
        None => {
            run_daemon().await;
        }
    }
}
