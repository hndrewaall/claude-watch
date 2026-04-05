//! Command execution helpers with timeout.

use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// Run a command with timeout. Returns stdout on success or non-zero exit
/// (if stdout is non-empty). Returns None on timeout or empty output from failure.
pub async fn run_cmd(args: &[&str], timeout_secs: u64) -> Option<String> {
    let result = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        Command::new(args[0])
            .args(&args[1..])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) if output.status.success() => {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        Ok(Ok(output)) => {
            // Command ran but non-zero exit -- still return stdout for pgrep etc.
            let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !out.is_empty() {
                Some(out)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Run a command, returning stdout regardless of exit code.
pub async fn run_cmd_any(args: &[&str], timeout_secs: u64) -> (String, bool) {
    let result = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        Command::new(args[0])
            .args(&args[1..])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (out, output.status.success())
        }
        _ => (String::new(), false),
    }
}
