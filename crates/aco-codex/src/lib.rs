//! The `codex` codec: `~/.codex/sessions/**/rollout-*.jsonl` files.
//!
//! Native record shape: `{timestamp, type, payload}` per line.
//! `response_item` payloads carry the conversation (messages, reasoning,
//! tool calls/outputs); `session_meta`/`turn_context`/`world_state`/
//! `token_usage_record` carry metadata; `event_msg` lines are UI
//! projections of the same records and decode as `meta` so an exported
//! document never double-counts content.

use std::path::{Path, PathBuf};

use aco::{
    Attachment, Body, Capability, CapabilityMatrix, Codec, Conversation, Decoded, Encoded, Error,
    Record, Result, Skipped,
};
use serde_json::{json, Value};

const CAPS: CapabilityMatrix = CapabilityMatrix::new(
    true,
    [
        Capability::Native,      // user
        Capability::Native,      // assistant
        Capability::Adapted,     // reasoning (summary only; encrypted blobs are not reproducible)
        Capability::Adapted,     // tool-call (emitted as custom_tool_call)
        Capability::Adapted,     // tool-result (emitted as custom_tool_call_output)
        Capability::Adapted,     // system (emitted as role=developer message)
        Capability::Unsupported, // meta
    ],
);

const SESSION_KINDS: &[&str] = &["codex-session-store", "codex-archived-session-store"];

pub struct CodexCodec;

pub const CODEC: CodexCodec = CodexCodec;

/// The codec as a trait object for registries.
pub fn codec() -> &'static dyn Codec {
    &CODEC
}

impl Codec for CodexCodec {
    fn id(&self) -> &'static str {
        "codex"
    }

    /// Store locations come from the `a4aa-codex` card — the descriptor
    /// stays the single authority; this codec only projects them into
    /// ACO's `HistoryRoot` vocabulary.
    fn history(&self) -> Vec<aco::store::HistoryRoot> {
        a4aa_codex::card()
            .history
            .iter()
            .map(|r| aco::store::HistoryRoot {
                path: r.path,
                kind: r.kind,
                store: match r.store {
                    a4aa::StoreKind::JsonlFile => aco::store::StoreKind::JsonlFile,
                    a4aa::StoreKind::JsonlDir => aco::store::StoreKind::JsonlDir,
                    a4aa::StoreKind::SqliteDb => aco::store::StoreKind::SqliteDb,
                    a4aa::StoreKind::MarkdownFile => aco::store::StoreKind::MarkdownFile,
                    a4aa::StoreKind::Directory => aco::store::StoreKind::Directory,
                    a4aa::StoreKind::Other => aco::store::StoreKind::Other,
                },
            })
            .collect()
    }

    fn session_kinds(&self) -> &'static [&'static str] {
        SESSION_KINDS
    }

    fn capabilities(&self) -> &'static CapabilityMatrix {
        &CAPS
    }

    /// `rollout-<ts>-<uuid>` → `<uuid>`; other stems pass through.
    fn session_id(&self, path: &Path) -> String {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match stem.strip_prefix("rollout-") {
            Some(rest) if rest.len() >= 36 => rest[rest.len() - 36..].to_string(),
            _ => stem,
        }
    }

    fn decode(&self, path: &Path) -> Result<Decoded> {
        let text = std::fs::read_to_string(path).map_err(Error::io(path))?;
        let mut conv = Conversation::new(self.session_id(path));
        conv.agent = Some("codex".into());
        let mut record_lines = Vec::new();
        let mut skipped = Vec::new();
        let mut line_count = 0usize;

        for (idx, raw) in text.lines().enumerate() {
            line_count = idx + 1;
            if raw.trim().is_empty() {
                continue;
            }
            let line: Value = serde_json::from_str(raw).map_err(|e| Error::Decode {
                path: path.to_path_buf(),
                line: idx + 1,
                message: e.to_string(),
            })?;
            let ts = line
                .get("timestamp")
                .and_then(Value::as_str)
                .map(String::from);
            let ty = line.get("type").and_then(Value::as_str).unwrap_or("");
            let payload = line.get("payload").cloned().unwrap_or(Value::Null);
            let ptype = payload.get("type").and_then(Value::as_str).unwrap_or("");
            let native_id = payload.get("id").and_then(Value::as_str).map(String::from);

            let body = match (ty, ptype) {
                ("session_meta", _) => {
                    if let Some(id) = payload.get("id").and_then(Value::as_str) {
                        conv.session_id = id.to_string();
                    }
                    conv.cwd = payload
                        .get("cwd")
                        .and_then(Value::as_str)
                        .map(String::from)
                        .or(conv.cwd);
                    conv.created_at = payload
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .map(String::from);
                    Body::Meta {
                        subtype: "session_meta".into(),
                        data: payload,
                    }
                }
                ("response_item", "message") => message_body(&payload),
                ("response_item", "reasoning") => Body::Reasoning {
                    text: reasoning_text(&payload),
                },
                ("response_item", "function_call" | "custom_tool_call") => Body::ToolCall {
                    call_id: payload
                        .get("call_id")
                        .and_then(Value::as_str)
                        .map(String::from),
                    name: payload
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_string(),
                    args: payload
                        .get("arguments")
                        .or_else(|| payload.get("input"))
                        .cloned()
                        .unwrap_or(Value::Null),
                },
                ("response_item", "function_call_output" | "custom_tool_call_output") => {
                    Body::ToolResult {
                        call_id: payload
                            .get("call_id")
                            .and_then(Value::as_str)
                            .map(String::from),
                        name: None,
                        content: output_text(&payload),
                        is_error: false,
                    }
                }
                ("response_item", "local_shell_call" | "web_search_call") => Body::ToolCall {
                    call_id: payload
                        .get("call_id")
                        .and_then(Value::as_str)
                        .map(String::from),
                    name: ptype.trim_end_matches("_call").to_string(),
                    args: payload.get("action").cloned().unwrap_or(payload),
                },
                ("response_item", other) => Body::Meta {
                    subtype: format!("response_item:{other}"),
                    data: payload,
                },
                // event_msg lines project the same records to the UI; keep
                // them as meta (out of conversation content, still exported).
                ("event_msg", sub) => Body::Meta {
                    subtype: format!("event:{sub}"),
                    data: payload,
                },
                ("turn_context", _) | ("world_state", _) | ("token_usage_record", _) => {
                    Body::Meta {
                        subtype: ty.to_string(),
                        data: payload,
                    }
                }
                ("", _) => {
                    skipped.push(Skipped {
                        line: idx,
                        native_type: None,
                        reason: "untyped line".into(),
                    });
                    continue;
                }
                (other, _) => Body::Meta {
                    subtype: format!("codex:{other}"),
                    data: payload,
                },
            };

            let ordinal = conv.records.len() as u64;
            conv.records.push(Record {
                id: native_id,
                parent: None,
                ts: ts.clone(),
                ordinal,
                body,
            });
            record_lines.push(idx);
        }
        conv.updated_at = conv.records.iter().rev().find_map(|r| r.ts.clone());
        Ok(Decoded {
            conversation: conv,
            record_lines,
            line_count,
            skipped,
        })
    }

    fn encode(&self, conv: &Conversation) -> Result<Encoded> {
        let (kept, projection) = self.project(conv);
        let now = aco::util::now_rfc3339();
        let mut lines = Vec::with_capacity(kept.len() + 1);
        lines.push(
            json!({
                "timestamp": conv.created_at.clone().unwrap_or_else(|| now.clone()),
                "type": "session_meta",
                "payload": {
                    "id": conv.session_id,
                    "timestamp": conv.created_at.clone().unwrap_or_else(|| now.clone()),
                    "cwd": conv.cwd,
                    "originator": "aco",
                }
            })
            .to_string(),
        );
        for rec in kept {
            let ts = rec.ts.clone().unwrap_or_else(|| now.clone());
            let payload = match &rec.body {
                Body::User { text, .. } => json!({
                    "type": "message", "id": rec_id("msg", rec), "role": "user",
                    "content": [{"type": "input_text", "text": text}],
                }),
                Body::Assistant { text, .. } => json!({
                    "type": "message", "id": rec_id("msg", rec), "role": "assistant",
                    "content": [{"type": "output_text", "text": text}],
                }),
                Body::System { text, .. } => json!({
                    "type": "message", "id": rec_id("msg", rec), "role": "developer",
                    "content": [{"type": "input_text", "text": text}],
                }),
                Body::Reasoning { text } => json!({
                    "type": "reasoning", "id": rec_id("rs", rec),
                    "summary": [{"type": "summary_text", "text": text}],
                    "content": [{"type": "reasoning_text", "text": text}],
                    "encrypted_content": Value::Null,
                }),
                Body::ToolCall {
                    call_id,
                    name,
                    args,
                } => json!({
                    "type": "custom_tool_call", "id": rec_id("ctc", rec),
                    "call_id": call_id.clone().unwrap_or_else(|| rec_id("call", rec)),
                    "name": name,
                    "input": match args { Value::String(s) => s.clone(), v => v.to_string() },
                }),
                Body::ToolResult {
                    call_id, content, ..
                } => json!({
                    "type": "custom_tool_call_output", "id": rec_id("ctco", rec),
                    "call_id": call_id,
                    "output": [{"type": "input_text", "text": match content {
                        Value::String(s) => s.clone(), v => v.to_string() }}],
                }),
                Body::Meta { .. } => continue, // filtered by projection already
            };
            lines.push(
                json!({"timestamp": ts, "type": "response_item", "payload": payload}).to_string(),
            );
        }
        Ok(Encoded { lines, projection })
    }

    /// `~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-<ts>-<uuid>.jsonl`
    fn store_path(&self, home: &Path, conv: &Conversation) -> Result<PathBuf> {
        let today = aco::util::today();
        let (y, m, d) = (&today[0..4], &today[5..7], &today[8..10]);
        let stamp = aco::util::now_rfc3339()
            .replace(':', "-")
            .trim_end_matches('Z')
            .to_string();
        Ok(home
            .join(".codex/sessions")
            .join(y)
            .join(m)
            .join(d)
            .join(format!("rollout-{stamp}-{}.jsonl", conv.session_id)))
    }

    fn redact_line(&self, line: &Value, marker: &str) -> Option<Value> {
        let mut line = line.clone();
        let payload = line.get_mut("payload")?;
        match payload.get("type").and_then(Value::as_str) {
            Some("message") => {
                for part in payload.get_mut("content").and_then(Value::as_array_mut)? {
                    if part.get("text").is_some() {
                        part["text"] = json!(marker);
                    }
                }
            }
            Some("reasoning") => {
                if let Some(summary) = payload.get_mut("summary").and_then(Value::as_array_mut) {
                    for part in summary {
                        if part.get("text").is_some() {
                            part["text"] = json!(marker);
                        }
                    }
                }
                if let Some(content) = payload.get_mut("content").and_then(Value::as_array_mut) {
                    for part in content {
                        if part.get("text").is_some() {
                            part["text"] = json!(marker);
                        }
                    }
                }
                if payload.get("encrypted_content").is_some() {
                    payload["encrypted_content"] = Value::Null;
                }
            }
            Some("function_call") | Some("custom_tool_call") => {
                if payload.get("arguments").is_some() {
                    payload["arguments"] = json!(marker);
                }
                if payload.get("input").is_some() {
                    payload["input"] = json!(marker);
                }
            }
            Some("function_call_output") | Some("custom_tool_call_output") => {
                match payload.get_mut("output")? {
                    Value::String(_) => payload["output"] = json!(marker),
                    Value::Array(parts) => {
                        for part in parts {
                            if part.get("text").is_some() {
                                part["text"] = json!(marker);
                            }
                        }
                    }
                    _ => return None,
                }
            }
            _ => return None,
        }
        Some(line)
    }
}

fn rec_id(prefix: &str, rec: &Record) -> String {
    rec.id
        .clone()
        .unwrap_or_else(|| format!("{prefix}_{}", uuid::Uuid::new_v4().simple()))
}

fn message_body(payload: &Value) -> Body {
    let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
    let text = payload
        .get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    match role {
        "user" => Body::User {
            text,
            attachments: Vec::<Attachment>::new(),
        },
        "assistant" => Body::Assistant { text, model: None },
        other => Body::System {
            subtype: format!("role:{other}"),
            text,
        },
    }
}

fn reasoning_text(payload: &Value) -> String {
    let mut text = payload
        .get("summary")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    if text.is_empty() {
        text = payload
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
    }
    if text.is_empty() && payload.get("encrypted_content").is_some() {
        text = "<encrypted reasoning>".to_string();
    }
    text
}

fn output_text(payload: &Value) -> Value {
    match payload.get("output") {
        Some(Value::Array(parts)) => {
            let joined = parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            Value::String(joined)
        }
        Some(v) => v.clone(),
        None => Value::Null,
    }
}
