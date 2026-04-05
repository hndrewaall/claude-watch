//! Structured JSON logging and legacy log writing.

use chrono::{Local, Utc};
use serde::Serialize;
use std::io::Write;

#[derive(Debug, Serialize)]
pub struct LogEvent {
    pub timestamp: String,
    pub level: String,
    pub event: String,
    #[serde(flatten)]
    pub fields: serde_json::Value,
}

pub fn write_jsonl_log(path: &str, event: &str, fields: serde_json::Value) {
    let entry = LogEvent {
        timestamp: Utc::now().to_rfc3339(),
        level: "INFO".to_string(),
        event: event.to_string(),
        fields,
    };
    if let Ok(line) = serde_json::to_string(&entry) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{}", line);
        }
    }
}

pub fn write_legacy_log(path: &str, msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let ts = Local::now().format("%Y-%m-%dT%H:%M:%S");
        let _ = writeln!(f, "[{}] {}", ts, msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_event_serialization() {
        let event = LogEvent {
            timestamp: "2026-03-16T12:00:00Z".to_string(),
            level: "INFO".to_string(),
            event: "check".to_string(),
            fields: serde_json::json!({
                "tokens": 50000,
                "bashes": 10,
            }),
        };
        let json = serde_json::to_string(&event).expect("should serialize");
        assert!(json.contains("\"event\":\"check\""));
        assert!(json.contains("\"tokens\":50000"));
        assert!(json.contains("\"bashes\":10"));
        assert!(json.contains("\"level\":\"INFO\""));
        assert!(json.contains("\"timestamp\":\"2026-03-16T12:00:00Z\""));
    }

    #[test]
    fn test_log_event_flattened_fields() {
        let event = LogEvent {
            timestamp: "2026-03-16T12:00:00Z".to_string(),
            level: "WARN".to_string(),
            event: "alert".to_string(),
            fields: serde_json::json!({
                "stuck_reason": "heartbeat stale",
            }),
        };
        let json = serde_json::to_string(&event).unwrap();
        // Fields should be flattened (not nested under "fields" key)
        assert!(json.contains("\"stuck_reason\":\"heartbeat stale\""));
        assert!(!json.contains("\"fields\""));
    }

    #[test]
    fn test_log_event_empty_fields() {
        let event = LogEvent {
            timestamp: "2026-03-16T12:00:00Z".to_string(),
            level: "INFO".to_string(),
            event: "daemon_stop".to_string(),
            fields: serde_json::json!({}),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"daemon_stop\""));
    }

    #[test]
    fn test_write_jsonl_log_creates_file() {
        let path = "/tmp/claude-watch-test-jsonl.log";
        let _ = std::fs::remove_file(path);

        write_jsonl_log(path, "test_event", serde_json::json!({"key": "value"}));

        let content = std::fs::read_to_string(path).expect("file should exist");
        assert!(content.contains("\"event\":\"test_event\""));
        assert!(content.contains("\"key\":\"value\""));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_write_legacy_log_creates_file() {
        let path = "/tmp/claude-watch-test-legacy.log";
        let _ = std::fs::remove_file(path);

        write_legacy_log(path, "test message");

        let content = std::fs::read_to_string(path).expect("file should exist");
        assert!(content.contains("test message"));
        assert!(content.contains("[20")); // timestamp starts with year
        let _ = std::fs::remove_file(path);
    }
}
