//! aco codec: the `pi` (Pi coding agent) conversation store.
//!
//! Sessions live as JSONL transcripts under `~/.pi/agent/sessions`
//! (`PI_CODING_AGENT_SESSION_DIR` relocates the root the way pi's own
//! tooling honors). Each transcript opens with a `{"type":"session"}`
//! header (`id`, `timestamp`, `cwd`), optionally names itself via
//! `{"type":"session_info","name"}`, and carries one `message` object
//! per event whose `content` blocks hold text, `thinking` and
//! `toolCall` payloads; `toolResult` messages return tool output.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use aco::codec::{Capability, CapabilityMatrix, Codec, Decoded, Encoded, Skipped};
use aco::model::{Body, Conversation, Record, RecordKind};
use aco::store::HistoryRoot;
use aco::Result;

/// The codec instance aco's registry hands out.
pub struct PiCodec;

const CAPABILITIES: CapabilityMatrix = CapabilityMatrix::new(
    true,
    [
        Capability::Native,      // user
        Capability::Native,      // assistant
        Capability::Native,      // reasoning
        Capability::Native,      // tool-call
        Capability::Native,      // tool-result
        Capability::Unsupported, // system — pi records no harness events
        Capability::Adapted,     // meta — usage rides inside message objects
    ],
);

/// One codec instance for the registry.
pub fn codec() -> &'static dyn Codec {
    &PiCodec
}

impl Codec for PiCodec {
    fn id(&self) -> &'static str {
        "pi"
    }

    fn history(&self) -> Vec<HistoryRoot> {
        a4aa_pi::card().history.to_vec()
    }

    fn session_kinds(&self) -> &'static [&'static str] {
        &["pi-session-store"]
    }

    fn capabilities(&self) -> &'static CapabilityMatrix {
        &CAPABILITIES
    }

    /// `PI_CODING_AGENT_SESSION_DIR` relocates the whole session root.
    fn expand_root(&self, root: &HistoryRoot, home: &Path) -> PathBuf {
        if root.kind == "pi-session-store" {
            if let Some(dir) = std::env::var("PI_CODING_AGENT_SESSION_DIR")
                .ok()
                .filter(|v| !v.trim().is_empty())
            {
                return PathBuf::from(dir);
            }
        }
        aco::util::expand_home(root.path, home)
    }

    /// Native session id lives in the header line; fall back to the
    /// filename's `<name>_<id>` suffix the way pi names its files.
    fn session_id(&self, path: &Path) -> String {
        if let Some(id) = session_header(path).id {
            return id;
        }
        path.file_stem()
            .and_then(|s| s.to_str())
            .and_then(|stem| stem.rsplit_once('_').map(|(_, id)| id.to_string()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "pi-session".to_string())
    }

    fn decode(&self, path: &Path) -> Result<Decoded> {
        let file = File::open(path).map_err(aco::Error::io(path))?;
        let mut session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|stem| stem.rsplit_once('_').map(|(_, id)| id.to_string()))
            .unwrap_or_else(|| "pi-session".to_string());
        let mut conv = Conversation::new(session_id.clone());
        conv.agent = Some("pi".to_string());
        let mut records = Vec::new();
        let mut record_lines = Vec::new();
        let mut skipped = Vec::new();
        let mut line_count = 0;

        for (index, line) in BufReader::new(file).lines().enumerate() {
            line_count = index + 1;
            let Ok(line) = line else { continue };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                skipped.push(Skipped {
                    line: index,
                    native_type: None,
                    reason: "corrupt".to_string(),
                });
                continue;
            };
            match value.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "session" => {
                    if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
                        session_id = id.to_string();
                        conv.session_id = session_id.clone();
                    }
                    conv.created_at = value
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .or(conv.created_at);
                    conv.cwd = value
                        .get("cwd")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .or(conv.cwd);
                }
                "session_info" => {
                    conv.title = value
                        .get("name")
                        .and_then(|v| v.as_str())
                        .filter(|n| !n.trim().is_empty())
                        .map(str::to_string)
                        .or(conv.title);
                }
                "message" => {
                    let Some(message) = value.get("message") else {
                        continue;
                    };
                    let ts = value
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("");
                    let first = records.len();
                    match role {
                        "user" => {
                            if let Some(text) = content_text(message) {
                                push(
                                    &mut records,
                                    &mut record_lines,
                                    index,
                                    ts,
                                    Body::User {
                                        text,
                                        attachments: Vec::new(),
                                    },
                                );
                            }
                        }
                        "assistant" => {
                            if let Some(blocks) = message.get("content").and_then(|c| c.as_array())
                            {
                                for block in blocks {
                                    match block.get("type").and_then(|t| t.as_str()) {
                                        Some("thinking") => {
                                            if let Some(text) =
                                                block.get("thinking").and_then(|v| v.as_str())
                                            {
                                                push(
                                                    &mut records,
                                                    &mut record_lines,
                                                    index,
                                                    ts.clone(),
                                                    Body::Reasoning {
                                                        text: text.to_string(),
                                                    },
                                                );
                                            }
                                        }
                                        Some("toolCall") => {
                                            push(
                                                &mut records,
                                                &mut record_lines,
                                                index,
                                                ts.clone(),
                                                Body::ToolCall {
                                                    call_id: block
                                                        .get("id")
                                                        .and_then(|v| v.as_str())
                                                        .map(str::to_string),
                                                    name: block
                                                        .get("name")
                                                        .and_then(|v| v.as_str())
                                                        .unwrap_or("tool")
                                                        .to_string(),
                                                    args: block
                                                        .get("arguments")
                                                        .or_else(|| block.get("input"))
                                                        .cloned()
                                                        .unwrap_or(serde_json::json!({})),
                                                },
                                            );
                                        }
                                        _ => {
                                            if let Some(text) = block_text(block) {
                                                push(
                                                    &mut records,
                                                    &mut record_lines,
                                                    index,
                                                    ts.clone(),
                                                    Body::Assistant { text, model: None },
                                                );
                                            }
                                        }
                                    }
                                }
                            } else if let Some(text) = content_text(message) {
                                push(
                                    &mut records,
                                    &mut record_lines,
                                    index,
                                    ts.clone(),
                                    Body::Assistant { text, model: None },
                                );
                            }
                            usage_meta(&value, message).into_iter().for_each(|body| {
                                push(&mut records, &mut record_lines, index, ts.clone(), body)
                            });
                        }
                        "toolResult" => {
                            push(
                                &mut records,
                                &mut record_lines,
                                index,
                                ts,
                                Body::ToolResult {
                                    call_id: message
                                        .get("toolCallId")
                                        .or_else(|| message.get("id"))
                                        .and_then(|v| v.as_str())
                                        .map(str::to_string),
                                    name: message
                                        .get("toolName")
                                        .and_then(|v| v.as_str())
                                        .map(str::to_string),
                                    content: message
                                        .get("content")
                                        .cloned()
                                        .unwrap_or(serde_json::Value::Null),
                                    is_error: message
                                        .get("isError")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false),
                                },
                            );
                        }
                        _ => {
                            if first == records.len() {
                                skipped.push(Skipped {
                                    line: index,
                                    native_type: Some(format!("message:{role}")),
                                    reason: "projection".to_string(),
                                });
                            }
                        }
                    }
                }
                other => {
                    skipped.push(Skipped {
                        line: index,
                        native_type: if other.is_empty() {
                            None
                        } else {
                            Some(other.to_string())
                        },
                        reason: "projection".to_string(),
                    });
                }
            }
        }
        conv.updated_at = records
            .last()
            .and_then(|r| r.ts.clone())
            .or(conv.created_at.clone());
        for (ordinal, rec) in records.iter_mut().enumerate() {
            rec.ordinal = ordinal as u64;
        }
        conv.records = records;
        Ok(Decoded {
            conversation: conv,
            record_lines,
            line_count,
            skipped,
        })
    }

    fn encode(&self, conversation: &Conversation) -> Result<Encoded> {
        let (kept, projection) = self.project(conversation);
        let mut lines = Vec::new();
        lines.push(
            serde_json::json!({
                "type": "session",
                "id": conversation.session_id,
                "timestamp": conversation.created_at,
                "cwd": conversation.cwd,
            })
            .to_string(),
        );
        if let Some(title) = &conversation.title {
            lines.push(serde_json::json!({"type": "session_info", "name": title}).to_string());
        }
        let mut pending: Vec<&Record> = Vec::new();
        for rec in kept {
            match rec.kind() {
                RecordKind::Assistant
                | RecordKind::Reasoning
                | RecordKind::ToolCall
                | RecordKind::Meta => pending.push(rec),
                RecordKind::User => {
                    flush_assistant(&mut pending, &mut lines);
                    lines.push(
                        serde_json::json!({
                            "type": "message",
                            "timestamp": rec.ts,
                            "message": {"role": "user", "content": record_text(rec)},
                        })
                        .to_string(),
                    );
                }
                RecordKind::ToolResult => {
                    flush_assistant(&mut pending, &mut lines);
                    if let Body::ToolResult {
                        call_id,
                        name,
                        content,
                        is_error,
                    } = &rec.body
                    {
                        lines.push(
                            serde_json::json!({
                                "type": "message",
                                "timestamp": rec.ts,
                                "message": {
                                    "role": "toolResult",
                                    "toolCallId": call_id,
                                    "toolName": name,
                                    "content": content,
                                    "isError": is_error,
                                },
                            })
                            .to_string(),
                        );
                    }
                }
                RecordKind::System => {}
            }
        }
        flush_assistant(&mut pending, &mut lines);
        Ok(Encoded { lines, projection })
    }

    fn store_path(&self, home: &Path, conversation: &Conversation) -> Result<PathBuf> {
        let root = self
            .history()
            .into_iter()
            .find(|r| r.kind == "pi-session-store")
            .map(|r| self.expand_root(&r, home))
            .ok_or_else(|| aco::Error::Usage("pi session root undeclared".to_string()))?;
        Ok(root.join(format!("{}.jsonl", conversation.session_id)))
    }

    fn redact_line(&self, line: &serde_json::Value, marker: &str) -> Option<serde_json::Value> {
        if line.get("type").and_then(|t| t.as_str()) != Some("message") {
            return None;
        }
        let mut out = line.clone();
        let message = out.get_mut("message")?;
        let mut touched = false;
        if let Some(blocks) = message.get_mut("content").and_then(|c| c.as_array_mut()) {
            for block in blocks {
                for key in ["text", "thinking"] {
                    if block.get(key).and_then(|v| v.as_str()).is_some() {
                        block[key] = serde_json::json!(marker);
                        touched = true;
                    }
                }
            }
        } else if message.get("content").and_then(|c| c.as_str()).is_some() {
            message["content"] = serde_json::json!(marker);
            touched = true;
        }
        touched.then_some(out)
    }
}

/// Emit one assistant `message` line for a run of assistant-side records:
/// text/thinking/toolCall become `content` blocks, a trailing `usage`
/// meta lands on the message object the way pi persists it.
fn flush_assistant(pending: &mut Vec<&Record>, lines: &mut Vec<String>) {
    if pending.is_empty() {
        return;
    }
    let mut usage = None;
    let blocks: Vec<serde_json::Value> = pending
        .iter()
        .filter_map(|rec| match &rec.body {
            Body::Reasoning { text } => {
                Some(serde_json::json!({"type": "thinking", "thinking": text}))
            }
            Body::ToolCall {
                call_id,
                name,
                args,
            } => Some(serde_json::json!({
                "type": "toolCall",
                "id": call_id,
                "name": name,
                "arguments": args,
            })),
            Body::Assistant { text, .. } => Some(serde_json::json!({"type": "text", "text": text})),
            Body::Meta { data, .. } => {
                usage = Some(data.clone());
                None
            }
            _ => None,
        })
        .collect();
    lines.push(
        serde_json::json!({
            "type": "message",
            "timestamp": pending.first().and_then(|r| r.ts.clone()),
            "message": {"role": "assistant", "content": blocks, "usage": usage},
        })
        .to_string(),
    );
    pending.clear();
}

fn push(
    records: &mut Vec<Record>,
    lines: &mut Vec<usize>,
    line: usize,
    ts: Option<String>,
    body: Body,
) {
    records.push(Record {
        id: None,
        parent: None,
        ts,
        ordinal: 0,
        body,
    });
    lines.push(line);
}

/// pi `content` may be a plain string or an array of text blocks.
fn content_text(message: &serde_json::Value) -> Option<String> {
    match message.get("content")? {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        serde_json::Value::Array(blocks) => {
            let text: String = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.trim().is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn block_text(block: &serde_json::Value) -> Option<String> {
    block
        .get("text")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

fn record_text(rec: &Record) -> String {
    rec.body.text()
}

/// Usage counters ride inside assistant `message` objects (and their
/// wrapper) as a `usage` object; project them as a trailing meta record.
fn usage_meta(event: &serde_json::Value, message: &serde_json::Value) -> Option<Body> {
    let usage = message
        .get("usage")
        .or_else(|| event.get("usage"))
        .filter(|u| u.is_object())?;
    Some(Body::Meta {
        subtype: "usage".to_string(),
        data: usage.clone(),
    })
}

/// Header facts for `session_id` without a full decode.
struct Header {
    id: Option<String>,
}

fn session_header(path: &Path) -> Header {
    let id = File::open(path)
        .ok()
        .and_then(|f| BufReader::new(f).lines().next())
        .and_then(|l| l.ok())
        .and_then(|l| serde_json::from_str::<serde_json::Value>(&l).ok())
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("session"))
        .and_then(|v| v.get("id").and_then(|i| i.as_str()).map(str::to_string));
    Header { id }
}
