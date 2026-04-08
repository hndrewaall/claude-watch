//! Stdin/stdout text filters used by task-watch panes.
//!
//! - `timestamp_lines` — prepend each stdin line with `[HH:MM:SS]`
//! - `format_jsonl`   — pretty-print Claude Code agent JSONL event streams
//!
//! Ported from the Python `task-watch format-jsonl` and `task-watch timestamp-lines`
//! subcommands so the tail pipeline in `add_pane` can call the single Rust binary.

use std::io::{self, BufRead, Write};

use chrono::Local;
use serde_json::Value;

const BLUE: &str = "\x1b[34m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const CYAN: &str = "\x1b[36m";

/// Prefix each stdin line with `[HH:MM:SS]` and write to stdout.
/// Exits 0 on EOF or broken pipe.
pub fn cmd_timestamp_lines() -> i32 {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let ts = Local::now().format("%H:%M:%S");
        if writeln!(out, "[{}] {}", ts, line).is_err() {
            break;
        }
        // Best-effort flush; ignore broken pipe.
        let _ = out.flush();
    }
    0
}

/// Pretty-print agent JSONL from stdin.
///
/// Reads Claude Code agent JSONL lines and extracts human-readable content:
/// - Assistant text messages (what the agent says)
/// - Tool calls (abbreviated: tool name + key params)
/// - Bash command output (progress updates)
///
/// Skips raw tool results and internal protocol messages.
pub fn cmd_format_jsonl() -> i32 {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut prompt_shown = false;

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if let Some(formatted) = format_line(&line, &mut prompt_shown) {
            if writeln!(out, "{}", formatted).is_err() {
                break;
            }
            let _ = out.flush();
        }
    }
    0
}

/// Pure formatter: return Some(rendered) if the JSONL line produced output.
/// `prompt_shown` tracks whether we've already rendered the agent prompt block.
pub fn format_line(line: &str, prompt_shown: &mut bool) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let obj: Value = serde_json::from_str(line).ok()?;
    let msg_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

    // First user message → show agent prompt
    if msg_type == "user" && !*prompt_shown {
        return format_prompt(&obj, prompt_shown);
    }

    if msg_type == "assistant" {
        let content = obj.get("message").and_then(|m| m.get("content"))?;
        let arr = content.as_array()?;
        let mut parts: Vec<String> = Vec::new();
        for block in arr {
            let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match btype {
                "text" => {
                    let text = block
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim();
                    if !text.is_empty() {
                        parts.push(format!("{}{}{}", BLUE, text, RESET));
                    }
                }
                "tool_use" => {
                    let tool = block.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    match tool {
                        "Bash" => {
                            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
                            let desc = input
                                .get("description")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            if !desc.is_empty() {
                                parts.push(format!("{}$ {}{}", GREEN, desc, RESET));
                            } else if !cmd.is_empty() {
                                let short: String = if cmd.chars().count() > 120 {
                                    let t: String = cmd.chars().take(120).collect();
                                    format!("{}...", t)
                                } else {
                                    cmd.to_string()
                                };
                                parts.push(format!("{}$ {}{}", GREEN, short, RESET));
                            }
                        }
                        "Read" => {
                            let path = input
                                .get("file_path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            parts.push(format!("{}[read {}]{}", DIM, path, RESET));
                        }
                        "Edit" => {
                            let path = input
                                .get("file_path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            parts.push(format!("{}[edit {}]{}", YELLOW, path, RESET));
                        }
                        "Write" => {
                            let path = input
                                .get("file_path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            parts.push(format!("{}[write {}]{}", YELLOW, path, RESET));
                        }
                        "Grep" => {
                            let pat = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
                            parts.push(format!("{}[grep {}]{}", DIM, pat, RESET));
                        }
                        "Glob" => {
                            let pat = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
                            parts.push(format!("{}[glob {}]{}", DIM, pat, RESET));
                        }
                        other => {
                            parts.push(format!("{}[{}]{}", DIM, other, RESET));
                        }
                    }
                }
                _ => {}
            }
        }
        if !parts.is_empty() {
            return Some(parts.join("\n"));
        }
        return None;
    }

    if msg_type == "progress" {
        let data = obj.get("data")?;
        let dtype = data.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if dtype == "bash_progress" {
            let output = data
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim_end();
            if !output.is_empty() {
                let lines: Vec<&str> = output.lines().collect();
                let tail: Vec<&str> = if lines.len() > 3 {
                    lines[lines.len() - 3..].to_vec()
                } else {
                    lines
                };
                return Some(format!("{}{}{}", DIM, tail.join("\n"), RESET));
            }
        }
    }

    None
}

/// Format the agent prompt block (first user message).
fn format_prompt(obj: &Value, prompt_shown: &mut bool) -> Option<String> {
    *prompt_shown = true;
    let msg = obj.get("message")?;
    let content = msg.get("content")?;

    let prompt_text = if let Some(s) = content.as_str() {
        s.to_string()
    } else if let Some(arr) = content.as_array() {
        let mut parts: Vec<String> = Vec::new();
        for block in arr {
            if let Some(s) = block.as_str() {
                parts.push(s.to_string());
            } else if let Some(obj) = block.as_object() {
                if obj.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(t) = obj.get("text").and_then(|v| v.as_str()) {
                        parts.push(t.to_string());
                    }
                }
            }
        }
        parts.join("\n")
    } else {
        return None;
    };

    let prompt_text = prompt_text.trim();
    if prompt_text.is_empty() {
        return None;
    }

    // Wrap long prompts to width=100; cap at 20 lines
    let wrapped = wrap_text(prompt_text, 100);
    let mut lines: Vec<String> = wrapped.lines().map(|s| s.to_string()).collect();
    if lines.len() > 20 {
        lines.truncate(20);
        lines.push(format!("{}... (prompt truncated){}", DIM, RESET));
    }

    let header = format!("{}{}--- Agent Prompt ---{}", CYAN, BOLD, RESET);
    let footer = format!("{}{}--- End Prompt ---{}", CYAN, BOLD, RESET);
    let body: Vec<String> = lines
        .iter()
        .map(|l| format!("{}{}{}", CYAN, l, RESET))
        .collect();
    Some(format!("{}\n{}\n{}\n", header, body.join("\n"), footer))
}

/// Simple word-wrap: break text at word boundaries to fit within `width` columns.
/// Preserves existing newlines.
fn wrap_text(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut first_para = true;
    for para in text.split('\n') {
        if !first_para {
            out.push('\n');
        }
        first_para = false;
        if para.is_empty() {
            continue;
        }
        let mut line = String::new();
        for word in para.split_whitespace() {
            if line.is_empty() {
                line.push_str(word);
            } else if line.chars().count() + 1 + word.chars().count() <= width {
                line.push(' ');
                line.push_str(word);
            } else {
                out.push_str(&line);
                out.push('\n');
                line = word.to_string();
            }
        }
        out.push_str(&line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_text_short() {
        assert_eq!(wrap_text("hello world", 100), "hello world");
    }

    #[test]
    fn wrap_text_breaks_long_line() {
        let input = "a b c d e f g h";
        let out = wrap_text(input, 3);
        // Each word fits but "a b" = 3, adding " c" would be 5 > 3
        assert!(out.contains('\n'));
    }

    #[test]
    fn wrap_text_preserves_newlines() {
        let out = wrap_text("line1\nline2", 100);
        assert_eq!(out, "line1\nline2");
    }

    #[test]
    fn format_line_assistant_text() {
        let mut shown = true; // skip prompt handling
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"}]}}"#;
        let out = format_line(line, &mut shown).unwrap();
        assert!(out.contains("Hello"));
    }

    #[test]
    fn format_line_bash_tool_with_description() {
        let mut shown = true;
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls","description":"list files"}}]}}"#;
        let out = format_line(line, &mut shown).unwrap();
        assert!(out.contains("list files"));
        assert!(out.contains("$ "));
    }

    #[test]
    fn format_line_bash_tool_truncates_long_cmd() {
        let mut shown = true;
        let long = "x".repeat(200);
        let line = format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"Bash","input":{{"command":"{}"}}}}]}}}}"#,
            long
        );
        let out = format_line(&line, &mut shown).unwrap();
        assert!(out.contains("..."));
    }

    #[test]
    fn format_line_read_tool() {
        let mut shown = true;
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/x"}}]}}"#;
        let out = format_line(line, &mut shown).unwrap();
        assert!(out.contains("[read /x]"));
    }

    #[test]
    fn format_line_empty_returns_none() {
        let mut shown = true;
        assert!(format_line("", &mut shown).is_none());
    }

    #[test]
    fn format_line_invalid_json_returns_none() {
        let mut shown = true;
        assert!(format_line("{not json}", &mut shown).is_none());
    }

    #[test]
    fn format_line_user_prompt_shown_only_once() {
        let mut shown = false;
        let line = r#"{"type":"user","message":{"content":"hello agent"}}"#;
        let out = format_line(line, &mut shown);
        assert!(out.is_some());
        assert!(shown);
        let out2 = format_line(line, &mut shown);
        assert!(out2.is_none());
    }

    #[test]
    fn format_line_progress_bash() {
        let mut shown = true;
        let line = r#"{"type":"progress","data":{"type":"bash_progress","output":"line1\nline2\nline3\nline4"}}"#;
        let out = format_line(line, &mut shown).unwrap();
        // Should keep only last 3 lines
        assert!(!out.contains("line1"));
        assert!(out.contains("line2"));
        assert!(out.contains("line4"));
    }
}
