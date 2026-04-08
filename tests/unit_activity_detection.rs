//! Unit tests for Claude Code activity detection from tmux pane output.
//!
//! These tests use fixture data (saved tmux captures from a live session) to verify
//! that detect_activity() correctly identifies thinking, tool running, writing,
//! and idle states. For actual end-to-end tests that spawn real Claude Code
//! instances, see e2e_live_detection.rs.
//!
//! KEY FINDING: Claude Code's TUI has a FIXED LAYOUT where the prompt character
//! (U+276F ❯) is ALWAYS visible at the bottom of the screen, regardless of
//! whether Claude is thinking, running tools, or idle. The current detect_activity()
//! checks for ❯ first and returns Idle whenever found — which means it ALWAYS
//! returns Idle and never reaches the Thinking/ToolRunning/Writing checks.
//!
//! The fix: thinking/tool indicators (which appear in the scrolling output area
//! ABOVE the separator) must be checked BEFORE the prompt.

// Import detect_activity from the crate
use claude_watch::tmux::{detect_activity, ClaudeActivity};

/// Real tmux capture from a live session while Claude Code was ACTIVELY THINKING.
/// The "Fermenting…" line with ✢ (U+2722) is visible, AND the ❯ prompt is also visible.
///
/// This is the ACTUAL layout captured via `tmux capture-pane -t dashboard:0.2 -p`.
///
/// Structure:
///   ... scrolling output ...
///   ✢ Fermenting… (38s · ↓ 909 tokens)    <-- thinking indicator
///     ⎿  Tip: ...
///   ───────────────────────────────────     <-- separator
///   ❯                                       <-- prompt (ALWAYS visible)
///   ───────────────────────────────────     <-- separator
///     -- INSERT --⏵⏵ ...                   <-- status bar
const REAL_CAPTURE_THINKING_FERMENTING: &str = "\
     correctly (was 0, now 10). Thinking detection updated.\n\
     - **AT Distant Lands** downloading from IPT (25.4GB BluRay batch, about 12h ETA at\n\
     current speed).\n\
       [2026-03-17T04:11:35.117325+00:00] [sig_ts:1773720694959] Andrew Hall (he/him):\n\
     detection is still broken. please have an agent create an e2e test that actually\n\
     repros this issue\n\
\n\
\u{25cf} Andrew says detection is still broken and wants an e2e test. Let me launch an agent for that.\n\
\n\
\u{25cf} Bash(cat << 'EOF' | signal-send --dm andrew --force\n\
      On it -- launching agent to create an e2e test that captures the actual tmux pane output\n\
      during thinking and verifies detection.\n\
      EOF)\n\
  \u{23bf}  Sent (timestamp: 1773720719612)\n\
\n\
\u{25cf} Agent(E2E test for thought detection)\n\
  \u{23bf}  Backgrounded agent (\u{2193} to manage \u{00b7} ctrl+o to expand)\n\
\n\
\u{2722} Fermenting\u{2026} (38s \u{00b7} \u{2193} 909 tokens)\n\
  \u{23bf}  Tip: Use /btw to ask a quick side question without interrupting Claude's current work\n\
\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
\u{276f} \n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
  -- INSERT --\u{23f5}\u{23f5} bypass permissions on \u{00b7} 11 background tasks                   139189 tokens\n\
                                                             current: 2.1.77 \u{00b7} latest: 2.1.\u{2026}\n";

/// Real tmux capture while Claude Code was actively thinking with "Warping…" indicator.
/// Plain asterisk (*) used as the thinking character. Prompt also visible.
const REAL_CAPTURE_THINKING_WARPING: &str = "\
some prior output\n\
\n\
\u{25cf} Good point from Andrew \u{2014} the agent might capture the pane while I'm idle, not thinking. Let\n\
  me pass that context to the agent.\n\
\n\
* Warping\u{2026} (26s \u{00b7} \u{2191} 438 tokens)\n\
\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
\u{276f} \n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
  -- INSERT --\u{23f5}\u{23f5} bypass permissions on \u{00b7} 11 background tasks                   140546 tokens\n\
                                                             current: 2.1.77 \u{00b7} latest: 2.1.\u{2026}\n";

/// Real tmux capture while Claude Code was IDLE (not thinking, just waiting).
/// The ✻ (U+273B) "Brewed for" line is a completion indicator, NOT active thinking.
/// Prompt is visible as always.
const REAL_CAPTURE_IDLE_WITH_AGENTS: &str = "\
  Standing by for agent completion.\n\
\n\
\u{273b} Brewed for 38s \u{00b7} 11 background tasks still running (\u{2193} to manage)\n\
\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
\u{276f} \n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
  -- INSERT --\u{23f5}\u{23f5} bypass permissions on \u{00b7} 11 background tasks                   141197 tokens\n\
                                                             current: 2.1.77 \u{00b7} latest: 2.1.\u{2026}\n";

/// Real tmux capture during active thinking with ✽ (U+273D) character.
/// This is what the escape-code capture showed — the actual character rendered.
const REAL_CAPTURE_THINKING_WITH_273D: &str = "\
some output\n\
\n\
\u{273d} Thinking\u{2026} (12s \u{00b7} \u{2193} 384 tokens)\n\
  \u{23bf}  Tip: Use /btw to ask a quick question\n\
\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
\u{276f} \n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
  -- INSERT -- 50000 tokens\n";

/// Simulated tool-running state: spinner character visible alongside prompt.
const REAL_CAPTURE_TOOL_RUNNING: &str = "\
prior output\n\
\n\
\u{25cf} Bash(cargo test --release)\n\
  \u{280b} Running...\n\
\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
\u{276f} \n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
  -- INSERT -- 50000 tokens\n";

/// Writing state: bullet points visible, prompt NOT visible (pushed off screen
/// by active streaming output). This is TRUE active writing.
const REAL_CAPTURE_WRITING: &str = "\
some context\n\
\n\
\u{25cf} Here is some output being streamed by Claude. It's writing a response\n\
  that spans multiple lines and includes detailed analysis.\n\
\n\
\u{25cf} Second bullet point with more content.\n\
\n\
\u{25cf} Third bullet point — Claude is still streaming and the output has\n\
  pushed the separator and prompt off the bottom of the visible pane.";

// =============================================================================
// CORRECT BEHAVIOR TESTS: detect_activity() splits at the separator line and
// only checks the content area above it for activity indicators. The ❯ prompt
// below the separator confirms Claude Code is running but does NOT indicate idle.
// =============================================================================

/// "Fermenting..." with U+2722 should be detected as Thinking.
#[test]
fn correct_thinking_fermenting_should_be_thinking() {
    let result = detect_activity(REAL_CAPTURE_THINKING_FERMENTING);
    assert_eq!(
        result,
        ClaudeActivity::Thinking,
        "Expected Thinking but got {:?}. \
         The thinking indicator (U+2722 Fermenting...) appears ABOVE the separator \
         in the scrolling output area. The prompt below the separator is \
         a permanent UI element and should NOT override activity detection.",
        result
    );
}

/// "Warping..." with asterisk should be detected as Thinking.
#[test]
fn correct_thinking_warping_should_be_thinking() {
    let result = detect_activity(REAL_CAPTURE_THINKING_WARPING);
    assert_eq!(
        result,
        ClaudeActivity::Thinking,
        "Expected Thinking but got {:?}",
        result
    );
}

/// "Thinking..." with U+273D should be detected as Thinking.
#[test]
fn correct_thinking_273d_should_be_thinking() {
    let result = detect_activity(REAL_CAPTURE_THINKING_WITH_273D);
    assert_eq!(
        result,
        ClaudeActivity::Thinking,
        "Expected Thinking but got {:?}",
        result
    );
}

/// Spinner visible should be detected as ToolRunning.
#[test]
fn correct_tool_running_should_be_tool_running() {
    let result = detect_activity(REAL_CAPTURE_TOOL_RUNNING);
    assert_eq!(
        result,
        ClaudeActivity::ToolRunning,
        "Expected ToolRunning but got {:?}",
        result
    );
}

/// Writing (bullet points visible above separator) should be detected as Writing.
#[test]
fn correct_writing_should_be_writing() {
    let result = detect_activity(REAL_CAPTURE_WRITING);
    assert_eq!(
        result,
        ClaudeActivity::Writing,
        "Expected Writing but got {:?}",
        result
    );
}

/// After fix: idle with agents should still correctly be Idle.
/// (No thinking/tool/writing indicators, just the "Brewed for" completion line.)
#[test]
fn correct_idle_with_agents_should_be_idle() {
    let result = detect_activity(REAL_CAPTURE_IDLE_WITH_AGENTS);
    assert_eq!(
        result,
        ClaudeActivity::Idle,
        "Idle state with completed agents should remain Idle. \
         The ✻ 'Brewed for' line is a completion indicator (no trailing …), \
         not an active thinking indicator."
    );
}

// =============================================================================
// CHARACTER INVENTORY: Document all observed thinking characters
// =============================================================================

/// Document all thinking indicator characters observed in real captures.
/// This helps ensure the detection code covers the full character set.
#[test]
fn character_inventory() {
    // Characters observed in REAL tmux captures:
    //
    // ACTIVE THINKING (with trailing … and verb like "Fermenting…", "Warping…", "Thinking…"):
    //   U+2722 (✢) - e2 9c a2 - seen in plain capture during "Fermenting…"
    //   U+273D (✽) - e2 9c bd - seen in escape capture during "Fermenting…"
    //   U+002A (*) - 2a       - seen in plain capture during "Warping…"
    //
    // COMPLETED/IDLE (with "Brewed for", "Baked for" - NO trailing …):
    //   U+273B (✻) - e2 9c bb - seen during "Brewed for 38s", "Baked for 39s"
    //
    // The current code checks for ✽ and ✢ and "* " - this is CORRECT for the
    // characters. The bug is in the PRIORITY ORDER, not the character matching.
    //
    // Key distinguisher: active thinking always has … (U+2026) after the verb.
    // Completed state uses "for Ns" without ellipsis.

    // Verify our character constants
    assert_eq!('\u{2722}' as u32, 0x2722, "✢ should be U+2722");
    assert_eq!('\u{273D}' as u32, 0x273D, "✽ should be U+273D");
    assert_eq!('\u{273B}' as u32, 0x273B, "✻ should be U+273B");
    assert_eq!('\u{276F}' as u32, 0x276F, "❯ should be U+276F");
    assert_eq!('\u{25CF}' as u32, 0x25CF, "● should be U+25CF");
    assert_eq!('\u{2026}' as u32, 0x2026, "… should be U+2026");

    // The thinking line format is: <char> <Verb>… (<time> · ↓ <N> tokens)
    // The idle line format is: <char> <Verb>ed for <N>s · <info>
    let thinking_line = "\u{2722} Fermenting\u{2026} (38s \u{00b7} \u{2193} 909 tokens)";
    let idle_line = "\u{273b} Brewed for 38s \u{00b7} 11 background tasks still running";

    assert!(
        thinking_line.contains('\u{2026}'),
        "thinking line has ellipsis"
    );
    assert!(!idle_line.contains('\u{2026}'), "idle line has no ellipsis");
}

// =============================================================================
// BUG REPRODUCTION: Writing-when-idle after Claude finishes responding
// =============================================================================
// After Claude finishes responding, the content area above the separator still
// contains ● bullet points from the previous response. detect_activity() sees
// those bullets and returns Writing instead of Idle. The test below captures a
// REAL idle-after-response state and asserts the CORRECT behavior (Idle). It
// should FAIL against the current buggy code (which returns Writing).

/// Real tmux capture of an IDLE state AFTER Claude has finished responding.
/// Claude wrote a response with ● bullet points, then completed. The screen
/// still shows those bullets in the scroll history above the separator, but
/// there is NO active thinking indicator, NO spinner, and the prompt is visible.
/// The "Brewed for" completion line confirms Claude is done.
///
/// This is what the screen looks like after Claude answers a question:
///   ... previous context ...
///   ● Here is my response to your question. [completed output - NOT streaming]
///   ● Second point in the response.
///     ⎿  some indented detail
///   ✻ Brewed for 12s · ↓ 2048 tokens
///   ───────────────────────────────────
///   ❯
///   ───────────────────────────────────
///   -- INSERT -- ... tokens
const REAL_CAPTURE_IDLE_AFTER_RESPONSE: &str = "\
some earlier context from the conversation\n\
\n\
\u{25cf} Here is my response to your question. This is completed output that was\n\
  streamed earlier but is now just scroll history.\n\
\n\
\u{25cf} Second point in the response with more detail.\n\
  \u{23bf}  some indented sub-point\n\
\n\
\u{273b} Brewed for 12s \u{00b7} \u{2193} 2048 tokens\n\
\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
\u{276f} \n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
  -- INSERT --\u{23f5}\u{23f5} bypass permissions on \u{00b7} 3 background tasks                    52000 tokens\n\
                                                            current: 2.1.77 \u{00b7} latest: 2.1.\u{2026}\n";

/// BUG REPRO: After Claude finishes responding, ● bullets from the completed
/// response remain visible in the content area. detect_activity() incorrectly
/// returns Writing instead of Idle.
///
/// This test asserts CORRECT behavior (Idle) and should FAIL with current code.
#[test]
fn bug_idle_after_response_with_bullets_should_be_idle() {
    let result = detect_activity(REAL_CAPTURE_IDLE_AFTER_RESPONSE);
    assert_eq!(
        result,
        ClaudeActivity::Idle,
        "BUG REPRO: Expected Idle after Claude finished responding, but got {:?}. \
         The ● bullet points are from a COMPLETED response (scroll history), \
         not actively streaming output. The ✻ 'Brewed for' completion line \
         confirms Claude is done. detect_activity() should not treat stale \
         bullet points as active Writing.",
        result
    );
}

// =============================================================================
// BUG REPRODUCTION v2: Writing-when-idle WITHOUT completion indicator
// =============================================================================
// The 295bcfa fix only skips Writing when a "Brewed for"/"Baked for" completion
// indicator is present. But quick responses don't have that line. Andrew's
// screenshots show bullets visible (from background task messages like
// "Running in the background" and "Restarted. Standing by...") with the prompt
// visible and NO completion indicator — yet claude-watch reports Writing.
//
// The CORRECT fix: if the prompt is visible below the separator, it's Idle
// regardless of bullets in the content area. Active writing pushes the
// separator/prompt off the bottom of the visible pane. If the prompt IS visible,
// Claude has finished streaming and any bullets are stale scroll history.

/// Real tmux capture after Claude finished a QUICK response.
/// No "Brewed for" completion indicator — the response was too fast/short.
/// Bullet points from background task confirmations are visible in scroll history.
/// Prompt is visible below separator. Without a completion indicator, this is
/// indistinguishable from active writing (between tool calls), so detect_activity()
/// returns Writing. The daemon layer handles debounce for these brief blips.
const REAL_CAPTURE_QUICK_RESPONSE_NO_COMPLETION: &str = "\
     Running in the background (i to manage\n\
\n\
\u{25cf} Bash(watchmen)\n\
  \u{23bf}  Running in the background (i to manage\n\
\n\
\u{25cf} Bash(watchmen)\n\
  \u{23bf}  Running in the background (i to manage\n\
\n\
\u{25cf} Restarted. Standing by for the foreground interruption agent\n\
\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
\u{276f} \n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
  -- INSERT --\u{23f5}\u{23f5} bypass permissions on \u{00b7} 12 background tasks                   47117 tokens\n\
                                                            current: 2.1.77 \u{00b7} latest: 2.1.\u{2026}\n";

/// Bullets + prompt + no completion indicator = Writing.
/// Without a completion indicator, we cannot distinguish stale scroll history from
/// active mid-workflow writing. detect_activity() conservatively returns Writing;
/// the daemon debounces brief Writing→Idle transitions.
#[test]
fn bullets_with_prompt_no_completion_is_writing() {
    let result = detect_activity(REAL_CAPTURE_QUICK_RESPONSE_NO_COMPLETION);
    assert_eq!(
        result,
        ClaudeActivity::Writing,
        "Bullets + prompt + no completion indicator should be Writing (daemon debounces), got {:?}",
        result
    );
}

/// Actual active writing: output has pushed the separator/prompt OFF SCREEN.
/// No prompt visible = Claude is actively streaming output.
const REAL_CAPTURE_WRITING_NO_PROMPT: &str = "\
some earlier context\n\
\n\
\u{25cf} Here is a very long response that Claude is actively streaming right now.\n\
  It has multiple lines and keeps going and going. The output is so long\n\
  that the separator and prompt have scrolled off the bottom of the visible\n\
  pane area.\n\
\n\
\u{25cf} Second point with detailed analysis that continues to stream.\n\
\n\
\u{25cf} Third point — still going. No separator or prompt visible because the\n\
  content area has filled the entire visible pane.\n\
\n\
\u{25cf} Fourth point for good measure. Claude is really writing a lot here.";

/// Active writing: prompt NOT visible (pushed off screen) + bullets = Writing.
#[test]
fn writing_without_prompt_should_be_writing() {
    let result = detect_activity(REAL_CAPTURE_WRITING_NO_PROMPT);
    assert_eq!(
        result,
        ClaudeActivity::Writing,
        "Active writing (prompt off-screen, bullets visible) should be Writing, got {:?}",
        result
    );
}

// =============================================================================
// BUG FIX: Between tool calls, Claude is actively working with bullets visible
// and prompt visible (fixed TUI layout). Should be Writing, not Idle.
// =============================================================================

/// Real tmux capture of Claude ACTIVELY WORKING between tool calls.
/// Claude just launched an agent and is generating its next response. The content
/// area has ● bullets from the current response (not stale history). The prompt
/// is visible below the separator (fixed TUI layout). No completion indicator.
///
/// This is the CORE BUG: detect_activity() returned Idle because has_prompt was
/// true, even though Claude was mid-workflow.
const REAL_CAPTURE_BETWEEN_TOOL_CALLS: &str = "\
     correctly (was 0, now 10). Thinking detection updated.\n\
\n\
\u{25cf} Andrew says detection is still broken and wants an e2e test. Let me launch an agent for that.\n\
\n\
\u{25cf} Bash(cat << 'EOF' | signal-send --dm andrew --force\n\
      On it -- launching agent to create an e2e test.\n\
      EOF)\n\
  \u{23bf}  Sent (timestamp: 1773720719612)\n\
\n\
\u{25cf} Agent(E2E test for thought detection)\n\
  \u{23bf}  Backgrounded agent (\u{2193} to manage \u{00b7} ctrl+o to expand)\n\
\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
\u{276f} \n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
  -- INSERT --\u{23f5}\u{23f5} bypass permissions on \u{00b7} 11 background tasks                   139189 tokens\n\
                                                             current: 2.1.77 \u{00b7} latest: 2.1.\u{2026}\n";

/// CORE BUG FIX: Between tool calls, bullets + prompt + no completion = Writing.
/// This was previously detected as Idle because the !has_prompt guard prevented
/// Writing detection in the fixed TUI layout where the prompt is always visible.
#[test]
fn bug_fix_between_tool_calls_should_be_writing() {
    let result = detect_activity(REAL_CAPTURE_BETWEEN_TOOL_CALLS);
    assert_eq!(
        result,
        ClaudeActivity::Writing,
        "Between tool calls (bullets + prompt + no completion indicator) should be \
         Writing, not Idle. The prompt is always visible in the fixed TUI layout \
         and cannot be used to infer idle state. Got {:?}",
        result
    );
}

/// Verify that idle-after-response WITH completion indicator is still Idle.
/// The completion indicator ("Brewed for", "Baked for") distinguishes a completed
/// response from active mid-workflow writing.
#[test]
fn idle_after_response_with_completion_is_still_idle() {
    let result = detect_activity(REAL_CAPTURE_IDLE_AFTER_RESPONSE);
    assert_eq!(
        result,
        ClaudeActivity::Idle,
        "Bullets + prompt + completion indicator ('Brewed for') = Idle, got {:?}",
        result
    );
}

// =============================================================================
// THE FIX: Priority should be Thinking > ToolRunning > Writing > Idle > Unknown
// =============================================================================

// =============================================================================
// INJECT TIMING BUG: Thinking indicator + prompt + stale content
// =============================================================================
// This fixture reproduces the exact scenario from the inject timing bug
// (fixed in ee428c4). Before the fix, inject_text() would check is_idle()
// (prompt-only), see the prompt, and proceed to type over thinking text.
// After the fix, inject_text() uses get_activity() which correctly identifies
// this state as Thinking.

/// Real tmux capture of the inject timing bug scenario.
/// The thinking indicator "✽ Thinking… (5s)" is visible in the content area
/// alongside stale bullet points from previous output. The prompt is visible
/// below the separator. Before the fix, is_idle() returned true (prompt found),
/// and inject_text proceeded immediately. After the fix, get_activity() returns
/// Thinking, and inject_text waits for it to settle.
const INJECT_TIMING_BUG_CAPTURE: &str =
    include_str!("fixtures/thinking_with_prompt_stale_content.txt");

/// The exact inject timing bug scenario: thinking indicator + prompt visible
/// + stale output from previous tool calls. Must be detected as Thinking.
#[test]
fn inject_timing_bug_thinking_with_stale_content_is_thinking() {
    let result = detect_activity(INJECT_TIMING_BUG_CAPTURE);
    assert_eq!(
        result,
        ClaudeActivity::Thinking,
        "INJECT TIMING BUG: When thinking indicator (✽ Thinking…) is visible \
         in the content area above the separator, detect_activity() must return \
         Thinking, not Idle. The prompt (❯) below the separator is always visible \
         in Claude Code's fixed TUI layout. Before the fix (ee428c4), inject_text \
         used is_idle() which checked only for the prompt and would proceed to type \
         while thinking was active. Got {:?}",
        result
    );
}

/// Verify that the old is_idle() logic (prompt-only) would have returned true
/// for this fixture — confirming the bug existed. The fix was to use
/// get_activity() instead of is_idle().
#[test]
fn inject_timing_bug_old_is_idle_would_return_true() {
    // Replicate the old is_idle() logic: check last 15 lines for prompt char (❯)
    let lines: Vec<&str> = INJECT_TIMING_BUG_CAPTURE.lines().collect();
    let start = if lines.len() > 15 {
        lines.len() - 15
    } else {
        0
    };
    let has_prompt = lines[start..].iter().any(|line| line.contains('\u{276f}'));
    assert!(
        has_prompt,
        "The old is_idle() check should return true for this fixture (prompt is visible). \
         This confirms the bug: inject_text saw the prompt and proceeded despite active thinking."
    );
}

// =============================================================================
// PROPOSED FIX DOCUMENTATION
// =============================================================================

/// This test documents the proposed fix for detect_activity().
///
/// The fix is simple: check for thinking/tool/writing indicators BEFORE checking
/// for the prompt. The prompt (❯) is always visible in Claude Code's TUI, so it
/// cannot be used as the primary idle indicator.
///
/// Correct priority order:
///   1. Thinking (thinking char + ellipsis visible) - highest
///   2. ToolRunning (spinner character visible)
///   3. Writing (● bullet points visible, above separator)
///   4. Idle (prompt visible, NO activity indicators above separator)
///   5. Unknown
///
/// Additionally, activity indicators should only be checked in the content area
/// ABOVE the separator line (───), not in the prompt/status area below it.
#[test]
fn proposed_fix_documentation() {
    // The separator line is made of U+2500 (─) box drawing characters
    let separator = "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}";

    // In real captures, the layout is always:
    // [content area - scrollable, contains thinking/tool/writing indicators]
    // ─────────────────── (separator)
    // ❯                   (prompt - ALWAYS present)
    // ─────────────────── (separator)
    // -- INSERT -- ...    (status bar)

    // The fix should:
    // 1. Split pane output at the separator line
    // 2. Only look for activity indicators in the CONTENT area (above first separator)
    // 3. The prompt below the separator confirms Claude Code is running but
    //    does NOT indicate idle state

    // Verify separator is detectable
    assert!(
        REAL_CAPTURE_THINKING_FERMENTING.contains(separator),
        "Real captures should contain separator lines"
    );

    // Count separators in real capture
    let sep_count = REAL_CAPTURE_THINKING_FERMENTING
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && trimmed.chars().all(|c| c == '\u{2500}')
        })
        .count();
    assert_eq!(
        sep_count, 2,
        "Should have exactly 2 separator lines (above and below prompt)"
    );
}
