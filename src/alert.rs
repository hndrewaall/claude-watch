//! Alerting: pingme notifications and interrupt-then-inject.

use crate::cmd::run_cmd;
use crate::tmux;

pub async fn send_pingme(message: &str) {
    send_pingme_with_priority(message, "normal").await;
}

pub async fn send_pingme_with_priority(message: &str, priority: &str) {
    let _ = run_cmd(&["pingme", "-p", priority, message, "claude-watch"], 15).await;
}

pub async fn alert(message: &str, pane: &str, resume_prompt: &str, use_pingme: bool) {
    if use_pingme {
        send_pingme(message).await;
    }

    // Actively interrupt, then inject resume prompt
    tmux::interrupt_and_wait(pane, 30).await;
    tmux::inject_text(pane, resume_prompt).await;
}
