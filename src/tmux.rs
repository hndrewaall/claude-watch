//! All tmux interaction: send keys, capture pane, idle/mode detection, injection.

use crate::cmd::{run_cmd, run_cmd_any};
use std::fmt;
use std::time::Duration;
use tokio::time::sleep;
use tracing::debug;

/// Current activity state of Claude Code as observed from tmux pane output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeActivity {
    /// Prompt (❯) visible — waiting for input
    Idle,
    /// ✽ thinking indicator visible (e.g. "✽ Thinking… (12s · ↓ 384 tokens)")
    Thinking,
    /// Spinner + tool name visible (e.g. "⠋ Read(~/some/file)")
    ToolRunning,
    /// ● output being streamed, no prompt visible
    Writing,
    /// Can't determine current state
    Unknown,
}

impl fmt::Display for ClaudeActivity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClaudeActivity::Idle => write!(f, "idle"),
            ClaudeActivity::Thinking => write!(f, "thinking"),
            ClaudeActivity::ToolRunning => write!(f, "tool_running"),
            ClaudeActivity::Writing => write!(f, "writing"),
            ClaudeActivity::Unknown => write!(f, "unknown"),
        }
    }
}

pub async fn send_keys(pane: &str, keys: &[&str]) {
    let mut args = vec!["tmux", "send-keys", "-t", pane];
    args.extend_from_slice(keys);
    let _ = run_cmd(&args, 5).await;
}

pub async fn send_literal(pane: &str, text: &str) {
    let _ = run_cmd(&["tmux", "send-keys", "-t", pane, "-l", text], 5).await;
}

pub async fn capture_pane(pane: &str) -> Option<String> {
    run_cmd(&["tmux", "capture-pane", "-t", pane, "-p"], 5).await
}

/// Capture pane with -J flag to join wrapped lines. Use for status bar parsing
/// where text may be truncated at pane width (e.g. "275898 tokens" → "275898 toke…").
pub async fn capture_pane_joined(pane: &str) -> Option<String> {
    run_cmd(&["tmux", "capture-pane", "-t", pane, "-p", "-J"], 5).await
}

pub async fn capture_pane_history(pane: &str, lines: i32) -> Option<String> {
    let start = format!("-{}", lines);
    run_cmd(&["tmux", "capture-pane", "-t", pane, "-p", "-S", &start], 5).await
}

/// Check if the Claude Code prompt (>) is visible in the last 15 lines.
pub async fn is_idle(pane: &str) -> bool {
    if let Some(out) = capture_pane(pane).await {
        return check_lines_for_idle_prompt(&out);
    }
    false
}

/// Pure function: check if any of the last 15 lines contain the Claude prompt character.
pub(crate) fn check_lines_for_idle_prompt(pane_output: &str) -> bool {
    let lines: Vec<&str> = pane_output.lines().collect();
    let start = if lines.len() > 15 {
        lines.len() - 15
    } else {
        0
    };
    for line in &lines[start..] {
        if line.contains('\u{276f}') {
            return true;
        }
    }
    false
}

/// Check if pane shows exit teardown indicators ("Goodbye!" or "Background command was stopped").
/// During /exit, Claude Code prints these before the process fully terminates.
pub async fn is_exit_teardown(pane: &str) -> bool {
    if let Some(out) = capture_pane(pane).await {
        return check_lines_for_exit_teardown(&out);
    }
    false
}

/// Pure function: check if the last 30 lines contain exit teardown markers.
/// "Goodbye!" is printed by Claude Code on /exit. "Background command was stopped"
/// follows as each background task is cleaned up.
pub(crate) fn check_lines_for_exit_teardown(pane_output: &str) -> bool {
    let lines: Vec<&str> = pane_output.lines().collect();
    // Check more lines (30) since "Goodbye!" may scroll up as
    // "Background command was stopped" messages accumulate
    let start = if lines.len() > 30 {
        lines.len() - 30
    } else {
        0
    };
    for line in &lines[start..] {
        let trimmed = line.trim();
        if trimmed == "Goodbye!" || trimmed.contains("Background command was stopped") {
            return true;
        }
    }
    false
}

/// Check if pane shows INSERT mode indicator.
pub async fn is_insert_mode(pane: &str) -> bool {
    if let Some(out) = capture_pane(pane).await {
        return out.contains("-- INSERT");
    }
    false
}

/// Check if pane shows a shell prompt (Claude Code not running).
pub async fn is_shell_prompt(pane: &str) -> bool {
    if let Some(out) = capture_pane(pane).await {
        return check_lines_for_shell_prompt(&out);
    }
    false
}

/// Pure function: check if any of the last 5 lines look like a shell prompt.
pub(crate) fn check_lines_for_shell_prompt(pane_output: &str) -> bool {
    let lines: Vec<&str> = pane_output.lines().collect();
    let start = if lines.len() > 5 { lines.len() - 5 } else { 0 };
    for line in &lines[start..] {
        let trimmed = line.trim();
        if trimmed.ends_with('$') || trimmed.ends_with('%') {
            return true;
        }
        if trimmed.contains("\u{279c}") || trimmed.contains("\u{2570}\u{2500}") {
            return true;
        }
    }
    false
}

/// Check if pane is showing the session feedback prompt.
pub async fn has_feedback_prompt(pane: &str) -> bool {
    if let Some(out) = capture_pane(pane).await {
        return check_lines_for_feedback_prompt(&out);
    }
    false
}

/// Pure function: check if output contains feedback prompt markers.
pub(crate) fn check_lines_for_feedback_prompt(pane_output: &str) -> bool {
    pane_output.contains("How is Claude doing") || pane_output.contains("0: Dismiss")
}

/// Dismiss the feedback prompt by sending '0'.
pub async fn dismiss_feedback_prompt(pane: &str) {
    for _ in 0..3 {
        if !has_feedback_prompt(pane).await {
            return;
        }
        send_literal(pane, "0").await;
        sleep(Duration::from_secs(1)).await;
    }
}

/// Actively interrupt Claude Code: rapid-fire Escape, periodically Ctrl-B x2.
/// Returns true if idle state confirmed within timeout.
///
/// Uses `get_activity()` (content-area aware) instead of `is_idle()` (prompt-only)
/// to ensure the thinking indicator has fully cleared before returning.
pub async fn interrupt_and_wait(pane: &str, timeout_secs: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    let mut escape_count: u32 = 0;

    while tokio::time::Instant::now() < deadline {
        if get_activity(pane).await == ClaudeActivity::Idle {
            sleep(Duration::from_millis(300)).await;
            if get_activity(pane).await == ClaudeActivity::Idle {
                return true;
            }
        }

        if escape_count > 0 && escape_count % 5 == 0 {
            send_keys(pane, &["C-b"]).await;
            sleep(Duration::from_millis(300)).await;
            send_keys(pane, &["C-b"]).await;
            sleep(Duration::from_millis(500)).await;
        } else {
            send_keys(pane, &["Escape"]).await;
            sleep(Duration::from_millis(500)).await;
        }
        escape_count += 1;
    }
    false
}

/// Inject text into Claude Code via vim-mode keystrokes.
/// Escape(s) -> wait for Idle -> dd -> i -> type -> Escape -> Enter
pub async fn inject_text(pane: &str, text: &str) {
    // Step 1: Escape to NORMAL mode (up to 3 attempts)
    for _ in 0..3 {
        send_keys(pane, &["Escape"]).await;
        sleep(Duration::from_secs(1)).await;
        if !is_insert_mode(pane).await {
            break;
        }
    }
    sleep(Duration::from_millis(500)).await;

    // Step 1b: Wait for activity to settle to Idle (thinking indicator cleared).
    // The prompt may be visible while thinking text is still rendering in the
    // content area above. Wait up to 10s for get_activity() == Idle before
    // proceeding with dd/i/type to avoid typing over stale thinking text.
    let idle_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < idle_deadline {
        if get_activity(pane).await == ClaudeActivity::Idle {
            break;
        }
        sleep(Duration::from_millis(500)).await;
    }

    // Step 2: dd -- delete entire line
    send_keys(pane, &["d"]).await;
    sleep(Duration::from_millis(100)).await;
    send_keys(pane, &["d"]).await;
    sleep(Duration::from_millis(500)).await;

    // Step 3: i -- enter INSERT mode
    send_keys(pane, &["i"]).await;
    sleep(Duration::from_millis(1500)).await;

    // Step 4: Type the text
    send_literal(pane, text).await;
    sleep(Duration::from_millis(500)).await;

    // Step 5: Escape + Enter to submit
    send_keys(pane, &["Escape"]).await;
    sleep(Duration::from_millis(300)).await;
    send_keys(pane, &["Enter"]).await;
}

/// Inject a command into a shell prompt.
pub async fn inject_shell(pane: &str, cmd: &str) {
    send_literal(pane, cmd).await;
    sleep(Duration::from_millis(300)).await;
    send_keys(pane, &["Enter"]).await;
}

/// Check if Claude Code appears to be executing a foreground bash command.
pub async fn is_foreground_busy(pane: &str) -> bool {
    if let Some(out) = capture_pane(pane).await {
        return check_lines_for_foreground_busy(&out);
    }
    false
}

/// Pure function: check if pane output indicates foreground busy state.
/// No prompt visible + spinner characters = foreground busy.
pub(crate) fn check_lines_for_foreground_busy(pane_output: &str) -> bool {
    let lines: Vec<&str> = pane_output.lines().collect();
    let start = if lines.len() > 10 {
        lines.len() - 10
    } else {
        0
    };
    let tail = &lines[start..];

    // If prompt is visible, not in foreground
    for line in tail {
        if line.contains('\u{276f}') {
            return false;
        }
    }

    // No prompt visible -- check for signs of active work (spinner characters)
    for line in tail {
        for &spinner in SPINNER_CHARS {
            if line.contains(spinner) {
                return true;
            }
        }
    }
    false
}

/// Spinner characters used by Claude Code for tool execution indicators.
/// Extracted from Claude Code v2.1.77 binary via:
///   strings <binary> | grep -oP '\\u280[0-9a-fA-F]|\\u281[0-9a-fA-F]|...'
/// These are braille pattern characters used in the dots spinner animation.
const SPINNER_CHARS: &[char] = &[
    '\u{2802}', '\u{2807}', '\u{280b}', '\u{280f}', '\u{2810}', '\u{2819}', '\u{2826}', '\u{2827}',
    '\u{2834}', '\u{2838}', '\u{2839}', '\u{283c}',
];

/// Check if a line is a separator (composed entirely of U+2500 box-drawing chars).
fn is_separator_line(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && trimmed.chars().all(|c| c == '\u{2500}')
}

/// Pure function: detect Claude Code's current activity from pane output.
///
/// Claude Code's TUI has a fixed layout:
///   [scrolling content area - thinking indicators, tool output, writing]
///   ─────────────────── (separator line, U+2500 repeated)
///   ❯                   (prompt - ALWAYS visible when Claude Code is running)
///   ─────────────────── (separator)
///   -- INSERT -- ...    (status bar)
///
/// The ❯ prompt is always visible regardless of activity state, so we split
/// at the first separator and only check the content area above it.
///
/// Priority order: Thinking > ToolRunning > Writing > Idle > Unknown.
pub fn detect_activity(pane_output: &str) -> ClaudeActivity {
    let lines: Vec<&str> = pane_output.lines().collect();
    let start = if lines.len() > 15 {
        lines.len() - 15
    } else {
        0
    };
    let tail = &lines[start..];

    // Find the first separator line to split content area from prompt/status area
    let separator_idx = tail.iter().position(|line| is_separator_line(line));

    // Determine content area and whether the prompt is visible below the separator
    let (content_lines, has_prompt) = if let Some(sep_idx) = separator_idx {
        let content = &tail[..sep_idx];
        let below = &tail[sep_idx..];
        let prompt_visible = below.iter().any(|line| line.contains('\u{276f}'));
        (content, prompt_visible)
    } else {
        // No separator found — not in Claude Code's fixed TUI layout.
        // Fall back to the legacy behavior: prompt means Idle (highest priority).
        for line in tail {
            if line.contains('\u{276f}') {
                return ClaudeActivity::Idle;
            }
        }
        (tail, false)
    };

    // Helper: check if a line has a thinking indicator character
    let has_indicator_char = |trimmed: &str| -> bool {
        trimmed.contains('\u{273d}')  // ✽
            || trimmed.contains('\u{273b}')  // ✻
            || trimmed.contains('\u{2722}')  // ✢
            || trimmed.contains('\u{2733}')  // ✳
            || trimmed.contains('\u{2736}')  // ✶
            || trimmed.contains('\u{00b7}')  // · (middle dot)
            || trimmed.starts_with("* ")
    };

    // 1. Completion check FIRST (when prompt is visible).
    // Completion lines ("✻ Brewed for 38s") mean Claude finished responding.
    // A stale thinking indicator ("✽ Thinking… (5s)") may still be visible
    // in the scroll history above. Completion + prompt = Idle, always.
    // Must be checked before thinking to avoid false "prolonged thinking".
    if has_prompt {
        let has_completion = content_lines.iter().any(|line| {
            let trimmed = line.trim();
            has_indicator_char(trimmed)
                && trimmed.contains(" for ")
                && !trimmed.contains('\u{2026}')
        });
        if has_completion {
            return ClaudeActivity::Idle;
        }
    }

    // 2. Thinking — indicator char + verb ending in … (U+2026)
    //
    // Extracted from Claude Code v2.1.77 binary. To update, run:
    //   strings <binary> | grep -oP '\\u273[0-9a-fA-F]|\\u272[0-9a-fA-F]' | sort -u
    // Then find the CdH() function context:
    //   strings <binary> | grep -oP '.{0,100}\\u273[0-9a-fA-F].{0,100}' | grep CdH
    //
    // Claude Code uses these indicator characters (from CdH() in source):
    //   Ghostty:  · (U+00B7), ✢ (U+2722), ✳ (U+2733), ✶ (U+2736), ✻ (U+273B), * (U+002A)
    //   Other:    · (U+00B7), ✢ (U+2722), * (U+002A), ✶ (U+2736), ✻ (U+273B), ✽ (U+273D)
    //
    // Thinking verbs (168 total, from "Accomplishing" to "Zigzagging"):
    //   strings <binary> | grep -oP '"Accomplishing".{0,10000}?"Zigzagging"\]' | tr ',' '\n'
    //
    // U+273B also appears in completion lines ("Brewed for") but those lack …
    // Require trailing … (U+2026) to distinguish active thinking from completion.
    for line in content_lines {
        let trimmed = line.trim();
        if has_indicator_char(trimmed) && trimmed.contains('\u{2026}') {
            return ClaudeActivity::Thinking;
        }
    }

    // 3. ToolRunning (spinner character present in content area)
    for line in content_lines {
        for &spinner in SPINNER_CHARS {
            if line.contains(spinner) {
                return ClaudeActivity::ToolRunning;
            }
        }
    }

    // 4. Writing (● bullet points visible in content area)
    for line in content_lines {
        if line.trim_start().starts_with('\u{25cf}') {
            return ClaudeActivity::Writing;
        }
    }

    // 5. Idle (prompt visible but no activity indicators above separator)
    if has_prompt {
        return ClaudeActivity::Idle;
    }

    ClaudeActivity::Unknown
}

/// Capture the pane and detect Claude Code's current activity state.
pub async fn get_activity(pane: &str) -> ClaudeActivity {
    if let Some(out) = capture_pane(pane).await {
        return detect_activity(&out);
    }
    ClaudeActivity::Unknown
}

/// Find the dashboard pane for Claude Code.
///
/// When `dashboard_session` and `dashboard_pane` are configured (non-empty),
/// checks those specific locations first. When unconfigured (empty/default),
/// falls back to `find_claude_pane()` which auto-discovers across all tmux
/// sessions. This makes the [tmux] config section optional for fresh installs.
pub async fn find_dashboard_pane(config: &crate::config::TmuxConfig) -> Option<String> {
    // If no session configured, skip session-specific checks and auto-detect
    if config.dashboard_session.is_empty() {
        debug!("no dashboard_session configured, falling back to find_claude_pane()");
        return crate::status::find_claude_pane().await;
    }

    // Check if dashboard session exists
    let (_, ok) = run_cmd_any(&["tmux", "has-session", "-t", &config.dashboard_session], 5).await;
    if !ok {
        return None;
    }

    // Check known pane (only if explicitly configured)
    if !config.dashboard_pane.is_empty() {
        let (out, ok) = run_cmd_any(
            &[
                "tmux",
                "display-message",
                "-t",
                &config.dashboard_pane,
                "-p",
                "#{pane_id}",
            ],
            5,
        )
        .await;
        if ok && !out.is_empty() {
            return Some(config.dashboard_pane.clone());
        }
    }

    // Fallback: search for shell panes in dashboard session
    let (out, ok) = run_cmd_any(
        &[
            "tmux",
            "list-panes",
            "-s",
            "-t",
            &config.dashboard_session,
            "-F",
            "#{session_name}:#{window_index}.#{pane_index} #{pane_current_command}",
        ],
        5,
    )
    .await;
    if ok {
        for line in out.lines() {
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if parts.len() == 2 && (parts[1] == "zsh" || parts[1] == "bash") {
                return Some(parts[0].to_string());
            }
        }
    }
    None
}

/// Check if Claude Code is still running in the pane (vs. shell prompt visible).
///
/// After /exit, the pane shows a zsh prompt. During Claude Code, the status bar
/// shows token count and version info. Shell prompt detection takes priority.
pub async fn is_claude_running(pane: &str) -> bool {
    if let Some(out) = capture_pane(pane).await {
        return check_claude_running(&out);
    }
    false
}

/// Pure function: check if Claude Code is running from pane output.
/// Returns false if a shell prompt is detected, true if Claude indicators found.
///
/// Shell prompt detection is stricter than `check_lines_for_shell_prompt` to avoid
/// false positives from Claude Code status bar content (e.g. "42%" compact remaining).
/// We only look for bira theme patterns (╰─$, ╰─#) and arrow prompts (➜).
pub(crate) fn check_claude_running(pane_output: &str) -> bool {
    let lines: Vec<&str> = pane_output.lines().collect();

    // Check for shell prompt FIRST (stronger signal — means Claude exited)
    // Use strict bira-theme patterns to avoid false positives from status bar "42%" etc.
    let start = if lines.len() > 5 { lines.len() - 5 } else { 0 };
    for line in &lines[start..] {
        let trimmed = line.trim();
        // Bira theme: "╰─$" or "╰─# " (root)
        if trimmed.contains("\u{2570}\u{2500}$") || trimmed.contains("\u{2570}\u{2500}#") {
            return false;
        }
        // Arrow prompt (oh-my-zsh robbyrussell etc.)
        if trimmed.contains("\u{279c}") {
            return false;
        }
    }

    // Only if no shell prompt found, check for Claude Code indicators.
    // Match "tok" (not "tokens") to tolerate the `502064 tok…` ellipsis
    // truncation Claude Code applies in narrow panes.
    let tail_start = if lines.len() > 10 {
        lines.len() - 10
    } else {
        0
    };
    let tail: String = lines[tail_start..].join("\n");
    if tail.contains("tok")
        && (tail.contains("auto-compact")
            || tail.contains("latest:")
            || tail.contains("background tasks")
            || tail.contains("bypass permissi"))
    {
        return true;
    }

    // Default: assume still running (conservative)
    true
}

/// Wait for Claude Code to exit (shell prompt appears). Returns true if exited within timeout.
pub async fn wait_for_exit(pane: &str, timeout_secs: u64) -> bool {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while tokio::time::Instant::now() < deadline {
        if !is_claude_running(pane).await {
            return true;
        }
        sleep(std::time::Duration::from_secs(1)).await;
    }
    false
}

/// Check if the actual Claude binary (not a wrapper script) is running under a pane's process tree.
/// Walks /proc looking for an exe pointing to ~/.local/share/claude/versions/.
pub async fn wait_for_claude_binary(pane: &str, timeout_secs: u64) -> bool {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while tokio::time::Instant::now() < deadline {
        if has_claude_binary(pane).await {
            return true;
        }
        sleep(std::time::Duration::from_secs(2)).await;
    }
    false
}

/// Check if the pane's process tree includes the actual claude binary.
async fn has_claude_binary(pane: &str) -> bool {
    let (pid_str, ok) = run_cmd_any(
        &["tmux", "display-message", "-t", pane, "-p", "#{pane_pid}"],
        5,
    )
    .await;
    if !ok || pid_str.is_empty() {
        return false;
    }

    let versions_dir = format!(
        "{}/.local/share/claude/versions",
        std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string())
    );

    // Spawn blocking since we're walking /proc
    let pid_str_owned = pid_str.clone();
    let versions_dir_owned = versions_dir;
    tokio::task::spawn_blocking(move || check_proc_tree(&pid_str_owned, &versions_dir_owned, 0))
        .await
        .unwrap_or(false)
}

/// Recursively check process tree for claude binary.
fn check_proc_tree(pid: &str, versions_dir: &str, depth: u32) -> bool {
    if depth > 4 {
        return false;
    }

    // Check this PID's exe link
    let exe_path = format!("/proc/{}/exe", pid);
    if let Ok(target) = std::fs::read_link(&exe_path) {
        let target_str = target.to_string_lossy();
        if target_str.starts_with(versions_dir) {
            return true;
        }
    }

    // Check children via /proc/PID/task/PID/children or pgrep
    if let Ok(output) = std::process::Command::new("pgrep")
        .args(["-P", pid])
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let child_pid = line.trim();
            if !child_pid.is_empty() && check_proc_tree(child_pid, versions_dir, depth + 1) {
                return true;
            }
        }
    }

    false
}

/// Wait for the Claude idle prompt (❯) to appear. Returns true if found within timeout.
pub async fn wait_for_idle_prompt(pane: &str, timeout_secs: u64) -> bool {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while tokio::time::Instant::now() < deadline {
        if is_idle(pane).await {
            sleep(std::time::Duration::from_millis(500)).await;
            if is_idle(pane).await {
                return true;
            }
        }
        sleep(std::time::Duration::from_secs(2)).await;
    }
    false
}

/// Pure function: check if pane output indicates Claude Code needs API reauth.
///
/// Two phases of auth failure:
/// 1. **401 error** — TUI is still visible but shows "API Error: 401" with
///    `authentication_error` JSON. Detected even with TUI elements present.
/// 2. **Login screen** — TUI is gone, replaced by "Browser didn't open?" / OAuth URL.
///    Detected by auth-specific patterns AND absence of normal TUI indicators.
pub(crate) fn check_lines_for_reauth(pane_output: &str) -> bool {
    let lower = pane_output.to_lowercase();

    // Phase 1: 401/authentication errors — these appear WITH the TUI still visible.
    // The pane shows: "Please run /login · API Error: 401" and JSON with
    // "authentication_error" / "Invalid authentication credentials".
    // These are unambiguous — no false positives from conversation content because
    // they include the structured error JSON format.
    if lower.contains("api error: 401")
        || lower.contains("\"authentication_error\"")
        || lower.contains("invalid authentication credentials")
    {
        return true;
    }

    // Phase 2: Login screen — TUI elements are NOT visible (replaced by auth flow).
    // Guard: if TUI is still showing, only 401 patterns above should match.
    if lower.contains("tokens") || lower.contains("bashes") || lower.contains("\u{276f}") {
        return false;
    }

    // Auth-specific patterns for the login screen
    // Current login screen shows: "Browser didn't open?", "Paste code here",
    // and a claude.ai/oauth/authorize URL
    lower.contains("browser didn't open")
        || lower.contains("paste code here")
        || lower.contains("claude.ai/oauth/authorize")
        || lower.contains("open this url") && lower.contains("login")
        || lower.contains("session expired")
        || lower.contains("login required")
        || lower.contains("re-authenticate")
        || lower.contains("authentication required")
        || lower.contains("auth required")
        || lower.contains("api key expired")
}

/// Extract the login URL from pane output, handling possible line wrapping.
/// Looks for URLs starting with `https://claude.ai/oauth/authorize`.
/// tmux line wrapping splits a URL across lines with NO separator, so we
/// reassemble by joining consecutive lines that look like URL continuations
/// (no whitespace at start, valid URL chars).
pub(crate) fn extract_login_url(pane_output: &str) -> Option<String> {
    let lines: Vec<&str> = pane_output.lines().collect();

    // Find the line containing the URL start
    let mut url_line_idx = None;
    let mut url_start_col = 0;
    for (i, line) in lines.iter().enumerate() {
        if let Some(pos) = line.find("https://claude.ai/oauth/authorize") {
            url_line_idx = Some(i);
            url_start_col = pos;
            break;
        }
    }
    let start_idx = url_line_idx?;

    // Start with the URL portion from the first line
    let first_part = &lines[start_idx][url_start_col..];
    let mut url = String::new();

    // Check if first line's URL portion ends at a whitespace boundary
    if let Some(end) = first_part.find(|c: char| c.is_whitespace()) {
        url.push_str(&first_part[..end]);
    } else {
        // URL may continue on next line(s) — tmux wraps with no separator
        url.push_str(first_part);
        for line in &lines[start_idx + 1..] {
            // A continuation line starts with URL-valid chars (no space/control)
            // and the line is non-empty
            if line.is_empty() || line.starts_with(' ') {
                break;
            }
            // Append until whitespace
            if let Some(end) = line.find(|c: char| c.is_whitespace()) {
                url.push_str(&line[..end]);
                break;
            } else {
                url.push_str(line);
            }
        }
    }

    Some(url)
}

/// Check if the pane is showing a reauth/login prompt.
/// Returns the login URL if reauth is needed (or empty string if needed but URL not found).
pub async fn needs_reauth(pane: &str) -> Option<String> {
    if let Some(out) = capture_pane(pane).await {
        if check_lines_for_reauth(&out) {
            return Some(extract_login_url(&out).unwrap_or_default());
        }
    }
    None
}

/// Run tmux healthcheck brief.
pub async fn healthcheck_brief() -> String {
    run_cmd(&["tmux-healthcheck", "--brief"], 5)
        .await
        .unwrap_or_else(|| "tmux-healthcheck: unavailable".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_idle_prompt_detected() {
        // U+276F is the "heavy right-pointing angle quotation mark ornament" used as Claude prompt
        let output = "some output\nmore output\n\u{276f} ";
        assert!(check_lines_for_idle_prompt(output));
    }

    #[test]
    fn test_idle_prompt_not_present() {
        let output = "some output\nmore output\nstill working...";
        assert!(!check_lines_for_idle_prompt(output));
    }

    #[test]
    fn test_idle_prompt_only_checks_last_15_lines() {
        // Prompt in line 1, but 20 lines of other stuff after
        let mut lines = vec!["\u{276f} old prompt"];
        for _ in 0..20 {
            lines.push("busy output line");
        }
        let output = lines.join("\n");
        assert!(!check_lines_for_idle_prompt(&output));
    }

    #[test]
    fn test_idle_prompt_within_last_15() {
        let mut lines: Vec<&str> = Vec::new();
        for _ in 0..10 {
            lines.push("busy output");
        }
        lines.push("\u{276f} ready");
        for _ in 0..3 {
            lines.push("");
        }
        let output = lines.join("\n");
        assert!(check_lines_for_idle_prompt(&output));
    }

    #[test]
    fn test_shell_prompt_dollar() {
        let output = "line1\nline2\nuser@host:~$ ";
        assert!(check_lines_for_shell_prompt(output));
    }

    #[test]
    fn test_shell_prompt_percent() {
        let output = "line1\nline2\nuser@host:~% ";
        assert!(check_lines_for_shell_prompt(output));
    }

    #[test]
    fn test_shell_prompt_arrow() {
        let output = "line1\nline2\n\u{279c} ~ ";
        assert!(check_lines_for_shell_prompt(output));
    }

    #[test]
    fn test_no_shell_prompt() {
        let output = "Claude Code is running\nTokens: 50000\nBashes: 10";
        assert!(!check_lines_for_shell_prompt(output));
    }

    #[test]
    fn test_feedback_prompt_detected() {
        let output = "How is Claude doing today?\n0: Dismiss\n1: Great";
        assert!(check_lines_for_feedback_prompt(output));
    }

    #[test]
    fn test_feedback_prompt_dismiss_only() {
        let output = "some output\n0: Dismiss\nother stuff";
        assert!(check_lines_for_feedback_prompt(output));
    }

    #[test]
    fn test_no_feedback_prompt() {
        let output = "normal claude output\nno feedback here";
        assert!(!check_lines_for_feedback_prompt(output));
    }

    #[test]
    fn test_foreground_busy_with_spinner() {
        // No prompt + spinner = busy
        let output = "Running command...\n\u{280b} processing...";
        assert!(check_lines_for_foreground_busy(output));
    }

    #[test]
    fn test_foreground_not_busy_with_prompt() {
        // Prompt visible = not busy even with spinner
        let output = "\u{276f} \n\u{280b} processing...";
        assert!(!check_lines_for_foreground_busy(output));
    }

    #[test]
    fn test_foreground_not_busy_no_spinner() {
        // No prompt, no spinner = not busy (indeterminate)
        let output = "some text\nmore text";
        assert!(!check_lines_for_foreground_busy(output));
    }

    // --- check_claude_running tests ---

    #[test]
    fn test_claude_running_with_shell_prompt_bira() {
        // Bira theme shell prompt means Claude exited
        let output = "some output\nold tokens stuff\n\u{256e}\u{2500}\u{2500}\n\u{2570}\u{2500}$ ";
        assert!(!check_claude_running(output));
    }

    #[test]
    fn test_claude_running_with_bira_dollar_prompt() {
        // Bira theme: ╰─$
        let output = "some output\n\u{2570}\u{2500}$ ";
        assert!(!check_claude_running(output));
    }

    #[test]
    fn test_claude_running_with_arrow_prompt() {
        // Arrow prompt (robbyrussell): ➜
        let output = "some output\n\u{279c} ~ ";
        assert!(!check_claude_running(output));
    }

    #[test]
    fn test_claude_running_with_status_bar() {
        let output = "some output\n50,000 tokens  5 bashes\nContext left until auto-compact: 42%\ncurrent: 2.1.77   latest: 2.1.78";
        assert!(check_claude_running(output));
    }

    #[test]
    fn test_claude_running_no_indicators() {
        // No shell prompt, no Claude indicators — default to true (conservative)
        let output = "some random text\nnothing here";
        assert!(check_claude_running(output));
    }

    #[test]
    fn test_claude_running_shell_prompt_overrides_tokens() {
        // Bira shell prompt takes priority over stale token text in buffer
        let output = "50,000 tokens  latest: 2.1.78\n\u{2570}\u{2500}$ ";
        assert!(!check_claude_running(output));
    }

    #[test]
    fn test_claude_running_percent_in_status_bar_not_shell() {
        // "42%" in status bar should NOT be mistaken for a zsh %  prompt
        let output = "Context left until auto-compact: 42%";
        assert!(check_claude_running(output));
    }

    // --- detect_activity tests ---

    #[test]
    fn test_activity_idle() {
        let output = "some output\nmore output\n\u{276f} ";
        assert_eq!(detect_activity(output), ClaudeActivity::Idle);
    }

    #[test]
    fn test_activity_idle_takes_priority_over_thinking() {
        // Both prompt and thinking indicator present — idle wins
        let output = "\u{273d} Thinking\u{2026} (5s)\n\u{276f} ";
        assert_eq!(detect_activity(output), ClaudeActivity::Idle);
    }

    #[test]
    fn test_activity_idle_takes_priority_over_spinner() {
        let output = "\u{280b} Read(file)\n\u{276f} ";
        assert_eq!(detect_activity(output), ClaudeActivity::Idle);
    }

    #[test]
    fn test_activity_thinking_standard() {
        let output =
            "previous output\n  \u{273d} Thinking\u{2026} (12s \u{00b7} \u{2193} 384 tokens)";
        assert_eq!(detect_activity(output), ClaudeActivity::Thinking);
    }

    #[test]
    fn test_activity_thinking_honking() {
        let output = "some stuff\n  \u{273d} Honking\u{2026} (44s \u{00b7} \u{2193} 384 tokens)";
        assert_eq!(detect_activity(output), ClaudeActivity::Thinking);
    }

    #[test]
    fn test_activity_thinking_pondering() {
        let output = "line1\n\u{273d} Pondering\u{2026} (2s)";
        assert_eq!(detect_activity(output), ClaudeActivity::Thinking);
    }

    #[test]
    fn test_activity_thinking_273b_flowing() {
        // U+273B (✻) is also used as thinking indicator, not just completion
        let output = "some output\n\u{273b} Flowing\u{2026} (45s \u{00b7} \u{2193} 377 tokens)";
        assert_eq!(detect_activity(output), ClaudeActivity::Thinking);
    }

    #[test]
    fn test_activity_thinking_273b_with_separator_and_prompt() {
        // Real capture: ✻ Flowing… with separator + prompt below
        let output = "\u{25cf} Some bullet\n\u{273b} Flowing\u{2026} (45s)\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\u{276f} \n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n  -- INSERT --";
        assert_eq!(detect_activity(output), ClaudeActivity::Thinking);
    }

    #[test]
    fn test_activity_thinking_takes_priority_over_spinner() {
        // Both thinking and spinner — thinking wins
        let output = "\u{280b} Bash(cmd)\n\u{273d} Thinking\u{2026} (5s)";
        assert_eq!(detect_activity(output), ClaudeActivity::Thinking);
    }

    #[test]
    fn test_activity_tool_running_read() {
        let output = "output\n\u{280b} Read(~/some/file.rs)";
        assert_eq!(detect_activity(output), ClaudeActivity::ToolRunning);
    }

    #[test]
    fn test_activity_tool_running_bash() {
        let output = "output\n\u{2819} Bash(cargo test)";
        assert_eq!(detect_activity(output), ClaudeActivity::ToolRunning);
    }

    #[test]
    fn test_activity_tool_running_various_spinners() {
        // Test each spinner character
        for &spinner in SPINNER_CHARS {
            let output = format!("output\n{} SomeTool(arg)", spinner);
            assert_eq!(
                detect_activity(&output),
                ClaudeActivity::ToolRunning,
                "spinner {:?} should be detected",
                spinner,
            );
        }
    }

    #[test]
    fn test_activity_writing_no_prompt() {
        // Writing is only detected when prompt is NOT visible (pushed off screen)
        let output = "some context\n\u{25cf} Here is some output being streamed";
        assert_eq!(detect_activity(output), ClaudeActivity::Writing);
    }

    #[test]
    fn test_activity_writing_indented_no_prompt() {
        let output = "context\n  \u{25cf} Indented bullet point";
        assert_eq!(detect_activity(output), ClaudeActivity::Writing);
    }

    #[test]
    fn test_activity_writing_multiple_bullets_no_prompt() {
        let output = "\u{25cf} First point\n\u{25cf} Second point\n\u{25cf} Third point";
        assert_eq!(detect_activity(output), ClaudeActivity::Writing);
    }

    #[test]
    fn test_activity_bullets_with_prompt_no_completion_is_writing() {
        // Bullets visible + prompt visible below separator + NO completion indicator
        // = Writing (could be active mid-workflow or stale; daemon debounces)
        let output = "\u{25cf} Some old output\n\u{25cf} More output\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\u{276f} \n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n  -- INSERT --";
        assert_eq!(detect_activity(output), ClaudeActivity::Writing);
    }

    #[test]
    fn test_activity_bullets_with_prompt_and_completion_is_idle() {
        // Bullets visible + prompt visible + completion indicator ("Brewed for") = Idle
        let output = "\u{25cf} Some output\n\u{273b} Brewed for 12s\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\u{276f} \n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n  -- INSERT --";
        assert_eq!(detect_activity(output), ClaudeActivity::Idle);
    }

    #[test]
    fn test_activity_stale_thinking_with_completion_is_idle() {
        // Stale thinking indicator ("✽ Thinking… (5s)") still visible in scroll history
        // + completion indicator ("✻ Brewed for 12s") + prompt = Idle.
        // This was the false positive that caused spurious "prolonged thinking" alerts.
        let output = "\u{273d} Thinking\u{2026} (5s)\n\u{25cf} Some output\n\u{273b} Brewed for 12s\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\u{276f} \n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n  -- INSERT --";
        assert_eq!(detect_activity(output), ClaudeActivity::Idle);
    }

    #[test]
    fn test_activity_unknown() {
        let output = "some random text\nnothing recognizable here";
        assert_eq!(detect_activity(output), ClaudeActivity::Unknown);
    }

    #[test]
    fn test_activity_unknown_empty() {
        assert_eq!(detect_activity(""), ClaudeActivity::Unknown);
    }

    #[test]
    fn test_activity_only_checks_last_15_lines() {
        // Prompt in line 1, but 20 lines of other stuff after
        let mut lines = vec!["\u{276f} old prompt"];
        for _ in 0..20 {
            lines.push("busy output line");
        }
        let output = lines.join("\n");
        assert_eq!(detect_activity(&output), ClaudeActivity::Unknown);
    }

    #[test]
    fn test_activity_thinking_within_last_15() {
        let mut lines: Vec<String> = Vec::new();
        for _ in 0..10 {
            lines.push("busy output".to_string());
        }
        lines.push(format!("\u{273d} Thinking\u{2026} (3s)"));
        for _ in 0..3 {
            lines.push(String::new());
        }
        let output = lines.join("\n");
        assert_eq!(detect_activity(&output), ClaudeActivity::Thinking);
    }

    // --- check_lines_for_reauth tests ---

    #[test]
    fn test_reauth_login_url() {
        let output = "Open this URL to login:\nhttps://console.anthropic.com/login";
        assert!(check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_session_expired() {
        let output = "Session expired\nPlease re-login";
        assert!(check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_auth_required() {
        let output = "Authentication required\nPlease run /login";
        assert!(check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_browser_didnt_open() {
        // Current Claude Code login screen
        let output = "Login\n\nBrowser didn't open? Use the url below to sign in (c to copy)\n\nhttps://claude.ai/oauth/authorize?code=true&client_id=abc123\n\nPaste code here if prompted >\n\nEsc to cancel";
        assert!(check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_paste_code() {
        let output = "Paste code here if prompted >";
        assert!(check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_oauth_url() {
        let output = "https://claude.ai/oauth/authorize?code=true&client_id=abc";
        assert!(check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_not_detected_normal_tui() {
        // Normal TUI has "tokens" visible — should NOT trigger
        let output = "57,129 tokens  9 bashes\n\u{276f} ";
        assert!(!check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_not_detected_conversation_about_auth() {
        // Auth words in normal conversation — should NOT trigger (has TUI elements)
        let output = "Let me authenticate to the API\n57,129 tokens  9 bashes\n\u{276f} ";
        assert!(!check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_not_detected_empty() {
        assert!(!check_lines_for_reauth(""));
    }

    #[test]
    fn test_reauth_not_detected_startup() {
        // During startup, pane may show /login command but with TUI elements
        let output = "Type /login to authenticate\n1,234 tokens  0 bashes";
        assert!(!check_lines_for_reauth(output));
    }

    // --- 401 error detection (phase 1 — TUI still visible) ---

    #[test]
    fn test_reauth_401_error_with_tui() {
        // Real 401 error from screenshot — TUI is still visible (tokens in status bar)
        let output = concat!(
            "resume\n",
            "Please run /login · API Error: 401\n",
            "{\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",",
            "\"message\":\"Invalid authentication credentials\"},",
            "\"request_id\":\"req_011CZRQBx7F5yvuN8z9bJTnp\"}\n",
            "\n",
            "❯ \n",
            "-- INSERT --  871864 tokens"
        );
        assert!(check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_401_api_error() {
        let output = "API Error: 401\n57,129 tokens  9 bashes\n❯ ";
        assert!(check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_authentication_error_json() {
        let output = "\"authentication_error\"\n57,129 tokens  9 bashes\n❯ ";
        assert!(check_lines_for_reauth(output));
    }

    #[test]
    fn test_reauth_not_detected_conversation_about_401() {
        // Conversation ABOUT 401 errors shouldn't trigger — no actual error JSON
        let output = "The server returns a 401 status code when auth is invalid\n57,129 tokens  9 bashes\n❯ ";
        assert!(!check_lines_for_reauth(output));
    }

    // --- extract_login_url tests ---

    #[test]
    fn test_extract_login_url_basic() {
        let output = "Login\n\nhttps://claude.ai/oauth/authorize?code=true&client_id=abc123\n\nPaste code here";
        let url = extract_login_url(output);
        assert_eq!(
            url,
            Some("https://claude.ai/oauth/authorize?code=true&client_id=abc123".to_string())
        );
    }

    #[test]
    fn test_extract_login_url_wrapped() {
        // URL wraps across two tmux lines
        let output = "https://claude.ai/oauth/authorize?code=true&client_id=abc123&code_chall\nenge=xyz789&code_challenge_method=S256";
        let url = extract_login_url(output);
        assert_eq!(url, Some("https://claude.ai/oauth/authorize?code=true&client_id=abc123&code_challenge=xyz789&code_challenge_method=S256".to_string()));
    }

    #[test]
    fn test_extract_login_url_none() {
        let output = "Session expired\nPlease re-login";
        assert_eq!(extract_login_url(output), None);
    }

    #[test]
    fn test_activity_display() {
        assert_eq!(format!("{}", ClaudeActivity::Idle), "idle");
        assert_eq!(format!("{}", ClaudeActivity::Thinking), "thinking");
        assert_eq!(format!("{}", ClaudeActivity::ToolRunning), "tool_running");
        assert_eq!(format!("{}", ClaudeActivity::Writing), "writing");
        assert_eq!(format!("{}", ClaudeActivity::Unknown), "unknown");
    }

    // --- exit teardown detection tests ---

    #[test]
    fn test_exit_teardown_goodbye() {
        let output = "some output\nGoodbye!\n";
        assert!(check_lines_for_exit_teardown(output));
    }

    #[test]
    fn test_exit_teardown_background_stopped() {
        let output = "some output\nGoodbye!\nBackground command was stopped: signal-wait\nBackground command was stopped: torrent-wait\n";
        assert!(check_lines_for_exit_teardown(output));
    }

    #[test]
    fn test_exit_teardown_only_background_stopped() {
        let output = "some output\nBackground command was stopped: signal-wait\n";
        assert!(check_lines_for_exit_teardown(output));
    }

    #[test]
    fn test_no_exit_teardown_normal_output() {
        let output = "Claude Code is running\nTokens: 3000\nBashes: 0";
        assert!(!check_lines_for_exit_teardown(output));
    }

    #[test]
    fn test_no_exit_teardown_goodbye_in_content() {
        // "Goodbye!" must be the entire trimmed line, not part of a sentence
        let output = "He said Goodbye! to his friend\nTokens: 3000";
        assert!(!check_lines_for_exit_teardown(output));
    }
}
