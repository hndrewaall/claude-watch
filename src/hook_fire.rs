//! `claude-watch hook-fire <type>` CLI subcommand.
//!
//! Invoked by Claude Code hooks (SessionStart / Stop / PreCompact) to fire a
//! reminder into the current conversation. Writes a marker to
//! `~/.cache/claude-watch/reminders/<type>.json` so the daemon knows to
//! defer its heavy-handed fallback (tmux inject) for a grace period.
//!
//! Hook output protocol: Claude Code hooks consume JSON on stdout with
//! `hookSpecificOutput.additionalContext` to inject text into the
//! conversation. For PreCompact we can also set `continue: false` with
//! `stopReason` to block the compaction and ask Claude to run `/clear`.
//!
//! Exit code is always 0 so a hook failure never breaks a Claude session.

use crate::config::try_load_config;
use crate::reminders::{record_fire, ReminderType};
use crate::status;

/// Grace threshold below which the "context high" reminder should fire,
/// expressed as a percentage of max_context_tokens. 80% matches the
/// design doc.
const CONTEXT_HIGH_THRESHOLD_PCT: f64 = 80.0;

/// Return the context-usage percentage from the current Claude Code
/// status, or None if we can't parse it.
async fn context_usage_pct() -> Option<(u64, u64, f64)> {
    // Fall back to 1M tokens if we can't load a config — hooks must not
    // fail just because the daemon hasn't been set up yet.
    let max = try_load_config()
        .map(|c| c.claude.max_context_tokens)
        .unwrap_or(1_000_000)
        .max(1);
    let cs = status::get_claude_status().await?;
    let pct = (cs.tokens as f64 / max as f64) * 100.0;
    Some((cs.tokens, max, pct))
}

/// Build the hook JSON response that injects `text` back into the
/// conversation via `hookSpecificOutput.additionalContext`.
fn additional_context_response(hook_event_name: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": hook_event_name,
            "additionalContext": text,
        }
    })
}

/// Build a `continue: false` response that blocks the hook's event and
/// shows `stop_reason` + `additional_context` back to the model.
/// Used by PreCompact to ask Claude to `/clear` instead of auto-compacting.
fn block_with_context(hook_event_name: &str, stop_reason: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "continue": false,
        "stopReason": stop_reason,
        "hookSpecificOutput": {
            "hookEventName": hook_event_name,
            "additionalContext": text,
        }
    })
}

/// Empty JSON — hook does nothing. Used when the trigger condition isn't
/// met (e.g. context is fine on Stop, versions match on SessionStart).
fn noop_response() -> serde_json::Value {
    serde_json::json!({})
}

/// CLI entry point for `claude-watch hook-fire <type> [--hook-event NAME]`.
///
/// Prints a hook JSON response to stdout. Always exits 0 — a broken hook
/// must not break Claude Code sessions.
pub async fn cmd_hook_fire(kind_str: &str, hook_event: Option<&str>) -> i32 {
    let kind = match ReminderType::from_str(kind_str) {
        Some(k) => k,
        None => {
            // Unknown type — emit empty response so the hook is a no-op.
            // We deliberately don't error to stderr here because some
            // hooks capture stderr.
            println!("{}", noop_response());
            return 0;
        }
    };

    let response = match kind {
        ReminderType::ContextHigh => handle_context_high(hook_event).await,
        ReminderType::VersionUpdate => handle_version_update(hook_event).await,
        ReminderType::PreCompact => handle_pre_compact(hook_event).await,
    };

    println!("{}", response);
    0
}

async fn handle_context_high(hook_event: Option<&str>) -> serde_json::Value {
    let event_name = hook_event.unwrap_or("Stop");
    let (tokens, max, pct) = match context_usage_pct().await {
        Some(v) => v,
        None => return noop_response(),
    };

    if pct < CONTEXT_HIGH_THRESHOLD_PCT {
        return noop_response();
    }

    let text = format!(
        "[claude-watch] Context usage is at {:.0}% ({}k / {}k tokens). \
         Consider running `/clear` before continuing — the daemon will \
         force a self-clear if usage stays high.",
        pct,
        tokens / 1000,
        max / 1000,
    );

    record_fire(
        ReminderType::ContextHigh,
        Some(serde_json::json!({"tokens": tokens, "max": max, "pct": pct})),
    );

    additional_context_response(event_name, &text)
}

async fn handle_version_update(hook_event: Option<&str>) -> serde_json::Value {
    let event_name = hook_event.unwrap_or("SessionStart");
    let info = tokio::task::spawn_blocking(status::get_version_info)
        .await
        .unwrap_or_default();

    let running = match info.running {
        Some(v) => v,
        None => return noop_response(),
    };
    let installed = match info.installed {
        Some(v) => v,
        None => return noop_response(),
    };

    if running == installed {
        return noop_response();
    }

    let text = format!(
        "[claude-watch] A newer Claude Code is installed: {} -> {}. \
         Run `/restart` (or exit and re-launch) to pick it up. The daemon \
         will fall back to `claude update` if the mismatch persists.",
        running, installed,
    );

    record_fire(
        ReminderType::VersionUpdate,
        Some(serde_json::json!({"running": running, "installed": installed})),
    );

    additional_context_response(event_name, &text)
}

async fn handle_pre_compact(hook_event: Option<&str>) -> serde_json::Value {
    let event_name = hook_event.unwrap_or("PreCompact");
    // Record the fire regardless of context usage — the hook only runs on
    // `auto` matcher, so every invocation is a "Claude is about to auto-
    // compact" event we want to track.
    record_fire(ReminderType::PreCompact, None);

    let text = "[claude-watch] Auto-compaction is about to run. Consider \
        running `/clear` instead for a cleaner reset. If `/clear` isn't \
        appropriate, let compaction proceed.";

    block_with_context(
        event_name,
        "Auto-compaction blocked by claude-watch — consider /clear instead.",
        text,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn additional_context_shape() {
        let v = additional_context_response("Stop", "hello");
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "Stop");
        assert_eq!(
            v["hookSpecificOutput"]["additionalContext"],
            serde_json::Value::String("hello".to_string())
        );
    }

    #[test]
    fn block_shape() {
        let v = block_with_context("PreCompact", "nope", "try /clear");
        assert_eq!(v["continue"], serde_json::Value::Bool(false));
        assert_eq!(v["stopReason"], "nope");
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreCompact");
    }

    #[test]
    fn noop_shape() {
        let v = noop_response();
        assert!(v.is_object());
        assert_eq!(v.as_object().map(|o| o.is_empty()), Some(true));
    }
}
