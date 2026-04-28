//! Policy: the main check logic including dead process detection, fresh /clear,
//! heartbeat stale, foreground monitor, and watcher health.

use chrono::{DateTime, Local, Timelike, Utc};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::os::unix::process::CommandExt;
use std::time::SystemTime;
use tracing::{debug, info, warn};

use crate::alert;
use crate::config::Config;
use crate::logging::{write_jsonl_log, write_legacy_log};
use crate::reminders::{seconds_since_fire, should_defer_to_hook, ReminderType};
use crate::state::{FailureDetail, State, StatusSnapshot, WatcherState};
use crate::status;
use crate::tmux;

/// Parse elapsed seconds since an ISO datetime string.
pub(crate) fn elapsed_since(dt_str: &str) -> Option<f64> {
    let dt = DateTime::parse_from_rfc3339(dt_str).ok()?;
    let now = Utc::now();
    Some((now - dt.with_timezone(&Utc)).num_milliseconds() as f64 / 1000.0)
}

/// Pure function: compute the next thinking interrupt threshold with exponential backoff.
/// Formula: min(base_threshold * backoff_multiplier^interrupt_count, max_backoff)
/// E.g. with base=60, mult=2, max=960: 60, 120, 240, 480, 960, 960, ...
/// With base=300, mult=3, max=1800: 300, 900, 1800, 1800, ...
///
/// This 2-multiplier wrapper is retained for backward-compatibility and is
/// used by the legacy-compat test. The daemon's check_foreground path now
/// calls `thinking_backoff_threshold_with_multiplier` directly, reading the
/// multiplier from config.
#[allow(dead_code)]
pub(crate) fn thinking_backoff_threshold(
    base_threshold: u64,
    max_backoff: u64,
    interrupt_count: u32,
) -> u64 {
    thinking_backoff_threshold_with_multiplier(base_threshold, max_backoff, interrupt_count, 2)
}

/// Generalised version of `thinking_backoff_threshold` with a configurable
/// multiplier per step. Uses saturating arithmetic so huge `interrupt_count`
/// values never panic — they just cap at `max_backoff`.
pub(crate) fn thinking_backoff_threshold_with_multiplier(
    base_threshold: u64,
    max_backoff: u64,
    interrupt_count: u32,
    multiplier: u64,
) -> u64 {
    let mut threshold = base_threshold;
    for _ in 0..interrupt_count {
        threshold = threshold.saturating_mul(multiplier);
        if threshold >= max_backoff {
            return max_backoff;
        }
    }
    threshold.min(max_backoff)
}

/// Returns true if a previous interrupt fired within the last
/// `cooldown_secs` seconds. Used to suppress cascading interrupts across
/// the prolonged-thinking and context-warning fire paths.
///
/// NOTE (2026-04-28): The watcher-down inject path is intentionally
/// EXEMPT from this gate. A down watcher (signal-wait, claude-event-
/// watch, torrent-wait, etc.) is a hard liveness failure — silence in
/// the cooldown window means inbound messages, events, and torrents go
/// unprocessed for as long as it takes to clear. The watcher-down
/// inject must be allowed to fire even when another interrupt fired
/// recently. The per-watcher `last_watcher_inject` cooldown
/// (`watcher_monitor.inject_cooldown`, default 60s) still rate-limits
/// re-injects on the same fire path.
///
/// A `cooldown_secs` of 0 disables the gate entirely.
pub(crate) fn interrupt_in_global_cooldown(state: &State, cooldown_secs: u64) -> bool {
    if cooldown_secs == 0 {
        return false;
    }
    state
        .last_interrupt_at
        .as_deref()
        .and_then(elapsed_since)
        .is_some_and(|e| e < cooldown_secs as f64)
}

/// Pure predicate: should the watcher-down inject path fire now, given
/// the timestamp of the last watcher-inject and the configured cooldown?
///
/// - `None` last-inject (never fired before) -> always allow.
/// - `Some(ts)` -> allow iff elapsed >= cooldown_secs (or the timestamp
///   is malformed and `elapsed_since` returns None — fail-open so the
///   gate never wedges).
///
/// Intentionally does NOT consult `interrupt_in_global_cooldown`: see
/// the doc-comment above for the rationale.
pub(crate) fn watcher_inject_due(
    last_watcher_inject: Option<&str>,
    cooldown_secs: u64,
) -> bool {
    match last_watcher_inject {
        Some(last) => elapsed_since(last).is_none_or(|e| e >= cooldown_secs as f64),
        None => true,
    }
}

/// If the given reminder fired within the last `max_age_secs` (we default
/// to 1 hour — beyond that we assume the self-action is unrelated),
/// record the reminder -> action latency sample into the state-based
/// counters that `claude-watch metrics` exports. No-op otherwise.
///
/// `short` selects the shorter "context clear" latency window (1h); the
/// longer version-update path uses `short = false` (6h cap) because
/// updates can legitimately take many turns to propagate.
fn record_reminder_latency_if_recent(kind: ReminderType, state: &mut State, short: bool) {
    let max_age = if short { 3600.0 } else { 21600.0 };
    let elapsed = match seconds_since_fire(kind) {
        Some(e) if e >= 0.0 && e < max_age => e,
        _ => return,
    };
    match kind {
        ReminderType::ContextHigh => {
            state.reminder_to_clear_latency_secs_sum += elapsed;
            state.reminder_to_clear_latency_count =
                state.reminder_to_clear_latency_count.saturating_add(1);
        }
        ReminderType::VersionUpdate => {
            state.reminder_to_update_latency_secs_sum += elapsed;
            state.reminder_to_update_latency_count =
                state.reminder_to_update_latency_count.saturating_add(1);
        }
        ReminderType::PreCompact => {
            // PreCompact is a blocking hook — there's no "latency to
            // action" concept the same way as the other two. Skip.
        }
    }
}

/// Restart Claude Code by writing a relaunch script and injecting it.
async fn restart_claude(pane: &str, state: &mut State, config: &crate::config::ClaudeConfig) {
    let now = Local::now().to_rfc3339();

    // Try to find session ID from pane history
    let mut session_id: Option<String> = None;
    if let Some(out) = tmux::capture_pane_history(pane, 100).await {
        let re = regex_lite::Regex::new(r"--resume\s+([0-9a-f-]{36})").unwrap();
        if let Some(caps) = re.captures(&out) {
            session_id = Some(caps[1].to_string());
        }
    }

    // NOTE: Do NOT use --append-system-prompt here. It persists for the lifetime of the
    // process (survives /clear), causing misleading messages on subsequent context clears.
    // The resume prompt injection handles session startup instead.
    let launch = if let Some(ref sid) = session_id {
        info!(session_id = %sid, "restarting Claude Code with --resume");
        format!("claude --resume {}", sid)
    } else {
        info!("restarting Claude Code with --continue (no session ID found)");
        "claude --continue".to_string()
    };

    // Write relaunch script
    let script_content = format!(
        "#!/bin/bash\ncd $HOME\n{}\necho \"\\n[claude-watch-relaunch] Claude exited with code $?\"\n",
        launch
    );
    if let Err(e) = std::fs::write(&config.relaunch_script, &script_content) {
        tracing::error!(error = %e, "failed to write relaunch script");
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            &config.relaunch_script,
            std::fs::Permissions::from_mode(0o755),
        );
    }

    tmux::inject_shell(pane, &format!("bash {}", config.relaunch_script)).await;

    state.last_restart = Some(now);
    state.restart_count += 1;
    state.restart_claude_interrupts_total =
        state.restart_claude_interrupts_total.saturating_add(1);
    state.pending_resume_inject = true;

    alert::send_pingme("claude-watch: Claude Code crashed -- auto-restarting").await;
}

/// Run a foreground-only check cycle. This is called more frequently than
/// the full check_cycle to provide responsive foreground blocking detection.
/// Requires a known pane to check against.
pub async fn check_foreground(
    config: &Config,
    state: &mut State,
    pane: &str,
    tokens: u64,
    bashes: u64,
) {
    if !config.foreground_monitor.enabled || pane.is_empty() {
        return;
    }

    let now = chrono::Local::now().to_rfc3339();
    let fg_busy = tmux::is_foreground_busy(pane).await;

    // Also check thinking state at 3s resolution
    let activity = tmux::get_activity(pane).await;
    let is_thinking = matches!(activity, tmux::ClaudeActivity::Thinking);
    debug!(fg_busy, is_thinking, activity = %activity, tokens, bashes, "foreground check");

    // --- Thinking duration tracking (with exponential backoff) ---
    if is_thinking {
        if state.thinking_start.is_none() {
            state.thinking_start = Some(now.clone());
            state.thinking_alerted = false;
            // Don't reset thinking_interrupt_count here — it persists across
            // brief non-thinking blips within the same stall episode. It only
            // resets when we see a genuinely active state (below).
        } else if let Some(ref start) = state.thinking_start {
            if let Some(elapsed) = elapsed_since(start) {
                let next_threshold = thinking_backoff_threshold_with_multiplier(
                    config.foreground_monitor.threshold_seconds,
                    config.foreground_monitor.max_thinking_backoff,
                    state.thinking_interrupt_count,
                    config.foreground_monitor.thinking_backoff_multiplier,
                );
                if elapsed >= next_threshold as f64 {
                    // Global post-interrupt cooldown: if ANY interrupt fired
                    // recently (watcher-down, context-warning, or a prior
                    // thinking one), suppress this fire. Prevents the
                    // cascade where e.g. a watcher-down interrupt resets the
                    // thinking timer and the new thought trips prolonged
                    // thinking immediately afterward.
                    if interrupt_in_global_cooldown(
                        state,
                        config.general.post_interrupt_cooldown_secs,
                    ) {
                        debug!(
                            elapsed_secs = elapsed,
                            threshold = next_threshold,
                            cooldown = config.general.post_interrupt_cooldown_secs,
                            "prolonged thinking would fire but global post-interrupt cooldown active"
                        );
                        return;
                    }
                    warn!(
                        elapsed_secs = elapsed,
                        threshold = next_threshold,
                        interrupt_count = state.thinking_interrupt_count,
                        "prolonged thinking detected — interrupting (backoff)"
                    );
                    write_jsonl_log(
                        &config.general.log_file,
                        "prolonged_thinking",
                        serde_json::json!({
                            "elapsed_secs": elapsed,
                            "tokens": tokens,
                            "bashes": bashes,
                            "interrupt_count": state.thinking_interrupt_count,
                            "next_threshold_secs": next_threshold,
                            "action": if config.foreground_monitor.interrupt_enabled { "interrupt" } else { "log-only" },
                        }),
                    );
                    state.thinking_alerted = true;
                    state.thinking_interrupt_count += 1;
                    // Reset thinking_start so the next backoff interval
                    // counts from NOW, not from the original start
                    state.thinking_start = Some(now.clone());

                    if config.foreground_monitor.interrupt_enabled {
                        info!(
                            interrupt_count = state.thinking_interrupt_count,
                            next_backoff_secs = thinking_backoff_threshold_with_multiplier(
                                config.foreground_monitor.threshold_seconds,
                                config.foreground_monitor.max_thinking_backoff,
                                state.thinking_interrupt_count,
                                config.foreground_monitor.thinking_backoff_multiplier,
                            ),
                            "thinking interrupt: Escape + inject prompt"
                        );
                        // Stamp the global interrupt cooldown so other fire
                        // paths (watcher-down, context-warning) see this
                        // interrupt and back off.
                        state.last_interrupt_at = Some(now.clone());
                        state.prolonged_thinking_interrupts_total = state
                            .prolonged_thinking_interrupts_total
                            .saturating_add(1);
                        tmux::interrupt_and_wait(pane, 30).await;
                        let msg = format!(
                                "[CLAUDE-WATCH] Prolonged thinking detected (>{}s in thinking state, interrupt #{}). \
                                You appear to be stuck in a long generation. If you have complex work to do, \
                                delegate it to a background Agent instead of doing it inline. \
                                Use run_in_background: true for long Bash commands. \
                                Resume your current task now.",
                                next_threshold,
                                state.thinking_interrupt_count,
                            );
                        tmux::inject_text(pane, &msg).await;
                        write_jsonl_log(
                            &config.general.log_file,
                            "thinking_interrupted",
                            serde_json::json!({
                                "elapsed_secs": elapsed,
                                "tokens": tokens,
                                "bashes": bashes,
                                "interrupt_count": state.thinking_interrupt_count,
                            }),
                        );
                    } else {
                        info!(
                            elapsed_secs = elapsed,
                            interrupt_count = state.thinking_interrupt_count,
                            "thinking would interrupt (log-only mode)"
                        );
                    }
                }
            }
        }
    } else {
        state.thinking_start = None;
        state.thinking_alerted = false;
        state.thinking_interrupt_count = 0;
    }

    // --- Foreground blocking tracking ---
    if fg_busy {
        if state.foreground_start.is_none() {
            state.foreground_start = Some(now);
            state.foreground_alerted = false;
        } else if !state.foreground_alerted {
            if let Some(ref start) = state.foreground_start {
                if let Some(elapsed) = elapsed_since(start) {
                    if elapsed >= config.foreground_monitor.threshold_seconds as f64 {
                        warn!(
                            elapsed_secs = elapsed,
                            threshold = config.foreground_monitor.threshold_seconds,
                            "foreground blocking detected"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "foreground_blocking",
                            serde_json::json!({
                                "elapsed_secs": elapsed,
                                "tokens": tokens,
                                "bashes": bashes,
                            }),
                        );
                        state.foreground_alerted = true;

                        if config.foreground_monitor.interrupt_enabled {
                            info!("foreground interrupt: sending Ctrl-B x2 + inject message");
                            state.foreground_blocking_interrupts_total = state
                                .foreground_blocking_interrupts_total
                                .saturating_add(1);
                            tmux::interrupt_and_wait(pane, 30).await;
                            tmux::inject_text(pane, &config.foreground_monitor.interrupt_message)
                                .await;
                            write_jsonl_log(
                                &config.general.log_file,
                                "foreground_interrupted",
                                serde_json::json!({
                                    "elapsed_secs": elapsed,
                                    "tokens": tokens,
                                    "bashes": bashes,
                                    "message": config.foreground_monitor.interrupt_message,
                                }),
                            );
                        } else {
                            info!(
                                elapsed_secs = elapsed,
                                "foreground would interrupt (log-only mode)"
                            );
                            write_jsonl_log(
                                &config.general.log_file,
                                "foreground_would_interrupt",
                                serde_json::json!({
                                    "elapsed_secs": elapsed,
                                    "tokens": tokens,
                                    "bashes": bashes,
                                }),
                            );
                        }
                    }
                }
            }
        }
    } else {
        state.foreground_start = None;
        state.foreground_alerted = false;
    }
}

/// Check if a PID is still alive (signal 0 probe).
fn is_pid_alive(pid: u32) -> bool {
    kill(Pid::from_raw(pid as i32), Signal::SIGCONT)
        .map(|_| true)
        .unwrap_or(false)
}

/// Spawn `self-clear` immediately (no grace period). Used for the
/// wedged-pane recovery path: when the agent is too stuck to run any tool
/// call (context limit reached, persistent 429), claude-watch must drive
/// `/clear` itself rather than waiting for the agent to cooperate.
///
/// Detached via setsid() so it survives a daemon restart, same as
/// `spawn_deferred_clear`.
fn spawn_immediate_clear(state: &mut State) {
    // Don't double-spawn if a deferred clear child is already running.
    if let Some(pid) = state.context_clear_child_pid {
        if is_pid_alive(pid) {
            debug!(pid, "self-clear child already running, skipping immediate spawn");
            return;
        }
    }

    // SAFETY: setsid() is async-signal-safe and we call it before exec.
    match unsafe {
        std::process::Command::new("self-clear")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .pre_exec(|| {
                nix::unistd::setsid()
                    .map(|_| ())
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
            })
            .spawn()
    } {
        Ok(child) => {
            state.context_clear_child_pid = Some(child.id());
            info!(pid = child.id(), "spawned immediate self-clear (wedged recovery)");
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to spawn immediate self-clear");
        }
    }
}

/// Spawn a deferred self-clear child process.
/// The child sleeps for the grace period, then checks if tokens are still high.
/// If so, it runs `self-clear` to force a context clear.
fn spawn_deferred_clear(config: &Config, state: &mut State) {
    // If there's already a living child, skip
    if let Some(pid) = state.context_clear_child_pid {
        if is_pid_alive(pid) {
            debug!(pid, "deferred self-clear child already running");
            return;
        }
    }

    let grace = config.context_monitor.grace_period;
    // The child: sleep for grace period, polling every 10s.
    // If tokens drop below 30000 (Claude cleared on its own), exit cleanly.
    // If grace expires with tokens still high, run self-clear.
    let script = format!(
        r#"elapsed=0; while [ "$elapsed" -lt {grace} ]; do sleep 10; elapsed=$((elapsed + 10)); tokens=$(claude-watch status --tokens 2>/dev/null); if [ "$tokens" != "?" ] && [ "$tokens" -lt 30000 ] 2>/dev/null; then exit 0; fi; done; self-clear"#,
        grace = grace
    );

    // SAFETY: setsid() is async-signal-safe and we call it before exec.
    // This detaches the child into its own session so it survives
    // systemd's cgroup-wide SIGTERM when claude-watch restarts.
    match unsafe {
        std::process::Command::new("bash")
            .args(["-c", &script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .pre_exec(|| {
                nix::unistd::setsid()
                    .map(|_| ())
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
            })
            .spawn()
    } {
        Ok(child) => {
            state.context_clear_child_pid = Some(child.id());
            info!(pid = child.id(), grace, "spawned deferred self-clear child");
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to spawn deferred self-clear");
        }
    }
}

/// Inject a context warning message into the Claude Code pane.
async fn inject_context_warning(pane: &str, pct: f64, compact_remaining: Option<u32>, grace: u64) {
    let context_info = if let Some(cr) = compact_remaining {
        format!("{}% compact remaining", cr)
    } else {
        format!("{:.0}% token usage", pct)
    };

    let msg = format!(
        "[CLAUDE-WATCH] CONTEXT CRITICALLY LOW ({}). \
        You MUST act IMMEDIATELY: (1) session-task set '<state>', \
        (2) commit/push repos, (3) self-clear. \
        Forced clear in {}s if you don't act.",
        context_info, grace
    );
    tmux::interrupt_and_wait(pane, 30).await;
    tmux::inject_text(pane, &msg).await;
}

/// Determine if context threshold is exceeded.
/// Returns Some((pct, triggered_by_compact)) if triggered, None otherwise.
pub(crate) fn check_context_threshold_with_margin(
    tokens: u64,
    max_context_tokens: u64,
    compact_remaining: Option<u32>,
    threshold_percent: u64,
    compact_trigger_percent: u32,
    threshold_margin: Option<u64>,
) -> Option<(f64, bool)> {
    let pct = (tokens as f64 / max_context_tokens as f64) * 100.0;

    if let Some(cr) = compact_remaining {
        if cr <= compact_trigger_percent {
            return Some((pct, true));
        }
    } else if let Some(margin) = threshold_margin {
        // Fixed margin: trigger when tokens >= max - margin
        if max_context_tokens > margin && tokens >= max_context_tokens - margin {
            return Some((pct, false));
        }
    } else if pct >= threshold_percent as f64 {
        return Some((pct, false));
    }

    None
}

/// Check if an auto-update should be triggered, and if so, spawn the update task.
/// This is called from check_cycle() on each iteration.
/// Check if Claude Code needs API reauth and send high-priority alert.
///
/// Two-phase flow:
/// 1. **401 detected** (TUI visible, error JSON in pane) — inject `/login`, no alert yet.
/// 2. **Login screen visible** (OAuth URL present) — send high-priority alert with URL.
///
/// Alerts are rate-limited to once per `alert_interval_seconds` (default 3 hours).
async fn check_reauth(config: &Config, state: &mut State, pane: &str) {
    let reauth_result = tmux::needs_reauth(pane).await;

    if let Some(login_url) = reauth_result {
        if !state.reauth_detected {
            info!("reauth needed: first detection");
            state.reauth_detected = true;
        }

        // Inject /login once per reauth cycle so the login screen appears
        if !state.login_injected {
            info!("injecting /login command into pane");
            tmux::inject_text(pane, "/login").await;
            state.login_injected = true;
            state.reauth_inject_interrupts_total = state
                .reauth_inject_interrupts_total
                .saturating_add(1);
            write_jsonl_log(
                &config.general.log_file,
                "login_injected",
                serde_json::json!({ "pane": pane }),
            );
            write_legacy_log(
                &config.general.legacy_log_file,
                "Reauth: injected /login command",
            );
            crate::state::save_state(&config.general.state_file, state);
        }

        // Only send the high-priority alert once we have the OAuth URL.
        // Phase 1 (401 error) has no URL — we just inject /login and wait.
        // Phase 2 (login screen) has the URL — send the alert so Andrew can
        // open it on his phone and SSH in to paste the auth code.
        if !login_url.is_empty() {
            // Check alert cooldown
            let should_alert = match &state.last_reauth_alert {
                Some(last) => {
                    if let Some(elapsed) = elapsed_since(last) {
                        elapsed >= config.reauth.alert_interval_seconds as f64
                    } else {
                        true
                    }
                }
                None => true,
            };

            if should_alert {
                let now = Local::now().to_rfc3339();
                warn!("sending high-priority reauth alert with URL");
                let alert_msg = format!("Claude Code login needed. URL: {}", login_url);
                alert::send_pingme_with_priority(&alert_msg, "high").await;
                write_jsonl_log(
                    &config.general.log_file,
                    "reauth_alert",
                    serde_json::json!({ "pane": pane, "url": login_url }),
                );
                write_legacy_log(
                    &config.general.legacy_log_file,
                    "Reauth needed: sent high-priority alert with URL",
                );
                state.last_reauth_alert = Some(now);
                crate::state::save_state(&config.general.state_file, state);
            } else {
                debug!("reauth still needed, alert cooldown active");
            }
        } else {
            debug!("reauth detected (401) but no URL yet — waiting for login screen");
        }
    } else if state.reauth_detected {
        // Reauth resolved
        info!("reauth resolved");
        write_jsonl_log(
            &config.general.log_file,
            "reauth_resolved",
            serde_json::json!({}),
        );
        write_legacy_log(&config.general.legacy_log_file, "Reauth resolved");
        state.reauth_detected = false;
        state.last_reauth_alert = None;
        state.login_injected = false;
        crate::state::save_state(&config.general.state_file, state);
    }
}

/// Check for a manual update trigger file written by `claude-watch update`.
/// If found, force-run the auto-update regardless of schedule.
pub async fn check_update_trigger(config: &Config, state: &mut State, pane: &str) {
    const TRIGGER_FILE: &str = "/tmp/claude-watch-update-trigger";

    let content = match std::fs::read_to_string(TRIGGER_FILE) {
        Ok(c) => c,
        Err(_) => return, // No trigger file
    };

    // Remove the trigger file immediately to avoid re-triggering
    let _ = std::fs::remove_file(TRIGGER_FILE);

    let force = content.trim() == "force";
    info!(force, "manual update trigger detected");
    write_jsonl_log(
        &config.general.log_file,
        "manual_update_trigger",
        serde_json::json!({ "force": force }),
    );

    if pane.is_empty() {
        warn!("manual update trigger found but no pane detected");
        return;
    }

    // Check version mismatch (or force)
    let version_info = tokio::task::spawn_blocking(crate::status::get_version_info)
        .await
        .unwrap_or_default();

    let running = match version_info.running {
        Some(v) => v,
        None => {
            warn!("manual update trigger: cannot determine running version");
            return;
        }
    };
    let installed = match version_info.installed {
        Some(v) => v,
        None => {
            warn!("manual update trigger: cannot determine installed version");
            return;
        }
    };

    if running == installed && !force {
        info!(running = %running, "manual update trigger: already up to date");
        return;
    }

    info!(
        running = %running,
        installed = %installed,
        force,
        "manual update trigger — starting update"
    );

    write_jsonl_log(
        &config.general.log_file,
        "manual_update_start",
        serde_json::json!({
            "running": running,
            "installed": installed,
            "force": force,
        }),
    );

    state.last_update_attempt = Some(chrono::Local::now().to_rfc3339());
    state.update_in_progress = true;
    state.auto_update_count += 1;
    state.auto_update_interrupts_total =
        state.auto_update_interrupts_total.saturating_add(1);
    crate::state::save_state(&config.general.state_file, state);

    let pane = pane.to_string();
    let config = config.clone();
    let state_file = config.general.state_file.clone();
    tokio::spawn(async move {
        run_auto_update(&pane, &running, &installed, &config).await;
        let mut st = crate::state::load_state(&state_file);
        st.update_in_progress = false;
        crate::state::save_state(&state_file, &st);
    });
}

pub async fn check_auto_update(config: &Config, state: &mut State, pane: &str) {
    if !config.auto_update.enabled || pane.is_empty() {
        return;
    }

    // Don't run if an update is already in progress (with 1-hour staleness timeout)
    if state.update_in_progress {
        if let Some(ref last_attempt) = state.last_update_attempt {
            if let Some(elapsed) = elapsed_since(last_attempt) {
                if elapsed > 3600.0 {
                    warn!(
                        "auto-update: update_in_progress stuck for {:.0}s, clearing",
                        elapsed
                    );
                    state.update_in_progress = false;
                    crate::state::save_state(&config.general.state_file, state);
                } else {
                    debug!(
                        "auto-update already in progress ({:.0}s ago), skipping",
                        elapsed
                    );
                    return;
                }
            } else {
                debug!("auto-update already in progress, skipping");
                return;
            }
        } else {
            // No last_attempt but update_in_progress is true — stale, clear it
            warn!("auto-update: update_in_progress with no last_attempt, clearing");
            state.update_in_progress = false;
            crate::state::save_state(&config.general.state_file, state);
        }
    }

    let now = Local::now();

    // Check if we're at the configured minute of the hour
    let current_minute = now.minute();
    if current_minute != config.auto_update.check_minute {
        return;
    }

    // Check cooldown since last attempt
    if let Some(ref last_attempt) = state.last_update_attempt {
        if let Some(elapsed) = elapsed_since(last_attempt) {
            let cooldown_secs = config.auto_update.cooldown_hours * 3600;
            if elapsed < cooldown_secs as f64 {
                return;
            }
        }
    }

    // Check version mismatch
    let version_info = tokio::task::spawn_blocking(crate::status::get_version_info)
        .await
        .unwrap_or_default();

    let running = match version_info.running {
        Some(v) => v,
        None => return,
    };
    let installed = match version_info.installed {
        Some(v) => v,
        None => return,
    };

    if running == installed {
        state.last_update_check = Some(now.to_rfc3339());
        debug!(running = %running, installed = %installed, "versions match, no update needed");
        // Claude Code picked up the new binary (either via /restart after the
        // hook reminder or via the previous fallback). Record the latency.
        record_reminder_latency_if_recent(ReminderType::VersionUpdate, state, false);
        return;
    }

    // Hybrid gate: if the version_update hook fired recently, give Claude
    // a grace window to `/restart` on its own before falling back to the
    // heavy-handed `claude update` injection.
    if config.hybrid.enabled
        && should_defer_to_hook(
            ReminderType::VersionUpdate,
            config.hybrid.version_fallback_secs as f64,
        )
    {
        debug!(
            running = %running,
            installed = %installed,
            grace = config.hybrid.version_fallback_secs,
            "version mismatch detected but deferring to recent hook reminder"
        );
        write_jsonl_log(
            &config.general.log_file,
            "auto_update_hook_deferred",
            serde_json::json!({
                "running": running,
                "installed": installed,
                "grace_secs": config.hybrid.version_fallback_secs,
            }),
        );
        state.last_update_check = Some(now.to_rfc3339());
        return;
    }

    info!(
        running = %running,
        installed = %installed,
        "version mismatch detected — starting auto-update (hybrid fallback)"
    );

    write_jsonl_log(
        &config.general.log_file,
        "auto_update_start",
        serde_json::json!({
            "running": running,
            "installed": installed,
            "hybrid_fallback": true,
        }),
    );

    state.last_update_attempt = Some(now.to_rfc3339());
    state.last_update_check = Some(now.to_rfc3339());
    state.update_in_progress = true;
    state.auto_update_count += 1;
    state.fallback_update_count = state.fallback_update_count.saturating_add(1);
    state.auto_update_interrupts_total =
        state.auto_update_interrupts_total.saturating_add(1);
    crate::state::save_state(&config.general.state_file, state);

    // Spawn the long-running update sequence as a background task
    let pane = pane.to_string();
    let config = config.clone();
    let state_file = config.general.state_file.clone();
    tokio::spawn(async move {
        run_auto_update(&pane, &running, &installed, &config).await;
        // Clear update_in_progress in state file
        let mut st = crate::state::load_state(&state_file);
        st.update_in_progress = false;
        crate::state::save_state(&state_file, &st);
    });
}

/// Execute the auto-update sequence: interrupt → /exit → wait → relaunch → resume.
async fn run_auto_update(pane: &str, old_version: &str, new_version: &str, config: &Config) {
    info!("auto-update: interrupting Claude Code...");
    write_jsonl_log(
        &config.general.log_file,
        "auto_update_interrupt",
        serde_json::json!({}),
    );

    // Step 1: Interrupt and wait for idle
    if tmux::interrupt_and_wait(pane, 30).await {
        info!("auto-update: Claude Code is idle");
    } else {
        warn!("auto-update: could not confirm idle after 30s, proceeding anyway");
    }

    // Settle time after interruption
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Step 2: Inject /exit
    info!("auto-update: injecting /exit...");
    tmux::inject_text(pane, "/exit").await;

    // Step 3: Wait for Claude to exit
    info!("auto-update: waiting for Claude Code to exit...");
    if !tmux::wait_for_exit(pane, 45).await {
        warn!("auto-update: Claude Code did not exit within 45s, aborting");
        write_jsonl_log(
            &config.general.log_file,
            "auto_update_failed",
            serde_json::json!({"reason": "exit_timeout"}),
        );
        alert::send_pingme("claude-watch: auto-update FAILED — Claude Code did not exit").await;
        return;
    }
    info!("auto-update: Claude Code exited");

    // Brief delay for shell prompt to fully render
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Step 4: Capture session ID from pane content
    let mut session_id: Option<String> = None;
    if let Some(out) = tmux::capture_pane_history(pane, 100).await {
        let re = regex_lite::Regex::new(r"--resume\s+([0-9a-f-]{36})").unwrap();
        if let Some(caps) = re.captures(&out) {
            session_id = Some(caps[1].to_string());
        }
    }

    if let Some(ref sid) = session_id {
        info!(session_id = %sid, "auto-update: captured session ID");
    } else {
        info!("auto-update: no session ID found, will use --continue");
    }

    // Step 5: Write relaunch script
    // NOTE: Do NOT use --append-system-prompt here. It persists for the lifetime of the
    // process (survives /clear), causing misleading "version update" messages on subsequent
    // context clears. The resume prompt (step 9) handles session startup instead.
    let launch = if let Some(ref sid) = session_id {
        format!("claude --resume {}", sid)
    } else {
        "claude --continue".to_string()
    };

    let script_content = format!(
        "#!/bin/bash\ncd $HOME\n{}\necho \"\\n[claude-watch-update] Claude exited with code $?\"\n",
        launch
    );
    if let Err(e) = std::fs::write(&config.claude.relaunch_script, &script_content) {
        tracing::error!(error = %e, "auto-update: failed to write relaunch script");
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            &config.claude.relaunch_script,
            std::fs::Permissions::from_mode(0o755),
        );
    }

    // Step 6: Inject relaunch command into shell
    info!("auto-update: injecting relaunch command...");
    tmux::inject_shell(pane, &format!("bash {}", config.claude.relaunch_script)).await;

    // Step 7: Wait for claude binary to appear in process tree
    info!("auto-update: waiting for Claude binary to start...");
    if !tmux::wait_for_claude_binary(pane, 120).await {
        warn!("auto-update: claude binary not detected after 120s");
        write_jsonl_log(
            &config.general.log_file,
            "auto_update_warning",
            serde_json::json!({"reason": "binary_not_found"}),
        );
    }

    // Step 8: Wait for ❯ prompt (Claude Code is ready for input)
    info!("auto-update: waiting for idle prompt...");
    if !tmux::wait_for_idle_prompt(pane, 90).await {
        warn!("auto-update: prompt not found after 90s, trying inject anyway");
    }

    // Brief settle after prompt appears
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Step 9: Inject resume text
    info!("auto-update: injecting resume prompt...");
    tmux::inject_text(pane, &config.auto_update.resume_prompt).await;

    // Step 10: Log and notify
    write_jsonl_log(
        &config.general.log_file,
        "auto_update_complete",
        serde_json::json!({
            "old_version": old_version,
            "new_version": new_version,
            "session_id": session_id,
        }),
    );

    let msg = format!(
        "claude-watch: auto-update complete ({} → {})",
        old_version, new_version
    );
    alert::send_pingme(&msg).await;
    info!("auto-update: complete ({} → {})", old_version, new_version);
}

/// Pure function: decide whether a self-heal retry should reset the dead-check
/// counter. Returns true if `dead_checks` has reached the configured threshold
/// AND the retry observed a non-zero status (tokens or bashes).
///
/// Split out so the decision logic can be unit-tested without mocking tmux.
pub(crate) fn should_self_heal(
    dead_checks: u32,
    checks_required: u32,
    retry_tokens: u64,
    retry_bashes: u64,
) -> bool {
    dead_checks >= checks_required && (retry_tokens > 0 || retry_bashes > 0)
}

/// Run a single check cycle.
pub async fn check_cycle(config: &Config, state: &mut State) {
    let now = Local::now().to_rfc3339();

    // Get Claude Code status
    let cs = status::get_claude_status().await;

    if cs.is_none() {
        debug!("claude-status returned None -- not running");
        write_legacy_log(
            &config.general.legacy_log_file,
            "claude-status returned None -- not running",
        );
        // Claude Code not running at all — if a new session starts later,
        // it should be eligible for fresh inject regardless of old state.
        if state.fresh_session_injected {
            // Only reset if Claude was alive at some point since the inject,
            // or if the inject is expired (>5min without activity).
            let inject_expired = state
                .last_fresh_inject
                .as_ref()
                .and_then(|ts| elapsed_since(ts))
                .is_some_and(|elapsed| elapsed >= 300.0);

            if state.was_alive_since_inject || inject_expired {
                debug!("resetting fresh_session_injected — no Claude Code running (was_alive={}, expired={})",
                    state.was_alive_since_inject, inject_expired);
                state.fresh_session_injected = false;
                state.was_alive_since_inject = false;
            } else {
                debug!("fresh_session_injected set but Claude never became active — not resetting");
            }
        }
        state.last_check = Some(now);
        state.consecutive_failures = 0;
        crate::state::save_state(&config.general.state_file, state);
        return;
    }

    let cs = cs.unwrap();
    let pane = &cs.pane;
    let tokens = cs.tokens;
    let bashes = cs.bashes;
    let watchmen_count = status::check_watchmen_count().await;

    // --- Activity detection (Phase 1: logging only) ---
    if !pane.is_empty() {
        let activity = tmux::get_activity(pane).await;
        debug!(activity = %activity, "claude activity state");
    }

    // --- Post-restart resume injection ---
    if state.pending_resume_inject && !pane.is_empty() && tokens > 0 {
        // Don't inject during /exit teardown
        if tmux::is_exit_teardown(pane).await {
            debug!("post-restart: skipping — exit teardown detected");
            state.last_check = Some(now);
            crate::state::save_state(&config.general.state_file, state);
            return;
        }
        if tmux::is_idle(pane).await {
            info!("post-restart: injecting resume prompt");
            tmux::inject_text(
                pane,
                "[CLAUDE-WATCH-RESUME] Claude Code was restarted after a crash. \
                 All background task handles were lost. Run the full resume \
                 checklist at your configured resume-checklist path immediately.",
            )
            .await;
            state.pending_resume_inject = false;
            state.post_restart_resume_inject_interrupts_total = state
                .post_restart_resume_inject_interrupts_total
                .saturating_add(1);
            state.last_check = Some(now);
            crate::state::save_state(&config.general.state_file, state);
            return;
        }
        debug!(tokens, "post-restart: Claude running but not idle yet");
        state.last_check = Some(now);
        crate::state::save_state(&config.general.state_file, state);
        return;
    }

    // --- Find pane when claude-status can't (process crashed) ---
    let effective_pane: String = if pane.is_empty() && tokens == 0 && bashes == 0 {
        if let Some(p) = tmux::find_dashboard_pane(&config.tmux).await {
            debug!(pane = %p, "found dashboard pane via fallback");
            p
        } else {
            String::new()
        }
    } else {
        pane.clone()
    };

    // Detect pane change (new Claude Code session, e.g. dashboard --recreate).
    // Reset fresh_session_injected so the new session can get its resume inject,
    // and reset dead_checks so the countdown restarts for the new session.
    if !effective_pane.is_empty()
        && !state.last_known_pane.is_empty()
        && effective_pane != state.last_known_pane
    {
        info!(
            old_pane = %state.last_known_pane,
            new_pane = %effective_pane,
            "pane change detected — resetting fresh_session_injected"
        );
        state.fresh_session_injected = false;
        state.was_alive_since_inject = false;
        state.consecutive_dead_checks = 0;
    }

    // Store last known values for foreground polling between full check cycles.
    // Only update tokens/bashes when we got a valid parse (non-zero) to avoid
    // writing 0 to Prometheus during transient status bar parsing failures.
    state.last_known_pane = effective_pane.clone();
    if tokens > 0 {
        state.last_known_tokens = tokens;
    }
    if bashes > 0 || tokens > 0 {
        state.last_known_bashes = bashes;
    }

    // --- Dead process detection ---
    if tokens == 0 && bashes == 0 && !effective_pane.is_empty() {
        state.consecutive_dead_checks += 1;
        let dead_checks = state.consecutive_dead_checks;
        info!(dead_checks, "dead process detected: tokens=0, bashes=0");

        // --- Self-heal: once we reach the alert threshold, retry status
        // discovery from scratch before committing to any dead-check actions.
        // Addresses a stale-latch bug where the daemon read tokens=0 for 45+
        // minutes across 250+ loops while the same binary's CLI
        // (`claude-watch status --json`) parsed the same pane correctly.
        // A fresh get_claude_status() call re-runs pane discovery and
        // capture, which recovers from the stuck state.
        if dead_checks >= config.dead_process.checks_required {
            if let Some(retry) = status::get_claude_status().await {
                if should_self_heal(
                    dead_checks,
                    config.dead_process.checks_required,
                    retry.tokens,
                    retry.bashes,
                ) {
                    warn!(
                        recovered_tokens = retry.tokens,
                        recovered_bashes = retry.bashes,
                        pane = %retry.pane,
                        prior_dead_checks = dead_checks,
                        "self-heal triggered: retry returned non-zero status, \
                         resetting consecutive_dead_checks"
                    );
                    write_jsonl_log(
                        &config.general.log_file,
                        "self_heal_retry",
                        serde_json::json!({
                            "recovered_tokens": retry.tokens,
                            "recovered_bashes": retry.bashes,
                            "pane": &retry.pane,
                            "prior_dead_checks": dead_checks,
                        }),
                    );
                    state.consecutive_dead_checks = 0;
                    state.last_known_pane = retry.pane.clone();
                    if retry.tokens > 0 {
                        state.last_known_tokens = retry.tokens;
                    }
                    if retry.bashes > 0 || retry.tokens > 0 {
                        state.last_known_bashes = retry.bashes;
                    }
                    // Mirror the active-session bookkeeping from the
                    // non-dead branch below so inject state stays coherent.
                    if state.fresh_session_injected {
                        state.was_alive_since_inject = true;
                        state.fresh_session_injected = false;
                    }
                    state.last_check = Some(now);
                    crate::state::save_state(&config.general.state_file, state);
                    return;
                }
            }
        }

        write_legacy_log(
            &config.general.legacy_log_file,
            &format!(
                "Dead process detected: tokens=0, bashes=0, dead_checks={}",
                dead_checks
            ),
        );

        if dead_checks >= config.dead_process.checks_required {
            // Reset fresh_session_injected when Claude was alive and then died.
            // This handles both cases: (1) shell prompt visible after old session died,
            // and (2) rapid session replacement where the pane ID doesn't change
            // (dashboard --recreate always creates dashboard:0.0). Without this,
            // the flag stays true from a previous inject and blocks the next one.
            //
            // IMPORTANT: Only reset if was_alive_since_inject is true, meaning Claude
            // actually reached an active state (tokens > 0) after the last inject.
            // Without this guard, we get an inject loop: inject → startup (tokens=0,
            // looks "dead") → reset flag → re-inject → repeat.
            //
            // Fallback: if the inject was >5 minutes ago and Claude never became active,
            // reset anyway — the session likely died during startup and a new one may
            // need injection.
            if state.fresh_session_injected {
                let inject_expired = state
                    .last_fresh_inject
                    .as_ref()
                    .and_then(|ts| elapsed_since(ts))
                    .is_some_and(|elapsed| elapsed >= 300.0);

                if state.was_alive_since_inject {
                    info!("dead state reached after active session — resetting fresh_session_injected");
                    state.fresh_session_injected = false;
                    state.was_alive_since_inject = false;
                } else if inject_expired {
                    info!("dead state reached — inject expired (>5min, never active) — resetting fresh_session_injected");
                    state.fresh_session_injected = false;
                    state.was_alive_since_inject = false;
                } else {
                    debug!("dead state but inject recent and Claude never active — not resetting (preventing inject loop)");
                }
            }

            // Check restart cooldown
            if let Some(ref last) = state.last_restart {
                if let Some(elapsed) = elapsed_since(last) {
                    if elapsed < config.dead_process.restart_cooldown as f64 {
                        info!(
                            elapsed_secs = elapsed,
                            cooldown = config.dead_process.restart_cooldown,
                            "restart cooldown active"
                        );
                        state.last_check = Some(now);
                        crate::state::save_state(&config.general.state_file, state);
                        return;
                    }
                }
            }

            if tmux::is_shell_prompt(&effective_pane).await {
                info!(
                    dead_checks,
                    "shell prompt confirmed -- restarting Claude Code"
                );
                restart_claude(&effective_pane, state, &config.claude).await;
                state.consecutive_dead_checks = 0;
                state.consecutive_failures = 0;
                state.alert_count = 0;
            } else if dead_checks >= config.dead_process.fresh_inject_checks
                && !state.fresh_session_injected
                && tmux::is_idle(&effective_pane).await
            {
                // Claude Code is running (idle prompt visible) but tokens=0 — this is
                // a fresh session launched externally (e.g. dashboard --fresh), not by
                // claude-watch. Inject "resume" to kick-start the checklist.
                info!(
                    dead_checks,
                    "fresh external session detected — injecting resume"
                );
                tmux::inject_text(&effective_pane, "resume").await;
                state.fresh_session_injected = true;
                state.was_alive_since_inject = false;
                state.last_fresh_inject = Some(Local::now().to_rfc3339());
                state.consecutive_dead_checks = 0;
                state.fresh_session_inject_interrupts_total = state
                    .fresh_session_inject_interrupts_total
                    .saturating_add(1);
                write_jsonl_log(
                    &config.general.log_file,
                    "fresh_session_inject",
                    serde_json::json!({
                        "dead_checks": dead_checks,
                        "pane": &effective_pane,
                    }),
                );
            } else {
                debug!("dead but no shell prompt -- Claude may be starting up");
            }
        }

        state.last_check = Some(now);
        crate::state::save_state(&config.general.state_file, state);
        return;
    }
    state.consecutive_dead_checks = 0;
    // Session is active (tokens > 0). Mark that Claude was alive since inject,
    // then clear the inject flag. The was_alive_since_inject flag allows the dead
    // state handler to distinguish "was alive, then died" from "never started up".
    if state.fresh_session_injected {
        state.was_alive_since_inject = true;
        state.fresh_session_injected = false;
    }

    // --- Check for manual update trigger ---
    check_update_trigger(config, state, &effective_pane).await;

    // --- Auto-update check ---
    check_auto_update(config, state, &effective_pane).await;

    // --- Reauth detection ---
    if config.reauth.enabled && !effective_pane.is_empty() {
        check_reauth(config, state, &effective_pane).await;
    }

    // --- Fresh /clear detection ---
    if tokens >= config.fresh_clear.min_tokens
        && tokens < config.fresh_clear.max_tokens
        && bashes == 0
    {
        // Skip if /exit teardown is in progress — "Goodbye!" or
        // "Background command was stopped" visible in pane output.
        // Injecting resume during teardown is useless and confusing.
        if !effective_pane.is_empty() && tmux::is_exit_teardown(&effective_pane).await {
            debug!("fresh /clear check: skipping — exit teardown detected");
            state.consecutive_fast_detections = 0;
            state.last_check = Some(now);
            crate::state::save_state(&config.general.state_file, state);
            return;
        }

        if !effective_pane.is_empty() && tmux::is_idle(&effective_pane).await {
            state.consecutive_fast_detections += 1;
            if state.consecutive_fast_detections < config.fresh_clear.detections_required {
                state.last_check = Some(now);
                crate::state::save_state(&config.general.state_file, state);
                return;
            }

            // Check cooldown
            if let Some(ref last) = state.last_fast_path_alert {
                if let Some(elapsed) = elapsed_since(last) {
                    if elapsed < config.fresh_clear.cooldown as f64 {
                        state.last_check = Some(now);
                        crate::state::save_state(&config.general.state_file, state);
                        return;
                    }
                }
            }

            info!(tokens, "fresh /clear detected -- injecting resume");
            alert::send_pingme(&format!(
                "Fresh /clear detected (tokens={}, bashes=0). Injecting resume.",
                tokens
            ))
            .await;

            // Dismiss feedback prompt if present
            tmux::dismiss_feedback_prompt(&effective_pane).await;

            tmux::inject_text(&effective_pane, &config.alerts.resume_prompt).await;

            state.last_fast_path_alert = Some(now.clone());
            state.last_alert = Some(now.clone());
            state.consecutive_failures = 0;
            state.consecutive_fast_detections = 0;
            state.fresh_clear_resume_inject_interrupts_total = state
                .fresh_clear_resume_inject_interrupts_total
                .saturating_add(1);
            state.last_check = Some(now);
            crate::state::save_state(&config.general.state_file, state);
            return;
        }
    } else {
        state.consecutive_fast_detections = 0;
    }

    // --- Heartbeat stale detection ---
    let mut stuck = false;
    let mut stuck_reason = String::new();

    match std::fs::metadata(&config.claude.heartbeat_file) {
        Ok(meta) => {
            if let Ok(modified) = meta.modified() {
                let age = SystemTime::now()
                    .duration_since(modified)
                    .unwrap_or_default()
                    .as_secs();
                let stale_secs = config.heartbeat.stale_minutes * 60;
                if age >= stale_secs {
                    stuck = true;
                    stuck_reason = format!(
                        "heartbeat stale ({}min, threshold={}min, watchmen={})",
                        age / 60,
                        config.heartbeat.stale_minutes,
                        watchmen_count
                    );
                    state.heartbeat_stale_count += 1;
                }
            }
        }
        Err(_) => {
            // No heartbeat file -- give it time
        }
    }

    // --- Foreground blocking detection ---
    // Delegated to check_foreground() which runs on its own timer in the main loop.
    // Also run it here during full check cycles to ensure it runs at least as often
    // as the general interval.
    check_foreground(config, state, &effective_pane, tokens, bashes).await;

    // --- Context monitoring ---
    if config.context_monitor.enabled && tokens > 0 {
        if let Some((pct, _by_compact)) = check_context_threshold_with_margin(
            tokens,
            config.claude.max_context_tokens,
            cs.compact_remaining,
            config.context_monitor.threshold_percent,
            config.context_monitor.compact_trigger_percent,
            config.context_monitor.threshold_margin,
        ) {
            if !state.context_clear_triggered {
                // Check cooldown
                let can_trigger = match &state.last_context_clear {
                    Some(last) => elapsed_since(last)
                        .map(|e| e >= config.context_monitor.cooldown as f64)
                        .unwrap_or(true),
                    None => true,
                };

                if can_trigger {
                    // Hybrid gate: if a recent context_high hook fired the
                    // reminder, give Claude a grace window to self-act
                    // before we tmux-inject a warning + schedule the
                    // deferred clear.
                    let hook_deferred = config.hybrid.enabled
                        && should_defer_to_hook(
                            ReminderType::ContextHigh,
                            config.hybrid.context_fallback_secs as f64,
                        );

                    // Global post-interrupt cooldown: if a recent interrupt
                    // (thinking, watcher-down, or a prior context-warning)
                    // fired, defer this context warning too. The deferred
                    // self-clear child still runs if the token level stays
                    // high; the cooldown only gates the tmux interrupt +
                    // warning message.
                    let global_cooldown_blocks = interrupt_in_global_cooldown(
                        state,
                        config.general.post_interrupt_cooldown_secs,
                    );

                    if hook_deferred {
                        debug!(
                            tokens,
                            pct,
                            grace = config.hybrid.context_fallback_secs,
                            "context threshold exceeded but deferring to recent hook reminder"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "context_threshold_hook_deferred",
                            serde_json::json!({
                                "tokens": tokens,
                                "pct": pct,
                                "grace_secs": config.hybrid.context_fallback_secs,
                            }),
                        );
                    } else if global_cooldown_blocks {
                        debug!(
                            tokens,
                            pct,
                            cooldown = config.general.post_interrupt_cooldown_secs,
                            "context threshold exceeded but global post-interrupt cooldown active"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "context_threshold_global_cooldown_deferred",
                            serde_json::json!({
                                "tokens": tokens,
                                "pct": pct,
                                "cooldown_secs": config.general.post_interrupt_cooldown_secs,
                            }),
                        );
                    } else {
                        warn!(
                            tokens,
                            pct,
                            compact_remaining = ?cs.compact_remaining,
                            "context threshold exceeded — triggering deferred clear (hybrid fallback)"
                        );
                        write_jsonl_log(
                            &config.general.log_file,
                            "context_threshold",
                            serde_json::json!({
                                "tokens": tokens,
                                "pct": pct,
                                "compact_remaining": cs.compact_remaining,
                                "grace_period": config.context_monitor.grace_period,
                                "hybrid_fallback": true,
                            }),
                        );

                        // Run session-event compact-prep
                        let note = format!("auto-clear at {:.0}% tokens", pct);
                        let _ = crate::cmd::run_cmd(
                            &["session-event", "compact-prep", "--note", &note],
                            10,
                        )
                        .await;

                        // Spawn deferred self-clear child
                        spawn_deferred_clear(config, state);

                        // Inject warning message into Claude Code pane
                        if !effective_pane.is_empty() {
                            inject_context_warning(
                                &effective_pane,
                                pct,
                                cs.compact_remaining,
                                config.context_monitor.grace_period,
                            )
                            .await;
                        }

                        state.context_clear_triggered = true;
                        state.last_context_clear = Some(now.clone());
                        state.last_interrupt_at = Some(now.clone());
                        state.fallback_clear_count = state.fallback_clear_count.saturating_add(1);
                        state.context_warning_interrupts_total = state
                            .context_warning_interrupts_total
                            .saturating_add(1);
                    }
                }
            }
        }

        // Reset trigger flag when tokens drop (clear happened)
        if state.context_clear_triggered && tokens < 30000 {
            info!(tokens, "context clear detected — resetting trigger");
            write_jsonl_log(
                &config.general.log_file,
                "context_clear_reset",
                serde_json::json!({
                    "tokens": tokens,
                }),
            );
            record_reminder_latency_if_recent(ReminderType::ContextHigh, state, true);
            state.context_clear_triggered = false;
            state.context_clear_child_pid = None;
            state.last_context_clear = Some(now.clone());
        }

        // Detect external clears (self-clear, user /clear) that claude-watch didn't trigger.
        // If tokens drop below 30K but we didn't trigger the clear, still update the timestamp
        // so the "Since Last Clear" dashboard metric stays accurate.
        if !state.context_clear_triggered && tokens < 30000 {
            // Only log if we previously saw high tokens (avoid re-logging on every check
            // while tokens are still low during boot)
            if state.last_seen_tokens.unwrap_or(0) >= 30000 {
                info!(tokens, prev_tokens = state.last_seen_tokens, "external context clear detected");
                write_jsonl_log(
                    &config.general.log_file,
                    "context_clear_reset",
                    serde_json::json!({
                        "tokens": tokens,
                        "external": true,
                    }),
                );
                record_reminder_latency_if_recent(ReminderType::ContextHigh, state, true);
                state.last_context_clear = Some(now.clone());
            }
        }
        state.last_seen_tokens = Some(tokens);
    }

    // --- Wedged-pane detection (context limit / persistent rate limit) ---
    //
    // If the pane shows "Context limit reached. /compact or /clear to continue"
    // or repeated "API Error: Request rejected (429)", the agent is wedged: it
    // cannot make any tool call (every attempt errors out before it runs), so
    // it cannot run the normal compact-prep checklist or `self-clear`. The
    // token-based context_monitor above does NOT cover this — the agent may
    // hit the wall *below* its configured threshold (Anthropic API can return
    // context-limit errors before our token counter says "max"), and 429s are
    // entirely independent of token count.
    //
    // Recovery: claude-watch runs `self-clear` itself, the same way the
    // deferred-clear child does after the grace period expires — but
    // immediately, no grace period, no agent dependency.
    //
    // To avoid false positives from chat-history references to the strings,
    // we require N consecutive cycles before firing.
    if config.context_monitor.wedged_detection_enabled && !effective_pane.is_empty() {
        let wedged = tmux::detect_wedged(&effective_pane).await;

        if let Some(reason) = wedged {
            state.wedged_consecutive += 1;
            debug!(
                reason = %reason,
                consecutive = state.wedged_consecutive,
                threshold = config.context_monitor.wedged_consecutive,
                "wedged pane detected"
            );

            if state.wedged_consecutive >= config.context_monitor.wedged_consecutive {
                // Cooldown gate: don't re-fire within wedged_cooldown seconds.
                let in_cooldown = state
                    .last_wedged_clear
                    .as_deref()
                    .and_then(elapsed_since)
                    .is_some_and(|e| e < config.context_monitor.wedged_cooldown as f64);

                if !in_cooldown {
                    warn!(
                        reason = %reason,
                        consecutive = state.wedged_consecutive,
                        "wedged pane sustained — running self-clear immediately (no agent cooperation possible)"
                    );
                    write_jsonl_log(
                        &config.general.log_file,
                        "wedged_clear",
                        serde_json::json!({
                            "reason": reason.to_string(),
                            "consecutive": state.wedged_consecutive,
                            "tokens": tokens,
                        }),
                    );
                    write_legacy_log(
                        &config.general.legacy_log_file,
                        &format!(
                            "wedged pane ({reason}) — running self-clear (consecutive={})",
                            state.wedged_consecutive,
                        ),
                    );

                    // Run session-event compact-prep so the next session has a
                    // breadcrumb in the session log explaining why context was
                    // dropped. Best-effort — if it fails, still proceed with
                    // self-clear.
                    let note = format!("auto-clear: pane wedged ({reason})");
                    let _ = crate::cmd::run_cmd(
                        &["session-event", "compact-prep", "--note", &note],
                        10,
                    )
                    .await;

                    // Notify Andrew so he knows claude-watch had to step in.
                    let alert_msg = format!(
                        "claude-watch: agent wedged ({reason}) -- running self-clear",
                    );
                    alert::send_pingme(&alert_msg).await;

                    spawn_immediate_clear(state);

                    state.last_wedged_clear = Some(now.clone());
                    state.wedged_clear_count += 1;
                    state.wedged_clear_interrupts_total =
                        state.wedged_clear_interrupts_total.saturating_add(1);
                    state.wedged_consecutive = 0;
                } else {
                    debug!(
                        reason = %reason,
                        "wedged pane detected but cooldown active"
                    );
                }
            }
        } else {
            // Pane is no longer wedged — reset the counter.
            if state.wedged_consecutive > 0 {
                debug!(
                    prev_consecutive = state.wedged_consecutive,
                    "wedged pane cleared — resetting counter"
                );
                state.wedged_consecutive = 0;
            }
        }
    }

    // --- Individual watcher health monitoring ---
    if config.watcher_monitor.enabled {
        let entries = status::parse_watchers_config(&config.watcher_monitor.watchers_config);
        let mut any_critical_missing = false;
        let mut missing_names: Vec<String> = Vec::new();

        for entry in &entries {
            if !entry.enabled {
                continue;
            }
            let count = status::check_process_count(&entry.pattern).await;
            let health = state
                .watcher_health
                .entry(entry.name.clone())
                .or_insert_with(|| WatcherState {
                    last_seen_running: None,
                    consecutive_missing: 0,
                    enabled: entry.enabled,
                });

            if count >= entry.min_count {
                health.last_seen_running = Some(now.clone());
                health.consecutive_missing = 0;
            } else {
                // Grace period: if the watcher was seen running within the
                // last 90 seconds, don't count this as a miss. Short-lived
                // watchers (e.g. signal-wait exits when a message arrives)
                // have a natural gap between exit and the main loop's
                // restart. Without this grace period we fire spurious
                // "watcher missing" alerts every time a message is received.
                let in_grace = health
                    .last_seen_running
                    .as_deref()
                    .and_then(elapsed_since)
                    .is_some_and(|e| e < 90.0);
                if in_grace {
                    continue;
                }
                health.consecutive_missing += 1;
                // Log after 3 consecutive misses (~30s at 10s interval)
                if health.consecutive_missing == 3 {
                    warn!(
                        watcher = %entry.name,
                        pattern = %entry.pattern,
                        consecutive_missing = health.consecutive_missing,
                        "watcher missing"
                    );
                    write_jsonl_log(
                        &config.general.log_file,
                        "watcher_missing",
                        serde_json::json!({
                            "watcher": entry.name,
                            "pattern": entry.pattern,
                            "consecutive_missing": health.consecutive_missing,
                        }),
                    );
                }
                if health.consecutive_missing >= config.watcher_monitor.inject_threshold {
                    any_critical_missing = true;
                    missing_names.push(entry.name.clone());
                }
            }
        }

        // Inject restart commands if watchers are down and cooldown has passed.
        //
        // NOTE (2026-04-28): The watcher-down inject path is intentionally
        // EXEMPT from `interrupt_in_global_cooldown`. A down watcher is a
        // hard liveness failure — we have no signal-wait, no claude-event-
        // watch, no torrent-wait — and silence here means messages /
        // events / completions sit unprocessed for the cooldown window.
        // Prior attempts to gate this on "main loop actively turning"
        // (q-2026-04-27-b8e1) or supervise the auto-restart via systemd
        // (q-2026-04-28-6602) violated the heartbeat-liveness invariant
        // and were reverted. The correct shape is: keep the spawn target
        // in the main-loop tmux pane (watchers must die when the main
        // loop dies), and let the inject re-fire on the per-watcher
        // cooldown regardless of recent unrelated interrupts.
        if any_critical_missing && !effective_pane.is_empty() {
            let should_inject = watcher_inject_due(
                state.last_watcher_inject.as_deref(),
                config.watcher_monitor.inject_cooldown,
            );
            if should_inject {
                let missing_list = missing_names.join(", ");
                warn!(missing = %missing_list, "watchers down — interrupting and injecting restart");
                write_jsonl_log(
                    &config.general.log_file,
                    "watcher_inject",
                    serde_json::json!({
                        "missing": missing_names,
                    }),
                );

                // Interrupt first (like prolonged-thinking) to break any inline work
                if tmux::interrupt_and_wait(&effective_pane, 10).await {
                    info!("watcher inject: Claude Code is idle after interrupt");
                } else {
                    warn!("watcher inject: could not confirm idle, injecting anyway");
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                // Build specific restart commands
                let restart_cmds: Vec<String> = missing_names
                    .iter()
                    .map(|n| format!("watcher-ctl run {}", n))
                    .collect();
                let prompt = format!(
                    "[CLAUDE-WATCH] WATCHER(S) DOWN: {}. You MUST restart them NOW. \
                     Run these as background tasks immediately: {}",
                    missing_list,
                    restart_cmds.join(", ")
                );
                tmux::inject_text(&effective_pane, &prompt).await;
                state.last_watcher_inject = Some(now.clone());
                state.last_interrupt_at = Some(now.clone());
                state.watcher_inject_count += 1;
                state.watcher_down_interrupts_total =
                    state.watcher_down_interrupts_total.saturating_add(1);
                crate::state::save_state(&config.general.state_file, state);
            }
        }
    }

    // --- tmux healthcheck brief ---
    let tmux_brief = tmux::healthcheck_brief().await;

    // --- Log this check ---
    let log_msg = format!(
        "pane={} tokens={} bashes={} watchmen={} stuck={} reason={} failures={} {}",
        effective_pane,
        tokens,
        bashes,
        watchmen_count,
        stuck,
        stuck_reason,
        state.consecutive_failures,
        tmux_brief
    );
    write_legacy_log(&config.general.legacy_log_file, &log_msg);
    write_jsonl_log(
        &config.general.log_file,
        "check",
        serde_json::json!({
            "pane": effective_pane,
            "tokens": tokens,
            "bashes": bashes,
            "watchmen": watchmen_count,
            "stuck": stuck,
            "stuck_reason": stuck_reason,
            "consecutive_failures": state.consecutive_failures,
            "tmux_health": tmux_brief,
        }),
    );

    // --- Stuck handling with exponential backoff ---
    if stuck {
        state.consecutive_failures += 1;
        state.last_failure = Some(now.clone());
        state.last_failure_detail = Some(FailureDetail {
            bashes,
            watchmen: watchmen_count,
            stuck_reason: stuck_reason.clone(),
        });

        // Alert after 2 consecutive failures
        if state.consecutive_failures >= 2 {
            let alert_count = state.alert_count;

            // Exponential backoff via escalation tiers
            let cooldown = if (alert_count as usize) < config.alerts.escalation_tiers.len() {
                config.alerts.escalation_tiers[alert_count as usize]
            } else {
                *config.alerts.escalation_tiers.last().unwrap_or(&3600)
            };

            // Cooldown check
            if let Some(ref last) = state.last_alert {
                if let Some(elapsed) = elapsed_since(last) {
                    if elapsed < cooldown as f64 {
                        debug!(
                            elapsed_secs = elapsed,
                            cooldown_secs = cooldown,
                            alert_count,
                            "alert cooldown active"
                        );
                        crate::state::save_state(&config.general.state_file, state);
                        return;
                    }
                }
            }

            state.alert_count += 1;
            let use_pingme = state.alert_count <= config.alerts.max_pingme_alerts;

            info!(
                stuck_reason = %stuck_reason,
                failures = state.consecutive_failures,
                alert_number = state.alert_count,
                use_pingme,
                "ALERTING"
            );
            write_jsonl_log(
                &config.general.log_file,
                "alert",
                serde_json::json!({
                    "stuck_reason": stuck_reason,
                    "failures": state.consecutive_failures,
                    "alert_number": state.alert_count,
                    "use_pingme": use_pingme,
                }),
            );

            let alert_pane = if !effective_pane.is_empty() {
                effective_pane.clone()
            } else {
                tmux::find_dashboard_pane(&config.tmux)
                    .await
                    .unwrap_or_default()
            };

            if !alert_pane.is_empty() {
                let msg = format!(
                    "Claude stuck: {}. {} consecutive checks failed.",
                    stuck_reason, state.consecutive_failures
                );
                alert::alert(&msg, &alert_pane, &config.alerts.resume_prompt, use_pingme).await;
            }

            state.last_alert = Some(now.clone());
        }
    } else {
        state.consecutive_failures = 0;
        state.alert_count = 0;
    }

    state.last_check = Some(now);
    state.last_status = Some(StatusSnapshot {
        bashes,
        watchmen: watchmen_count,
    });
    crate::state::save_state(&config.general.state_file, state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_elapsed_since_valid() {
        // Use a timestamp 60 seconds ago
        let dt = Utc::now() - chrono::Duration::seconds(60);
        let dt_str = dt.to_rfc3339();
        let elapsed = elapsed_since(&dt_str).expect("should parse");
        // Should be approximately 60 seconds (allow some tolerance)
        assert!(
            elapsed >= 59.0 && elapsed <= 62.0,
            "elapsed was {}",
            elapsed
        );
    }

    #[test]
    fn test_elapsed_since_invalid() {
        assert!(elapsed_since("not a date").is_none());
        assert!(elapsed_since("").is_none());
    }

    // --- should_self_heal tests ---

    #[test]
    fn test_self_heal_triggers_at_threshold_with_tokens() {
        assert!(should_self_heal(5, 5, 12345, 0));
    }

    #[test]
    fn test_self_heal_triggers_at_threshold_with_bashes() {
        assert!(should_self_heal(5, 5, 0, 3));
    }

    #[test]
    fn test_self_heal_triggers_above_threshold() {
        assert!(should_self_heal(250, 5, 100, 0));
    }

    #[test]
    fn test_self_heal_no_trigger_below_threshold() {
        // Not at threshold yet — even if retry has tokens, don't self-heal.
        assert!(!should_self_heal(4, 5, 12345, 0));
    }

    #[test]
    fn test_self_heal_no_trigger_when_retry_still_zero() {
        // At threshold but retry also returned zero — no recovery possible.
        assert!(!should_self_heal(5, 5, 0, 0));
    }

    #[test]
    fn test_self_heal_no_trigger_at_zero() {
        assert!(!should_self_heal(0, 5, 1000, 2));
    }

    // --- check_context_threshold tests ---

    #[test]
    fn test_context_threshold_compact_remaining_triggers() {
        // compact_remaining = 3% <= 5% trigger
        let result = check_context_threshold_with_margin(150000, 200000, Some(3), 75, 5, None);
        assert!(result.is_some());
        let (pct, by_compact) = result.unwrap();
        assert!(by_compact, "should trigger via compact_remaining");
        assert!((pct - 75.0).abs() < 0.1);
    }

    #[test]
    fn test_context_threshold_compact_remaining_at_boundary() {
        // compact_remaining = 5% == 5% trigger (inclusive)
        let result = check_context_threshold_with_margin(150000, 200000, Some(5), 75, 5, None);
        assert!(result.is_some());
        let (_, by_compact) = result.unwrap();
        assert!(by_compact);
    }

    #[test]
    fn test_context_threshold_compact_remaining_safe() {
        // compact_remaining = 50% > 5% trigger — no trigger
        let result = check_context_threshold_with_margin(150000, 200000, Some(50), 75, 5, None);
        assert!(result.is_none());
    }

    #[test]
    fn test_context_threshold_compact_zero() {
        // compact_remaining = 0% — should definitely trigger
        let result = check_context_threshold_with_margin(190000, 200000, Some(0), 75, 5, None);
        assert!(result.is_some());
        let (_, by_compact) = result.unwrap();
        assert!(by_compact);
    }

    #[test]
    fn test_context_threshold_fallback_token_percent_triggers() {
        // No compact_remaining, token pct = 80% >= 75% threshold
        let result = check_context_threshold_with_margin(160000, 200000, None, 75, 5, None);
        assert!(result.is_some());
        let (pct, by_compact) = result.unwrap();
        assert!(
            !by_compact,
            "should trigger via token fallback, not compact"
        );
        assert!((pct - 80.0).abs() < 0.1);
    }

    #[test]
    fn test_context_threshold_fallback_token_percent_safe() {
        // No compact_remaining, token pct = 50% < 75% threshold
        let result = check_context_threshold_with_margin(100000, 200000, None, 75, 5, None);
        assert!(result.is_none());
    }

    #[test]
    fn test_context_threshold_compact_overrides_token() {
        // compact_remaining is present and safe (50%), even though tokens are at 80%
        // Primary trigger (compact) takes precedence — not triggered
        let result = check_context_threshold_with_margin(160000, 200000, Some(50), 75, 5, None);
        assert!(
            result.is_none(),
            "compact_remaining safe should prevent trigger even with high tokens"
        );
    }

    #[test]
    fn test_context_threshold_margin_triggers() {
        // 1M max, 30K margin: trigger at 970K+
        let result = check_context_threshold_with_margin(975000, 1000000, None, 75, 5, Some(30000));
        assert!(result.is_some(), "should trigger at 975K with 30K margin");
    }

    #[test]
    fn test_context_threshold_margin_safe() {
        // 1M max, 30K margin: 960K < 970K threshold
        let result = check_context_threshold_with_margin(960000, 1000000, None, 75, 5, Some(30000));
        assert!(
            result.is_none(),
            "should not trigger at 960K with 30K margin"
        );
    }

    #[test]
    fn test_context_threshold_margin_overrides_percent() {
        // 750K would trigger at 75% but margin says 970K — should NOT trigger
        let result = check_context_threshold_with_margin(750000, 1000000, None, 75, 5, Some(30000));
        assert!(result.is_none(), "margin should override percent threshold");
    }

    // --- Thinking backoff threshold tests ---

    #[test]
    fn test_thinking_backoff_first_interrupt() {
        // First interrupt (count=0): base threshold unchanged
        assert_eq!(thinking_backoff_threshold(60, 960, 0), 60);
    }

    #[test]
    fn test_thinking_backoff_sequence() {
        // Exponential doubling: 60, 120, 240, 480, 960
        assert_eq!(thinking_backoff_threshold(60, 960, 0), 60);
        assert_eq!(thinking_backoff_threshold(60, 960, 1), 120);
        assert_eq!(thinking_backoff_threshold(60, 960, 2), 240);
        assert_eq!(thinking_backoff_threshold(60, 960, 3), 480);
        assert_eq!(thinking_backoff_threshold(60, 960, 4), 960);
    }

    #[test]
    fn test_thinking_backoff_caps_at_max() {
        // Once we hit max_backoff, it stays there
        assert_eq!(thinking_backoff_threshold(60, 960, 4), 960);
        assert_eq!(thinking_backoff_threshold(60, 960, 5), 960);
        assert_eq!(thinking_backoff_threshold(60, 960, 10), 960);
        assert_eq!(thinking_backoff_threshold(60, 960, 100), 960);
    }

    #[test]
    fn test_thinking_backoff_different_base() {
        // With base=120, max=960: 120, 240, 480, 960, 960
        assert_eq!(thinking_backoff_threshold(120, 960, 0), 120);
        assert_eq!(thinking_backoff_threshold(120, 960, 1), 240);
        assert_eq!(thinking_backoff_threshold(120, 960, 2), 480);
        assert_eq!(thinking_backoff_threshold(120, 960, 3), 960);
        assert_eq!(thinking_backoff_threshold(120, 960, 4), 960);
    }

    #[test]
    fn test_thinking_backoff_overflow_safety() {
        // Extremely high interrupt count should not panic (saturating math)
        let result = thinking_backoff_threshold(60, 960, 63);
        assert_eq!(result, 960); // Capped at max
        let result = thinking_backoff_threshold(60, 960, u32::MAX);
        assert_eq!(result, 960); // Capped at max, no panic
    }

    // --- Configurable-multiplier backoff tests (2026-04-21) ---

    #[test]
    fn test_thinking_backoff_multiplier_3() {
        // With base=300, mult=3, max=960: 300, 900, 960 (cap), 960, ...
        assert_eq!(thinking_backoff_threshold_with_multiplier(300, 960, 0, 3), 300);
        assert_eq!(thinking_backoff_threshold_with_multiplier(300, 960, 1, 3), 900);
        assert_eq!(thinking_backoff_threshold_with_multiplier(300, 960, 2, 3), 960);
        assert_eq!(thinking_backoff_threshold_with_multiplier(300, 960, 10, 3), 960);
    }

    #[test]
    fn test_thinking_backoff_multiplier_2_matches_legacy() {
        // multiplier=2 should produce the same output as the legacy doubling.
        for count in 0..6 {
            assert_eq!(
                thinking_backoff_threshold_with_multiplier(60, 960, count, 2),
                thinking_backoff_threshold(60, 960, count),
                "legacy-compat check failed at count={}", count
            );
        }
    }

    #[test]
    fn test_thinking_backoff_multiplier_overflow_safety() {
        // Huge counts with multiplier>1 must not panic.
        let result = thinking_backoff_threshold_with_multiplier(300, 960, u32::MAX, 3);
        assert_eq!(result, 960);
    }

    // --- Global post-interrupt cooldown tests (2026-04-21) ---

    #[test]
    fn test_global_cooldown_disabled_when_zero() {
        // cooldown=0 always returns false, regardless of last_interrupt_at.
        let mut state = State::default();
        state.last_interrupt_at = Some(Utc::now().to_rfc3339());
        assert!(!interrupt_in_global_cooldown(&state, 0));
    }

    #[test]
    fn test_global_cooldown_inactive_when_no_prior_interrupt() {
        // No last_interrupt_at -> never in cooldown.
        let state = State::default();
        assert!(!interrupt_in_global_cooldown(&state, 60));
    }

    #[test]
    fn test_global_cooldown_active_within_window() {
        // Last interrupt was 10s ago, window is 60s -> in cooldown.
        let mut state = State::default();
        let ts = Utc::now() - chrono::Duration::seconds(10);
        state.last_interrupt_at = Some(ts.to_rfc3339());
        assert!(interrupt_in_global_cooldown(&state, 60));
    }

    #[test]
    fn test_global_cooldown_expired_after_window() {
        // Last interrupt was 120s ago, window is 60s -> cooldown expired.
        let mut state = State::default();
        let ts = Utc::now() - chrono::Duration::seconds(120);
        state.last_interrupt_at = Some(ts.to_rfc3339());
        assert!(!interrupt_in_global_cooldown(&state, 60));
    }

    #[test]
    fn test_global_cooldown_ignores_malformed_timestamp() {
        // Garbage timestamp should not count as "in cooldown" (fail-open so
        // the gate never wedges).
        let mut state = State::default();
        state.last_interrupt_at = Some("not a date".to_string());
        assert!(!interrupt_in_global_cooldown(&state, 60));
    }

    // --- Fresh session inject loop prevention tests ---

    /// Helper: simulate the inject loop scenario state transitions.
    /// Returns state after applying the described transition.
    fn make_state_with_inject(was_alive: bool, inject_time_ago_secs: Option<i64>) -> State {
        let mut state = State::default();
        state.fresh_session_injected = true;
        state.was_alive_since_inject = was_alive;
        state.last_fresh_inject = inject_time_ago_secs.map(|secs| {
            let dt = Utc::now() - chrono::Duration::seconds(secs);
            dt.to_rfc3339()
        });
        state
    }

    #[test]
    fn test_inject_loop_prevention_never_alive_recent() {
        // Inject was recent (30s ago), Claude never became active.
        // Should NOT reset fresh_session_injected — prevents the inject loop.
        let state = make_state_with_inject(false, Some(30));
        let inject_expired = state
            .last_fresh_inject
            .as_ref()
            .and_then(|ts| elapsed_since(ts))
            .map_or(false, |elapsed| elapsed >= 300.0);

        assert!(!state.was_alive_since_inject);
        assert!(!inject_expired);
        // The dead state handler would NOT reset because neither condition is true.
    }

    #[test]
    fn test_inject_loop_prevention_was_alive_then_died() {
        // Claude was alive (tokens > 0) after inject, then died.
        // Should reset fresh_session_injected — this is a real session death.
        let state = make_state_with_inject(true, Some(120));

        assert!(state.was_alive_since_inject);
        // The dead state handler WOULD reset because was_alive_since_inject is true.
    }

    #[test]
    fn test_inject_loop_prevention_expired_never_alive() {
        // Inject was 6 minutes ago, Claude never became active.
        // Should reset fresh_session_injected — the session is stuck/dead, allow retry.
        let state = make_state_with_inject(false, Some(360));
        let inject_expired = state
            .last_fresh_inject
            .as_ref()
            .and_then(|ts| elapsed_since(ts))
            .map_or(false, |elapsed| elapsed >= 300.0);

        assert!(!state.was_alive_since_inject);
        assert!(inject_expired);
        // The dead state handler WOULD reset because inject_expired is true.
    }

    #[test]
    fn test_inject_loop_prevention_no_timestamp() {
        // fresh_session_injected is true but no timestamp (legacy state).
        // Should NOT reset (conservative — treat as recent).
        let state = make_state_with_inject(false, None);
        let inject_expired = state
            .last_fresh_inject
            .as_ref()
            .and_then(|ts| elapsed_since(ts))
            .map_or(false, |elapsed| elapsed >= 300.0);

        assert!(!state.was_alive_since_inject);
        assert!(!inject_expired);
        // Conservative: don't reset without evidence.
    }

    #[test]
    fn test_inject_active_session_marks_alive() {
        // Simulates tokens > 0 path: fresh_session_injected → was_alive_since_inject
        let mut state = State::default();
        state.fresh_session_injected = true;
        state.was_alive_since_inject = false;

        // This mirrors the "session is active (tokens > 0)" block in check_cycle:
        if state.fresh_session_injected {
            state.was_alive_since_inject = true;
            state.fresh_session_injected = false;
        }

        assert!(!state.fresh_session_injected);
        assert!(state.was_alive_since_inject);
    }

    #[test]
    fn test_inject_pane_change_resets_both_flags() {
        // Pane change is definitive — always reset both flags.
        let mut state = State::default();
        state.fresh_session_injected = true;
        state.was_alive_since_inject = true;

        // This mirrors the pane change block in check_cycle:
        state.fresh_session_injected = false;
        state.was_alive_since_inject = false;

        assert!(!state.fresh_session_injected);
        assert!(!state.was_alive_since_inject);
    }

    // --- Interrupt counter tests (2026-04-22) ---
    //
    // These sanity-check that each per-interrupt counter uses saturating
    // addition and accumulates across multiple fires. The full tmux-driven
    // fire paths are exercised in the e2e tests; these tests pin down the
    // arithmetic primitive that every fire site uses.

    #[test]
    fn test_interrupt_counter_saturating_increment_accumulates() {
        let mut state = State::default();
        for _ in 0..5 {
            state.prolonged_thinking_interrupts_total = state
                .prolonged_thinking_interrupts_total
                .saturating_add(1);
        }
        assert_eq!(state.prolonged_thinking_interrupts_total, 5);
    }

    #[test]
    fn test_interrupt_counter_saturating_increment_does_not_panic_at_u64_max() {
        let mut state = State::default();
        state.prolonged_thinking_interrupts_total = u64::MAX;
        // saturating_add(1) must not panic at u64::MAX; it saturates.
        state.prolonged_thinking_interrupts_total = state
            .prolonged_thinking_interrupts_total
            .saturating_add(1);
        assert_eq!(state.prolonged_thinking_interrupts_total, u64::MAX);
    }

    #[test]
    fn test_interrupt_counter_independent_of_backoff_index() {
        // The cumulative counter must not be reset by the per-episode
        // thinking_interrupt_count reset (which happens when Claude exits
        // the thinking state — see `check_foreground` else branch).
        let mut state = State::default();
        state.prolonged_thinking_interrupts_total = 42;
        state.thinking_interrupt_count = 3;

        // Mirror the reset branch at the non-thinking else arm:
        state.thinking_start = None;
        state.thinking_alerted = false;
        state.thinking_interrupt_count = 0;

        // Cumulative counter must NOT be reset.
        assert_eq!(state.prolonged_thinking_interrupts_total, 42);
        assert_eq!(state.thinking_interrupt_count, 0);
    }

    #[test]
    fn test_interrupt_counters_independent_per_kind() {
        // Incrementing one kind must not affect the others.
        let mut state = State::default();
        state.watcher_down_interrupts_total = state
            .watcher_down_interrupts_total
            .saturating_add(1);
        state.context_warning_interrupts_total = state
            .context_warning_interrupts_total
            .saturating_add(1);
        state.context_warning_interrupts_total = state
            .context_warning_interrupts_total
            .saturating_add(1);

        assert_eq!(state.watcher_down_interrupts_total, 1);
        assert_eq!(state.context_warning_interrupts_total, 2);
        // Untouched kinds stay at 0
        assert_eq!(state.prolonged_thinking_interrupts_total, 0);
        assert_eq!(state.wedged_clear_interrupts_total, 0);
        assert_eq!(state.auto_update_interrupts_total, 0);
        assert_eq!(state.restart_claude_interrupts_total, 0);
    }

    // --- Watcher-down inject due-predicate tests (2026-04-28) ---
    //
    // These pin the new behavior:
    //   1. Never-injected -> always due.
    //   2. Recent inject (< cooldown) -> NOT due.
    //   3. Old inject (>= cooldown) -> due.
    //   4. Malformed timestamp -> due (fail-open).
    //   5. cooldown=0 with recent inject -> due (cooldown disabled).
    //   6. The watcher-down predicate does NOT consult
    //      interrupt_in_global_cooldown — i.e. an unrelated recent
    //      interrupt MUST NOT block the watcher-down fire path.
    //      This is the regression guard for the actual bug Andrew filed
    //      (q-2026-04-28-713a) and for the prior reverted attempts.

    #[test]
    fn test_watcher_inject_due_never_injected() {
        assert!(watcher_inject_due(None, 60));
    }

    #[test]
    fn test_watcher_inject_due_within_cooldown() {
        let recent = (Utc::now() - chrono::Duration::seconds(10)).to_rfc3339();
        assert!(!watcher_inject_due(Some(&recent), 60));
    }

    #[test]
    fn test_watcher_inject_due_after_cooldown() {
        let old = (Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
        assert!(watcher_inject_due(Some(&old), 60));
    }

    #[test]
    fn test_watcher_inject_due_malformed_timestamp_fails_open() {
        // Garbage timestamp must fail OPEN (allow inject) rather than
        // wedge the gate forever.
        assert!(watcher_inject_due(Some("not a date"), 60));
    }

    #[test]
    fn test_watcher_inject_due_cooldown_zero_always_due() {
        // cooldown=0 means "no rate limit"; even a 1s-ago inject is due.
        let just_now = (Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
        assert!(watcher_inject_due(Some(&just_now), 0));
    }

    #[test]
    fn test_watcher_inject_ignores_global_cooldown() {
        // REGRESSION GUARD (q-2026-04-28-713a): the watcher-down inject
        // path is intentionally exempt from interrupt_in_global_cooldown.
        // Set up state where a different interrupt fired 5s ago; the
        // global cooldown gate would block, but the watcher-down
        // predicate does not consult it.
        let mut state = State::default();
        state.last_interrupt_at =
            Some((Utc::now() - chrono::Duration::seconds(5)).to_rfc3339());
        // Sanity: global cooldown would block.
        assert!(interrupt_in_global_cooldown(&state, 60));
        // But watcher-down predicate ignores last_interrupt_at and only
        // considers last_watcher_inject. With None, it's due.
        assert!(watcher_inject_due(state.last_watcher_inject.as_deref(), 60));
    }

    #[test]
    fn test_default_watcher_inject_cooldown_is_aggressive() {
        // Pin the new 60s default in case someone bumps it back to
        // 300s without realizing the original cascade-suppression
        // rationale was retired. If you genuinely want a longer
        // default, also update CLAUDE.md / the comment in config.rs.
        use crate::config::parse_config;
        let cfg = r#"
[general]
check_interval = 10
state_file = "/tmp/s.json"
log_file = "/tmp/s.jsonl"
legacy_log_file = "/tmp/s.log"

[claude]
max_context_tokens = 200000
heartbeat_file = "/tmp/hb"
relaunch_script = "/tmp/rel.sh"

[dead_process]
checks_required = 3
restart_cooldown = 60

[fresh_clear]
min_tokens = 1000
max_tokens = 5000
detections_required = 2
cooldown = 60

[heartbeat]
stale_minutes = 10

[alerts]
initial_cooldown = 60
escalation_tiers = [60]
max_pingme_alerts = 3
resume_prompt = "r"

[foreground_monitor]
enabled = false
threshold_seconds = 180
check_interval = 3

[watcher_monitor]
enabled = true
watchers_config = "/tmp/w.conf"
expected_watchmen = 0

[context_monitor]
enabled = true
threshold_percent = 75
compact_trigger_percent = 5
grace_period = 120
cooldown = 300
"#;
        let cfg = parse_config(cfg).expect("parse");
        assert_eq!(
            cfg.watcher_monitor.inject_cooldown, 60,
            "default watcher inject_cooldown should be 60s (aggressive re-inject); \
             see src/policy.rs::watcher_inject_due doc comment"
        );
    }
}
