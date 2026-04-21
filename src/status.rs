//! Claude Code status parsing and watcher/process checks.

use crate::cmd::{run_cmd, run_cmd_any};
use regex_lite::Regex;
use serde::Serialize;
use tracing::{debug, warn};

/// Parsed Claude Code status from tmux pane capture + /proc.
#[derive(Debug, Serialize, Clone)]
pub struct ClaudeStatus {
    pub pane: String,
    pub tokens: u64,
    pub bashes: u64,
    pub compact_remaining: Option<u32>,
    pub version: Option<String>,
    pub latest: Option<String>,
}

/// Parsed status bar fields (pure data, no I/O).
#[derive(Debug, Default, PartialEq)]
pub(crate) struct ParsedStatusBar {
    pub tokens: Option<u64>,
    pub bashes: Option<u64>,
    pub compact_remaining: Option<u32>,
}

/// Version info from /proc and symlinks.
#[derive(Debug, Default)]
pub struct VersionInfo {
    pub running: Option<String>,
    pub installed: Option<String>,
}

/// Watcher config entry parsed from watchers.conf.
#[derive(Debug, Clone)]
pub struct WatcherEntry {
    pub name: String,
    pub pattern: String,
    pub min_count: u32,
    pub enabled: bool,
    pub start_cmd: Option<String>,
}

/// Pure function: parse status bar fields from pane capture text.
///
/// Looks at the last 10 lines for:
/// - Token count: `(\d[\d,]*)\s+tokens`
/// - Bash/background task count: `(\d+)\s+(?:bashes|background\s+tasks)`
/// - Compact remaining: `Context left until auto-compact:\s*(\d+)%`
pub(crate) fn parse_status_bar(pane_text: &str) -> ParsedStatusBar {
    let mut result = ParsedStatusBar::default();

    let lines: Vec<&str> = pane_text.lines().collect();
    let start = if lines.len() > 10 {
        lines.len() - 10
    } else {
        0
    };

    // Match "N tokens" or truncated "N tok…" / "N toke" — but ONLY on status
    // bar lines (contain permission mode, INSERT, or background tasks
    // indicator). This prevents matching thinking indicator text ("↓ 400
    // tokens") or Claude's output text that mentions tokens.
    //
    // Claude Code truncates the status bar with an ellipsis when the pane is
    // narrow, producing `502064 tok…` (only three letters of "tokens"). We
    // match `tok` followed by anything that is NOT a letter — that excludes
    // false positives like "took" / "token" in prose while still catching
    // both the truncated and full forms.
    let token_re = Regex::new(r"(\d[\d,]*)\s+tok").unwrap();
    // Claude Code has used multiple names for the concurrent-task counter:
    // `bashes` (old), `background tasks` (mid), and `shells` (2.1.94+). Match
    // all of them. The `\b` word boundary prevents e.g. "5 shellscript" from
    // matching.
    let bash_re =
        Regex::new(r"(\d+)\s+(?:bashes|background\s+tasks|shells)\b").unwrap();
    let compact_re = Regex::new(r"Context left until auto-compact:\s*(\d+)%").unwrap();

    // Check if ANY line in the bottom section is a status bar line.
    // When the tmux pane is narrow, the status bar wraps across multiple lines —
    // e.g. "bypass permissions" on one line and "175630 tokens" on the next.
    // Narrow wrapping can ALSO split "bypass permissions" itself across a
    // separator ("bypass permissi ·  on"), so we match the more reliable prefix
    // "bypass permissi" instead of the full word.
    //
    // EXTREME wraps (2026-04-18 incident) split the bar across many logical
    // lines so even "bypass permissi" doesn't appear on any one line — just
    // `bypass` alone on its line, then `INSERT` alone, then `606746 tokens`
    // alone. The `⏵⏵` permission-mode icon is the most reliable anchor:
    // Claude Code emits it at the left edge of the status bar whenever
    // bypass or accept-edits permissions are active, and it never appears in
    // Claude's chat output or model responses. Match it first.
    //
    // If we see a status bar indicator anywhere, enable token parsing for all
    // lines in the tail.
    let has_status_bar = lines[start..].iter().any(|line| {
        line.contains('\u{23f5}') // ⏵ — permission mode icon (bypass / accept edits)
            || line.contains("bypass permissi")
            || line.contains("-- INSERT --")
            || line.contains("background tasks")
            || line.contains("bashes")
            || line.contains(" shells")
            || line.contains("auto-compact")
    });

    for line in &lines[start..] {
        if has_status_bar {
            if let Some(caps) = token_re.captures(line) {
                if let Some(m) = caps.get(1) {
                    let cleaned = m.as_str().replace(',', "");
                    if let Ok(v) = cleaned.parse::<u64>() {
                        result.tokens = Some(v);
                    }
                }
            }
        }
        if let Some(caps) = bash_re.captures(line) {
            if let Some(m) = caps.get(1) {
                if let Ok(v) = m.as_str().parse::<u64>() {
                    result.bashes = Some(v);
                }
            }
        }
        if let Some(caps) = compact_re.captures(line) {
            if let Some(m) = caps.get(1) {
                if let Ok(v) = m.as_str().parse::<u32>() {
                    result.compact_remaining = Some(v);
                }
            }
        }
    }

    result
}

/// Pure function: determine whether a parse-bar result + pane capture
/// represents a suspicious "parse miss" — i.e. the pane had non-whitespace
/// content but we extracted neither tokens nor bashes. This is the case we
/// want to log loudly so we can diagnose stale-latch bugs where the daemon
/// repeatedly reads 0 from a pane that clearly has a status bar.
///
/// A capture that is empty or all-whitespace is *not* a parse miss — that's
/// "process is actually gone" and shouldn't spam logs.
pub(crate) fn is_parse_miss(pane_text: &str, parsed: &ParsedStatusBar) -> bool {
    if parsed.tokens.is_some() || parsed.bashes.is_some() {
        return false;
    }
    pane_text.chars().any(|c| !c.is_whitespace())
}

/// Pure function: extract a short diagnostic tail from a pane capture for
/// logging. Returns the last `max_lines` non-empty lines, each truncated to
/// `max_line_len` characters. Keeps log volume bounded even if the pane has
/// huge lines.
pub(crate) fn parse_miss_tail(pane_text: &str, max_lines: usize, max_line_len: usize) -> String {
    let lines: Vec<&str> = pane_text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..]
        .iter()
        .map(|line| {
            if line.chars().count() > max_line_len {
                let truncated: String = line.chars().take(max_line_len).collect();
                format!("{}…", truncated)
            } else {
                (*line).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Extract a version string from a path containing `/versions/X.Y.Z/`.
pub(crate) fn extract_version_from_path(path: &str) -> Option<String> {
    let re = Regex::new(r"/versions/([\d.]+)").unwrap();
    re.captures(path)
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
}

/// Get installed and running Claude Code versions via symlink and /proc.
///
/// - Installed: `readlink ~/.local/bin/claude` → extract version from path
/// - Running: `pgrep -a claude` → `readlink /proc/PID/exe` → extract version
pub fn get_version_info() -> VersionInfo {
    let mut info = VersionInfo::default();

    // Installed version from symlink
    let claude_bin = format!(
        "{}/.local/bin/claude",
        std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string())
    );
    if let Ok(target) = std::fs::canonicalize(&claude_bin) {
        info.installed = extract_version_from_path(&target.to_string_lossy());
    }

    // Running version from /proc
    if let Ok(output) = std::process::Command::new("pgrep")
        .args(["-a", "claude"])
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(pid_str) = line.split_whitespace().next() {
                let exe_path = format!("/proc/{}/exe", pid_str);
                if let Ok(target) = std::fs::read_link(&exe_path) {
                    if let Some(ver) = extract_version_from_path(&target.to_string_lossy()) {
                        info.running = Some(ver);
                        break;
                    }
                }
            }
        }
    }

    info
}

/// Find the tmux pane running Claude Code.
///
/// Primary: look for `pane_current_command == "claude"`.
/// Fallback: check "bash"/"node" panes for Claude Code status bar content
/// (handles wrapper scripts).
pub async fn find_claude_pane() -> Option<String> {
    let out = run_cmd(
        &[
            "tmux",
            "list-panes",
            "-a",
            "-F",
            "#{session_name}:#{window_index}.#{pane_index} #{pane_current_command}",
        ],
        5,
    )
    .await?;

    let mut candidates = Vec::new();

    for line in out.lines() {
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        if parts.len() == 2 {
            if parts[1] == "claude" {
                return Some(parts[0].to_string());
            }
            if parts[1] == "bash" || parts[1] == "node" {
                candidates.push(parts[0].to_string());
            }
        }
    }

    // Fallback: capture candidate panes and check for Claude Code status bar.
    //
    // Use joined capture (-J) so wrapped status bar lines reassemble into one
    // line — narrow panes wrap and truncate, but -J gives us the full logical
    // line before terminal truncation.
    //
    // Match on "tok" (not "tokens") because Claude Code truncates the status
    // bar with an ellipsis when the pane is narrow, producing things like
    // `502064 tok…`. Similarly, "bypass permissi" covers truncated
    // "bypass permissions". Also accept "background tasks" / "bashes" as
    // status-bar indicators (already used by parse_status_bar).
    //
    // The status bar format varies — sometimes shows "auto-compact", "latest",
    // "current:", or just "N tok…". Use multiple heuristics.
    for pane in candidates {
        let content = match crate::tmux::capture_pane_joined(&pane).await {
            Some(c) => Some(c),
            None => crate::tmux::capture_pane(&pane).await,
        };
        if let Some(content) = content {
            if content.contains("tok")
                && (content.contains("auto-compact")
                    || content.contains("latest")
                    || content.contains("current:")
                    || content.contains("❯")
                    || content.contains("-- INSERT --")
                    || content.contains("bypass permissi")
                    || content.contains("background tasks")
                    || content.contains("bashes")
                    || content.contains(" shells"))
            {
                return Some(pane);
            }
        }
    }

    None
}

/// Get Claude Code status by natively finding the pane, parsing the status bar,
/// and reading version info from /proc.
///
/// Falls back to shelling out to `claude-status --json` if native pane discovery
/// fails or if `CLAUDE_STATUS_CMD` env var is set (for test environments).
pub async fn get_claude_status() -> Option<ClaudeStatus> {
    // If CLAUDE_STATUS_CMD is set (test mode), skip native discovery and use fallback
    if std::env::var("CLAUDE_STATUS_CMD").is_ok() {
        debug!("CLAUDE_STATUS_CMD set, using fallback");
        return get_claude_status_fallback().await;
    }

    // Try native pane discovery first
    if let Some(pane) = find_claude_pane().await {
        debug!(pane = %pane, "found claude pane (native)");

        // Use joined capture (-J) for status bar parsing to avoid truncation
        if let Some(capture) = crate::tmux::capture_pane_joined(&pane).await {
            let parsed = parse_status_bar(&capture);

            // Diagnostic: if we got nothing out of the parser but the pane
            // clearly has content, log the tail so we can debug stale-latch
            // bugs where the daemon reads tokens=0 forever while the CLI
            // parses the same pane correctly.
            if is_parse_miss(&capture, &parsed) {
                warn!(
                    pane = %pane,
                    tail = %parse_miss_tail(&capture, 10, 200),
                    "status parse miss: pane non-empty but no tokens/bashes extracted"
                );
            }

            let version_info = tokio::task::spawn_blocking(get_version_info)
                .await
                .unwrap_or_default();

            let status = ClaudeStatus {
                pane,
                tokens: parsed.tokens.unwrap_or(0),
                bashes: parsed.bashes.unwrap_or(0),
                compact_remaining: parsed.compact_remaining,
                version: version_info.running,
                latest: version_info.installed,
            };

            debug!(
                tokens = status.tokens,
                bashes = status.bashes,
                pane = %status.pane,
                compact_remaining = ?status.compact_remaining,
                version = ?status.version,
                latest = ?status.latest,
                "parsed claude status (native)"
            );

            return Some(status);
        }
    }

    // Fallback: shell out to claude-status --json (for test environments with mocks)
    debug!("native pane discovery failed, trying claude-status fallback");
    get_claude_status_fallback().await
}

/// Fallback: shell out to `claude-status --json` for status.
/// Used when native pane discovery fails (e.g. test environments with mock scripts).
async fn get_claude_status_fallback() -> Option<ClaudeStatus> {
    let out = run_cmd(&["claude-status", "--json"], 15).await?;
    debug!(raw_output = %out, "claude-status fallback response");
    let data: serde_json::Value = serde_json::from_str(&out).ok()?;

    let status = ClaudeStatus {
        pane: data["pane"].as_str().unwrap_or("").to_string(),
        tokens: data["tokens"].as_u64().unwrap_or(0),
        bashes: data["bashes"].as_u64().unwrap_or(0),
        compact_remaining: data["compact_remaining"].as_u64().map(|v| v as u32),
        version: data["version"].as_str().map(|s| s.to_string()),
        latest: data["latest"].as_str().map(|s| s.to_string()),
    };
    debug!(tokens = status.tokens, bashes = status.bashes, pane = %status.pane, "parsed claude status (fallback)");
    Some(status)
}

pub async fn check_watchmen_count() -> u32 {
    let (out, _) = run_cmd_any(&["pgrep", "-fc", "bin/watchmen"], 5).await;
    out.parse().unwrap_or(0)
}

pub async fn check_process_count(pattern: &str) -> u32 {
    // Use "--" to prevent pgrep from interpreting patterns starting with "--" as options
    let (out, _) = run_cmd_any(&["pgrep", "-fc", "--", pattern], 5).await;
    out.parse().unwrap_or(0)
}

/// Parse watchers config file. Format: `name|pattern|min_count|enabled|start_cmd`
pub fn parse_watchers_config(path: &str) -> Vec<WatcherEntry> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    parse_watchers_config_str(&content)
}

/// Pure function: parse watchers config from a string.
pub(crate) fn parse_watchers_config_str(content: &str) -> Vec<WatcherEntry> {
    content
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('|').collect();
            if parts.len() < 2 {
                return None;
            }
            let name = parts[0].to_string();
            let pattern = parts[1].to_string();
            let min_count = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
            let enabled = parts.get(3).map(|s| *s == "true").unwrap_or(true);
            let start_cmd = parts
                .get(4)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            Some(WatcherEntry {
                name,
                pattern,
                min_count,
                enabled,
                start_cmd,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_status_bar tests ---

    #[test]
    fn test_parse_status_bar_full() {
        let input = "some output\nmore output\n\
                      50,000 tokens  10 bashes\n\
                      Context left until auto-compact: 85%";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(50000));
        assert_eq!(parsed.bashes, Some(10));
        assert_eq!(parsed.compact_remaining, Some(85));
    }

    #[test]
    fn test_parse_status_bar_tokens_no_commas() {
        let input = "-- INSERT -- 5000 tokens";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(5000));
    }

    #[test]
    fn test_parse_status_bar_large_tokens() {
        let input = "bypass permissions on · 1,234,567 tokens";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(1234567));
    }

    #[test]
    fn test_parse_status_bar_background_tasks() {
        let input = "3 background tasks";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.bashes, Some(3));
    }

    #[test]
    fn test_parse_status_bar_bashes() {
        let input = "5 bashes";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.bashes, Some(5));
    }

    #[test]
    fn test_parse_status_bar_shells() {
        // Claude Code 2.1.94+ renamed "background tasks" / "bashes" to "shells".
        let input = "7 shells";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.bashes, Some(7));
    }

    #[test]
    fn test_parse_status_bar_shells_realistic() {
        // Full realistic status bar line as emitted by Claude Code 2.1.94+
        // in the dashboard pane.
        let input = "output\n\
                     \u{23f5}\u{23f5} bypass permissions on \u{00b7} 6 shells \u{00b7} esc to interrupt \u{00b7} \u{2193} to manage   849577 tokens";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(849577));
        assert_eq!(parsed.bashes, Some(6));
    }

    #[test]
    fn test_parse_status_bar_missing_fields() {
        let input = "nothing relevant here\njust some text";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, None);
        assert_eq!(parsed.bashes, None);
        assert_eq!(parsed.compact_remaining, None);
    }

    #[test]
    fn test_parse_status_bar_empty() {
        let parsed = parse_status_bar("");
        assert_eq!(parsed, ParsedStatusBar::default());
    }

    #[test]
    fn test_parse_status_bar_only_last_10_lines() {
        let mut lines = vec!["99,999 tokens"];
        for _ in 0..15 {
            lines.push("filler line");
        }
        let input = lines.join("\n");
        let parsed = parse_status_bar(&input);
        // Token line is beyond last 10 lines, should not be found
        assert_eq!(parsed.tokens, None);
    }

    #[test]
    fn test_parse_status_bar_compact_zero() {
        let input = "Context left until auto-compact: 0%";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.compact_remaining, Some(0));
    }

    #[test]
    fn test_parse_status_bar_realistic() {
        // Realistic Claude Code status bar content
        let input = "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                      \u{276f} \n\
                      \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                      -- INSERT --  123,456 tokens  5 bashes  Context left until auto-compact: 42%\n\
                      current: 2.1.77   latest: 2.1.78";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(123456));
        assert_eq!(parsed.bashes, Some(5));
        assert_eq!(parsed.compact_remaining, Some(42));
    }

    #[test]
    fn test_parse_status_bar_wrapped_narrow_pane() {
        // When tmux pane is narrow, the status bar wraps across lines.
        // "bypass permissions" is on one line, "175630 tokens" on the next.
        let input = "some output\n\
                     more output\n\
                     \u{23f5}\u{23f5} bypass permissions on \u{00b7} 5 shells \u{00b7} esc to interrupt \u{00b7} \u{2193}\u{2026}\n\
                     175630 tokens";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(175630));
        assert_eq!(parsed.bashes, Some(5));
    }

    #[test]
    fn test_parse_status_bar_shells_wrapped_permissi() {
        // Real capture from Claude Code 2.1.94: status bar uses "N shells"
        // (new terminology) AND the word "permissions" is wrapped, splitting
        // into "bypass permissi ·  on". Previously neither the has_status_bar
        // check nor bash_re matched "shells", so tokens + bashes both parsed
        // as None and the daemon emitted 696 ClaudeProcessDead false alerts
        // in a few hours.
        let input = "some output\n\
                     \u{23f5}\u{23f5} bypass permissi \u{00b7}  on   5 shells \u{00b7} esc to interrupt \u{00b7} \u{2193} to manage   580828 tokens\n\
                     current: 2.1.94 \u{00b7} latest: 2.1.96";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(580828));
        assert_eq!(parsed.bashes, Some(5));
    }

    #[test]
    fn test_parse_status_bar_truncated_ellipsis() {
        // Real capture from a pane where Claude Code truncated the status bar
        // with an ellipsis: "bypass permissi" (not "permissions") and
        // "502064 tok…" (not "tokens"). Previously parsed as tokens=None
        // which caused spurious ClaudeProcessDead Prometheus alerts.
        let input = "output line\n\
                     \u{23f5}\u{23f5} bypass permissi \u{00b7}  on   6 background tasks \u{00b7} ctrl+x ctrl+k to stop agen502064 tok\u{2026}";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(502064));
        assert_eq!(parsed.bashes, Some(6));
    }

    #[test]
    fn test_parse_status_bar_wrapped_with_compact() {
        // Wrapped status bar with compact info on a separate line
        let input = "output\n\
                     \u{23f5}\u{23f5} bypass permissions on \u{00b7} 3 bashes \u{00b7} esc to interrupt\n\
                     42,000 tokens  Context left until auto-compact: 30%";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(42000));
        assert_eq!(parsed.bashes, Some(3));
        assert_eq!(parsed.compact_remaining, Some(30));
    }

    #[test]
    fn test_parse_status_bar_extreme_wrap_incident_2026_04_18() {
        // 2026-04-18 21:23 ET — extremely narrow tmux pane ate the usual
        // "bypass permissi" and "-- INSERT --" indicators by splitting them
        // across multiple LOGICAL lines (not just visual wraps that -J would
        // rejoin). The pane tail captured by parse_miss_tail reads:
        //     partial response | received | ───── | ❯ | ───── |
        //     --   ⏵⏵ bypass | INSERT | -- | 606746 tokens | ◉ xhigh · /effort
        //
        // Previously parse_status_bar returned tokens=None because
        // has_status_bar couldn't match any line: "bypass" stood alone
        // (no "permissi"), "INSERT" stood alone (no dashes), no "shells" /
        // "background tasks" / "auto-compact" keyword anywhere. The daemon
        // then spuriously flagged dead_checks=4 even though the pane
        // clearly showed "606746 tokens". Andrew pkilled tmux at 21:24 ET
        // because the main loop was unresponsive and no alert had fired.
        //
        // The fix: recognize `⏵⏵` (the permission-mode icon, unique to the
        // status bar) as a status-bar indicator. It is always present when
        // the bar is rendered with `bypass` or `accept edits` permissions,
        // regardless of how narrowly the terminal wraps the adjacent text.
        let input = "\
                     partial response\n\
                     received\n\
                     \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                     \u{276f}\n\
                     \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                     --      \u{23f5}\u{23f5} bypass\n\
                     INSERT\n\
                     --\n\
                     606746 tokens\n\
                     \u{25c9} xhigh \u{00b7} /effort";
        let parsed = parse_status_bar(input);
        assert_eq!(
            parsed.tokens,
            Some(606746),
            "status bar with only ⏵⏵ icon (no \"permissi\" / \"INSERT --\" substrings \
             on any single line) must still be recognized — this was the 2026-04-18 \
             incident where Andrew killed tmux"
        );
    }

    #[test]
    fn test_parse_status_bar_accept_edits_icon_alone() {
        // Similar to the wrap incident but with a narrower wrap that splits
        // even the emoji from its words. `⏵⏵` + a tokens line on its own
        // must be enough.
        let input = "some chat output\n\
                     \u{23f5}\u{23f5}\n\
                     128000 tokens";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(128000));
    }

    #[test]
    fn test_parse_status_bar_single_chevron_not_enough() {
        // A lone `>` or the prompt character `❯` isn't a status-bar marker —
        // Claude's chat output frequently contains chevrons. We do NOT want
        // to widen the indicator set so far that we match prose that happens
        // to mention "500 tokens" somewhere.
        let input = "Hey, cost about 500 tokens per request.\n\
                     \u{276f}";
        let parsed = parse_status_bar(input);
        assert_eq!(
            parsed.tokens, None,
            "must not match token counts in chat prose just because the \
             prompt char is visible"
        );
    }

    // --- is_parse_miss tests ---

    #[test]
    fn test_is_parse_miss_empty_capture() {
        // Empty pane capture is "process gone", not a parse miss.
        let parsed = ParsedStatusBar::default();
        assert!(!is_parse_miss("", &parsed));
        assert!(!is_parse_miss("   \n\t\n  ", &parsed));
    }

    #[test]
    fn test_is_parse_miss_has_content_but_nothing_parsed() {
        // Non-empty pane with no tokens/bashes is the suspicious case.
        let parsed = ParsedStatusBar::default();
        assert!(is_parse_miss("hello world\nno status bar here", &parsed));
    }

    #[test]
    fn test_is_parse_miss_tokens_found() {
        // Any successful parse = not a miss.
        let parsed = ParsedStatusBar {
            tokens: Some(100),
            bashes: None,
            compact_remaining: None,
        };
        assert!(!is_parse_miss("some content", &parsed));
    }

    #[test]
    fn test_is_parse_miss_bashes_found() {
        let parsed = ParsedStatusBar {
            tokens: None,
            bashes: Some(3),
            compact_remaining: None,
        };
        assert!(!is_parse_miss("some content", &parsed));
    }

    // --- parse_miss_tail tests ---

    #[test]
    fn test_parse_miss_tail_basic() {
        let input = "line1\nline2\nline3\nline4";
        let tail = parse_miss_tail(input, 2, 100);
        assert_eq!(tail, "line3 | line4");
    }

    #[test]
    fn test_parse_miss_tail_truncates_long_lines() {
        let long = "x".repeat(500);
        let input = format!("short\n{}", long);
        let tail = parse_miss_tail(&input, 5, 50);
        assert!(tail.contains("short"));
        assert!(tail.contains("…"));
        let segments: Vec<&str> = tail.split(" | ").collect();
        assert_eq!(segments.len(), 2);
        // Truncated segment = 50 chars + ellipsis
        assert!(segments[1].chars().count() <= 51);
    }

    #[test]
    fn test_parse_miss_tail_skips_blank_lines() {
        let input = "keep1\n\n   \nkeep2\n\nkeep3";
        let tail = parse_miss_tail(input, 10, 100);
        assert_eq!(tail, "keep1 | keep2 | keep3");
    }

    #[test]
    fn test_parse_miss_tail_fewer_lines_than_max() {
        let tail = parse_miss_tail("one\ntwo", 10, 100);
        assert_eq!(tail, "one | two");
    }

    // --- extract_version_from_path tests ---

    #[test]
    fn test_extract_version_simple() {
        let path = "/home/user/.local/share/claude/versions/2.1.77/node_modules/.bin/claude";
        assert_eq!(extract_version_from_path(path), Some("2.1.77".to_string()));
    }

    #[test]
    fn test_extract_version_three_part() {
        let path = "/opt/versions/1.0.0/bin/claude";
        assert_eq!(extract_version_from_path(path), Some("1.0.0".to_string()));
    }

    #[test]
    fn test_extract_version_no_match() {
        let path = "/usr/bin/claude";
        assert_eq!(extract_version_from_path(path), None);
    }

    #[test]
    fn test_extract_version_empty() {
        assert_eq!(extract_version_from_path(""), None);
    }

    // --- parse_watchers_config tests ---

    #[test]
    fn test_parse_watchers_basic() {
        let config = "signal-wait|signal-wait$|1|true|watcher-ctl run signal-wait\n\
                       torrent-wait|torrent-wait$|1|true|watcher-ctl run torrent-wait";
        let entries = parse_watchers_config_str(config);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "signal-wait");
        assert_eq!(entries[0].pattern, "signal-wait$");
        assert_eq!(entries[0].min_count, 1);
        assert!(entries[0].enabled);
        assert_eq!(
            entries[0].start_cmd.as_deref(),
            Some("watcher-ctl run signal-wait")
        );
        assert_eq!(entries[1].name, "torrent-wait");
        assert_eq!(
            entries[1].start_cmd.as_deref(),
            Some("watcher-ctl run torrent-wait")
        );
    }

    #[test]
    fn test_parse_watchers_disabled() {
        let config = "watcher-a|pattern-a|1|false|cmd-a";
        let entries = parse_watchers_config_str(config);
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].enabled);
        assert_eq!(entries[0].start_cmd.as_deref(), Some("cmd-a"));
    }

    #[test]
    fn test_parse_watchers_comments_and_blanks() {
        let config = "# This is a comment\n\
                       \n\
                       watcher-a|pattern-a|2|true|cmd-a\n\
                       # Another comment\n\
                       \n";
        let entries = parse_watchers_config_str(config);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "watcher-a");
        assert_eq!(entries[0].min_count, 2);
    }

    #[test]
    fn test_parse_watchers_minimal_fields() {
        let config = "watcher-a|pattern-a";
        let entries = parse_watchers_config_str(config);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].min_count, 1); // default
        assert!(entries[0].enabled); // default
        assert_eq!(entries[0].start_cmd, None); // no start_cmd
    }

    #[test]
    fn test_parse_watchers_single_field_rejected() {
        let config = "just-a-name";
        let entries = parse_watchers_config_str(config);
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn test_parse_watchers_empty() {
        let entries = parse_watchers_config_str("");
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn test_parse_watchers_invalid_min_count() {
        let config = "watcher-a|pattern-a|notanumber|true|cmd-a";
        let entries = parse_watchers_config_str(config);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].min_count, 1); // falls back to default
    }

    #[test]
    fn test_parse_watchers_config_missing_file() {
        let entries = parse_watchers_config("/tmp/nonexistent-watchers-test.conf");
        assert_eq!(entries.len(), 0);
    }
}
