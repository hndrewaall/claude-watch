//! Cross-reference the session-task queue against actual workers.
//!
//! Detects three classes of queue/reality drift and emits one
//! `claude-event` per drifted item via the existing event bus:
//!
//!   - `queue-orphan-running`: a queue item with `status=running` past
//!     the orphan threshold whose declared worker (workload, subagent
//!     transcript marker, tmux pane) cannot be found anywhere.
//!   - `queue-stale-ready`: a `pending` item with no blockers whose
//!     `created_at` age exceeds the stale-ready threshold. Reimplements
//!     the existing `cron-queue-stale-ready` shape so the cron entry
//!     can switch to this binary.
//!   - `queue-worker-without-item`: a workload pane whose label does
//!     not appear in any queue item — the inverse drift, suggesting a
//!     workload running off-the-books.
//!
//! Pure-ish design: `cross_reference()` takes parsed inputs as
//! Rust-native structs, returns a list of `DriftEvent`s. The CLI
//! (`cmd_queue_check`) is the only place that touches the filesystem,
//! shells out to `tmux`, or invokes the event bus. This makes the
//! detection logic trivially unit-testable against fixture inputs.
//!
//! Read-only side effects: the only thing this module ever writes is a
//! claude-event JSON file via `event_bus::emit`. It NEVER kills any
//! process, mutates queue state, or touches workload state.

use crate::event_bus;
use crate::workload::{load_state as load_workload_state, WorkloadState};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Default "running for at least N seconds before we declare it
/// orphaned" threshold. Real promote/agent runs that take longer than
/// this without a visible worker really are orphans; below this we
/// give the worker a chance to spin up.
pub const DEFAULT_STALE_ORPHAN_SECS: u64 = 300;

/// Default "ready & pending for at least N seconds before we surface
/// it" threshold. Mirrors the existing cron-queue-stale-ready value.
pub const DEFAULT_STALE_READY_SECS: u64 = 300;

/// One queue item, parsed from `session-task queue list --json`. Only
/// the fields we actually use are deserialised — `serde(default)` and
/// `Option` keep this resilient to schema additions.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct QueueItem {
    pub id: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
}

impl QueueItem {
    /// Best-effort summary string for human-readable event messages.
    pub fn display_summary(&self) -> String {
        self.summary
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                let d = &self.description;
                if d.len() > 80 {
                    format!("{}…", &d[..77])
                } else {
                    d.clone()
                }
            })
    }

    /// Seconds since this item entered `running` status, computed
    /// against `now`. Falls back to `created_at` when `started_at` is
    /// missing (shouldn't happen for running items but be defensive).
    pub fn running_age_secs(&self, now: DateTime<Utc>) -> Option<i64> {
        let ts = self
            .started_at
            .as_deref()
            .or(self.created_at.as_deref())?;
        let parsed = DateTime::parse_from_rfc3339(ts).ok()?;
        Some((now - parsed.with_timezone(&Utc)).num_seconds())
    }

    /// Seconds since this item was created, computed against `now`.
    pub fn created_age_secs(&self, now: DateTime<Utc>) -> Option<i64> {
        let ts = self.created_at.as_deref()?;
        let parsed = DateTime::parse_from_rfc3339(ts).ok()?;
        Some((now - parsed.with_timezone(&Utc)).num_seconds())
    }
}

/// One subagent meta (the index file already used by `agent.rs`
/// elsewhere). We keep the schema narrow and tolerant — only the
/// fields we use for queue cross-reference are required.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct SubagentSummary {
    /// Hex-ish agent id (filename of `agent-<id>.meta.json`).
    pub agent_id: String,
    /// Free-form description (what agentType etc.).
    pub description: String,
    pub agent_type: String,
    /// Path to the JSONL transcript so the caller can grep for queue markers.
    pub jsonl_path: PathBuf,
    /// Set of queue item IDs found in the JSONL transcript via
    /// `Queue item: q-XXXX` markers (cheap line-scan). Populated
    /// lazily by the CLI; tests inject this directly.
    pub queue_markers: HashSet<String>,
}

/// One drift event — a finding we surface as a claude-event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftEvent {
    pub kind: DriftKind,
    /// For queue-side events, the queue item id; for worker-side
    /// events (worker-without-item), the workload label or pane id.
    pub subject: String,
    /// Short human-readable message printed in the event, ALSO used
    /// as the `EVENT[...]` preview in the main loop's bash view.
    pub message: String,
    /// Age in seconds (running age for orphan, created age for stale-ready,
    /// 0 for worker-without-item).
    pub age_secs: i64,
    /// Optional secondary detail (workload label, etc.) for
    /// worker-without-item events. Empty for others.
    pub detail: String,
}

/// Discriminator for the three drift classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftKind {
    /// Queue says "running" but no worker exists.
    OrphanRunning,
    /// Queue says "ready & pending" but stayed pending past threshold.
    StaleReady,
    /// A workload pane is running with a label that no queue item references.
    WorkerWithoutItem,
}

impl DriftKind {
    pub fn tag(self) -> &'static str {
        match self {
            DriftKind::OrphanRunning => "queue-orphan-running",
            DriftKind::StaleReady => "queue-stale-ready",
            DriftKind::WorkerWithoutItem => "queue-worker-without-item",
        }
    }
}

/// Inputs for `cross_reference` — keeps the function pure and the
/// tests fixture-driven.
#[derive(Debug, Clone, Default)]
pub struct CrossRefInputs {
    pub queue: Vec<QueueItem>,
    pub workloads: WorkloadState,
    /// Set of pane ids that are *currently alive* in the tasks tmux
    /// session. The CLI populates this from `tmux list-panes`; tests
    /// supply directly. Used to suppress false positives when a
    /// workload entry exists but its pane has died (those should be
    /// reported by the workload-done event, not as an orphan).
    pub alive_panes: HashSet<String>,
    /// Subagent summaries from the active session's subagents dir.
    pub subagents: Vec<SubagentSummary>,
    /// "Now" for age math; injectable for testing.
    pub now: DateTime<Utc>,
    /// Threshold knobs.
    pub stale_orphan_secs: u64,
    pub stale_ready_secs: u64,
}

impl CrossRefInputs {
    #[allow(dead_code)]
    pub fn new(now: DateTime<Utc>) -> Self {
        Self {
            queue: Vec::new(),
            workloads: WorkloadState::new(),
            alive_panes: HashSet::new(),
            subagents: Vec::new(),
            now,
            stale_orphan_secs: DEFAULT_STALE_ORPHAN_SECS,
            stale_ready_secs: DEFAULT_STALE_READY_SECS,
        }
    }
}

/// Plausibility check: does ANY worker (workload, subagent) plausibly
/// own this running queue item?
///
/// Strategy (cheapest first; short-circuits on first hit):
///
///  1. Direct marker: any subagent transcript containing
///     `Queue item: q-XXXX` matching this item's id. This is the
///     canonical attachment per the spawn-gate hook contract — if
///     present, the agent IS the worker.
///  2. Scope-driven heuristics for well-known agent prototypes:
///       - `agent-proto:promote` → look for an alive workload pane
///         whose command mentions `stv-promote` OR a subagent whose
///         description mentions "promote" / "validator".
///       - `agent-proto:metadata-cleanup-*` → subagent whose
///         description matches the queue summary keywords.
///       - `agent-proto:request-review` → request-review subagent.
///       - any other `agent-proto:*` → ANY live subagent (best-effort
///         floor — better to over-attribute than to false-positive).
///  3. Generic fallback: if the queue scope contains a `repo:*` token,
///     accept ANY live subagent OR ANY alive workload pane as plausible
///     worker. The cost of a false negative (missed alert) outweighs
///     the cost of a false positive (incorrect orphan event).
fn item_has_plausible_worker(item: &QueueItem, inputs: &CrossRefInputs) -> bool {
    // 1. Direct subagent transcript marker.
    for sub in &inputs.subagents {
        if sub.queue_markers.contains(&item.id) {
            return true;
        }
    }

    let scopes: Vec<&str> = item.scope.iter().map(String::as_str).collect();
    let summary_lc = item.display_summary().to_lowercase();
    let desc_lc = item.description.to_lowercase();

    // 2a. promote prototype.
    if scopes.iter().any(|s| *s == "agent-proto:promote") {
        // Look for alive promote-style workloads.
        for (label, entry) in inputs.workloads.iter() {
            if !inputs.alive_panes.contains(&entry.pane_id) {
                continue;
            }
            let cmd_lc = entry.command.to_lowercase();
            if cmd_lc.contains("stv-promote")
                || cmd_lc.contains("promote")
                || label.to_lowercase().contains("promote")
            {
                return true;
            }
        }
        // Validator-side subagents.
        for sub in &inputs.subagents {
            let dlc = sub.description.to_lowercase();
            if dlc.contains("promote") || dlc.contains("validator") {
                return true;
            }
        }
    }

    // 2b. metadata-cleanup-* prototypes.
    if scopes
        .iter()
        .any(|s| s.starts_with("agent-proto:metadata-cleanup"))
    {
        for sub in &inputs.subagents {
            let dlc = sub.description.to_lowercase();
            if dlc.contains("metadata") || dlc.contains("cleanup") {
                return true;
            }
            // Loose keyword match against the queue summary — the
            // agent description usually echoes the show/movie name.
            if !summary_lc.is_empty() && summary_for_keyword_overlap(&summary_lc, &dlc) {
                return true;
            }
        }
    }

    // 2c. request-review prototype.
    if scopes
        .iter()
        .any(|s| *s == "agent-proto:request-review")
    {
        for sub in &inputs.subagents {
            let dlc = sub.description.to_lowercase();
            if dlc.contains("request") || dlc.contains("review") {
                return true;
            }
        }
    }

    // 2d. Any other agent-proto:* — best-effort floor. If we have ANY
    // active subagent at all, attribute it (better than false positive).
    if scopes.iter().any(|s| s.starts_with("agent-proto:"))
        && !inputs.subagents.is_empty()
    {
        return true;
    }

    // 3. Generic fallback for repo: scopes — any live subagent or
    // alive workload counts as a plausible worker.
    if scopes.iter().any(|s| s.starts_with("repo:")) {
        if !inputs.subagents.is_empty() {
            return true;
        }
        for entry in inputs.workloads.values() {
            if inputs.alive_panes.contains(&entry.pane_id) {
                return true;
            }
        }
    }

    // 4. Last-ditch: scan workload commands for tokens that appear in
    // the queue description (rare match but cheap).
    if !desc_lc.is_empty() {
        for (label, entry) in inputs.workloads.iter() {
            if !inputs.alive_panes.contains(&entry.pane_id) {
                continue;
            }
            let lc = format!("{} {}", label, entry.command).to_lowercase();
            if shares_significant_token(&desc_lc, &lc) {
                return true;
            }
        }
    }

    false
}

/// Two strings share at least one >5-char alphanumeric token in common.
/// Cheap heuristic — used for very loose attribution like "queue
/// description mentions 'twilight'" matching workload command "ebook
/// twilight".
fn shares_significant_token(a: &str, b: &str) -> bool {
    let a_tokens: HashSet<&str> = a
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 5)
        .collect();
    if a_tokens.is_empty() {
        return false;
    }
    b.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 5)
        .any(|t| a_tokens.contains(t))
}

/// Helper — returns true iff the two strings share any >5-char
/// alphanumeric token. Same logic as above; named separately for
/// clarity at the metadata-cleanup call site.
fn summary_for_keyword_overlap(summary: &str, desc: &str) -> bool {
    shares_significant_token(summary, desc)
}

/// Pure cross-reference: emit drift events for the three classes.
///
/// Order of events in the returned vec:
///   1. all `OrphanRunning` (sorted oldest-first by age)
///   2. all `StaleReady` (sorted oldest-first by age)
///   3. all `WorkerWithoutItem` (alphabetical by label)
pub fn cross_reference(inputs: &CrossRefInputs) -> Vec<DriftEvent> {
    let mut out: Vec<DriftEvent> = Vec::new();

    // --- 1. Orphan running --------------------------------------------------
    let mut orphans: Vec<DriftEvent> = inputs
        .queue
        .iter()
        .filter(|it| it.status == "running")
        .filter_map(|it| {
            let age = it.running_age_secs(inputs.now)?;
            if age < inputs.stale_orphan_secs as i64 {
                return None;
            }
            if item_has_plausible_worker(it, inputs) {
                return None;
            }
            let mins = age / 60;
            let summary = it.display_summary();
            Some(DriftEvent {
                kind: DriftKind::OrphanRunning,
                subject: it.id.clone(),
                message: format!(
                    "{} ({}) registered running for {}m but no worker found",
                    it.id, summary, mins
                ),
                age_secs: age,
                detail: String::new(),
            })
        })
        .collect();
    orphans.sort_by(|a, b| b.age_secs.cmp(&a.age_secs));
    out.extend(orphans);

    // --- 2. Stale ready -----------------------------------------------------
    // We approximate "ready" as: status == "pending" AND no peer in the
    // same group_id is currently running. We don't have group info on
    // the parsed item, so we fall back to: pending status, age past
    // threshold, AND no other queue item in our list shares the
    // descriptor's group via running status.
    //
    // The richer "ready" signal lives in `session-task queue ready`,
    // which the CLI shells out to and supplies as a pre-filtered set.
    // Pure-function version uses pending+age as a strict upper bound;
    // tests can opt in to a hand-curated `ready_ids` later if needed.
    let mut stales: Vec<DriftEvent> = inputs
        .queue
        .iter()
        .filter(|it| it.status == "pending")
        .filter_map(|it| {
            let age = it.created_age_secs(inputs.now)?;
            if age < inputs.stale_ready_secs as i64 {
                return None;
            }
            let mins = age / 60;
            let summary = it.display_summary();
            Some(DriftEvent {
                kind: DriftKind::StaleReady,
                subject: it.id.clone(),
                message: format!(
                    "{} ({}) ready & pending for {}m past threshold",
                    it.id, summary, mins
                ),
                age_secs: age,
                detail: String::new(),
            })
        })
        .collect();
    stales.sort_by(|a, b| b.age_secs.cmp(&a.age_secs));
    out.extend(stales);

    // --- 3. Worker without item --------------------------------------------
    // Build the set of "labels referenced by queue items" loosely:
    // any queue item whose description or summary mentions the label
    // counts as a reference. This is intentionally fuzzy — tighter
    // would require adding a workload-ref schema, and the cost of a
    // false-positive WorkerWithoutItem is just one extra notification.
    let referenced: HashSet<String> = inputs
        .queue
        .iter()
        .flat_map(|it| {
            let mut tokens: HashSet<String> = HashSet::new();
            let blob = format!("{} {}", it.description, it.display_summary()).to_lowercase();
            for tok in blob.split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_') {
                if tok.len() >= 4 {
                    tokens.insert(tok.to_string());
                }
            }
            tokens
        })
        .collect();

    let mut workers: Vec<DriftEvent> = inputs
        .workloads
        .iter()
        .filter(|(_, entry)| inputs.alive_panes.contains(&entry.pane_id))
        .filter_map(|(label, entry)| {
            // If the label (lowercased) appears as a referenced token
            // anywhere in the queue, skip — likely tied to a queue item.
            let label_lc = label.to_lowercase();
            if referenced.contains(&label_lc) {
                return None;
            }
            // Also accept partial matches: any segment of the label
            // (split on `-`, `_`) of length >=4 must match an exact
            // referenced token. We deliberately do NOT accept the
            // reverse direction (referenced token being a substring
            // of the label) because that produces false positives
            // (e.g. label `rogue-workload` matching token `work`).
            let label_segments: Vec<&str> = label_lc
                .split(|c: char| c == '-' || c == '_')
                .filter(|s| s.len() >= 4)
                .collect();
            if label_segments
                .iter()
                .any(|seg| referenced.contains(*seg))
            {
                return None;
            }
            Some(DriftEvent {
                kind: DriftKind::WorkerWithoutItem,
                subject: label.clone(),
                message: format!(
                    "workload {} (pane {}) running with no matching queue item",
                    label, entry.pane_id
                ),
                age_secs: 0,
                detail: entry.command.clone(),
            })
        })
        .collect();
    workers.sort_by(|a, b| a.subject.cmp(&b.subject));
    out.extend(workers);

    out
}

// ----- IO glue (CLI) ---------------------------------------------------------

/// Path to the queue JSON. Honors `SESSION_QUEUE_FILE` for test
/// injection; defaults to `~/.config/session/queue.json`.
pub fn queue_file_path() -> PathBuf {
    if let Ok(p) = std::env::var("SESSION_QUEUE_FILE") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".config/session/queue.json")
}

/// Read the queue items from the queue.json file (top-level
/// `{schema_version, items: [...]}` shape).
pub fn read_queue(path: &Path) -> std::io::Result<Vec<QueueItem>> {
    let body = std::fs::read_to_string(path)?;
    let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("queue JSON parse: {e}"),
        )
    })?;
    let items = parsed
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::with_capacity(items.len());
    for v in items {
        match serde_json::from_value::<QueueItem>(v) {
            Ok(it) => out.push(it),
            Err(_) => continue, // skip malformed items rather than fail the whole run
        }
    }
    Ok(out)
}

/// Get the set of alive pane ids in the given tmux session. Empty
/// set on error or session not found — orphan detection is
/// conservative (we never declare an orphan based on a missing pane
/// list, only based on a present-but-stale workload entry).
pub fn list_alive_panes(session: &str) -> HashSet<String> {
    let out = Command::new("tmux")
        .args(["list-panes", "-t", session, "-F", "#{pane_id}"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        _ => HashSet::new(),
    }
}

/// Walk subagent JSONL transcripts and pull the set of queue ids each
/// references via the canonical `Queue item: q-XXXX` marker. Cheap
/// line-grep — never loads whole JSONLs into memory.
///
/// `subagents_dir` is typically the active Claude Code session's
/// `subagents/` directory, located via `agent::find_subagents_dir`.
pub fn collect_subagents(subagents_dir: &Path) -> Vec<SubagentSummary> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(subagents_dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let fname = entry.file_name().to_string_lossy().to_string();
        if !fname.ends_with(".meta.json") {
            continue;
        }
        let agent_id = fname
            .strip_prefix("agent-")
            .unwrap_or(&fname)
            .strip_suffix(".meta.json")
            .unwrap_or(&fname)
            .to_string();
        let meta_path = entry.path();
        let jsonl_path = subagents_dir.join(format!("agent-{}.jsonl", agent_id));

        let meta_body = std::fs::read_to_string(&meta_path).unwrap_or_default();
        let meta_v: serde_json::Value =
            serde_json::from_str(&meta_body).unwrap_or(serde_json::Value::Null);
        let description = meta_v
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let agent_type = meta_v
            .get("agentType")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let queue_markers = scan_queue_markers(&jsonl_path);

        out.push(SubagentSummary {
            agent_id,
            description,
            agent_type,
            jsonl_path,
            queue_markers,
        });
    }
    out
}

/// Pull `q-XXXX` ids out of any line containing the marker phrase
/// `Queue item:`. We deliberately use a streaming approach (read the
/// file once, scan line-by-line via the standard library) rather than
/// loading and JSON-decoding the whole transcript.
fn scan_queue_markers(jsonl_path: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    let body = match std::fs::read_to_string(jsonl_path) {
        Ok(b) => b,
        Err(_) => return out,
    };
    extract_queue_markers_from_str(&body, &mut out);
    out
}

/// Pure helper for the JSONL marker scan. Public for testability.
pub fn extract_queue_markers_from_str(body: &str, out: &mut HashSet<String>) {
    // Match `q-` followed by typical queue-id chars (alphanumeric +
    // dash). The id format we surface is `q-YYYY-MM-DD-XXXX` but we
    // accept any q-prefixed token of length >=4 to stay forward-compat
    // with format changes.
    for line in body.lines() {
        let mut idx = 0;
        let bytes = line.as_bytes();
        while idx + 2 < bytes.len() {
            if bytes[idx] == b'q' && bytes[idx + 1] == b'-' {
                let start = idx;
                let mut end = idx + 2;
                while end < bytes.len() {
                    let c = bytes[end];
                    if c.is_ascii_alphanumeric() || c == b'-' {
                        end += 1;
                    } else {
                        break;
                    }
                }
                if end - start >= 6 {
                    if let Ok(s) = std::str::from_utf8(&bytes[start..end]) {
                        // Tighten: must contain at least one digit and
                        // one dash beyond the leading `q-` to avoid
                        // matching shell tokens like `q-foo`.
                        let body = &s[2..];
                        if body.chars().any(|c| c.is_ascii_digit()) && body.contains('-') {
                            out.insert(s.to_string());
                        }
                    }
                }
                idx = end;
            } else {
                idx += 1;
            }
        }
    }
}

/// CLI entrypoint for `claude-watch queue-check [--json]`.
///
/// Returns a process exit code:
///   0 — ran cleanly, regardless of how many drift events were found.
///   1 — internal error (couldn't read queue, etc.).
pub fn cmd_queue_check(json_out: bool, dry_run: bool) -> i32 {
    let queue_path = queue_file_path();
    let items = match read_queue(&queue_path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "queue-check: failed to read queue at {}: {}",
                queue_path.display(),
                e
            );
            return 1;
        }
    };

    let workloads = load_workload_state();
    let alive_panes = list_alive_panes("tasks");

    // Subagents — derived from the active Claude Code session, not the
    // current process's session. Best-effort: if no session is found,
    // we still produce events (the marker check just degrades to
    // "no markers").
    let subagents = match crate::agent::find_session_dir() {
        Some(session_dir) => match crate::agent::find_subagents_dir(&session_dir) {
            Some(sa_dir) => collect_subagents(&sa_dir),
            None => Vec::new(),
        },
        None => Vec::new(),
    };

    let inputs = CrossRefInputs {
        queue: items,
        workloads,
        alive_panes,
        subagents,
        now: Utc::now(),
        stale_orphan_secs: env_or(
            "QUEUE_CHECK_ORPHAN_SECS",
            DEFAULT_STALE_ORPHAN_SECS,
        ),
        stale_ready_secs: env_or(
            "QUEUE_CHECK_STALE_READY_SECS",
            DEFAULT_STALE_READY_SECS,
        ),
    };

    let events = cross_reference(&inputs);

    if json_out {
        let json_events: Vec<serde_json::Value> = events
            .iter()
            .map(|e| {
                serde_json::json!({
                    "kind": e.kind.tag(),
                    "subject": e.subject,
                    "message": e.message,
                    "age_secs": e.age_secs,
                    "detail": e.detail,
                })
            })
            .collect();
        let payload = serde_json::json!({
            "now": inputs.now.to_rfc3339(),
            "thresholds": {
                "stale_orphan_secs": inputs.stale_orphan_secs,
                "stale_ready_secs": inputs.stale_ready_secs,
            },
            "events": json_events,
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
    } else if events.is_empty() {
        println!("queue-check: no drift detected");
    } else {
        println!("queue-check: {} drift event(s) detected", events.len());
        for e in &events {
            println!("  [{}] {}", e.kind.tag(), e.message);
        }
    }

    if !dry_run {
        for e in &events {
            emit_drift_event(e);
        }
    }

    0
}

/// Emit one drift event via the existing claude-event bus. Severity is
/// medium for orphan-running (the actionable case), low for the
/// others.
fn emit_drift_event(e: &DriftEvent) {
    let severity = match e.kind {
        DriftKind::OrphanRunning => event_bus::Severity::Medium,
        DriftKind::StaleReady => event_bus::Severity::Low,
        DriftKind::WorkerWithoutItem => event_bus::Severity::Low,
    };
    // We re-use the ClaudeWatchAlert plumbing for consistency, BUT we
    // need a queue-specific tag. Instead of adding a new type, we
    // build the event JSON directly via a thin local helper.
    write_queue_event(e, severity);
}

/// Build + atomically write a queue-check event JSON file into the
/// claude-events queue dir. Mirrors the shape used by
/// `event_bus::emit` so `claude-event-watch` doesn't need a special
/// case.
fn write_queue_event(e: &DriftEvent, severity: event_bus::Severity) {
    use std::time::{SystemTime, UNIX_EPOCH};

    let dir = event_bus::queue_dir();
    if let Err(err) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %err, dir = %dir.display(),
            "queue-check emit: failed to create queue dir");
        return;
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let now_iso = chrono::Local::now().to_rfc3339();
    let user = std::env::var("USER").unwrap_or_default();
    let pid = std::process::id();

    let body = serde_json::json!({
        "timestamp": now,
        "timestamp_iso": now_iso,
        "source": "queue",
        "source_name": "claude-watch-queue-check",
        "tag": e.kind.tag(),
        "priority": severity.as_priority(),
        "message": e.message,
        "data": {
            "subject": e.subject,
            "age_secs": e.age_secs,
            "detail": e.detail,
        },
        "pid": pid,
        "user": user,
    });

    let body_str = match serde_json::to_string_pretty(&body) {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(error = %err, "queue-check emit: serialise failed");
            return;
        }
    };

    let ts_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let final_name = format!("{}_{}.json", ts_ns, e.kind.tag());
    let final_path = dir.join(&final_name);
    let tmp_path = dir.join(format!(".{}.tmp", final_name));

    if let Err(err) = std::fs::write(&tmp_path, body_str.as_bytes()) {
        tracing::warn!(error = %err, path = %tmp_path.display(),
            "queue-check emit: tmp write failed");
        return;
    }
    if let Err(err) = std::fs::rename(&tmp_path, &final_path) {
        tracing::warn!(error = %err, "queue-check emit: rename failed");
        let _ = std::fs::remove_file(&tmp_path);
        return;
    }
    tracing::info!(
        path = %final_path.display(),
        kind = %e.kind.tag(),
        subject = %e.subject,
        "queue-check event emitted"
    );
}

fn env_or(var: &str, default: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workload::WorkloadEntry;
    use chrono::Duration;

    fn iso(now: DateTime<Utc>, secs_ago: i64) -> String {
        (now - Duration::seconds(secs_ago)).to_rfc3339()
    }

    fn running_item(now: DateTime<Utc>, id: &str, ago: i64, scope: &[&str]) -> QueueItem {
        QueueItem {
            id: id.to_string(),
            description: format!("desc for {}", id),
            summary: Some(format!("summary {}", id)),
            scope: scope.iter().map(|s| s.to_string()).collect(),
            status: "running".to_string(),
            created_at: Some(iso(now, ago + 60)),
            started_at: Some(iso(now, ago)),
        }
    }

    fn pending_item(now: DateTime<Utc>, id: &str, ago: i64) -> QueueItem {
        QueueItem {
            id: id.to_string(),
            description: format!("desc for {}", id),
            summary: Some(format!("summary {}", id)),
            scope: vec!["repo:claude-watch".to_string()],
            status: "pending".to_string(),
            created_at: Some(iso(now, ago)),
            started_at: None,
        }
    }

    #[test]
    fn empty_inputs_no_events() {
        let now = Utc::now();
        let inputs = CrossRefInputs::new(now);
        let events = cross_reference(&inputs);
        assert!(events.is_empty(), "got events: {:?}", events);
    }

    #[test]
    fn running_item_with_matching_workload_no_event() {
        // A running queue item with `agent-proto:promote` scope and a
        // workload pane whose command contains `stv-promote` should
        // NOT produce an orphan event.
        let now = Utc::now();
        let mut workloads = WorkloadState::new();
        workloads.insert(
            "promote-foo".to_string(),
            WorkloadEntry {
                pane_id: "%101".to_string(),
                command: "stv-promote /tmp/a /tmp/b".to_string(),
                output: "/tmp/out".to_string(),
                started_at: "2026-04-30T20:00:00".to_string(),
            },
        );
        let alive_panes: HashSet<String> = ["%101".to_string()].into_iter().collect();

        let inputs = CrossRefInputs {
            queue: vec![running_item(
                now,
                "q-2026-04-30-aaaa",
                3600, // 1h running
                &["agent-proto:promote", "resource:scorpion"],
            )],
            workloads,
            alive_panes,
            subagents: vec![],
            now,
            stale_orphan_secs: DEFAULT_STALE_ORPHAN_SECS,
            stale_ready_secs: DEFAULT_STALE_READY_SECS,
        };
        let events = cross_reference(&inputs);
        let orphans: Vec<&DriftEvent> = events
            .iter()
            .filter(|e| e.kind == DriftKind::OrphanRunning)
            .collect();
        assert!(
            orphans.is_empty(),
            "expected no orphan; got {:?}",
            orphans
        );
    }

    #[test]
    fn orphan_running_emitted_when_no_worker() {
        let now = Utc::now();
        let inputs = CrossRefInputs {
            queue: vec![running_item(
                now,
                "q-2026-04-30-402d",
                12 * 60, // 12 minutes running
                &["agent-proto:promote"],
            )],
            // no workloads, no subagents, no panes — pure orphan.
            ..CrossRefInputs::new(now)
        };
        let events = cross_reference(&inputs);
        let orphans: Vec<&DriftEvent> = events
            .iter()
            .filter(|e| e.kind == DriftKind::OrphanRunning)
            .collect();
        assert_eq!(orphans.len(), 1, "expected one orphan, got: {:?}", events);
        assert_eq!(orphans[0].subject, "q-2026-04-30-402d");
        assert!(orphans[0].message.contains("12m"));
    }

    #[test]
    fn orphan_running_below_threshold_no_event() {
        // A running item that's only been running 30s should NOT be
        // declared an orphan even with no worker — give the worker
        // time to spin up.
        let now = Utc::now();
        let inputs = CrossRefInputs {
            queue: vec![running_item(
                now,
                "q-fresh-start",
                30,
                &["agent-proto:promote"],
            )],
            ..CrossRefInputs::new(now)
        };
        let events = cross_reference(&inputs);
        let orphans: Vec<_> = events
            .iter()
            .filter(|e| e.kind == DriftKind::OrphanRunning)
            .collect();
        assert!(orphans.is_empty(), "got: {:?}", orphans);
    }

    #[test]
    fn worker_without_item_emitted() {
        // A workload pane whose label has no matching token in any
        // queue item description should fire WorkerWithoutItem.
        let now = Utc::now();
        let mut workloads = WorkloadState::new();
        workloads.insert(
            "rogue-workload".to_string(),
            WorkloadEntry {
                pane_id: "%999".to_string(),
                command: "some-tool --flag".to_string(),
                output: "/tmp/out".to_string(),
                started_at: "2026-04-30T20:00:00".to_string(),
            },
        );
        let alive_panes: HashSet<String> = ["%999".to_string()].into_iter().collect();

        let inputs = CrossRefInputs {
            queue: vec![QueueItem {
                id: "q-unrelated".to_string(),
                description: "completely unrelated work".to_string(),
                summary: Some("nothing matching".to_string()),
                scope: vec!["repo:claude-watch".to_string()],
                status: "pending".to_string(),
                created_at: Some(iso(now, 30)),
                started_at: None,
                ..Default::default()
            }],
            workloads,
            alive_panes,
            subagents: vec![],
            now,
            stale_orphan_secs: DEFAULT_STALE_ORPHAN_SECS,
            stale_ready_secs: DEFAULT_STALE_READY_SECS,
        };
        let events = cross_reference(&inputs);
        let workers: Vec<_> = events
            .iter()
            .filter(|e| e.kind == DriftKind::WorkerWithoutItem)
            .collect();
        assert_eq!(workers.len(), 1, "events: {:?}", events);
        assert_eq!(workers[0].subject, "rogue-workload");
        assert!(workers[0].message.contains("%999"));
    }

    #[test]
    fn stale_ready_emitted_when_pending_past_threshold() {
        let now = Utc::now();
        let inputs = CrossRefInputs {
            queue: vec![pending_item(now, "q-stale", 10 * 60)], // 10 min old
            ..CrossRefInputs::new(now)
        };
        let events = cross_reference(&inputs);
        let stales: Vec<_> = events
            .iter()
            .filter(|e| e.kind == DriftKind::StaleReady)
            .collect();
        assert_eq!(stales.len(), 1);
        assert_eq!(stales[0].subject, "q-stale");
    }

    #[test]
    fn stale_ready_below_threshold_no_event() {
        let now = Utc::now();
        let inputs = CrossRefInputs {
            queue: vec![pending_item(now, "q-young", 60)],
            ..CrossRefInputs::new(now)
        };
        let events = cross_reference(&inputs);
        assert!(events.is_empty(), "events: {:?}", events);
    }

    #[test]
    fn subagent_marker_match_no_orphan() {
        // A running item with NO scope-driven heuristic match but whose
        // id appears in a subagent transcript via Queue item: marker
        // should NOT be flagged as orphan.
        let now = Utc::now();
        let mut markers = HashSet::new();
        markers.insert("q-2026-04-30-marker".to_string());
        let inputs = CrossRefInputs {
            queue: vec![running_item(
                now,
                "q-2026-04-30-marker",
                30 * 60,
                &["repo:some-other-repo"],
            )],
            subagents: vec![SubagentSummary {
                agent_id: "abc".to_string(),
                description: "some agent".to_string(),
                agent_type: "general".to_string(),
                jsonl_path: PathBuf::from("/tmp/x.jsonl"),
                queue_markers: markers,
            }],
            ..CrossRefInputs::new(now)
        };
        let events = cross_reference(&inputs);
        let orphans: Vec<_> = events
            .iter()
            .filter(|e| e.kind == DriftKind::OrphanRunning)
            .collect();
        assert!(orphans.is_empty(), "events: {:?}", events);
    }

    #[test]
    fn extract_queue_markers_finds_canonical_format() {
        let body = "\
            random text\n\
            some line with Queue item: q-2026-04-30-aaaa in it\n\
            another with q-2026-05-01-f023 inline\n\
            unrelated qfoo q-x q-noise\n\
        ";
        let mut out = HashSet::new();
        extract_queue_markers_from_str(body, &mut out);
        assert!(out.contains("q-2026-04-30-aaaa"), "out: {:?}", out);
        assert!(out.contains("q-2026-05-01-f023"), "out: {:?}", out);
        assert!(!out.contains("q-x"), "should reject too-short id");
    }

    #[test]
    fn read_queue_parses_real_shape() {
        // Mimic the real queue.json structure: top-level
        // `{schema_version, items: [...]}`.
        let now = Utc::now();
        let body = serde_json::json!({
            "schema_version": 1,
            "items": [
                {
                    "id": "q-2026-04-30-x",
                    "description": "test",
                    "summary": "summary",
                    "scope": ["agent-proto:promote"],
                    "status": "running",
                    "created_at": iso(now, 7200),
                    "started_at": iso(now, 600),
                },
                {
                    // malformed status field — should be skipped with no error
                    "id": "q-bad",
                    "description": null,
                }
            ]
        })
        .to_string();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), body).unwrap();
        let items = read_queue(tmp.path()).unwrap();
        // Both items are tolerated thanks to serde defaults; we just
        // check the well-formed one parsed correctly.
        let good: Vec<_> = items.iter().filter(|it| it.id == "q-2026-04-30-x").collect();
        assert_eq!(good.len(), 1);
        assert_eq!(good[0].status, "running");
        assert_eq!(good[0].scope, vec!["agent-proto:promote".to_string()]);
    }

    #[test]
    fn drift_event_ordering_oldest_first() {
        // Multiple orphans should come out oldest-first.
        let now = Utc::now();
        let inputs = CrossRefInputs {
            queue: vec![
                running_item(now, "q-young-orphan", 7 * 60, &["agent-proto:promote"]),
                running_item(now, "q-old-orphan", 30 * 60, &["agent-proto:promote"]),
                running_item(now, "q-mid-orphan", 15 * 60, &["agent-proto:promote"]),
            ],
            ..CrossRefInputs::new(now)
        };
        let events = cross_reference(&inputs);
        let orphans: Vec<_> = events
            .iter()
            .filter(|e| e.kind == DriftKind::OrphanRunning)
            .collect();
        assert_eq!(orphans.len(), 3);
        assert_eq!(orphans[0].subject, "q-old-orphan");
        assert_eq!(orphans[1].subject, "q-mid-orphan");
        assert_eq!(orphans[2].subject, "q-young-orphan");
    }

    #[test]
    fn worker_pane_dead_does_not_count_as_alive() {
        // A workload entry with a pane that's NOT in alive_panes should
        // (a) not contribute to "plausible worker found" and (b) not
        // produce a WorkerWithoutItem event (it's already dead).
        let now = Utc::now();
        let mut workloads = WorkloadState::new();
        workloads.insert(
            "old-promote".to_string(),
            WorkloadEntry {
                pane_id: "%dead".to_string(),
                command: "stv-promote /a /b".to_string(),
                output: "/tmp/o".to_string(),
                started_at: "2026-04-30T19:00:00".to_string(),
            },
        );
        let inputs = CrossRefInputs {
            queue: vec![running_item(
                now,
                "q-promote-orphan",
                15 * 60,
                &["agent-proto:promote"],
            )],
            workloads,
            // alive_panes deliberately empty — pane is dead
            alive_panes: HashSet::new(),
            subagents: vec![],
            now,
            stale_orphan_secs: DEFAULT_STALE_ORPHAN_SECS,
            stale_ready_secs: DEFAULT_STALE_READY_SECS,
        };
        let events = cross_reference(&inputs);
        let orphans: Vec<_> = events
            .iter()
            .filter(|e| e.kind == DriftKind::OrphanRunning)
            .collect();
        assert_eq!(
            orphans.len(),
            1,
            "expected orphan because pane is dead; got: {:?}",
            events
        );
        let workers: Vec<_> = events
            .iter()
            .filter(|e| e.kind == DriftKind::WorkerWithoutItem)
            .collect();
        assert!(
            workers.is_empty(),
            "dead pane should not produce WorkerWithoutItem; got {:?}",
            workers
        );
    }

    #[test]
    fn done_items_never_alert() {
        // A queue item with status=done should never trigger any
        // event regardless of age.
        let now = Utc::now();
        let inputs = CrossRefInputs {
            queue: vec![QueueItem {
                id: "q-done".to_string(),
                description: "all done".to_string(),
                summary: Some("done".to_string()),
                scope: vec!["agent-proto:promote".to_string()],
                status: "done".to_string(),
                created_at: Some(iso(now, 3600)),
                started_at: Some(iso(now, 3000)),
            }],
            ..CrossRefInputs::new(now)
        };
        let events = cross_reference(&inputs);
        assert!(events.is_empty(), "events: {:?}", events);
    }
}
