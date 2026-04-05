//! End-to-end tests that launch a LIVE Claude Code instance in a test tmux session,
//! capture pane output during various activity states, and verify detect_activity().
//!
//! These tests are `#[ignore]` — run with:
//!   cargo test -- --ignored
//!   cargo test e2e_live -- --ignored
//!
//! Raw pane captures are saved to /tmp/claude-watch-e2e/ for manual inspection.

use claude_watch::tmux::{detect_activity, ClaudeActivity};
use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Count occurrences of each activity in a captures list.
fn count_activities(captures: &[(String, ClaudeActivity)]) -> (usize, usize, usize, usize, usize) {
    let mut idle = 0;
    let mut thinking = 0;
    let mut tool = 0;
    let mut writing = 0;
    let mut unknown = 0;
    for (_, activity) in captures {
        match activity {
            ClaudeActivity::Idle => idle += 1,
            ClaudeActivity::Thinking => thinking += 1,
            ClaudeActivity::ToolRunning => tool += 1,
            ClaudeActivity::Writing => writing += 1,
            ClaudeActivity::Unknown => unknown += 1,
        }
    }
    (idle, thinking, tool, writing, unknown)
}

/// RAII guard that kills the tmux session on drop (even on panic).
struct TmuxSession {
    name: String,
}

impl TmuxSession {
    fn new(name: &str) -> Self {
        TmuxSession {
            name: name.to_string(),
        }
    }
}

impl Drop for TmuxSession {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &self.name])
            .output();
    }
}

/// Capture the tmux pane content as a string.
fn capture_pane(session: &str) -> Option<String> {
    let output = Command::new("tmux")
        .args(["capture-pane", "-t", session, "-p"])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        None
    }
}

/// Send keys to the tmux session.
fn send_keys(session: &str, keys: &[&str]) {
    let mut args = vec!["send-keys", "-t", session];
    args.extend_from_slice(keys);
    let _ = Command::new("tmux").args(&args).output();
}

/// Generate a unique session name using PID + atomic counter (safe for parallel tests).
fn unique_session_name() -> String {
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("cw-test-{}-{}", std::process::id(), n)
}

/// Save a capture to disk for inspection.
fn save_capture(dir: &str, label: &str, index: usize, content: &str) {
    let path = format!("{}/{}_{:04}.txt", dir, label, index);
    let _ = fs::write(&path, content);
}

/// Wait for Claude Code to initialize (poll for ❯ prompt), with timeout.
/// Returns true if initialized, false if timed out.
fn wait_for_init(session: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Some(content) = capture_pane(session) {
            if content.contains('\u{276f}') {
                return true;
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
    false
}

/// Core test harness: launches Claude Code, sends a prompt, captures pane output
/// during the thinking/working phase, and returns all (capture_text, detected_activity) pairs.
///
/// Also saves all captures to /tmp/claude-watch-e2e/<run_id>/.
fn run_live_capture_session() -> (Vec<(String, ClaudeActivity)>, String) {
    let session_name = unique_session_name();
    let _guard = TmuxSession::new(&session_name);

    let capture_dir = format!("/tmp/claude-watch-e2e/{}", session_name);
    fs::create_dir_all(&capture_dir).expect("failed to create capture dir");

    // Create tmux session with Claude Code
    let status = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &session_name,
            "-x",
            "120",
            "-y",
            "40",
            "/home/user/.local/bin/claude",
            "--dangerously-skip-permissions",
        ])
        .status()
        .expect("failed to create tmux session");
    assert!(status.success(), "tmux new-session failed");

    // Wait for Claude Code to initialize
    assert!(
        wait_for_init(&session_name, Duration::from_secs(60)),
        "Claude Code did not initialize within 60s. Check captures in {}",
        capture_dir
    );

    // Capture the initial idle state
    let mut captures: Vec<(String, ClaudeActivity)> = Vec::new();
    if let Some(content) = capture_pane(&session_name) {
        let activity = detect_activity(&content);
        save_capture(&capture_dir, "00_init", 0, &content);
        captures.push((content, activity));
    }

    // Send a prompt that will trigger thinking
    send_keys(
        &session_name,
        &["explain the theory of relativity in detail", "Enter"],
    );

    // Poll rapidly during the thinking/working phase
    let poll_start = Instant::now();
    let max_duration = Duration::from_secs(90); // generous timeout
    let mut index = 0;

    loop {
        if poll_start.elapsed() > max_duration {
            break;
        }

        if let Some(content) = capture_pane(&session_name) {
            let activity = detect_activity(&content);
            save_capture(&capture_dir, "01_working", index, &content);
            let is_idle = activity == ClaudeActivity::Idle;
            captures.push((content, activity));
            index += 1;

            // If we see idle again after some working captures, we're done
            // (need at least a few captures first to avoid the initial idle)
            if index > 10 && is_idle {
                break;
            }
        }

        thread::sleep(Duration::from_millis(200));
    }

    // Final idle state capture
    thread::sleep(Duration::from_secs(1));
    if let Some(content) = capture_pane(&session_name) {
        let activity = detect_activity(&content);
        save_capture(&capture_dir, "02_final", 0, &content);
        captures.push((content, activity));
    }

    // Send Escape to clean up, then /exit
    send_keys(&session_name, &["Escape"]);
    thread::sleep(Duration::from_millis(500));
    send_keys(&session_name, &["/exit", "Enter"]);
    thread::sleep(Duration::from_secs(2));

    (captures, capture_dir)
}

/// After fix: at least some captures during the thinking phase should return Thinking.
/// detect_activity() now splits at the separator line and only checks the content area
/// above it, so the always-visible ❯ prompt no longer masks activity indicators.
#[test]
#[ignore]
fn live_thinking_should_be_detected() {
    let (captures, capture_dir) = run_live_capture_session();

    assert!(
        captures.len() > 5,
        "Expected at least 5 captures, got {}. Check {}",
        captures.len(),
        capture_dir
    );

    let (idle_count, thinking_count, tool_count, writing_count, unknown_count) =
        count_activities(&captures);

    eprintln!("=== Expected Behavior Test ===");
    eprintln!("Capture directory: {}", capture_dir);
    eprintln!("Total captures: {}", captures.len());
    eprintln!("  Idle: {}", idle_count);
    eprintln!("  Thinking: {}", thinking_count);
    eprintln!("  ToolRunning: {}", tool_count);
    eprintln!("  Writing: {}", writing_count);
    eprintln!("  Unknown: {}", unknown_count);

    // CORRECT BEHAVIOR: During the thinking phase, detect_activity should return
    // Thinking at least sometimes. The thinking indicator (✽ Thinking…) IS present
    // in the pane output — it's just overshadowed by the ❯ prompt check.
    assert!(
        thinking_count >= 1,
        "Expected at least 1 Thinking detection during Claude's thinking phase, \
         got 0. The ❯ prompt is masking the thinking state. \
         All {} captures: Idle={}, Thinking={}, ToolRunning={}, Writing={}, Unknown={}. Check {}",
        captures.len(),
        idle_count, thinking_count, tool_count, writing_count, unknown_count,
        capture_dir
    );
}

/// BUG REPRO: After Claude finishes responding, ● bullet points from the completed
/// response remain in the content area above the separator. detect_activity() sees
/// them and returns Writing instead of Idle.
///
/// This test sends a short prompt, waits for Claude to FINISH responding (no thinking
/// indicators for several seconds, prompt visible), then asserts Idle. It should FAIL
/// with current code because the ● bullets from the response trigger Writing.
#[test]
#[ignore]
fn live_idle_after_response_should_be_idle() {
    let session_name = unique_session_name();
    let _guard = TmuxSession::new(&session_name);

    let capture_dir = format!("/tmp/claude-watch-e2e/{}", session_name);
    fs::create_dir_all(&capture_dir).expect("failed to create capture dir");

    // Create tmux session with Claude Code
    let status = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &session_name,
            "-x",
            "120",
            "-y",
            "40",
            "/home/user/.local/bin/claude",
            "--dangerously-skip-permissions",
        ])
        .status()
        .expect("failed to create tmux session");
    assert!(status.success(), "tmux new-session failed");

    // Wait for Claude Code to initialize
    assert!(
        wait_for_init(&session_name, Duration::from_secs(60)),
        "Claude Code did not initialize within 60s"
    );

    // Send a SHORT prompt so Claude responds quickly
    send_keys(&session_name, &["say hi", "Enter"]);

    // Wait for Claude to finish responding: poll until we see the prompt AND
    // no thinking indicators for at least 5 consecutive seconds
    let start = Instant::now();
    let max_wait = Duration::from_secs(120);
    let mut stable_since: Option<Instant> = None;

    while start.elapsed() < max_wait {
        if let Some(content) = capture_pane(&session_name) {
            // Check if Claude appears done: prompt visible, no active thinking
            let has_prompt = content.contains('\u{276f}');
            let has_thinking = content.contains('\u{2026}')
                && (content.contains('\u{273d}')
                    || content.contains('\u{2722}')
                    || content.lines().any(|l| l.trim().starts_with("* ") && l.contains('\u{2026}')));
            let has_spinner = SPINNER_CHARS_FOR_TEST
                .iter()
                .any(|&c| content.contains(c));

            if has_prompt && !has_thinking && !has_spinner {
                match stable_since {
                    None => stable_since = Some(Instant::now()),
                    Some(t) if t.elapsed() >= Duration::from_secs(5) => break,
                    _ => {}
                }
            } else {
                stable_since = None;
            }
        }
        thread::sleep(Duration::from_millis(500));
    }

    assert!(
        stable_since.is_some(),
        "Claude never settled to idle after responding. Check {}",
        capture_dir
    );

    // Now capture and test detect_activity — Claude should be Idle
    let content = capture_pane(&session_name).expect("failed to capture pane");
    save_capture(&capture_dir, "idle_after_response", 0, &content);

    let activity = detect_activity(&content);
    eprintln!("=== Idle After Response Bug Repro ===");
    eprintln!("Capture directory: {}", capture_dir);
    eprintln!("Detected activity: {:?}", activity);
    eprintln!(
        "Content has ● bullets: {}",
        content.lines().any(|l| l.trim_start().starts_with('\u{25cf}'))
    );
    eprintln!(
        "Content has prompt: {}",
        content.contains('\u{276f}')
    );

    assert_eq!(
        activity,
        ClaudeActivity::Idle,
        "BUG REPRO: After Claude finished responding, detect_activity() returned {:?} \
         instead of Idle. The ● bullet points from the completed response are being \
         mistaken for active Writing. Check {}",
        activity,
        capture_dir
    );

    // Clean up
    send_keys(&session_name, &["Escape"]);
    thread::sleep(Duration::from_millis(500));
    send_keys(&session_name, &["/exit", "Enter"]);
    thread::sleep(Duration::from_secs(2));
}

/// Spinner characters for the e2e test polling (mirrors SPINNER_CHARS from tmux.rs).
const SPINNER_CHARS_FOR_TEST: &[char] = &[
    '\u{280b}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283c}',
    '\u{2834}', '\u{2826}', '\u{2827}', '\u{2807}', '\u{280f}',
];

/// Verify that the initial idle state IS correctly detected before sending any prompt.
/// This is the one case where detect_activity works correctly — when Claude is
/// genuinely idle and waiting for input.
#[test]
#[ignore]
fn live_initial_idle_is_correct() {
    let session_name = unique_session_name();
    let _guard = TmuxSession::new(&session_name);

    let capture_dir = format!("/tmp/claude-watch-e2e/{}", session_name);
    fs::create_dir_all(&capture_dir).expect("failed to create capture dir");

    // Create tmux session with Claude Code
    let status = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &session_name,
            "-x",
            "120",
            "-y",
            "40",
            "/home/user/.local/bin/claude",
            "--dangerously-skip-permissions",
        ])
        .status()
        .expect("failed to create tmux session");
    assert!(status.success(), "tmux new-session failed");

    // Wait for Claude Code to initialize
    assert!(
        wait_for_init(&session_name, Duration::from_secs(60)),
        "Claude Code did not initialize within 60s"
    );

    // Give it a moment to settle
    thread::sleep(Duration::from_secs(2));

    // Capture and verify idle
    let content = capture_pane(&session_name).expect("failed to capture pane");
    save_capture(&capture_dir, "idle_check", 0, &content);

    let activity = detect_activity(&content);
    eprintln!("Initial idle capture saved to {}", capture_dir);
    eprintln!("Detected activity: {:?}", activity);
    eprintln!(
        "Pane contains ❯: {}",
        content.contains('\u{276f}')
    );

    assert_eq!(
        activity,
        ClaudeActivity::Idle,
        "Initial idle state should be detected as Idle. Check {}",
        capture_dir
    );

    // Clean up
    send_keys(&session_name, &["/exit", "Enter"]);
    thread::sleep(Duration::from_secs(2));
}
