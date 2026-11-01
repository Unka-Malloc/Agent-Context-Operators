//! `aco-dsh` — the DeepSeek Harness conversation-store codec.
//!
//! The Harness records each session as an immutable artifact under
//! `~/.dsh/sessions/<project>/<session-id>/session[.vN].jsonl[.zstd]` — a
//! versioned event log: the first row is the session header, every later
//! row is one event `{seq, time(ms), type, data}`. Artifacts may be plain
//! JSONL or concatenated-Zstandard containers; the file name decides.
//! Only the highest generation per session directory is authoritative —
//! superseded generations stay on disk untouched and are never treated
//! as sessions.
//!
//! The physical format is owned by the vendor. This codec implements
//! format version 4 only: a header declaring any other version is
//! refused by name rather than folded against a schema nobody has read.
//! A fork records `inheritedEventCount` beside its header — rows below
//! that prefix are copied ancestry, not this session's conversation, and
//! are dropped exactly as the usage fold treats them.
//!
//! Session-id lookup reads the header row only — a listing never
//! materializes events it does not need.

use aco::codec::{Capability, CapabilityMatrix, Codec, Decoded, Encoded, Skipped};
use aco::model::{Attachment, Body, Conversation, Record, RecordKind};
use aco::{Error, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// The one physical session-log generation this codec is written against.
const FORMAT_VERSION: u64 = 4;
/// The suffix that marks a Zstandard-framed artifact.
const COMPRESSED_SUFFIX: &str = ".zstd";
/// Largest a single decoded row may be — the row reader streams so a log
/// never has to be buffered whole.
const ROW_MAX_BYTES: u64 = 4 * 1024 * 1024;

const SESSION_KINDS: &[&str] = &["deepseek-harness-session-store"];

static CAPABILITIES: CapabilityMatrix = CapabilityMatrix::new(
    true,
    [
        Capability::Native,      // user
        Capability::Native,      // assistant
        Capability::Native,      // reasoning
        Capability::Native,      // tool-call
        Capability::Native,      // tool-result
        Capability::Unsupported, // system — no native event row
        Capability::Unsupported, // meta — meta rows are store-internal
    ],
);

pub struct DshCodec;

impl DshCodec {
    pub fn new() -> Self {
        Self
    }
}

impl Default for DshCodec {
    fn default() -> Self {
        Self::new()
    }
}

/// The registered codec instance.
pub fn codec() -> &'static dyn Codec {
    &DshCodec
}

impl Codec for DshCodec {
    fn id(&self) -> &'static str {
        "dsh"
    }

    fn history(&self) -> Vec<aco::HistoryRoot> {
        a4aa_dsh::card().history.to_vec()
    }

    fn transcript_usage(&self) -> Option<aco::TranscriptUsage> {
        a4aa_dsh::card().usage.transcript
    }

    fn session_kinds(&self) -> &'static [&'static str] {
        SESSION_KINDS
    }

    fn capabilities(&self) -> &'static CapabilityMatrix {
        &CAPABILITIES
    }

    /// The durable session id lives in the header row, with the session
    /// directory name as the format's own fallback.
    fn session_id(&self, path: &Path) -> String {
        read_header_id(path)
            .filter(|id| !id.trim().is_empty())
            .or_else(|| {
                path.parent()
                    .and_then(|dir| dir.file_name())
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
            })
            .unwrap_or_default()
    }

    /// Only `session[.vN].jsonl[.zstd]` artifacts count, and only the
    /// newest generation of each session directory is authoritative.
    fn select_session_files(&self, files: Vec<PathBuf>) -> Vec<PathBuf> {
        newest_generation(files)
    }

    fn decode(&self, path: &Path) -> Result<Decoded> {
        decode_session(path)
    }

    fn encode(&self, conversation: &Conversation) -> Result<Encoded> {
        let (kept, projection) = self.project(conversation);
        let mut lines = Vec::with_capacity(kept.len() + 2);
        lines.push(
            json!({
                "type": "session",
                "version": FORMAT_VERSION,
                "id": conversation.session_id,
                "inheritedEventCount": 0,
                "createdAt": conversation
                    .created_at
                    .as_deref()
                    .and_then(aco::util::rfc3339_to_epoch_ms),
                "cwd": conversation.cwd,
            })
            .to_string(),
        );
        if let Some(title) = &conversation.title {
            lines.push(row(1, "session/title", json!({ "title": title })));
        }
        for (seq, rec) in (2_u64..).zip(kept) {
            let data = record_event_data(rec);
            lines.push(row(
                seq,
                match rec.body.kind() {
                    RecordKind::User => "user/message",
                    RecordKind::Assistant | RecordKind::Reasoning => "assistant/message",
                    RecordKind::ToolCall => "tool/call",
                    RecordKind::ToolResult => "tool/result",
                    kind => unreachable!("unsupported kind {kind:?} kept by projection"),
                },
                data,
            ));
        }
        Ok(Encoded { lines, projection })
    }

    /// Imported sessions land as a fresh plain-text generation under the
    /// session-store layout. The `<project>` segment is the recorded
    /// working directory's own name when the store knows one — the
    /// directory segments carry no schema weight, only grouping.
    fn store_path(&self, home: &Path, conversation: &Conversation) -> Result<PathBuf> {
        let project = conversation
            .cwd
            .as_deref()
            .and_then(|cwd| Path::new(cwd).file_name())
            .and_then(|name| name.to_str())
            .map(|name| name.replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '_', "-"))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "imported".to_string());
        Ok(home
            .join(".dsh/sessions")
            .join(project)
            .join(&conversation.session_id)
            .join("session.jsonl"))
    }

    /// Rewrite the text one event row carries. Header and meta rows hold
    /// no conversation text — `user/message`, `agent/inbox/spliced`
    /// inserts and `assistant/message` content blocks do.
    fn redact_line(&self, line: &Value, marker: &str) -> Option<Value> {
        let kind = line.get("type")?.as_str()?;
        let mut row = line.clone();
        let data = row.get_mut("data")?;
        match kind {
            "user/message" => {
                redact_content_blocks(data.get_mut("content")?, marker).then_some(row)
            }
            "agent/inbox/spliced" => {
                let mut touched = false;
                if let Some(inserts) = data.get_mut("inserted").and_then(Value::as_array_mut) {
                    for insert in inserts {
                        if let Some(content) = insert.get_mut("content") {
                            touched |= redact_content_blocks(content, marker);
                        }
                    }
                }
                touched.then_some(row)
            }
            "assistant/message" => {
                let message = data.get_mut("message")?;
                redact_content_blocks(message.get_mut("content")?, marker).then_some(row)
            }
            _ => None,
        }
    }
}

/// Which `session[.vN].jsonl[.zstd]` generation a file name declares.
/// `session.jsonl` is generation zero; `.zstd` wraps either spelling.
/// Leading-zero or non-numeric versions declare nothing.
fn generation(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let name = name.strip_suffix(COMPRESSED_SUFFIX).unwrap_or(name);
    if name == "session.jsonl" {
        return Some(0);
    }
    let version = name.strip_prefix("session.v")?.strip_suffix(".jsonl")?;
    if version.starts_with('0') || !version.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    version.parse().ok().filter(|v| *v < (1 << 53))
}

/// Newest generation per session directory — the authoritative artifact
/// set under a session root.
fn newest_generation(files: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut slots = BTreeMap::<PathBuf, (u64, PathBuf)>::new();
    for path in files {
        let Some(version) = generation(&path) else {
            continue;
        };
        let Some(dir) = path.parent() else {
            continue;
        };
        let slot = slots
            .entry(dir.to_path_buf())
            .or_insert((version, path.clone()));
        if version > slot.0 {
            *slot = (version, path);
        }
    }
    slots.into_values().map(|(_, path)| path).collect()
}

/// One session artifact as a decoded row source: a concatenated-Zstandard
/// container when the file name declares one, plain text otherwise.
fn decoded_reader(path: &Path) -> Result<Box<dyn BufRead>> {
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(COMPRESSED_SUFFIX))
    {
        let decoder = zstd::stream::read::Decoder::new(File::open(path).map_err(Error::io(path))?)
            .map_err(|_| Error::Decode {
                path: path.to_path_buf(),
                line: 0,
                message: "zstandard container undecodable".into(),
            })?;
        Ok(Box::new(BufReader::new(decoder)))
    } else {
        Ok(Box::new(BufReader::new(
            File::open(path).map_err(Error::io(path))?,
        )))
    }
}

/// The next non-empty row, bounded so one oversized row cannot turn the
/// line read into an unbounded allocation.
fn next_row(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut line = String::new();
    loop {
        line.clear();
        let mut limited = std::io::Read::take(&mut *reader, ROW_MAX_BYTES + 1);
        let read = limited.read_line(&mut line).map_err(|e| Error::Decode {
            path: PathBuf::new(),
            line: 0,
            message: e.to_string(),
        })?;
        if read == 0 {
            return Ok(None);
        }
        if line.len() as u64 > ROW_MAX_BYTES {
            return Err(Error::Decode {
                path: PathBuf::new(),
                line: 0,
                message: "a row exceeds the session row bound".into(),
            });
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return serde_json::from_str(trimmed)
            .map(Some)
            .map_err(|e| Error::Decode {
                path: PathBuf::new(),
                line: 0,
                message: format!("a row is not valid JSON: {e}"),
            });
    }
}

/// Header-only read for `session_id`: identity lives in the first row.
fn read_header_id(path: &Path) -> Option<String> {
    let mut reader = decoded_reader(path).ok()?;
    let row = next_row(&mut reader).ok()??;
    (row.get("type").and_then(Value::as_str) == Some("session"))
        .then(|| row.get("id")?.as_str().map(str::to_owned))
        .flatten()
}

fn decode_session(path: &Path) -> Result<Decoded> {
    let mut reader = decoded_reader(path)?;
    let header_row = next_row(&mut reader)?.ok_or_else(|| Error::Decode {
        path: path.to_path_buf(),
        line: 0,
        message: "the artifact carries no header row".into(),
    })?;
    if header_row.get("type").and_then(Value::as_str) != Some("session") {
        return Err(Error::Decode {
            path: path.to_path_buf(),
            line: 0,
            message: "the first row is not a session header".into(),
        });
    }
    let version = header_row
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::Decode {
            path: path.to_path_buf(),
            line: 0,
            message: "the header carries no format version".into(),
        })?;
    if version != FORMAT_VERSION {
        return Err(Error::Decode {
            path: path.to_path_buf(),
            line: 0,
            message: format!(
                "session artifact declares format version {version}, which this codec does not read"
            ),
        });
    }
    let inherited = header_row
        .get("inheritedEventCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let session_id = header_row
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| {
            path.parent()
                .and_then(|dir| dir.file_name())
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "dsh-session".to_string());

    let mut events = Vec::new();
    while let Some(row) = next_row(&mut reader)? {
        events.push(row);
    }

    // Splice receipts and the `user/message` they become share one message
    // id: project the canonical row; unclaimed inserts render from the
    // splice itself.
    let claimed_user_ids: BTreeSet<&str> = events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("user/message"))
        .filter_map(|e| e.get("data")?.get("id")?.as_str())
        .collect();
    let mut seen_call_ids = BTreeSet::new();

    let mut conversation = Conversation::new(session_id);
    conversation.agent = Some("dsh".to_string());
    conversation.cwd = header_row
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_owned);
    conversation.created_at = header_row
        .get("createdAt")
        .and_then(Value::as_u64)
        .map(aco::util::epoch_ms_rfc3339);
    conversation.records.push(Record {
        id: None,
        parent: None,
        ts: conversation.created_at.clone(),
        ordinal: 0,
        body: Body::Meta {
            subtype: "session-header".into(),
            data: header_row.clone(),
        },
    });

    let mut skipped = Vec::new();
    let mut record_lines = vec![0_usize];
    for (index, event) in events.iter().enumerate() {
        let line = index + 1;
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let data = event.get("data").cloned().unwrap_or(Value::Null);
        let seq = event.get("seq").and_then(Value::as_u64).unwrap_or_default();
        if seq < inherited {
            continue;
        }
        let ts = event
            .get("time")
            .and_then(Value::as_u64)
            .map(aco::util::epoch_ms_rfc3339);
        let before = conversation.records.len();
        match kind {
            "session/title" => {
                if let Some(title) = data
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|t| !t.trim().is_empty())
                {
                    conversation.title = Some(title.to_string());
                }
                push_meta(&mut conversation, ts, "session-title", data);
            }
            "subagent/descriptor" => push_meta(&mut conversation, ts, "subagent-descriptor", data),
            "request/context" => push_meta(&mut conversation, ts, "request-context", data),
            "request/header" => push_meta(&mut conversation, ts, "request-header", data),
            "llm/retry-started" => push_meta(&mut conversation, ts, "llm-retry-started", data),
            "assistant/attempt" => push_meta(&mut conversation, ts, "assistant-attempt", data),
            "user/message" => project_user(&mut conversation, &data, ts),
            "agent/inbox/spliced" => {
                if let Some(inserts) = data.get("inserted").and_then(Value::as_array) {
                    for insert in inserts {
                        let claimed = insert
                            .get("id")
                            .and_then(Value::as_str)
                            .is_some_and(|id| claimed_user_ids.contains(id));
                        if !claimed {
                            project_user(&mut conversation, insert, ts.clone());
                        }
                    }
                }
                push_meta(&mut conversation, ts, "agent-inbox-spliced", data);
            }
            "assistant/message" => {
                if let Some(blocks) = data
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                {
                    let model = data
                        .pointer("/message/source/model")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    for block in blocks {
                        let block_type = block
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        match block_type {
                            "reasoning" => {
                                if let Some(text) = non_empty(block.get("text")) {
                                    push(&mut conversation, ts.clone(), Body::Reasoning { text });
                                }
                            }
                            "tool-call" | "tool_call" => {
                                if let Some(id) = block.get("id").and_then(Value::as_str) {
                                    seen_call_ids.insert(id.to_string());
                                }
                                push(
                                    &mut conversation,
                                    ts.clone(),
                                    Body::ToolCall {
                                        call_id: block
                                            .get("id")
                                            .and_then(Value::as_str)
                                            .map(str::to_owned),
                                        name: block
                                            .get("name")
                                            .or_else(|| block.get("tool"))
                                            .and_then(Value::as_str)
                                            .unwrap_or("tool")
                                            .to_string(),
                                        args: block
                                            .get("input")
                                            .or_else(|| block.get("arguments"))
                                            .or_else(|| block.get("args"))
                                            .cloned()
                                            .unwrap_or(Value::Null),
                                    },
                                );
                            }
                            _ => {
                                if let Some(text) = extract_text(block) {
                                    push(
                                        &mut conversation,
                                        ts.clone(),
                                        Body::Assistant {
                                            text,
                                            model: model.clone(),
                                        },
                                    );
                                }
                            }
                        }
                    }
                }
                if let Some(usage) = event_usage(&data, event) {
                    push_meta(
                        &mut conversation,
                        ts,
                        "usage",
                        json!({
                            "usage": usage,
                            "turn": data.get("turn"),
                            "step": data.get("step"),
                            "model": data.pointer("/message/source/model"),
                            "provider": data.pointer("/message/source/provider"),
                            "messageId": data.pointer("/message/id"),
                        }),
                    );
                }
            }
            "tool/call" => {
                let call_id = data.get("callId").and_then(Value::as_str);
                if call_id.is_some_and(|id| seen_call_ids.contains(id)) {
                    skipped.push(Skipped {
                        line,
                        native_type: Some(kind.to_string()),
                        reason: "call already carried by its assistant message".into(),
                    });
                    continue;
                }
                if let Some(id) = call_id {
                    seen_call_ids.insert(id.to_string());
                }
                push(
                    &mut conversation,
                    ts,
                    Body::ToolCall {
                        call_id: call_id.map(str::to_owned),
                        name: data
                            .get("name")
                            .or_else(|| data.get("tool"))
                            .and_then(Value::as_str)
                            .unwrap_or("tool")
                            .to_string(),
                        args: data
                            .get("input")
                            .or_else(|| data.get("arguments"))
                            .cloned()
                            .unwrap_or(Value::Null),
                    },
                );
            }
            "tool/result" => {
                push(
                    &mut conversation,
                    ts,
                    Body::ToolResult {
                        call_id: data
                            .get("callId")
                            .or_else(|| data.get("call_id"))
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        name: data.get("name").and_then(Value::as_str).map(str::to_owned),
                        content: data.get("message").cloned().unwrap_or(data.clone()),
                        is_error: data
                            .get("isError")
                            .or_else(|| data.get("error"))
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    },
                );
            }
            // Every other event row is preserved as metadata — the
            // canonical transcript never silently loses persisted rows.
            _ => push_meta(&mut conversation, ts, kind, data),
        }
        for _ in before..conversation.records.len() {
            record_lines.push(line);
        }
    }

    if conversation.records.len() == 1 && conversation.title.is_none() {
        return Err(Error::Decode {
            path: path.to_path_buf(),
            line: 0,
            message: "session artifact carries no conversation rows".into(),
        });
    }
    for (i, rec) in conversation.records.iter_mut().enumerate() {
        rec.ordinal = i as u64;
    }
    Ok(Decoded {
        line_count: events.len() + 1,
        conversation,
        record_lines,
        skipped,
    })
}

fn push(conversation: &mut Conversation, ts: Option<String>, body: Body) {
    conversation.records.push(Record {
        id: None,
        parent: None,
        ts,
        ordinal: conversation.records.len() as u64,
        body,
    });
}

fn push_meta(conversation: &mut Conversation, ts: Option<String>, subtype: &str, data: Value) {
    push(
        conversation,
        ts,
        Body::Meta {
            subtype: subtype.to_string(),
            data,
        },
    );
}

/// One user-authored delivery: a `user/message` data object or one insert
/// of an `agent/inbox/spliced` event — both carry `content` blocks and a
/// role.
fn project_user(conversation: &mut Conversation, data: &Value, ts: Option<String>) {
    let role_assistant = data.get("role").and_then(Value::as_str) == Some("assistant");
    let mut text_parts = Vec::new();
    let mut attachments = Vec::new();
    if let Some(blocks) = data.get("content").and_then(Value::as_array) {
        for block in blocks {
            match block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "text" => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        text_parts.push(text.to_string());
                    }
                }
                other => attachments.push(Attachment {
                    kind: other.to_string(),
                    media_type: block
                        .get("mediaType")
                        .or_else(|| block.get("mimeType"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    data: block.clone(),
                }),
            }
        }
    }
    let text = if text_parts.is_empty() {
        extract_text(data)
    } else {
        let joined = text_parts.join("\n");
        (!joined.trim().is_empty()).then_some(joined)
    };
    let Some(text) = text else {
        return;
    };
    let id = data.get("id").and_then(Value::as_str).map(str::to_owned);
    let body = if role_assistant {
        Body::Assistant { text, model: None }
    } else {
        Body::User { text, attachments }
    };
    conversation.records.push(Record {
        id,
        parent: None,
        ts,
        ordinal: conversation.records.len() as u64,
        body,
    });
}

/// Pull the text a block carries: an explicit `text` field, or the first
/// string-shaped content carrier the block offers.
fn extract_text(block: &Value) -> Option<String> {
    block
        .get("text")
        .or_else(|| block.get("content"))
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .map(str::to_owned)
}

/// The settled usage one assistant event carries: `data.usage` or the
/// stream's last `usage` chunk — the vendor's own object, unchanged.
fn event_usage(data: &Value, event: &Value) -> Option<Value> {
    data.get("usage")
        .filter(|u| !u.is_null())
        .cloned()
        .or_else(|| {
            event
                .get("data")?
                .get("stream")?
                .as_array()?
                .iter()
                .rev()
                .find_map(|rec| {
                    (rec.pointer("/chunk/type").and_then(Value::as_str) == Some("usage"))
                        .then(|| rec.pointer("/chunk/usage").cloned())
                        .flatten()
                })
        })
        .or_else(|| {
            data.get("stream")?
                .as_array()?
                .iter()
                .rev()
                .find_map(|rec| {
                    (rec.pointer("/chunk/type").and_then(Value::as_str) == Some("usage"))
                        .then(|| rec.pointer("/chunk/usage").cloned())
                        .flatten()
                })
        })
}

fn non_empty(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .map(str::to_owned)
}

/// The native `data` object a canonical record encodes to.
fn record_event_data(rec: &Record) -> Value {
    match &rec.body {
        Body::User { text, attachments } => {
            let mut content = vec![json!({ "type": "text", "text": text })];
            for attachment in attachments {
                content.push(json!({ "type": attachment.kind, "data": attachment.data }));
            }
            json!({ "role": "user", "content": content })
        }
        Body::Assistant { text, model } => json!({
            "message": {
                "id": rec.id,
                "content": [{ "type": "text", "text": text }],
                "source": { "model": model },
            }
        }),
        Body::Reasoning { text } => json!({
            "message": {
                "id": rec.id,
                "content": [{ "type": "reasoning", "text": text }],
            }
        }),
        Body::ToolCall {
            call_id,
            name,
            args,
        } => json!({
            "callId": call_id,
            "name": name,
            "input": args,
        }),
        Body::ToolResult {
            call_id,
            name,
            content,
            is_error,
        } => json!({
            "callId": call_id,
            "name": name,
            "message": content,
            "isError": is_error,
        }),
        _ => Value::Null,
    }
}

fn row(seq: u64, kind: &str, data: Value) -> String {
    json!({ "seq": seq, "time": seq, "type": kind, "data": data }).to_string()
}

fn redact_content_blocks(content: &mut Value, marker: &str) -> bool {
    let Some(blocks) = content.as_array_mut() else {
        return false;
    };
    let mut touched = false;
    for block in blocks {
        if let Some(text) = block.get_mut("text") {
            if text.is_string() {
                *text = json!(marker);
                touched = true;
            }
        }
    }
    touched
}
