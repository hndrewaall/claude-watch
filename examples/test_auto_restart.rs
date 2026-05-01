//! Manual smoke-test helper: invoke the production `auto_restart_watcher`
//! against the live `~/.config/watchmen/watchers.conf` so we can verify
//! the spawn pattern end-to-end without waiting for the daemon's natural
//! detection cycle.
//!
//! Usage:
//!     cargo run --release --example test_auto_restart -- claude-event-watch
//!
//! Verification recipe (per q-2026-04-28-6602):
//!     1. `pkill -f claude-event-watch` (and any inotifywait children)
//!     2. `cargo run --release --example test_auto_restart -- claude-event-watch`
//!     3. `systemctl --user is-active claude-watch-watcher-claude-event-watch.service`
//!     4. `ps -p <returned_pid> -o pid,ppid,cmd` 30s+ later
//!
//! Prints `OK: pid=<n>` on success, `ERR: <reason>` on failure.

use claude_watch::watcher::auto_restart_watcher;

#[tokio::main]
async fn main() {
    let name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "claude-event-watch".to_string());
    let cfg = std::env::var("WATCHERS_CONFIG").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
        format!("{}/.config/watchmen/watchers.conf", home)
    });
    println!("Calling auto_restart_watcher(cfg={}, name={})...", cfg, name);
    match auto_restart_watcher(&cfg, &name).await {
        Ok((pid, n)) => {
            println!("OK: pid={} name={}", pid, n);
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("ERR: {}", e);
            std::process::exit(1);
        }
    }
}
