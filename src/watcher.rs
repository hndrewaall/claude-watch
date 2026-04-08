//! Watcher supervision: list, status, run, enable/disable, restart.
//!
//! Replaces the shell scripts `watcher-ctl`, `watcher-status`, and
//! `watcher-restart` with native Rust implementations.

use crate::cmd::run_cmd_any;
use crate::status::{parse_watchers_config, WatcherEntry};
use serde::Serialize;
use std::io::Write;

/// Default config path for watchers.
const DEFAULT_CONFIG: &str = ".config/watchmen/watchers.conf";

/// PID file directory for watcher liveness tracking.
const PID_DIR: &str = "/var/run/claude";

/// Resolve the watchers.conf path (respects $WATCHERS_CONFIG for testing).
pub fn config_path() -> String {
    if let Ok(p) = std::env::var("WATCHERS_CONFIG") {
        return p;
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
    format!("{}/{}", home, DEFAULT_CONFIG)
}

/// Status of a single watcher.
#[derive(Debug, Serialize)]
pub struct WatcherStatus {
    pub name: String,
    pub status: String, // "ok", "DOWN", "off"
    pub count: u32,
    pub required: u32,
    pub pids: String,
    pub enabled: bool,
}

/// Get process count for a pattern via `pgrep -fc`.
pub async fn process_count(pattern: &str) -> u32 {
    let (out, _) = run_cmd_any(&["pgrep", "-fc", "--", pattern], 5).await;
    out.trim().parse().unwrap_or(0)
}

/// Get PIDs matching a pattern via `pgrep -f`.
pub async fn process_pids(pattern: &str) -> Vec<u32> {
    let (out, _) = run_cmd_any(&["pgrep", "-f", "--", pattern], 5).await;
    out.lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .collect()
}

/// List all watcher entries from config.
pub fn watcher_list(config_path: &str) -> Vec<WatcherEntry> {
    parse_watchers_config(config_path)
}

/// Get status for all watchers.
pub async fn watcher_status(config_path: &str) -> Vec<WatcherStatus> {
    let entries = parse_watchers_config(config_path);
    let mut results = Vec::new();

    for entry in &entries {
        if !entry.enabled {
            results.push(WatcherStatus {
                name: entry.name.clone(),
                status: "off".to_string(),
                count: 0,
                required: entry.min_count,
                pids: String::new(),
                enabled: false,
            });
            continue;
        }

        let count = process_count(&entry.pattern).await;
        let pids = process_pids(&entry.pattern).await;
        let pid_str = pids
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(" ");

        let status = if count >= entry.min_count {
            "ok".to_string()
        } else {
            "DOWN".to_string()
        };

        results.push(WatcherStatus {
            name: entry.name.clone(),
            status,
            count,
            required: entry.min_count,
            pids: pid_str,
            enabled: true,
        });
    }

    results
}

/// Run a watcher by name. Looks up the entry, rejects if disabled or no
/// start_cmd, then execs the start_cmd and waits for it to complete.
/// Returns the exit code of the child process.
pub async fn watcher_run(config_path: &str, name: &str) -> Result<i32, String> {
    let entries = parse_watchers_config(config_path);
    let entry = entries
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| format!("watcher '{}' not found in config", name))?;

    if !entry.enabled {
        return Err(format!("watcher '{}' is disabled", name));
    }

    let start_cmd = entry
        .start_cmd
        .as_deref()
        .ok_or_else(|| format!("no start command configured for '{}'", name))?;

    // Create PID directory if needed
    let _ = std::fs::create_dir_all(PID_DIR);

    // Print history on restart (PID file exists from previous run)
    let pid_file = format!("{}/{}.pid", PID_DIR, name);
    if std::path::Path::new(&pid_file).exists() {
        // Fire handler: print relevant history so it appears in task output
        match name {
            "signal-wait-group" => {
                let _ = run_cmd_any(&["signal-history", "--group", "--since", "5m"], 10).await;
            }
            "signal-wait-dm" => {
                let _ = run_cmd_any(&["signal-history", "--dm", "--since", "5m"], 10).await;
            }
            "torrent-wait" => {
                let _ = run_cmd_any(&["torrent-check"], 10).await;
            }
            _ => {}
        }
    }

    // Parse start_cmd into args (shell-style split)
    let args: Vec<&str> = start_cmd.split_whitespace().collect();
    if args.is_empty() {
        return Err(format!("empty start command for '{}'", name));
    }

    // Spawn child process
    let mut child = tokio::process::Command::new(args[0])
        .args(&args[1..])
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| format!("failed to start '{}': {}", start_cmd, e))?;

    // Write PID file
    let pid = child.id().unwrap_or(0);
    let _ = std::fs::write(&pid_file, pid.to_string());

    // Wait for child to exit
    let status = child
        .wait()
        .await
        .map_err(|e| format!("failed to wait for '{}': {}", name, e))?;

    Ok(status.code().unwrap_or(1))
}

/// Enable or disable a watcher by rewriting the config file.
/// On disable, kills matching processes.
/// On enable, kills existing instances and starts via nohup.
pub async fn watcher_toggle(config_path: &str, name: &str, enable: bool) -> Result<String, String> {
    let content = std::fs::read_to_string(config_path)
        .map_err(|e| format!("failed to read config: {}", e))?;

    let new_val = if enable { "true" } else { "false" };
    let mut found = false;
    let mut target_pattern = String::new();
    let mut target_start_cmd: Option<String> = None;
    let mut output_lines = Vec::new();

    for line in content.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            output_lines.push(line.to_string());
            continue;
        }

        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() >= 2 && parts[0] == name {
            found = true;
            target_pattern = parts[1].to_string();
            let min_count = parts.get(2).unwrap_or(&"1");
            let start_cmd = parts.get(4).unwrap_or(&"");
            if !start_cmd.is_empty() {
                target_start_cmd = Some(start_cmd.to_string());
            }
            output_lines.push(format!(
                "{}|{}|{}|{}|{}",
                parts[0], parts[1], min_count, new_val, start_cmd
            ));
        } else {
            output_lines.push(line.to_string());
        }
    }

    if !found {
        return Err(format!("watcher '{}' not found in config", name));
    }

    // Write updated config
    let new_content = output_lines.join("\n") + "\n";
    let mut file =
        std::fs::File::create(config_path).map_err(|e| format!("failed to write config: {}", e))?;
    file.write_all(new_content.as_bytes())
        .map_err(|e| format!("failed to write config: {}", e))?;

    if enable {
        // Kill any existing instances first to avoid duplicates
        let pids = process_pids(&target_pattern).await;
        if !pids.is_empty() {
            for pid in &pids {
                let _ = run_cmd_any(&["kill", &pid.to_string()], 5).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        if let Some(cmd) = &target_start_cmd {
            let args: Vec<&str> = cmd.split_whitespace().collect();
            if !args.is_empty() {
                let child = tokio::process::Command::new("nohup")
                    .args(&args)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
                match child {
                    Ok(c) => {
                        let pid = c.id().unwrap_or(0);
                        return Ok(format!("{}: enabled (started, pid {})", name, pid));
                    }
                    Err(e) => {
                        return Ok(format!("{}: enabled (failed to start: {})", name, e));
                    }
                }
            }
        }
        Ok(format!(
            "{}: enabled (no start command configured -- start manually)",
            name
        ))
    } else {
        // Kill matching processes
        let pids = process_pids(&target_pattern).await;
        if !pids.is_empty() {
            let count = pids.len();
            for pid in &pids {
                let _ = run_cmd_any(&["kill", &pid.to_string()], 5).await;
            }
            Ok(format!("{}: disabled (killed {} process(es))", name, count))
        } else {
            Ok(format!("{}: disabled (no processes running)", name))
        }
    }
}

/// Kill all enabled watcher processes and clean PID files.
pub async fn watcher_restart(config_path: &str) -> String {
    let entries = parse_watchers_config(config_path);
    let mut total = 0u32;
    let mut messages = Vec::new();

    for entry in &entries {
        if !entry.enabled {
            continue;
        }
        let pids = process_pids(&entry.pattern).await;
        if !pids.is_empty() {
            let count = pids.len() as u32;
            for pid in &pids {
                let _ = run_cmd_any(&["kill", &pid.to_string()], 5).await;
            }
            messages.push(format!("Killed {} {} process(es)", count, entry.name));
            total += count;
        }
    }

    // Clean PID files
    if let Ok(dir) = std::fs::read_dir(PID_DIR) {
        for entry in dir.flatten() {
            if entry.path().extension().is_some_and(|ext| ext == "pid") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        messages.push("Cleaned PID files".to_string());
    }

    if total == 0 {
        messages.push("No watchers running.".to_string());
    } else {
        messages.push(format!(
            "\nKilled {} total process(es). All watchers stopped.",
            total
        ));
    }

    messages.join("\n")
}

// --- CLI command handlers ---

/// `claude-watch watcher list [--json]`
pub fn cmd_list(config_path: &str, json: bool) {
    let entries = watcher_list(config_path);

    if json {
        let items: Vec<serde_json::Value> = entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "name": e.name,
                    "pattern": e.pattern,
                    "min_count": e.min_count,
                    "enabled": e.enabled,
                    "start_cmd": e.start_cmd,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items).unwrap());
    } else {
        println!("{:<20} {:<8} PATTERN", "NAME", "ENABLED");
        println!("{:<20} {:<8} -------", "----", "-------");
        for e in &entries {
            println!("{:<20} {:<8} {}", e.name, e.enabled, e.pattern);
        }
    }
}

/// `claude-watch watcher status [--json]`
pub async fn cmd_status(config_path: &str, json: bool) {
    let statuses = watcher_status(config_path).await;

    if json {
        println!("{}", serde_json::to_string_pretty(&statuses).unwrap());
    } else {
        let mut all_ok = true;
        for s in &statuses {
            if s.status == "off" {
                println!("{:<20} {:<4} (disabled)", s.name, s.status);
            } else {
                if s.status == "DOWN" {
                    all_ok = false;
                }
                println!(
                    "{:<20} {:<4} ({}/{})  {}",
                    s.name, s.status, s.count, s.required, s.pids
                );
            }
        }
        if all_ok {
            println!("\nAll watchers healthy.");
        } else {
            println!("\nWARNING: Some watchers are down!");
        }
    }
}

/// `claude-watch watcher run <name>`
pub async fn cmd_run(config_path: &str, name: &str) -> i32 {
    match watcher_run(config_path, name).await {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("Error: {}", msg);
            1
        }
    }
}

/// `claude-watch watcher enable <name>` / `claude-watch watcher disable <name>`
pub async fn cmd_toggle(config_path: &str, name: &str, enable: bool) -> i32 {
    match watcher_toggle(config_path, name, enable).await {
        Ok(msg) => {
            println!("{}", msg);
            0
        }
        Err(msg) => {
            eprintln!("Error: {}", msg);
            1
        }
    }
}

/// `claude-watch watcher restart`
pub async fn cmd_restart(config_path: &str) {
    let output = watcher_restart(config_path).await;
    println!("{}", output);
}

// --- Pure function tests ---

/// Pure function: format watcher list output (for testing without I/O).
#[allow(dead_code)]
pub fn format_list(entries: &[WatcherEntry]) -> String {
    let mut out = String::new();
    out.push_str(&format!("{:<20} {:<8} {}\n", "NAME", "ENABLED", "PATTERN"));
    out.push_str(&format!("{:<20} {:<8} {}\n", "----", "-------", "-------"));
    for e in entries {
        out.push_str(&format!("{:<20} {:<8} {}\n", e.name, e.enabled, e.pattern));
    }
    out
}

/// Pure function: format watcher status output (for testing without I/O).
#[allow(dead_code)]
pub fn format_status(statuses: &[WatcherStatus]) -> String {
    let mut out = String::new();
    let mut all_ok = true;
    for s in statuses {
        if s.status == "off" {
            out.push_str(&format!("{:<20} {:<4} (disabled)\n", s.name, s.status));
        } else {
            if s.status == "DOWN" {
                all_ok = false;
            }
            out.push_str(&format!(
                "{:<20} {:<4} ({}/{})  {}\n",
                s.name, s.status, s.count, s.required, s.pids
            ));
        }
    }
    if all_ok {
        out.push_str("\nAll watchers healthy.\n");
    } else {
        out.push_str("\nWARNING: Some watchers are down!\n");
    }
    out
}

/// Pure function: rewrite config content toggling the enabled field for a watcher.
/// Returns the new config content, or None if the watcher was not found.
#[allow(dead_code)]
pub fn rewrite_config_toggle(content: &str, name: &str, enable: bool) -> Option<String> {
    let new_val = if enable { "true" } else { "false" };
    let mut found = false;
    let mut output_lines = Vec::new();

    for line in content.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            output_lines.push(line.to_string());
            continue;
        }

        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() >= 2 && parts[0] == name {
            found = true;
            let min_count = parts.get(2).unwrap_or(&"1");
            let start_cmd = parts.get(4).unwrap_or(&"");
            output_lines.push(format!(
                "{}|{}|{}|{}|{}",
                parts[0], parts[1], min_count, new_val, start_cmd
            ));
        } else {
            output_lines.push(line.to_string());
        }
    }

    if found {
        Some(output_lines.join("\n") + "\n")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_list_basic() {
        let entries = vec![
            WatcherEntry {
                name: "sig".to_string(),
                pattern: "sig$".to_string(),
                min_count: 1,
                enabled: true,
                start_cmd: Some("signal-wait".to_string()),
            },
            WatcherEntry {
                name: "torrent".to_string(),
                pattern: "torrent$".to_string(),
                min_count: 1,
                enabled: false,
                start_cmd: None,
            },
        ];
        let output = format_list(&entries);
        assert!(output.contains("sig"));
        assert!(output.contains("torrent"));
        assert!(output.contains("true"));
        assert!(output.contains("false"));
    }

    #[test]
    fn test_format_status_all_ok() {
        let statuses = vec![WatcherStatus {
            name: "sig".to_string(),
            status: "ok".to_string(),
            count: 1,
            required: 1,
            pids: "1234".to_string(),
            enabled: true,
        }];
        let output = format_status(&statuses);
        assert!(output.contains("ok"));
        assert!(output.contains("All watchers healthy."));
    }

    #[test]
    fn test_format_status_some_down() {
        let statuses = vec![
            WatcherStatus {
                name: "sig".to_string(),
                status: "ok".to_string(),
                count: 1,
                required: 1,
                pids: "1234".to_string(),
                enabled: true,
            },
            WatcherStatus {
                name: "torrent".to_string(),
                status: "DOWN".to_string(),
                count: 0,
                required: 1,
                pids: String::new(),
                enabled: true,
            },
        ];
        let output = format_status(&statuses);
        assert!(output.contains("DOWN"));
        assert!(output.contains("WARNING: Some watchers are down!"));
    }

    #[test]
    fn test_format_status_disabled() {
        let statuses = vec![WatcherStatus {
            name: "ctx".to_string(),
            status: "off".to_string(),
            count: 0,
            required: 1,
            pids: String::new(),
            enabled: false,
        }];
        let output = format_status(&statuses);
        assert!(output.contains("off"));
        assert!(output.contains("disabled"));
        assert!(output.contains("All watchers healthy."));
    }

    #[test]
    fn test_rewrite_config_enable() {
        let config =
            "# comment\nsig|sig$|1|false|signal-wait\ntorrent|torrent$|1|true|torrent-wait\n";
        let result = rewrite_config_toggle(config, "sig", true).unwrap();
        assert!(result.contains("sig|sig$|1|true|signal-wait"));
        assert!(result.contains("torrent|torrent$|1|true|torrent-wait"));
    }

    #[test]
    fn test_rewrite_config_disable() {
        let config = "sig|sig$|1|true|signal-wait\n";
        let result = rewrite_config_toggle(config, "sig", false).unwrap();
        assert!(result.contains("sig|sig$|1|false|signal-wait"));
    }

    #[test]
    fn test_rewrite_config_not_found() {
        let config = "sig|sig$|1|true|signal-wait\n";
        let result = rewrite_config_toggle(config, "nonexistent", true);
        assert!(result.is_none());
    }

    #[test]
    fn test_rewrite_config_preserves_comments() {
        let config = "# header comment\n\nsig|sig$|1|true|cmd\n# footer\n";
        let result = rewrite_config_toggle(config, "sig", false).unwrap();
        assert!(result.contains("# header comment"));
        assert!(result.contains("# footer"));
        assert!(result.contains("false"));
    }

    #[test]
    fn test_rewrite_config_minimal_fields() {
        let config = "sig|sig$\n";
        let result = rewrite_config_toggle(config, "sig", false).unwrap();
        assert!(result.contains("sig|sig$|1|false|"));
    }

    #[test]
    fn test_format_list_empty() {
        let entries: Vec<WatcherEntry> = vec![];
        let output = format_list(&entries);
        assert!(output.contains("NAME"));
        // Just headers, no entries
        assert_eq!(output.lines().count(), 2);
    }
}
