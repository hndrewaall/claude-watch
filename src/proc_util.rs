//! Shared /proc scanning utilities.
//!
//! Used by both `agent.rs` and `task_watch.rs` for process introspection.

use std::path::Path;

/// Known watcher/service command patterns — processes matching these are persistent
/// services, not ephemeral tasks.
pub const SERVICE_PATTERNS: &[&str] = &[
    "signal-wait",
    "torrent-wait",
    "tv-remind",
    "memory-remind",
    "context-watch",
    "watchmen",
    "watcher-ctl",
    "task-watch",
    "request-wait",
];

/// Output file content signatures — fallback for orphaned processes whose
/// parent chain is broken (reparented to init). Matched against first line.
pub const SERVICE_OUTPUT_SIGNATURES: &[&str] = &[
    "Watcher monitor started", // watchmen
];

/// Read a process's command line from /proc/PID/cmdline.
pub fn get_pid_cmdline(pid: &str) -> Option<String> {
    let path = format!("/proc/{}/cmdline", pid);
    let data = std::fs::read(&path).ok()?;
    let s = String::from_utf8_lossy(&data)
        .replace('\0', " ")
        .trim()
        .to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Get a process's parent PID from /proc/PID/status.
pub fn get_pid_ppid(pid: &str) -> Option<String> {
    let path = format!("/proc/{}/status", pid);
    let content = std::fs::read_to_string(&path).ok()?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            let ppid = rest.trim().to_string();
            if !ppid.is_empty() {
                return Some(ppid);
            }
        }
    }
    None
}

/// Check if an fd is open for writing via /proc/PID/fdinfo/FD.
///
/// Reads the `flags:` line from fdinfo and checks the lowest 2 bits:
/// O_RDONLY=0, O_WRONLY=1, O_RDWR=2. Writable if (flags & 3) >= 1.
pub fn fd_is_writable(pid: &str, fd: &str) -> bool {
    let path = format!("/proc/{}/fdinfo/{}", pid, fd);
    match std::fs::read_to_string(&path) {
        Ok(content) => parse_fdinfo_writable(&content),
        Err(_) => false,
    }
}

/// Parse fdinfo content to determine if the fd is writable.
/// Exported for testing.
pub fn parse_fdinfo_writable(content: &str) -> bool {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("flags:") {
            let flags_str = rest.trim();
            // Flags are in octal
            if let Ok(flags) = u32::from_str_radix(flags_str, 8) {
                return (flags & 3) >= 1;
            }
        }
    }
    false
}

/// Check if a process (or any ancestor up to 5 levels) is a known persistent service.
///
/// Walks the process tree checking cmdline against SERVICE_PATTERNS.
pub fn is_service_process(pid: &str) -> bool {
    let mut visited = std::collections::HashSet::new();
    let mut current = pid.to_string();

    for _ in 0..5 {
        if visited.contains(&current) || current.is_empty() || current == "0" || current == "1" {
            break;
        }
        visited.insert(current.clone());
        if let Some(cmdline) = get_pid_cmdline(&current) {
            if SERVICE_PATTERNS.iter().any(|pat| cmdline.contains(pat)) {
                return true;
            }
        }
        match get_pid_ppid(&current) {
            Some(ppid) => current = ppid,
            None => break,
        }
    }
    false
}

/// Check if a task's output file content matches known service signatures.
///
/// Fallback detection for orphaned processes whose parent chain is broken.
pub fn is_service_output(tasks_dir: &Path, task_id: &str) -> bool {
    let output_file = tasks_dir.join(format!("{}.output", task_id));
    match std::fs::read_to_string(&output_file) {
        Ok(content) => {
            if let Some(first_line) = content.lines().next() {
                SERVICE_OUTPUT_SIGNATURES
                    .iter()
                    .any(|sig| first_line.contains(sig))
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_fdinfo_writable_rdonly() {
        let content = "pos:\t0\nflags:\t0100000\nmnt_id:\t28\n";
        assert!(!parse_fdinfo_writable(content));
    }

    #[test]
    fn test_parse_fdinfo_writable_wronly() {
        // O_WRONLY = 1, plus O_LARGEFILE = 0100000 → 0100001
        let content = "pos:\t0\nflags:\t0100001\nmnt_id:\t28\n";
        assert!(parse_fdinfo_writable(content));
    }

    #[test]
    fn test_parse_fdinfo_writable_rdwr() {
        // O_RDWR = 2, plus O_LARGEFILE = 0100000 → 0100002
        let content = "pos:\t0\nflags:\t0100002\nmnt_id:\t28\n";
        assert!(parse_fdinfo_writable(content));
    }

    #[test]
    fn test_parse_fdinfo_writable_append() {
        // O_WRONLY|O_APPEND = 02001
        let content = "pos:\t0\nflags:\t0102001\nmnt_id:\t28\n";
        assert!(parse_fdinfo_writable(content));
    }

    #[test]
    fn test_parse_fdinfo_writable_empty() {
        assert!(!parse_fdinfo_writable(""));
    }

    #[test]
    fn test_parse_fdinfo_writable_no_flags_line() {
        let content = "pos:\t0\nmnt_id:\t28\n";
        assert!(!parse_fdinfo_writable(content));
    }

    #[test]
    fn test_is_service_output_no_file() {
        let dir = Path::new("/tmp/nonexistent-task-watch-test");
        assert!(!is_service_output(dir, "fake-task-id"));
    }
}
