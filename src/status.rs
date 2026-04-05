//! Claude Code status parsing and watcher/process checks.

use crate::cmd::{run_cmd, run_cmd_any};
use regex_lite::Regex;
use serde::Serialize;
use tracing::debug;

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
    let start = if lines.len() > 10 { lines.len() - 10 } else { 0 };

    // Match "N tokens" or truncated "N toke" — but ONLY on status bar lines
    // (contain permission mode, INSERT, or background tasks indicator).
    // This prevents matching thinking indicator text ("↓ 400 tokens") or
    // Claude's output text that mentions tokens.
    let token_re = Regex::new(r"(\d[\d,]*)\s+toke").unwrap();
    let bash_re = Regex::new(r"(\d+)\s+(?:bashes|background\s+tasks)").unwrap();
    let compact_re = Regex::new(r"Context left until auto-compact:\s*(\d+)%").unwrap();

    for line in &lines[start..] {
        // Only parse tokens from status bar lines (contain mode indicators)
        let is_status_bar = line.contains("bypass permissions")
            || line.contains("-- INSERT --")
            || line.contains("background tasks")
            || line.contains("bashes")
            || line.contains("auto-compact");

        if is_status_bar {
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

/// Extract a version string from a path containing `/versions/X.Y.Z/`.
pub(crate) fn extract_version_from_path(path: &str) -> Option<String> {
    let re = Regex::new(r"/versions/([\d.]+)").unwrap();
    re.captures(path).and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
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
            "tmux", "list-panes", "-a", "-F",
            "#{session_name}:#{window_index}.#{pane_index} #{pane_current_command}",
        ],
        5,
    ).await?;

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
    // The status bar format varies — sometimes shows "auto-compact", "latest",
    // "current:", or just "N tokens". Use multiple heuristics:
    //   1. "tokens" + version-related strings (original check)
    //   2. "tokens" + Claude Code prompt (❯) or vim mode (-- INSERT --)
    //   3. "tokens" + "bypass permissions" (Claude Code permission mode indicator)
    for pane in candidates {
        if let Some(content) = crate::tmux::capture_pane(&pane).await {
            if content.contains("tokens")
                && (content.contains("auto-compact")
                    || content.contains("latest")
                    || content.contains("current:")
                    || content.contains("❯")
                    || content.contains("-- INSERT --")
                    || content.contains("bypass permissions"))
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
            let enabled = parts
                .get(3)
                .map(|s| *s == "true")
                .unwrap_or(true);
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
        let input = "5000 tokens";
        let parsed = parse_status_bar(input);
        assert_eq!(parsed.tokens, Some(5000));
    }

    #[test]
    fn test_parse_status_bar_large_tokens() {
        let input = "1,234,567 tokens";
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
