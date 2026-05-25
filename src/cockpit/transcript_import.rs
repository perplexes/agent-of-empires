//! Import Claude Code's on-disk JSONL transcript into the cockpit event store.
//!
//! When a user flips a session from tmux to cockpit, the cockpit UI timeline
//! would otherwise start empty even though the agent has the full transcript
//! ready to resume via `session/load`. This module bridges the gap: it reads
//! `~/.claude/projects/{encoded-cwd}/{session-uuid}.jsonl`, translates each
//! recognised entry into a cockpit `Event`, and records them under seqs
//! 1..N. The caller is responsible for hydrating the supervisor's seq
//! counter to N so the worker's first live publish starts at N+1.
//!
//! Claude-only by design: other ACP-capable agents we ship today (codex,
//! aider, opencode, etc.) don't share Claude's JSONL transcript format.

use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use super::event_store::EventStore;
use super::state::{Event, ToolCall};

const MAX_ARGS_PREVIEW: usize = 16 * 1024;
const MAX_TOOL_RESULT_CONTENT: usize = 16 * 1024;

/// Resolve the on-disk path of a Claude session's JSONL transcript. Returns
/// `None` if `session_uuid` is not a UUID or we can't determine a home dir.
fn claude_jsonl_path(project_path: &str, session_uuid: &str) -> Option<PathBuf> {
    if Uuid::parse_str(session_uuid).is_err() {
        return None;
    }
    let home = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))?;
    let canonical =
        std::fs::canonicalize(project_path).unwrap_or_else(|_| PathBuf::from(project_path));
    let encoded: String = canonical
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    Some(
        home.join("projects")
            .join(encoded)
            .join(format!("{session_uuid}.jsonl")),
    )
}

/// Read Claude's JSONL transcript at the agent's session UUID, translate each
/// recognised entry into a cockpit `Event`, and record them with seqs 1..N.
/// Returns the highest seq written (0 if the file is missing, empty, or
/// holds no translatable entries).
pub fn import_claude_transcript(
    cockpit_session_id: &str,
    project_path: &str,
    claude_session_uuid: &str,
    event_store: &EventStore,
) -> Result<u64> {
    let Some(path) = claude_jsonl_path(project_path, claude_session_uuid) else {
        return Ok(0);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(
                target: "cockpit.transcript_import",
                session = %cockpit_session_id,
                path = %path.display(),
                "no Claude transcript at expected path; skipping import"
            );
            return Ok(0);
        }
        Err(e) => return Err(e.into()),
    };

    let mut events = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        translate_entry(&entry, &mut events);
    }

    let mut seq: u64 = 0;
    for event in &events {
        seq += 1;
        if let Err(e) = event_store.record(cockpit_session_id, seq, event) {
            tracing::warn!(
                target: "cockpit.transcript_import",
                session = %cockpit_session_id,
                seq,
                "record failed: {e}"
            );
        }
    }
    if seq > 0 {
        tracing::info!(
            target: "cockpit.transcript_import",
            session = %cockpit_session_id,
            imported = seq,
            path = %path.display(),
            "imported Claude JSONL transcript into cockpit event store"
        );
    }
    Ok(seq)
}

fn translate_entry(entry: &Value, out: &mut Vec<Event>) {
    let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let ts = parse_ts(entry.get("timestamp"));
    match entry_type {
        "user" => translate_user(entry, ts, out),
        "assistant" => translate_assistant(entry, ts, out),
        _ => {}
    }
}

fn parse_ts(v: Option<&Value>) -> DateTime<Utc> {
    v.and_then(|t| t.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(Utc::now)
}

fn translate_user(entry: &Value, ts: DateTime<Utc>, out: &mut Vec<Event>) {
    let Some(content) = entry.get("message").and_then(|m| m.get("content")) else {
        return;
    };
    if let Some(text) = content.as_str() {
        if !text.trim().is_empty() {
            out.push(Event::UserPromptSent {
                text: text.to_string(),
            });
        }
        return;
    }
    let Some(arr) = content.as_array() else {
        return;
    };
    for c in arr {
        let kind = c.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "text" => {
                let text = c.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if !text.trim().is_empty() {
                    out.push(Event::UserPromptSent {
                        text: text.to_string(),
                    });
                }
            }
            "tool_result" => {
                let tool_use_id = c
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if tool_use_id.is_empty() {
                    continue;
                }
                let is_error = c.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                let content_str = extract_tool_result_content(c.get("content"));
                out.push(Event::ToolCallCompleted {
                    tool_call_id: tool_use_id,
                    is_error,
                    content: content_str,
                    completed_at: ts,
                });
            }
            _ => {}
        }
    }
}

fn translate_assistant(entry: &Value, ts: DateTime<Utc>, out: &mut Vec<Event>) {
    let Some(content) = entry
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        return;
    };
    for c in content {
        let kind = c.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "text" => {
                let text = c.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if !text.is_empty() {
                    out.push(Event::AgentMessageChunk {
                        text: text.to_string(),
                    });
                }
            }
            "tool_use" => {
                let id = c
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = c
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if id.is_empty() || name.is_empty() {
                    continue;
                }
                let args_preview = c
                    .get("input")
                    .map(|i| serde_json::to_string(i).unwrap_or_default())
                    .unwrap_or_default();
                let args_preview = truncate(args_preview, MAX_ARGS_PREVIEW);
                let kind_str = map_tool_kind(&name).to_string();
                out.push(Event::ToolCallStarted {
                    tool_call: ToolCall {
                        id,
                        name,
                        kind: kind_str,
                        args_preview,
                        started_at: ts,
                        parent_tool_call_id: None,
                        memory_recall: None,
                    },
                });
            }
            _ => {}
        }
    }
}

fn extract_tool_result_content(v: Option<&Value>) -> String {
    let Some(v) = v else {
        return String::new();
    };
    if let Some(s) = v.as_str() {
        return truncate(s.to_string(), MAX_TOOL_RESULT_CONTENT);
    }
    if let Some(arr) = v.as_array() {
        let mut out = String::new();
        for item in arr {
            if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        return truncate(out, MAX_TOOL_RESULT_CONTENT);
    }
    String::new()
}

fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        s.truncate(max);
        s.push('…');
    }
    s
}

/// Map a Claude tool name to the ACP `ToolKind` token the cockpit's tool-card
/// dispatcher branches on. Lowercased; unknown tools fall through to `other`
/// (renders as a generic card).
fn map_tool_kind(name: &str) -> &'static str {
    match name.to_lowercase().as_str() {
        "bash" => "execute",
        "read" | "notebookread" => "read",
        "edit" | "write" | "multiedit" | "notebookedit" => "edit",
        "glob" | "grep" | "agent" | "task" | "websearch" | "webfetch" => "search",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_user_text_to_prompt() {
        let entry: Value = serde_json::from_str(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]},"timestamp":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let mut out = Vec::new();
        translate_entry(&entry, &mut out);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], Event::UserPromptSent { text } if text == "hi"));
    }

    #[test]
    fn translates_assistant_text_and_tool_use() {
        let entry: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"role":"assistant","content":[
                {"type":"text","text":"reading file"},
                {"type":"tool_use","id":"t1","name":"Read","input":{"path":"foo.rs"}}
            ]},"timestamp":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let mut out = Vec::new();
        translate_entry(&entry, &mut out);
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], Event::AgentMessageChunk { text } if text == "reading file"));
        let Event::ToolCallStarted { tool_call } = &out[1] else {
            panic!("expected ToolCallStarted");
        };
        assert_eq!(tool_call.id, "t1");
        assert_eq!(tool_call.name, "Read");
        assert_eq!(tool_call.kind, "read");
        assert!(tool_call.args_preview.contains("foo.rs"));
    }

    #[test]
    fn translates_user_tool_result() {
        let entry: Value = serde_json::from_str(
            r#"{"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t1","is_error":false,"content":"contents"}
            ]},"timestamp":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let mut out = Vec::new();
        translate_entry(&entry, &mut out);
        assert_eq!(out.len(), 1);
        let Event::ToolCallCompleted {
            tool_call_id,
            is_error,
            content,
            ..
        } = &out[0]
        else {
            panic!("expected ToolCallCompleted");
        };
        assert_eq!(tool_call_id, "t1");
        assert!(!is_error);
        assert_eq!(content, "contents");
    }

    #[test]
    fn tool_result_content_array_concatenates_text_blocks() {
        let entry: Value = serde_json::from_str(
            r#"{"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t2","content":[
                    {"type":"text","text":"line1"},
                    {"type":"text","text":"line2"}
                ]}
            ]},"timestamp":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let mut out = Vec::new();
        translate_entry(&entry, &mut out);
        let Event::ToolCallCompleted { content, .. } = &out[0] else {
            panic!("expected ToolCallCompleted");
        };
        assert_eq!(content, "line1\nline2");
    }

    #[test]
    fn skips_unknown_entry_types() {
        for raw in [
            r#"{"type":"queue-operation","operation":"enqueue"}"#,
            r#"{"type":"attachment"}"#,
            r#"{"type":"summary"}"#,
        ] {
            let entry: Value = serde_json::from_str(raw).unwrap();
            let mut out = Vec::new();
            translate_entry(&entry, &mut out);
            assert!(out.is_empty(), "expected no events for {raw}");
        }
    }

    #[test]
    fn skips_assistant_thinking_blocks() {
        let entry: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"role":"assistant","content":[
                {"type":"thinking","thinking":"hmm"},
                {"type":"text","text":"done"}
            ]},"timestamp":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let mut out = Vec::new();
        translate_entry(&entry, &mut out);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], Event::AgentMessageChunk { text } if text == "done"));
    }

    /// Manual smoke test: point at a real Claude JSONL on disk and report
    /// what got imported. Run with:
    ///   AOE_REAL_JSONL=<path> cargo test --features serve --lib \
    ///     cockpit::transcript_import::tests::smoke_real_jsonl -- --nocapture --ignored
    #[test]
    #[ignore]
    fn smoke_real_jsonl() {
        let Ok(path) = std::env::var("AOE_REAL_JSONL") else {
            eprintln!("set AOE_REAL_JSONL to a Claude .jsonl path to run this test");
            return;
        };
        let text = std::fs::read_to_string(&path).expect("read jsonl");
        let mut events = Vec::new();
        let mut skipped_lines = 0usize;
        let mut entry_types: std::collections::BTreeMap<String, usize> = Default::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<Value>(line) else {
                skipped_lines += 1;
                continue;
            };
            let t = entry
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            *entry_types.entry(t).or_insert(0) += 1;
            translate_entry(&entry, &mut events);
        }
        let mut event_kinds: std::collections::BTreeMap<&'static str, usize> = Default::default();
        for e in &events {
            let k = match e {
                Event::UserPromptSent { .. } => "UserPromptSent",
                Event::AgentMessageChunk { .. } => "AgentMessageChunk",
                Event::ToolCallStarted { .. } => "ToolCallStarted",
                Event::ToolCallCompleted { .. } => "ToolCallCompleted",
                _ => "Other",
            };
            *event_kinds.entry(k).or_insert(0) += 1;
        }
        eprintln!("entry types in JSONL: {entry_types:#?}");
        eprintln!("skipped (unparseable) lines: {skipped_lines}");
        eprintln!("translated events by kind: {event_kinds:#?}");
        eprintln!("total events: {}", events.len());
        // Now exercise the on-disk recording path against a temp store.
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStore::open(&tmp.path().join("events.db"), 0).unwrap();
        let n = {
            let mut seq: u64 = 0;
            for e in &events {
                seq += 1;
                store.record("smoke-session", seq, e).unwrap();
            }
            seq
        };
        let replay = store.replay_from("smoke-session", 0);
        assert_eq!(replay.len() as u64, n);
        eprintln!("recorded + replayed {n} events successfully");
        // Pair check: every ToolCallCompleted should have a matching ToolCallStarted by id.
        let mut started_ids: std::collections::HashSet<String> = Default::default();
        let mut completed_ids: std::collections::HashSet<String> = Default::default();
        for (_, e) in &replay {
            match e {
                Event::ToolCallStarted { tool_call } => {
                    started_ids.insert(tool_call.id.clone());
                }
                Event::ToolCallCompleted { tool_call_id, .. } => {
                    completed_ids.insert(tool_call_id.clone());
                }
                _ => {}
            }
        }
        let orphan_completed: Vec<_> = completed_ids.difference(&started_ids).collect();
        let unfinished_started: Vec<_> = started_ids.difference(&completed_ids).collect();
        eprintln!(
            "tool-call pairing: started={} completed={} orphan_completed={} unfinished_started={}",
            started_ids.len(),
            completed_ids.len(),
            orphan_completed.len(),
            unfinished_started.len()
        );
    }

    #[test]
    fn empty_text_is_dropped() {
        let entry: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"role":"assistant","content":[
                {"type":"text","text":""}
            ]}}"#,
        )
        .unwrap();
        let mut out = Vec::new();
        translate_entry(&entry, &mut out);
        assert!(out.is_empty());
    }
}
