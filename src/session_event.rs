//! Session lifecycle event logging and statistics.
//!
//! Replaces the Python `session-event` script. Logs events (boot, compaction,
//! restart, exit, checklist, compact-prep) to a JSONL file with optional token
//! counts auto-captured from the tmux status bar.

use chrono::{DateTime, Utc};
use regex_lite::Regex;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

/// Valid event types that can be logged.
pub const VALID_EVENTS: &[&str] = &[
    "boot",
    "compaction",
    "restart",
    "exit",
    "checklist",
    "compact-prep",
];

/// Path to the session events JSONL file.
pub fn events_file_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
    PathBuf::from(home)
        .join(".claude")
        .join("projects")
        .join("-home-user")  // NOTE: Adjust project slug to match your Claude Code project path
        .join("session-events.jsonl")
}

/// Path to the completed tasks JSONL file.
pub fn completed_tasks_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
    PathBuf::from(home)
        .join(".config")
        .join("session")
        .join("completed-tasks.jsonl")
}

/// A single session event entry.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SessionEvent {
    pub timestamp: String,
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
}

/// A completed task entry from session-task.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CompletedTask {
    pub task: String,
    pub completed_at: String,
}

/// Parse a duration string like "1h", "2d", "30m", "60s" into seconds.
pub fn parse_duration_secs(s: &str) -> Result<u64, String> {
    let re = Regex::new(r"^(\d+)([smhd])$").unwrap();
    if let Some(caps) = re.captures(s) {
        let n: u64 = caps[1].parse().map_err(|_| format!("Invalid number: {}", &caps[1]))?;
        let secs = match &caps[2] {
            "s" => n,
            "m" => n * 60,
            "h" => n * 3600,
            "d" => n * 86400,
            _ => unreachable!(),
        };
        Ok(secs)
    } else {
        Err(format!("Invalid duration: {}", s))
    }
}

/// Format a duration in seconds as a human-readable string (e.g., "1h23m", "45m12s", "30s").
pub fn fmt_duration(total_secs: i64) -> String {
    let total_secs = total_secs.unsigned_abs();
    if total_secs < 60 {
        return format!("{}s", total_secs);
    }
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    if mins < 60 {
        return format!("{}m{:02}s", mins, secs);
    }
    let hours = mins / 60;
    let mins = mins % 60;
    format!("{}h{:02}m", hours, mins)
}

/// Read the current token count from Claude Code's tmux status bar.
pub async fn read_tokens_from_tmux() -> Option<u64> {
    // Reuse the existing tmux pane discovery + status bar parsing
    if let Some(pane) = crate::status::find_claude_pane().await {
        if let Some(capture) = crate::tmux::capture_pane(&pane).await {
            let parsed = crate::status::parse_status_bar(&capture);
            return parsed.tokens;
        }
    }
    None
}

/// Log an event to the JSONL file. Auto-captures token count from tmux if not provided.
pub async fn log_event(
    event_type: &str,
    note: Option<&str>,
    tokens: Option<u64>,
    events_file: &std::path::Path,
) -> Result<SessionEvent, String> {
    // Ensure parent directory exists
    if let Some(parent) = events_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let actual_tokens = if tokens.is_some() {
        tokens
    } else {
        read_tokens_from_tmux().await
    };

    let entry = SessionEvent {
        timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        event: event_type.to_string(),
        note: note.map(|s| s.to_string()),
        tokens: actual_tokens,
    };

    let line =
        serde_json::to_string(&entry).map_err(|e| format!("Failed to serialize event: {}", e))?;

    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(events_file)
        .map_err(|e| format!("Failed to open events file: {}", e))?;

    writeln!(f, "{}", line).map_err(|e| format!("Failed to write event: {}", e))?;

    Ok(entry)
}

/// Read events from the JSONL file, optionally filtered by a time cutoff.
pub fn read_events(
    events_file: &std::path::Path,
    since_secs: Option<u64>,
) -> Vec<SessionEvent> {
    let content = match std::fs::read_to_string(events_file) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    read_events_from_str(&content, since_secs)
}

/// Pure function: parse events from JSONL content string.
pub fn read_events_from_str(content: &str, since_secs: Option<u64>) -> Vec<SessionEvent> {
    let cutoff = since_secs.map(|secs| Utc::now() - chrono::Duration::seconds(secs as i64));

    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let entry: SessionEvent = serde_json::from_str(line).ok()?;
            if let Some(ref cutoff) = cutoff {
                let ts: DateTime<Utc> = entry.timestamp.parse().ok()?;
                if ts < *cutoff {
                    return None;
                }
            }
            Some(entry)
        })
        .collect()
}

/// Read completed tasks from the JSONL file, returning the last N entries.
pub fn read_completed_tasks(tasks_file: &std::path::Path, n: usize) -> Vec<CompletedTask> {
    let content = match std::fs::read_to_string(tasks_file) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    read_completed_tasks_from_str(&content, n)
}

/// Pure function: parse completed tasks from JSONL content string.
pub fn read_completed_tasks_from_str(content: &str, n: usize) -> Vec<CompletedTask> {
    let all: Vec<CompletedTask> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let start = if all.len() > n { all.len() - n } else { 0 };
    all[start..].to_vec()
}

/// Parsed event with a concrete timestamp for statistics calculations.
#[derive(Debug, Clone)]
pub struct ParsedEvent {
    pub ts: DateTime<Utc>,
    pub event: SessionEvent,
}

/// Parse timestamps on events for statistics.
pub fn parse_event_timestamps(events: &[SessionEvent]) -> Vec<ParsedEvent> {
    events
        .iter()
        .filter_map(|e| {
            let ts: DateTime<Utc> = e.timestamp.parse().ok()?;
            Some(ParsedEvent {
                ts,
                event: e.clone(),
            })
        })
        .collect()
}

/// Format the stats summary output. Returns the formatted string.
pub fn format_stats(events: &[SessionEvent]) -> String {
    if events.is_empty() {
        return "No events recorded.".to_string();
    }

    let mut out = String::new();
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for e in events {
        *counts.entry(&e.event).or_default() += 1;
    }

    let first = &events[0].timestamp[..19];
    let last = &events[events.len() - 1].timestamp[..19];
    let total = events.len();

    out.push_str(&format!(
        "Session events: {} total ({} to {})\n\n",
        total, first, last
    ));

    for &event_type in VALID_EVENTS {
        let c = counts.get(event_type).copied().unwrap_or(0);
        if c > 0 {
            out.push_str(&format!("  {:15} {}\n", event_type, c));
        }
    }
    out.push('\n');

    // Show recent events (last 20)
    let start = if events.len() > 20 { events.len() - 20 } else { 0 };
    let recent = &events[start..];
    out.push_str(&format!("Last {} events:\n", recent.len()));
    for e in recent {
        let date = &e.timestamp[..10];
        let ts = &e.timestamp[11..19];
        let note_str = e
            .note
            .as_ref()
            .map(|n| format!("  ({})", n))
            .unwrap_or_default();
        let tok_str = e
            .tokens
            .map(|t| format!("  [{}tok]", format_number(t)))
            .unwrap_or_default();
        out.push_str(&format!(
            "  [{} {}] {}{}{}\n",
            date, ts, e.event, tok_str, note_str
        ));
    }

    out
}

/// Format the compaction stats output. Returns the formatted string.
pub fn format_compaction_stats(events: &[SessionEvent]) -> String {
    if events.is_empty() {
        return "No events recorded.".to_string();
    }

    let parsed = parse_event_timestamps(events);
    if parsed.is_empty() {
        return "No events recorded.".to_string();
    }

    // Extract compaction events (includes self-clears logged as "restart")
    let compactions: Vec<&ParsedEvent> = parsed
        .iter()
        .filter(|e| e.event.event == "compaction" || e.event.event == "restart")
        .collect();

    if compactions.is_empty() {
        return "No compaction events found.".to_string();
    }

    let mut out = String::new();

    // Overall time span
    let first_ts = parsed[0].ts;
    let last_ts = parsed[parsed.len() - 1].ts;
    let total_span = (last_ts - first_ts).num_seconds();

    out.push_str("=== Compaction Stats ===\n");
    out.push_str(&format!(
        "Period: {} to {} ({})\n",
        first_ts.format("%Y-%m-%d %H:%M"),
        last_ts.format("%Y-%m-%d %H:%M"),
        fmt_duration(total_span)
    ));
    out.push_str(&format!("Total compactions: {}\n", compactions.len()));

    if total_span > 0 {
        let per_hour = compactions.len() as f64 / (total_span as f64 / 3600.0);
        out.push_str(&format!("Frequency: {:.1}/hour\n", per_hour));
    }

    // Classify compactions as clean (had compact-prep within 10min before)
    let session_starts = ["boot", "restart", "checklist"];
    let mut clean_flags: Vec<bool> = Vec::new();
    let mut sessions: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::new(); // (start, end)

    for comp in &compactions {
        let had_prep = parsed.iter().any(|e| {
            e.ts < comp.ts
                && e.event.event == "compact-prep"
                && (comp.ts - e.ts).num_seconds() <= 600
        });
        clean_flags.push(had_prep);

        // Find most recent session start before this compaction
        let best_start = parsed
            .iter()
            .filter(|e| e.ts < comp.ts && session_starts.contains(&e.event.event.as_str()))
            .last();
        if let Some(start) = best_start {
            sessions.push((start.ts, comp.ts));
        }
    }

    let clean_count = clean_flags.iter().filter(|&&c| c).count();
    let unclean_count = compactions.len() - clean_count;
    let clean_pct = if !compactions.is_empty() {
        clean_count as f64 / compactions.len() as f64 * 100.0
    } else {
        0.0
    };

    out.push_str(&format!(
        "Clean: {}/{}  ({:.0}%)  |  Unclean: {}\n\n",
        clean_count,
        compactions.len(),
        clean_pct,
        unclean_count
    ));

    // Interval stats
    let intervals: Vec<i64> = compactions
        .windows(2)
        .map(|w| (w[1].ts - w[0].ts).num_seconds())
        .collect();

    if !intervals.is_empty() {
        let avg = intervals.iter().sum::<i64>() / intervals.len() as i64;
        let min_i = *intervals.iter().min().unwrap();
        let max_i = *intervals.iter().max().unwrap();
        let mut sorted = intervals.clone();
        sorted.sort();
        let median = sorted[sorted.len() / 2];

        out.push_str("Compaction intervals (time between consecutive compactions):\n");
        out.push_str(&format!("  Average:  {}\n", fmt_duration(avg)));
        out.push_str(&format!("  Median:   {}\n", fmt_duration(median)));
        out.push_str(&format!("  Min:      {}\n", fmt_duration(min_i)));
        out.push_str(&format!("  Max:      {}\n\n", fmt_duration(max_i)));
    }

    // Session duration stats
    if !sessions.is_empty() {
        let durations: Vec<i64> = sessions.iter().map(|(s, e)| (*e - *s).num_seconds()).collect();
        let avg = durations.iter().sum::<i64>() / durations.len() as i64;
        let min_d = *durations.iter().min().unwrap();
        let max_d = *durations.iter().max().unwrap();
        let mut sorted = durations.clone();
        sorted.sort();
        let median = sorted[sorted.len() / 2];

        out.push_str("Session durations (last checklist/boot → compaction):\n");
        out.push_str(&format!("  Average:  {}\n", fmt_duration(avg)));
        out.push_str(&format!("  Median:   {}\n", fmt_duration(median)));
        out.push_str(&format!("  Min:      {}\n", fmt_duration(min_d)));
        out.push_str(&format!("  Max:      {}\n\n", fmt_duration(max_d)));
    }

    // Compaction overhead: compaction → next checklist
    let mut overheads: Vec<i64> = Vec::new();
    for comp in &compactions {
        for e in &parsed {
            if e.ts > comp.ts && e.event.event == "checklist" {
                let overhead = (e.ts - comp.ts).num_seconds();
                if overhead < 600 {
                    overheads.push(overhead);
                }
                break;
            }
        }
    }

    if !overheads.is_empty() {
        let avg = overheads.iter().sum::<i64>() / overheads.len() as i64;
        let min_o = *overheads.iter().min().unwrap();
        let max_o = *overheads.iter().max().unwrap();
        let mut sorted = overheads.clone();
        sorted.sort();
        let median = sorted[sorted.len() / 2];

        out.push_str("Compaction overhead (compaction → checklist complete):\n");
        out.push_str(&format!("  Average:  {}\n", fmt_duration(avg)));
        out.push_str(&format!("  Median:   {}\n", fmt_duration(median)));
        out.push_str(&format!("  Min:      {}\n", fmt_duration(min_o)));
        out.push_str(&format!("  Max:      {}\n\n", fmt_duration(max_o)));
    }

    // Downtime: last event before compaction → checklist after compaction
    let mut downtimes: Vec<i64> = Vec::new();
    for comp in &compactions {
        let last_before = parsed.iter().filter(|e| e.ts < comp.ts).last();
        let next_checklist = parsed.iter().find(|e| {
            e.ts > comp.ts
                && e.event.event == "checklist"
                && (e.ts - comp.ts).num_seconds() < 600
        });
        if let (Some(before), Some(after)) = (last_before, next_checklist) {
            let dt = (after.ts - before.ts).num_seconds();
            if dt < 1800 {
                downtimes.push(dt);
            }
        }
    }

    if !downtimes.is_empty() {
        let avg = downtimes.iter().sum::<i64>() / downtimes.len() as i64;
        let min_d = *downtimes.iter().min().unwrap();
        let max_d = *downtimes.iter().max().unwrap();
        let mut sorted = downtimes.clone();
        sorted.sort();
        let median = sorted[sorted.len() / 2];

        out.push_str("Total downtime (last activity → checklist complete):\n");
        out.push_str(&format!("  Average:  {}\n", fmt_duration(avg)));
        out.push_str(&format!("  Median:   {}\n", fmt_duration(median)));
        out.push_str(&format!("  Min:      {}\n", fmt_duration(min_d)));
        out.push_str(&format!("  Max:      {}\n\n", fmt_duration(max_d)));
    }

    // Timeline
    out.push_str("Compaction timeline:\n");
    let mut prev_ts: Option<DateTime<Utc>> = None;
    for (i, comp) in compactions.iter().enumerate() {
        let interval_str = prev_ts
            .map(|p| format!("  (+{})", fmt_duration((comp.ts - p).num_seconds())))
            .unwrap_or_default();

        let dur_str = sessions
            .iter()
            .find(|(_, end)| *end == comp.ts)
            .map(|(start, end)| format!("  [session: {}]", fmt_duration((*end - *start).num_seconds())))
            .unwrap_or_default();

        let oh_str = parsed
            .iter()
            .find(|e| {
                e.ts > comp.ts
                    && e.event.event == "checklist"
                    && (e.ts - comp.ts).num_seconds() < 600
            })
            .map(|e| format!("  [resume: {}]", fmt_duration((e.ts - comp.ts).num_seconds())))
            .unwrap_or_default();

        let clean_str = if clean_flags[i] {
            " [CLEAN]"
        } else {
            " [UNCLEAN]"
        };

        let tok_str = comp
            .event
            .tokens
            .map(|t| format!("  [{}tok]", format_number(t)))
            .unwrap_or_default();

        let note_str = comp
            .event
            .note
            .as_ref()
            .map(|n| format!("  ({})", n))
            .unwrap_or_default();

        out.push_str(&format!(
            "  {}{}{}{}{}{}{}\n",
            comp.ts.format("%Y-%m-%d %H:%M:%S"),
            interval_str,
            dur_str,
            oh_str,
            clean_str,
            tok_str,
            note_str
        ));
        prev_ts = Some(comp.ts);
    }

    // Token stats
    let token_events: Vec<&ParsedEvent> = parsed.iter().filter(|e| e.event.tokens.is_some()).collect();
    if !token_events.is_empty() {
        let mut tok_by_type: std::collections::HashMap<&str, Vec<u64>> =
            std::collections::HashMap::new();
        for e in &token_events {
            if let Some(t) = e.event.tokens {
                tok_by_type
                    .entry(&e.event.event)
                    .or_default()
                    .push(t);
            }
        }

        out.push_str("\nToken counts by event type:\n");
        for etype in ["compact-prep", "boot", "restart", "checklist"] {
            if let Some(vals) = tok_by_type.get(etype) {
                let avg = vals.iter().sum::<u64>() / vals.len() as u64;
                let min_v = *vals.iter().min().unwrap();
                let max_v = *vals.iter().max().unwrap();
                out.push_str(&format!(
                    "  {:15}  avg: {}  min: {}  max: {}  (n={})\n",
                    etype,
                    format_number(avg),
                    format_number(min_v),
                    format_number(max_v),
                    vals.len()
                ));
            }
        }
    }

    out
}

/// Format the history output. Returns the formatted string.
pub fn format_history(tasks: &[CompletedTask]) -> String {
    if tasks.is_empty() {
        return "No completed tasks.".to_string();
    }

    let mut out = format!("Last {} completed tasks:\n", tasks.len());
    for task in tasks {
        let ts = if task.completed_at.contains('T') {
            let parts: Vec<&str> = task.completed_at.splitn(2, 'T').collect();
            if parts.len() == 2 {
                let time_part = if parts[1].len() >= 8 {
                    &parts[1][..8]
                } else {
                    parts[1]
                };
                format!("{} {}", parts[0], time_part)
            } else {
                task.completed_at.clone()
            }
        } else {
            task.completed_at.clone()
        };
        out.push_str(&format!("  [{}] {}\n", ts, task.task));
    }
    out
}

/// Format a number with comma separators.
pub fn format_number(n: u64) -> String {
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

/// Result of checking whether compaction stats DM is due.
pub enum CompactionStatsDue {
    /// Stats are due (hours since last post)
    Due(u64),
    /// Stats are not due yet (hours since last post)
    NotDue(u64),
    /// No timestamp file found (never posted)
    NeverPosted,
    /// Error reading/parsing the timestamp
    Error(String),
}

/// Path to the compaction stats timestamp file.
fn compaction_stats_timestamp_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
    PathBuf::from(home)
        .join(".config")
        .join("signal-stats")
        .join("last-compaction-post")
}

/// Check if the daily compaction stats DM is due (>=24h since last post).
pub fn check_compaction_stats_due() -> CompactionStatsDue {
    let path = compaction_stats_timestamp_path();
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c.trim().to_string(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return CompactionStatsDue::NeverPosted,
        Err(e) => return CompactionStatsDue::Error(format!("Failed to read {}: {}", path.display(), e)),
    };

    let last_ts: DateTime<Utc> = match content.parse::<DateTime<chrono::FixedOffset>>() {
        Ok(dt) => dt.with_timezone(&Utc),
        Err(_) => match content.parse::<DateTime<Utc>>() {
            Ok(dt) => dt,
            Err(e) => return CompactionStatsDue::Error(format!("Failed to parse timestamp '{}': {}", content, e)),
        },
    };

    let hours_ago = (Utc::now() - last_ts).num_hours() as u64;
    if hours_ago >= 24 {
        CompactionStatsDue::Due(hours_ago)
    } else {
        CompactionStatsDue::NotDue(hours_ago)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration_secs() {
        assert_eq!(parse_duration_secs("30s").unwrap(), 30);
        assert_eq!(parse_duration_secs("5m").unwrap(), 300);
        assert_eq!(parse_duration_secs("2h").unwrap(), 7200);
        assert_eq!(parse_duration_secs("1d").unwrap(), 86400);
        assert!(parse_duration_secs("abc").is_err());
        assert!(parse_duration_secs("10x").is_err());
        assert!(parse_duration_secs("").is_err());
    }

    #[test]
    fn test_fmt_duration() {
        assert_eq!(fmt_duration(0), "0s");
        assert_eq!(fmt_duration(30), "30s");
        assert_eq!(fmt_duration(59), "59s");
        assert_eq!(fmt_duration(60), "1m00s");
        assert_eq!(fmt_duration(90), "1m30s");
        assert_eq!(fmt_duration(3600), "1h00m");
        assert_eq!(fmt_duration(3661), "1h01m");
        assert_eq!(fmt_duration(7384), "2h03m");
    }

    #[test]
    fn test_format_number() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1000), "1,000");
        assert_eq!(format_number(1234567), "1,234,567");
        assert_eq!(format_number(50000), "50,000");
    }

    #[test]
    fn test_read_events_from_str_empty() {
        let events = read_events_from_str("", None);
        assert!(events.is_empty());
    }

    #[test]
    fn test_read_events_from_str_basic() {
        let content = r#"{"timestamp":"2026-03-16T12:00:00+00:00","event":"boot","note":"test"}
{"timestamp":"2026-03-16T12:30:00+00:00","event":"checklist"}
{"timestamp":"2026-03-16T13:00:00+00:00","event":"compaction","tokens":50000}
"#;
        let events = read_events_from_str(content, None);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event, "boot");
        assert_eq!(events[0].note.as_deref(), Some("test"));
        assert_eq!(events[1].event, "checklist");
        assert_eq!(events[1].note, None);
        assert_eq!(events[2].event, "compaction");
        assert_eq!(events[2].tokens, Some(50000));
    }

    #[test]
    fn test_read_events_from_str_skips_corrupt() {
        let content = "not json\n{\"timestamp\":\"2026-03-16T12:00:00+00:00\",\"event\":\"boot\"}\n{broken\n";
        let events = read_events_from_str(content, None);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "boot");
    }

    #[test]
    fn test_read_completed_tasks_from_str() {
        let content = r#"{"task":"Did thing A","completed_at":"2026-03-16T12:00:00+00:00"}
{"task":"Did thing B","completed_at":"2026-03-16T13:00:00+00:00"}
{"task":"Did thing C","completed_at":"2026-03-16T14:00:00+00:00"}
"#;
        let tasks = read_completed_tasks_from_str(content, 2);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].task, "Did thing B");
        assert_eq!(tasks[1].task, "Did thing C");
    }

    #[test]
    fn test_read_completed_tasks_from_str_all() {
        let content = r#"{"task":"A","completed_at":"2026-03-16T12:00:00+00:00"}
{"task":"B","completed_at":"2026-03-16T13:00:00+00:00"}
"#;
        let tasks = read_completed_tasks_from_str(content, 10);
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn test_format_stats_empty() {
        assert_eq!(format_stats(&[]), "No events recorded.");
    }

    #[test]
    fn test_format_stats_basic() {
        let events = vec![
            SessionEvent {
                timestamp: "2026-03-16T12:00:00+00:00".to_string(),
                event: "boot".to_string(),
                note: None,
                tokens: None,
            },
            SessionEvent {
                timestamp: "2026-03-16T12:30:00+00:00".to_string(),
                event: "checklist".to_string(),
                note: Some("test".to_string()),
                tokens: Some(50000),
            },
        ];
        let output = format_stats(&events);
        assert!(output.contains("Session events: 2 total"));
        assert!(output.contains("boot"));
        assert!(output.contains("checklist"));
        assert!(output.contains("50,000tok"));
        assert!(output.contains("(test)"));
    }

    #[test]
    fn test_format_compaction_stats_empty() {
        assert_eq!(format_compaction_stats(&[]), "No events recorded.");
    }

    #[test]
    fn test_format_compaction_stats_no_compactions() {
        let events = vec![SessionEvent {
            timestamp: "2026-03-16T12:00:00+00:00".to_string(),
            event: "boot".to_string(),
            note: None,
            tokens: None,
        }];
        assert_eq!(
            format_compaction_stats(&events),
            "No compaction events found."
        );
    }

    #[test]
    fn test_format_compaction_stats_basic() {
        let events = vec![
            SessionEvent {
                timestamp: "2026-03-16T12:00:00+00:00".to_string(),
                event: "boot".to_string(),
                note: None,
                tokens: None,
            },
            SessionEvent {
                timestamp: "2026-03-16T12:05:00+00:00".to_string(),
                event: "compact-prep".to_string(),
                note: None,
                tokens: Some(180000),
            },
            SessionEvent {
                timestamp: "2026-03-16T12:06:00+00:00".to_string(),
                event: "compaction".to_string(),
                note: None,
                tokens: Some(180000),
            },
            SessionEvent {
                timestamp: "2026-03-16T12:08:00+00:00".to_string(),
                event: "checklist".to_string(),
                note: None,
                tokens: Some(5000),
            },
        ];
        let output = format_compaction_stats(&events);
        assert!(output.contains("=== Compaction Stats ==="));
        assert!(output.contains("Total compactions: 1"));
        assert!(output.contains("[CLEAN]"));
        assert!(output.contains("Compaction timeline:"));
    }

    #[test]
    fn test_format_history_empty() {
        assert_eq!(format_history(&[]), "No completed tasks.");
    }

    #[test]
    fn test_format_history_basic() {
        let tasks = vec![
            CompletedTask {
                task: "Did something".to_string(),
                completed_at: "2026-03-16T12:00:00+00:00".to_string(),
            },
            CompletedTask {
                task: "Did another thing".to_string(),
                completed_at: "2026-03-16T13:00:00+00:00".to_string(),
            },
        ];
        let output = format_history(&tasks);
        assert!(output.contains("Last 2 completed tasks:"));
        assert!(output.contains("[2026-03-16 12:00:00] Did something"));
        assert!(output.contains("[2026-03-16 13:00:00] Did another thing"));
    }

    #[test]
    fn test_session_event_serialization() {
        let event = SessionEvent {
            timestamp: "2026-03-16T12:00:00+00:00".to_string(),
            event: "boot".to_string(),
            note: Some("test note".to_string()),
            tokens: Some(50000),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"boot\""));
        assert!(json.contains("\"note\":\"test note\""));
        assert!(json.contains("\"tokens\":50000"));
    }

    #[test]
    fn test_session_event_serialization_skip_none() {
        let event = SessionEvent {
            timestamp: "2026-03-16T12:00:00+00:00".to_string(),
            event: "boot".to_string(),
            note: None,
            tokens: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("note"));
        assert!(!json.contains("tokens"));
    }

    #[test]
    fn test_valid_events_constant() {
        assert!(VALID_EVENTS.contains(&"boot"));
        assert!(VALID_EVENTS.contains(&"compaction"));
        assert!(VALID_EVENTS.contains(&"restart"));
        assert!(VALID_EVENTS.contains(&"exit"));
        assert!(VALID_EVENTS.contains(&"checklist"));
        assert!(VALID_EVENTS.contains(&"compact-prep"));
        assert!(!VALID_EVENTS.contains(&"invalid"));
    }
}
