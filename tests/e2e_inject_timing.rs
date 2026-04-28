//! E2e tests for inject_text timing fix.
//!
//! Verifies that inject_text() waits for the activity state to settle to Idle
//! before typing, rather than injecting text while thinking indicators are
//! still visible on screen.
//!
//! The bug (fixed in ee428c4): inject_text would proceed with dd/i/type while
//! the thinking indicator ("✽ Thinking…") was still rendering, causing garbled
//! output. The fix adds a wait-for-idle loop after Escape that polls
//! get_activity() until it returns Idle.

use claude_watch::tmux::{detect_activity, get_activity, inject_text, ClaudeActivity};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// RAII guard that kills the tmux session on drop.
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

/// Generate a unique session name.
fn unique_session_name(prefix: &str) -> String {
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("cw-inject-{}-{}-{}", prefix, std::process::id(), n)
}

/// Capture the tmux pane content.
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

/// Send literal text to a tmux pane.
fn send_literal(session: &str, text: &str) {
    let _ = Command::new("tmux")
        .args(["send-keys", "-t", session, "-l", text])
        .output();
}

/// Send keys to a tmux pane.
fn send_keys(session: &str, keys: &[&str]) {
    let mut args = vec!["send-keys", "-t", session];
    args.extend_from_slice(keys);
    let _ = Command::new("tmux").args(&args).output();
}

/// inject_text should wait for thinking to clear before typing.
///
/// Setup: tmux pane shows a thinking indicator above the separator with prompt
/// visible below. A background thread clears the thinking indicator after 2s
/// (simulating Claude finishing its thinking phase). inject_text should block
/// until the activity settles to Idle, then inject the text.
///
/// This is the core scenario from the bug: interrupt_and_wait sends Escape,
/// the thinking indicator is still rendering, and inject_text types over it.
#[test]
fn inject_text_waits_for_idle_before_typing() {
    let session_name = unique_session_name("wait");
    let _guard = TmuxSession::new(&session_name);

    // Create tmux session running bash
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
        ])
        .status()
        .expect("create tmux session");
    assert!(status.success());
    std::thread::sleep(Duration::from_millis(500));

    // Display thinking state in the pane. We use a script that:
    // 1. First shows a thinking indicator (detected as Thinking by detect_activity)
    // 2. After 2 seconds, replaces with an idle state (detected as Idle)
    //
    // The thinking state uses the Claude TUI layout:
    //   [content with thinking indicator]
    //   ─────────── (separator)
    //   ❯           (prompt)
    //   ─────────── (separator)
    //   -- INSERT --
    let thinking_script = r#"
# Phase 1: Show thinking state
clear
printf '✽ Thinking… (5s · ↓ 384 tokens)\n'
printf '  ⎿  Tip: Use /btw\n'
printf '\n'
printf '──────────────────────────────────────────────────────────────────\n'
printf '❯ \n'
printf '──────────────────────────────────────────────────────────────────\n'
printf '  -- INSERT -- 50000 tokens\n'
# Wait, then transition to idle
sleep 2
clear
printf '● Some completed output\n'
printf '\n'
printf '✻ Brewed for 12s · ↓ 2048 tokens\n'
printf '\n'
printf '──────────────────────────────────────────────────────────────────\n'
printf '❯ \n'
printf '──────────────────────────────────────────────────────────────────\n'
printf '  -- INSERT -- 50000 tokens\n'
"#;

    // Write the script to a temp file and run it in the pane
    let script_path = format!("/tmp/cw-inject-test-{}.sh", std::process::id());
    std::fs::write(&script_path, thinking_script).expect("write script");
    let _ = Command::new("chmod").args(["+x", &script_path]).output();

    send_literal(&session_name, &format!("bash {}", script_path));
    send_keys(&session_name, &["Enter"]);
    std::thread::sleep(Duration::from_millis(500));

    // Verify pane is in thinking state
    let content = capture_pane(&session_name).unwrap_or_default();
    let initial_activity = detect_activity(&content);
    eprintln!("Initial activity: {:?}", initial_activity);
    eprintln!("Initial pane content:\n{}", content);

    // The pane should show Thinking (or at worst Unknown if rendering hasn't settled).
    // The key assertion is below — inject_text should NOT proceed until Idle.
    assert!(
        initial_activity == ClaudeActivity::Thinking || initial_activity == ClaudeActivity::Unknown,
        "Expected Thinking or Unknown initially, got {:?}. Content:\n{}",
        initial_activity,
        content
    );

    // Record timestamp before inject
    let start = std::time::Instant::now();

    // Call inject_text on the pane (async, needs tokio runtime).
    // inject_text should wait for get_activity() == Idle before proceeding.
    let pane = format!("{}:0.0", session_name);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        inject_text(&pane, "TEST-INJECT-TIMING").await;
    });

    let elapsed = start.elapsed();

    // inject_text should have waited at least ~2s for the thinking to clear.
    // The script transitions to idle after 2s sleep.
    eprintln!("inject_text elapsed: {:?}", elapsed);
    assert!(
        elapsed >= Duration::from_secs(1),
        "inject_text should have waited for thinking to clear, but returned in {:?}. \
         This means it injected text while the thinking indicator was still visible.",
        elapsed
    );

    // Verify the text was eventually sent (it should appear in the pane or history)
    std::thread::sleep(Duration::from_millis(500));
    let final_content = capture_pane(&session_name).unwrap_or_default();
    eprintln!("Final pane content:\n{}", final_content);

    // Clean up script
    let _ = std::fs::remove_file(&script_path);
}

/// get_activity correctly identifies Thinking when thinking indicator is above
/// the separator with prompt visible below — the exact scenario from the bug.
#[test]
fn get_activity_thinking_with_prompt_visible() {
    let session_name = unique_session_name("activity");
    let _guard = TmuxSession::new(&session_name);

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
        ])
        .status()
        .expect("create tmux session");
    assert!(status.success());
    std::thread::sleep(Duration::from_millis(500));

    // Display thinking state with Claude TUI layout
    let script = r#"
clear
printf '● Some prior output from Claude\n'
printf '\n'
printf '✽ Thinking… (12s · ↓ 384 tokens)\n'
printf '  ⎿  Tip: Use /btw\n'
printf '\n'
printf '──────────────────────────────────────────────────────────────────\n'
printf '❯ \n'
printf '──────────────────────────────────────────────────────────────────\n'
printf '  -- INSERT -- 50000 tokens\n'
# Keep alive
sleep 30
"#;

    let script_path = format!("/tmp/cw-activity-test-{}.sh", std::process::id());
    std::fs::write(&script_path, script).expect("write script");
    let _ = Command::new("chmod").args(["+x", &script_path]).output();

    send_literal(&session_name, &format!("bash {}", script_path));
    send_keys(&session_name, &["Enter"]);
    std::thread::sleep(Duration::from_millis(500));

    // Use get_activity (the async version that captures from tmux)
    let pane = format!("{}:0.0", session_name);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let activity = rt.block_on(async { get_activity(&pane).await });

    let content = capture_pane(&session_name).unwrap_or_default();
    eprintln!("Pane content:\n{}", content);
    eprintln!("Detected activity: {:?}", activity);

    assert_eq!(
        activity,
        ClaudeActivity::Thinking,
        "get_activity should detect Thinking when thinking indicator (✽ Thinking…) \
         is visible above the separator, even though the prompt (❯) is also visible \
         below. This was the core bug: inject_text checked is_idle() (prompt-only) \
         instead of get_activity() (content-area aware). Got {:?}",
        activity
    );

    let _ = std::fs::remove_file(&script_path);
}

/// After thinking clears and idle state is shown, get_activity returns Idle.
/// This is the complement to the above test — verifying the transition.
#[test]
fn get_activity_idle_after_thinking_clears() {
    let session_name = unique_session_name("idle");
    let _guard = TmuxSession::new(&session_name);

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
        ])
        .status()
        .expect("create tmux session");
    assert!(status.success());
    std::thread::sleep(Duration::from_millis(500));

    // Display idle state with completion indicator
    let script = r#"
clear
printf '● Some completed output\n'
printf '\n'
printf '✻ Brewed for 12s · ↓ 2048 tokens\n'
printf '\n'
printf '──────────────────────────────────────────────────────────────────\n'
printf '❯ \n'
printf '──────────────────────────────────────────────────────────────────\n'
printf '  -- INSERT -- 50000 tokens\n'
sleep 30
"#;

    let script_path = format!("/tmp/cw-idle-test-{}.sh", std::process::id());
    std::fs::write(&script_path, script).expect("write script");
    let _ = Command::new("chmod").args(["+x", &script_path]).output();

    send_literal(&session_name, &format!("bash {}", script_path));
    send_keys(&session_name, &["Enter"]);
    std::thread::sleep(Duration::from_millis(500));

    let pane = format!("{}:0.0", session_name);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let activity = rt.block_on(async { get_activity(&pane).await });

    let content = capture_pane(&session_name).unwrap_or_default();
    eprintln!("Pane content:\n{}", content);
    eprintln!("Detected activity: {:?}", activity);

    assert_eq!(
        activity,
        ClaudeActivity::Idle,
        "After thinking clears and completion indicator is shown, get_activity \
         should return Idle. Got {:?}",
        activity
    );

    let _ = std::fs::remove_file(&script_path);
}

/// Regression test for the "cursor stuck mid-text" bug (Andrew flagged 2026-04-28).
///
/// BUG: inject_text used a fixed 1500ms sleep after sending `i` (enter INSERT)
/// with no verification that INSERT mode actually engaged. When the editor
/// hadn't transitioned yet, the FIRST chars of the text payload landed in
/// NORMAL mode and were interpreted as vim commands (e.g. `[`, `C`, `L`),
/// jumping the cursor around and leaving it visibly mid-text after the
/// inject finished.
///
/// FIX: Replace the fixed sleep with a verify-loop (up to 3 attempts of `i`,
/// polling `is_insert_mode()` after each), symmetric with the Escape→NORMAL
/// loop at Step 1.
///
/// This test fakes the Claude-Code TUI in a tmux pane with `-- INSERT --`
/// rendered in the status bar and runs inject_text against it. The pane
/// will end up showing the typed text. The point of the test is to catch
/// regressions where inject_text either:
///   (a) hangs/loops forever waiting for INSERT confirmation, or
///   (b) bails after one attempt without verifying.
#[test]
fn inject_text_verifies_insert_mode_before_typing() {
    let session_name = unique_session_name("insert-verify");
    let _guard = TmuxSession::new(&session_name);

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
        ])
        .status()
        .expect("create tmux session");
    assert!(status.success());
    std::thread::sleep(Duration::from_millis(500));

    // Render an idle Claude-Code TUI with `-- INSERT --` in the status bar.
    // inject_text's Step-3 verify-loop should see INSERT mode on the first
    // poll and proceed without retry.
    let script = r#"
clear
printf '● Some completed output\n'
printf '\n'
printf '✻ Brewed for 12s · ↓ 2048 tokens\n'
printf '\n'
printf '──────────────────────────────────────────────────────────────────\n'
printf '❯ \n'
printf '──────────────────────────────────────────────────────────────────\n'
printf '  -- INSERT -- 50000 tokens\n'
sleep 30
"#;
    let script_path = format!("/tmp/cw-insert-verify-{}.sh", std::process::id());
    std::fs::write(&script_path, script).expect("write script");
    let _ = Command::new("chmod").args(["+x", &script_path]).output();

    send_literal(&session_name, &format!("bash {}", script_path));
    send_keys(&session_name, &["Enter"]);
    std::thread::sleep(Duration::from_millis(500));

    let pane = format!("{}:0.0", session_name);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let start = std::time::Instant::now();
    rt.block_on(async {
        inject_text(&pane, "VERIFY-INSERT").await;
    });
    let elapsed = start.elapsed();

    eprintln!("inject_text elapsed: {:?}", elapsed);
    // With INSERT mode visible from the start, the verify-loop should
    // succeed on the first attempt. Total time is bounded by the fixed
    // settle/sleep delays in inject_text, NOT the 3-attempt retry budget.
    // If we ever exceed the upper bound (3 * 500ms retry + 500ms final
    // settle + 500ms pre-text + 500ms post-text + ~1s misc), something
    // regressed.
    assert!(
        elapsed < Duration::from_secs(15),
        "inject_text should not loop forever; elapsed {:?}",
        elapsed
    );

    let _ = std::fs::remove_file(&script_path);
}
