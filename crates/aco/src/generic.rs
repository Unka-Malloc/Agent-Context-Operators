//! Agent-agnostic transcript decoders — the generic lane.
//!
//! Agents with no bespoke wire format persist conversations in one of
//! three shared shapes: line-delimited JSON (`.jsonl`/`.ndjson`), a
//! single JSON document (`.json` holding a `messages` array or a bare
//! array of message objects), and plain markdown/text transcripts
//! (`.md`/`.markdown`/`.txt`/`.log`). [`dispatch`] picks the decoder by
//! file extension; [`accepts`] is the format side of `accepts_file`.
//!
//! The decoding semantics are ported from LicoUp's service-neutral
//! `history/generic.rs`: message objects carry `role`/`type`/`content`/
//! `text` variants, assistant `content` block arrays fan out into one
//! record per block (`thinking`/`toolCall`-typed blocks become
//! `reasoning`/`tool-call`), `usage` objects on message-bearing lines
//! become `meta{usage}` records, and directory-layout session identity
//! (`brain/<conversation-uuid>/transcript.jsonl`) is recovered from the
//! path. Lines that parse but carry no recognizable message go to
//! [`Decoded::skipped`] as `projection`; unparseable lines as `corrupt`.
//!
//! The SQLite probe covers agents whose store is a `.sqlite`/`.db`
//! database with no known schema: tables are filtered by name, rows by
//! a conversation-payload check, and each surviving row's text/blob
//! columns are parsed as JSON through the same message extraction the
//! `.json` decoder uses — one session per row. [`sqlite_session_keys`]
//! enumerates the `table/key` selectors; [`decode_sqlite`] resolves
//! one.
//!
//! [`encode_jsonl`] writes the shape these decoders read back: one
//! `{"type":"message","role":…}`-style line per record. Its implied
//! capability table: user/assistant/reasoning/tool-call/tool-result are
//! native, `system` is adapted (a `{"type":"system",…}` line — the
//! generic shape has no standardized event record), `meta{usage}` is
//! adapted (rides inside an assistant message's `usage` field), and
//! every other `meta` subtype is unsupported and reported dropped.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, Row};
use serde_json::{json, Map, Value};

use crate::codec::{Decoded, Encoded, Projection, Skipped};
use crate::model::{Body, Conversation, Record};
use crate::{util, Error, Result};

/// Extensions the generic decoders handle, lowercase without the dot.
const ACCEPTED_EXTENSIONS: &[&str] = &["jsonl", "ndjson", "json", "md", "markdown", "txt", "log"];

/// Object keys that hold a session's own identity when one is declared
/// on a record line.
const LINE_SESSION_KEYS: &[&str] = &[
    "sessionId",
    "session_id",
    "conversationId",
    "conversation_id",
    "chatId",
    "chat_id",
    "threadId",
    "thread_id",
    "sessionKey",
    "session_key",
];

/// `type`/`kind`/`role` values that mark a line as session metadata —
/// only those lines may contribute a bare `id`, title or name to the
/// conversation; a message's own `id`/`name` must not become either.
const SESSION_HEADER_SEMANTICS: &[&str] = &[
    "session",
    "session-info",
    "sessioninfo",
    "session-meta",
    "sessionmeta",
    "header",
    "meta",
    "metadata",
    "summary",
    "title",
];

/// Keys that hold token-usage counter objects, at the line level or
/// inside a `message` payload.
const USAGE_KEYS: &[&str] = &[
    "usage",
    "tokenUsage",
    "token_usage",
    "usageMetadata",
    "usage_metadata",
    "responseUsage",
    "response_usage",
    "tokenCount",
    "token_count",
    "tokens",
];

/// Whether `ext` (lowercase, no dot) is a file format the generic
/// decoders handle — the generic side of `accepts_file`.
pub fn accepts(ext: &str) -> bool {
    ACCEPTED_EXTENSIONS.contains(&ext)
}

/// Dispatch on file extension to the right generic decoder. Extensions
/// this knows: jsonl, ndjson, json, md, markdown, txt, log.
pub fn dispatch(path: &Path, agent: &str) -> Result<Decoded> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "jsonl" | "ndjson" => decode_jsonl(path, agent),
        "json" => decode_json_document(path, agent),
        "md" | "markdown" | "txt" | "log" => decode_text(path, agent),
        _ => Err(Error::Usage(format!(
            "no generic decoder for `{}` (extension `{ext}`)",
            path.display()
        ))),
    }
}

/// Best-effort session id: a UUID-looking path component when one
/// exists, else the file stem. Directory-layout stores (for example
/// `brain/<conversation-uuid>/transcript.jsonl`) carry the conversation
/// identity in the path rather than in the records.
pub fn session_id(path: &Path) -> String {
    path.components()
        .rev()
        .skip(1)
        .find_map(|component| {
            let value = component.as_os_str().to_str()?;
            is_conversation_uuid_component(value).then(|| value.to_string())
        })
        .or_else(|| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::trim)
                .filter(|stem| !stem.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// Decode a `.jsonl`/`.ndjson` transcript where each line is a JSON
/// object — the shape shared by agents with no bespoke wire format.
///
/// A line's declared session id (`sessionId`, `conversationId`, …)
/// groups the file: the first declared id becomes the conversation's
/// identity and lines declaring a different session are skipped as
/// `projection` — a multi-session JSONL contributes the first session.
/// `cwd`/`title`/timestamps ride on whatever line carries them.
pub fn decode_jsonl(path: &Path, agent: &str) -> Result<Decoded> {
    let file = File::open(path).map_err(Error::io(path))?;
    let mut conversation = Conversation::new(session_id(path));
    conversation.agent = Some(agent.to_string());
    let mut record_lines = Vec::new();
    let mut skipped = Vec::new();
    let mut line_count = 0usize;
    let mut declared_session: Option<String> = None;

    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(Error::io(path))?;
        line_count = index + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            skipped.push(Skipped {
                line: index,
                native_type: None,
                reason: "corrupt".to_string(),
            });
            continue;
        };
        if let Some(id) = line_session_id(&value) {
            match &declared_session {
                None => {
                    declared_session = Some(id.clone());
                    conversation.session_id = id;
                }
                Some(declared) if *declared != id => {
                    skipped.push(Skipped {
                        line: index,
                        native_type: native_type(&value),
                        reason: "projection".to_string(),
                    });
                    continue;
                }
                _ => {}
            }
        }
        let semantic = normalize_semantic(&native_content_semantic(&value));
        if conversation.cwd.is_none() {
            conversation.cwd = working_directory(&value);
        }
        if conversation.title.is_none() && SESSION_HEADER_SEMANTICS.contains(&semantic.as_str()) {
            conversation.title = document_title(&value);
        }
        let ts = timestamp_of(&value);
        if conversation.created_at.is_none() {
            conversation.created_at = ts.clone();
        }
        if ts.is_some() {
            conversation.updated_at = ts.clone();
        }
        let id = declared_record_id(&value);
        let bodies = bodies_with_meta(&value);
        if bodies.is_empty() {
            skipped.push(Skipped {
                line: index,
                native_type: native_type(&value),
                reason: "projection".to_string(),
            });
            continue;
        }
        for body in bodies {
            push(
                &mut conversation.records,
                &mut record_lines,
                index,
                ts.clone(),
                id.clone(),
                body,
            );
        }
    }
    finalize(&mut conversation);
    Ok(Decoded {
        conversation,
        record_lines,
        line_count,
        skipped,
    })
}

/// Decode a single-document `.json` session file (object containing a
/// messages array, or a top-level array of message objects).
///
/// `record_lines` carries the source item's index inside the array it
/// was collected from (LicoUp's message-index semantics); `line_count`
/// is the document's text line count.
pub fn decode_json_document(path: &Path, agent: &str) -> Result<Decoded> {
    let text = std::fs::read_to_string(path).map_err(Error::io(path))?;
    let line_count = text.lines().count();
    let value: Value = serde_json::from_str(&text).map_err(|e| Error::Decode {
        path: path.to_path_buf(),
        line: e.line(),
        message: e.to_string(),
    })?;
    // A single-document session record commonly names itself with a bare
    // `id`; that is still record-native identity, never a file-stem or
    // positional placeholder.
    let session = document_session_id(&value).unwrap_or_else(|| session_id(path));
    let mut conversation = Conversation::new(session);
    conversation.agent = Some(agent.to_string());
    conversation.title = document_title(&value);
    conversation.cwd = working_directory(&value);
    conversation.created_at = timestamp_of(&value);
    conversation.updated_at = conversation.created_at.clone();

    let mut collected = Vec::<(usize, Option<String>, Body)>::new();
    collect_messages(&value, &mut collected);
    let mut record_lines = Vec::with_capacity(collected.len());
    for (index, ts, body) in collected {
        conversation.records.push(Record {
            id: None,
            parent: None,
            ts,
            ordinal: 0,
            body,
        });
        record_lines.push(index);
    }
    finalize(&mut conversation);
    Ok(Decoded {
        conversation,
        record_lines,
        line_count,
        skipped: Vec::new(),
    })
}

/// Decode a markdown/plaintext transcript (`.md`/`.markdown`/`.txt`/
/// `.log`) — role-prefixed lines where recognizable, otherwise
/// paragraph blocks.
///
/// `user:`/`human:`/`prompt:`/`question:` open `user` records and
/// `assistant:`/`agent:`/`response:`/`answer:` open `assistant` records
/// (prefix matched case-insensitively); the text after the prefix
/// starts the record and following non-blank lines continue it. A blank
/// line ends the open block. Text that owns no role prefix becomes a
/// `system{transcript}` paragraph record — unattributed prose is still
/// conversation content, never silently dropped. A leading `# ` heading
/// supplies the title. `created_at`/`updated_at` come from the file's
/// modification time, the only timestamp a flat transcript has.
pub fn decode_text(path: &Path, agent: &str) -> Result<Decoded> {
    let raw = std::fs::read_to_string(path).map_err(Error::io(path))?;
    let mut conversation = Conversation::new(session_id(path));
    conversation.agent = Some(agent.to_string());
    conversation.created_at = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| util::epoch_ms_rfc3339(d.as_millis() as u64));
    conversation.updated_at = conversation.created_at.clone();

    let mut record_lines = Vec::new();
    let mut open: Option<usize> = None;
    for (index, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            open = None;
            continue;
        }
        if conversation.title.is_none() && trimmed.starts_with("# ") {
            let title = trimmed[2..].trim();
            if !title.is_empty() {
                conversation.title = Some(title.to_string());
                continue;
            }
        }
        match role_prefixed(trimmed) {
            Some(RolePrefix::User) => {
                open = Some(conversation.records.len());
                push(
                    &mut conversation.records,
                    &mut record_lines,
                    index,
                    None,
                    None,
                    Body::User {
                        text: prefix_body(trimmed).to_string(),
                        attachments: Vec::new(),
                    },
                );
            }
            Some(RolePrefix::Assistant) => {
                open = Some(conversation.records.len());
                push(
                    &mut conversation.records,
                    &mut record_lines,
                    index,
                    None,
                    None,
                    Body::Assistant {
                        text: prefix_body(trimmed).to_string(),
                        model: None,
                    },
                );
            }
            None => {
                if let Some(at) = open {
                    append_text(&mut conversation.records[at].body, trimmed);
                } else {
                    open = Some(conversation.records.len());
                    push(
                        &mut conversation.records,
                        &mut record_lines,
                        index,
                        None,
                        None,
                        Body::System {
                            subtype: "transcript".to_string(),
                            text: trimmed.to_string(),
                        },
                    );
                }
            }
        }
    }
    let line_count = raw.lines().count();
    finalize(&mut conversation);
    Ok(Decoded {
        conversation,
        record_lines,
        line_count,
        skipped: Vec::new(),
    })
}

/// Generic JSONL encode: one `{"type":"message","role":…}`-style line
/// per record, preserving kind semantics where the shape allows. A run
/// of assistant-side records (text, reasoning, tool calls, a trailing
/// `meta{usage}`) folds into one assistant message whose `content`
/// block array carries them — the same shape `decode_jsonl` fans back
/// out. `system` records emit `{"type":"system","subtype":…}` lines
/// (adapted); `meta{usage}` rides in `message.usage` (adapted); other
/// `meta` subtypes are reported dropped.
pub fn encode_jsonl(conversation: &Conversation) -> Result<Encoded> {
    let mut lines = Vec::with_capacity(conversation.records.len());
    let mut projection = Projection::default();
    let mut pending: Vec<&Record> = Vec::new();
    let mut pending_usage: Option<Value> = None;

    for rec in &conversation.records {
        match &rec.body {
            Body::Assistant { .. } | Body::Reasoning { .. } | Body::ToolCall { .. } => {
                pending.push(rec);
            }
            Body::Meta { subtype, data } if subtype == "usage" => {
                projection.adapted.push((rec.ordinal, rec.kind()));
                if pending.is_empty() {
                    // No assistant context to carry the counters: the
                    // usage rides alone on a message-shaped line.
                    lines.push(usage_line(rec, data));
                } else {
                    pending_usage = Some(data.clone());
                }
            }
            Body::User { text, attachments } => {
                flush_assistant(&mut pending, &mut pending_usage, &mut lines);
                let mut content = vec![json!({"type": "text", "text": text})];
                for attachment in attachments {
                    content.push(attachment.data.clone());
                }
                lines.push(
                    json!({
                        "type": "message",
                        "timestamp": rec.ts,
                        "message": {"role": "user", "content": content},
                    })
                    .to_string(),
                );
            }
            Body::ToolResult {
                call_id,
                name,
                content,
                is_error,
            } => {
                flush_assistant(&mut pending, &mut pending_usage, &mut lines);
                lines.push(
                    json!({
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
            Body::System { subtype, text } => {
                flush_assistant(&mut pending, &mut pending_usage, &mut lines);
                projection.adapted.push((rec.ordinal, rec.kind()));
                lines.push(
                    json!({
                        "type": "system",
                        "timestamp": rec.ts,
                        "subtype": subtype,
                        "content": text,
                    })
                    .to_string(),
                );
            }
            Body::Meta { .. } => {
                projection.dropped.push((rec.ordinal, rec.kind()));
            }
        }
    }
    flush_assistant(&mut pending, &mut pending_usage, &mut lines);
    Ok(Encoded { lines, projection })
}

/// Replace textual content fields in one raw line for `aco edit
/// --redact`: walks `content`/`text`/`thinking`/message-payload fields.
/// `None` means the line holds no redactable text.
pub fn redact_line(line: &Value, marker: &str) -> Option<Value> {
    let mut out = line.clone();
    redact_value(&mut out, marker).then_some(out)
}

// --- decode helpers ---------------------------------------------------

fn push(
    records: &mut Vec<Record>,
    record_lines: &mut Vec<usize>,
    line: usize,
    ts: Option<String>,
    id: Option<String>,
    body: Body,
) {
    records.push(Record {
        id,
        parent: None,
        ts,
        ordinal: 0,
        body,
    });
    record_lines.push(line);
}

fn finalize(conversation: &mut Conversation) {
    for (ordinal, record) in conversation.records.iter_mut().enumerate() {
        record.ordinal = ordinal as u64;
    }
}

/// `messages_from_json`: one source object → canonical bodies. A
/// `message.content`/`content` block array fans out one body per block;
/// otherwise the object's own semantic decides — a typed event becomes
/// a structured record, a text-carrier becomes a role-split text body.
fn bodies_from_value(value: &Value) -> Vec<Body> {
    let role = extract_role(value);
    let model = native_model(value);
    if let Some(blocks) = content_blocks(value) {
        let bodies: Vec<Body> = blocks
            .iter()
            .filter_map(|block| body_from_block(&role, block, &model))
            .collect();
        if !bodies.is_empty() {
            return bodies;
        }
    }
    let semantic = native_content_semantic(value);
    let kind = kind_from_semantic(&semantic);
    if kind != MessageKind::Text {
        return vec![structured_body(kind, &normalize_semantic(&semantic), value)];
    }
    let Some(text) = extract_text(value) else {
        return Vec::new();
    };
    vec![text_body(&role, text, model, value)]
}

/// A line's bodies plus its usage payload: a `usage` object on a
/// message-bearing line (or as the line's only payload) becomes a
/// trailing `meta{usage}` record.
fn bodies_with_meta(value: &Value) -> Vec<Body> {
    let mut bodies = bodies_from_value(value);
    if let Some(usage) = usage_object(value) {
        bodies.push(Body::Meta {
            subtype: "usage".to_string(),
            data: usage,
        });
    }
    bodies
}

/// `direct_native_content_blocks`: `message.content` or `content` as a
/// block array.
fn content_blocks(value: &Value) -> Option<&Vec<Value>> {
    value
        .get("message")
        .and_then(|message| message.get("content"))
        .or_else(|| value.get("content"))
        .and_then(Value::as_array)
}

/// `message_from_native_content_block`: a typed block (`thinking`,
/// `toolCall`, `tool_result`, …) becomes its structured record; a text
/// block takes the enclosing message's role.
fn body_from_block(outer_role: &str, block: &Value, model: &Option<String>) -> Option<Body> {
    let semantic = native_content_semantic(block);
    let kind = kind_from_semantic(&semantic);
    if kind != MessageKind::Text {
        return Some(structured_body(kind, &normalize_semantic(&semantic), block));
    }
    extract_text(block).map(|text| text_body(outer_role, text, model.clone(), block))
}

/// The role split for a text-carrying record: user turns, model turns,
/// tool output (payload fields carry `toolCallId`/`isError`), and
/// anything else as a system record named by its raw role.
fn text_body(role: &str, text: String, model: Option<String>, source: &Value) -> Body {
    match role {
        "user" | "human" => Body::User {
            text,
            attachments: Vec::new(),
        },
        "agent" => Body::Assistant { text, model },
        "tool" => {
            let payload = message_payload(source);
            Body::ToolResult {
                call_id: call_id(payload),
                name: structured_name(payload).map(str::to_string),
                content: ["content", "output", "result", "text"]
                    .iter()
                    .find_map(|key| payload.get(*key))
                    .cloned()
                    .unwrap_or(Value::String(text)),
                is_error: ["isError", "is_error", "error"]
                    .iter()
                    .find_map(|key| payload.get(*key).and_then(Value::as_bool))
                    .unwrap_or(false),
            }
        }
        other => Body::System {
            subtype: if other.is_empty() { "system" } else { other }.to_string(),
            text,
        },
    }
}

/// A non-text semantic becomes the record kind it declares — the
/// structured-message half of LicoUp's projection.
fn structured_body(kind: MessageKind, semantic: &str, value: &Value) -> Body {
    let payload = message_payload(value);
    match kind {
        MessageKind::ToolCall => Body::ToolCall {
            call_id: call_id(payload),
            name: structured_name(payload).unwrap_or("tool").to_string(),
            args: ["input", "arguments", "args", "parameters", "params"]
                .iter()
                .find_map(|key| payload.get(*key))
                .or_else(|| payload.get("function").and_then(|f| f.get("arguments")))
                .cloned()
                .unwrap_or(Value::Null),
        },
        MessageKind::ToolResult => Body::ToolResult {
            call_id: call_id(payload),
            name: structured_name(payload).map(str::to_string),
            content: ["content", "output", "result", "text"]
                .iter()
                .find_map(|key| payload.get(*key))
                .cloned()
                .unwrap_or_else(|| extract_text(payload).map(Value::String).unwrap_or_default()),
            is_error: ["isError", "is_error", "error"]
                .iter()
                .find_map(|key| payload.get(*key).and_then(Value::as_bool))
                .unwrap_or(false),
        },
        MessageKind::Reasoning => Body::Reasoning {
            text: reasoning_text(payload),
        },
        MessageKind::Metadata => Body::Meta {
            subtype: if semantic.is_empty() {
                "metadata".to_string()
            } else {
                semantic.to_string()
            },
            data: value.clone(),
        },
        MessageKind::Error => Body::System {
            subtype: "error".to_string(),
            text: extract_text(payload).unwrap_or_else(|| payload.to_string()),
        },
        MessageKind::Event | MessageKind::Text => Body::System {
            subtype: if semantic.is_empty() {
                "event".to_string()
            } else {
                semantic.to_string()
            },
            text: extract_text(payload).unwrap_or_else(|| payload.to_string()),
        },
    }
}

/// The object a record's call/result fields actually live on: the
/// `message` payload when the shape wraps one, else the object itself.
fn message_payload(value: &Value) -> &Value {
    value
        .get("message")
        .filter(|message| message.is_object())
        .unwrap_or(value)
}

/// Reasoning payloads hide under `thinking`, `reasoning`, `summary` and
/// friends; the whole-object walk skips a `thinking`-typed object, so
/// the named fields are read first.
fn reasoning_text(value: &Value) -> String {
    for key in [
        "thinking",
        "reasoning",
        "text",
        "summary",
        "content",
        "analysis",
    ] {
        match value.get(key) {
            Some(Value::String(text)) if !text.trim().is_empty() => return text.clone(),
            Some(child) => {
                if let Some(text) = extract_text(child) {
                    return text;
                }
            }
            _ => {}
        }
    }
    extract_text(value).unwrap_or_default()
}

fn call_id(value: &Value) -> Option<String> {
    [
        "call_id",
        "callId",
        "toolCallId",
        "tool_call_id",
        "toolUseId",
        "tool_use_id",
        "id",
    ]
    .iter()
    .find_map(|key| value.get(*key).and_then(Value::as_str))
    .filter(|id| !id.trim().is_empty())
    .map(str::to_string)
}

/// `structured_name`: the tool's declared name, including a
/// function-call envelope's `function.name`.
fn structured_name(value: &Value) -> Option<&str> {
    [
        "name",
        "toolName",
        "tool_name",
        "functionName",
        "function_name",
        "tool",
    ]
    .iter()
    .find_map(|key| value.get(*key).and_then(Value::as_str))
    .or_else(|| {
        value
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
    })
}

/// `collect_messages_from_value`: nested conversation containers
/// (`messages`, `sessions`, `turns`, …) recurse; bare items decode or
/// recurse themselves. `(index, timestamp, body)` triples carry the
/// collecting array position for `record_lines`.
fn collect_messages(value: &Value, out: &mut Vec<(usize, Option<String>, Body)>) {
    match value {
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                if value_is_conversation_container(item) {
                    collect_messages(item, out);
                    continue;
                }
                let bodies = bodies_with_meta(item);
                if bodies.is_empty() {
                    collect_messages(item, out);
                } else {
                    let ts = timestamp_of(item);
                    out.extend(bodies.into_iter().map(|body| (index, ts.clone(), body)));
                }
            }
        }
        Value::Object(object) => {
            let before = out.len();
            for key in [
                "messages",
                "conversation",
                "conversations",
                "transcript",
                "turns",
                "items",
                "entries",
                "sessions",
                "chats",
                "chatSessions",
            ] {
                if let Some(child) = object.get(key) {
                    collect_messages(child, out);
                }
            }
            if out.len() == before {
                let bodies = bodies_with_meta(value);
                let ts = timestamp_of(value);
                out.extend(bodies.into_iter().map(|body| (0, ts.clone(), body)));
            }
        }
        _ => {}
    }
}

/// `value_is_conversation_container`: holds a nested message container
/// and no direct message text of its own.
fn value_is_conversation_container(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let has_container = [
        "messages",
        "conversation",
        "conversations",
        "transcript",
        "turns",
        "entries",
        "sessions",
        "chats",
        "chatSessions",
    ]
    .iter()
    .any(|key| object.contains_key(*key));
    let has_direct_message_text = [
        "text", "content", "prompt", "response", "answer", "summary", "value",
    ]
    .iter()
    .any(|key| object.contains_key(*key));
    has_container && !has_direct_message_text
}

// --- semantic classification (ported verbatim) ------------------------

/// `HistoryMessageKind`: the semantic classes a source object or
/// content block can declare.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MessageKind {
    Text,
    ToolCall,
    ToolResult,
    Reasoning,
    Metadata,
    Error,
    Event,
}

/// `history_message_kind_from_semantic`: classify one semantic tag.
fn kind_from_semantic(value: &str) -> MessageKind {
    let semantic = normalize_semantic(value);
    if semantic.is_empty()
        || matches!(
            semantic.as_str(),
            "text"
                | "input-text"
                | "output-text"
                | "markdown"
                | "message"
                | "summary-text"
                | "user"
                | "human"
                | "assistant"
                | "agent"
                | "model"
                | "ai"
                | "planner-response"
                | "generic"
        )
        || semantic.ends_with("-user-message")
        || semantic.ends_with("-assistant-message")
        || matches!(semantic.as_str(), "user-message" | "assistant-message")
    {
        return MessageKind::Text;
    }
    if semantic.contains("reasoning")
        || semantic.contains("analysis")
        || semantic.contains("thinking")
    {
        return MessageKind::Reasoning;
    }
    if semantic.contains("error")
        || semantic.contains("failure")
        || semantic.contains("failed")
        || semantic.contains("exception")
    {
        return MessageKind::Error;
    }
    if semantic == "metadata"
        || matches!(
            semantic.as_str(),
            "image" | "image-url" | "document" | "attachment" | "input-json-delta"
        )
    {
        return MessageKind::Metadata;
    }
    if matches!(
        semantic.as_str(),
        "tool-result"
            | "tool-output"
            | "function-result"
            | "function-output"
            | "function-call-output"
    ) || ((semantic.contains("tool") || semantic.contains("function"))
        && [
            "result",
            "output",
            "complete",
            "completed",
            "response",
            "end",
        ]
        .iter()
        .any(|marker| semantic.contains(marker)))
    {
        return MessageKind::ToolResult;
    }
    if matches!(
        semantic.as_str(),
        "tool"
            | "tool-call"
            | "tool-use"
            | "function"
            | "function-call"
            | "run-command"
            | "view-file"
            | "list-directory"
            | "grep-search"
            | "read-url-content"
            | "generate-image"
            | "code-action"
    ) || semantic.contains("tool")
        || semantic.contains("function")
    {
        return MessageKind::ToolCall;
    }
    MessageKind::Event
}

/// `normalize_history_message_semantic`: lowercase alnum runs joined
/// with `-` — `toolCall`, `tool_call` and `tool call` are one semantic.
fn normalize_semantic(value: &str) -> String {
    let mut normalized = String::new();
    let mut separator = false;
    for character in value.trim().chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !normalized.is_empty() {
                normalized.push('-');
            }
            separator = false;
            normalized.push(character.to_ascii_lowercase());
        } else {
            separator = true;
        }
    }
    normalized
}

/// `native_content_semantic`: the first string of the object's
/// type-ish fields.
fn native_content_semantic(value: &Value) -> String {
    value
        .as_object()
        .and_then(|object| {
            ["type", "kind", "role", "eventType", "event_type"]
                .iter()
                .find_map(|key| object.get(*key).and_then(Value::as_str))
        })
        .unwrap_or_default()
        .to_string()
}

/// The raw type tag a skipped line declared, when one was recognized.
fn native_type(value: &Value) -> Option<String> {
    let semantic = native_content_semantic(value);
    (!semantic.is_empty()).then_some(semantic)
}

// --- field extraction (ported from json_extract.rs) -------------------

/// `find_string`: first string/integer field among `keys`, descending
/// into a `message` payload when the outer object has none.
fn find_string(value: &Value, keys: &[&str]) -> Option<String> {
    let mut candidate = value;
    loop {
        let object = candidate.as_object()?;
        for key in keys {
            if let Some(text) = object.get(*key).and_then(Value::as_str) {
                return Some(text.to_string());
            }
            if let Some(number) = object.get(*key).and_then(Value::as_i64) {
                return Some(number.to_string());
            }
        }
        candidate = object.get("message")?;
    }
}

/// `extract_role`: normalized `user`/`agent`/`tool`/other role.
/// Numeric `type` codes are honored first (1 = user, 0/2 = agent); a
/// wrapped `message` payload's own role outranks the envelope's `type`
/// tag, which is transport metadata (`"message"`), not a speaker.
fn extract_role(value: &Value) -> String {
    if let Some(type_code) = value.get("type").and_then(Value::as_i64) {
        match type_code {
            1 => return "user".to_string(),
            0 | 2 => return "agent".to_string(),
            _ => {}
        }
    }
    let role = value
        .get("message")
        .filter(|message| message.is_object())
        .and_then(|message| find_string(message, &["role", "author", "speaker"]))
        .or_else(|| find_string(value, &["role", "author", "speaker", "type", "source"]))
        .unwrap_or_else(|| "system".to_string())
        .to_ascii_lowercase();
    if role.contains("user") || role.contains("human") || role == "1" {
        "user".to_string()
    } else if role.contains("assistant")
        || role.contains("agent")
        || role.contains("model")
        || role.contains("ai")
        || role == "0"
        || role == "2"
    {
        "agent".to_string()
    } else if role.contains("tool") {
        "tool".to_string()
    } else {
        role
    }
}

/// `extract_text`: deepest reachable message text. Text fields,
/// `content`/`message`/`prompt`/`response` nests, arrays and embedded
/// JSON strings are walked; tool/metadata-typed objects never leak
/// their internals as text.
fn extract_text(value: &Value) -> Option<String> {
    extract_text_iter(value, true)
}

fn extract_text_iter(value: &Value, decode_embedded_json: bool) -> Option<String> {
    let mut pending = vec![value];
    let mut parts = Vec::new();
    while let Some(candidate) = pending.pop() {
        match candidate {
            Value::String(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                // A JSON-in-string payload yields its inner message text;
                // a tool/metadata-shaped decode does not count as text.
                let embedded = decode_embedded_json
                    .then(|| parse_embedded_json_text(trimmed))
                    .flatten()
                    .filter(|decoded| {
                        !decoded
                            .as_object()
                            .map(structured_object_is_tool_or_metadata)
                            .unwrap_or(false)
                    })
                    .and_then(|decoded| extract_text_iter(&decoded, false))
                    .filter(|text| !text.trim().is_empty());
                match embedded {
                    Some(text) => parts.push(text),
                    None => parts.push(text.to_string()),
                }
            }
            Value::Array(items) => pending.extend(items.iter().rev()),
            Value::Object(object) => {
                if structured_object_is_tool_or_metadata(object) {
                    continue;
                }
                let child = if structured_object_is_text(object) {
                    object
                        .get("text")
                        .filter(|value| value_may_have_text(value))
                        .or_else(|| object.get("content"))
                } else {
                    [
                        "text", "content", "message", "messages", "prompt", "response", "answer",
                        "summary", "value", "parts", "items", "turns",
                    ]
                    .iter()
                    .find_map(|key| object.get(*key).filter(|value| value_may_have_text(value)))
                };
                if let Some(child) = child {
                    pending.push(child);
                }
            }
            _ => {}
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn value_may_have_text(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(object) => !object.is_empty(),
        _ => false,
    }
}

fn structured_object_is_tool_or_metadata(object: &Map<String, Value>) -> bool {
    let kind = object
        .get("type")
        .or_else(|| object.get("kind"))
        .or_else(|| object.get("role"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        kind.as_str(),
        "tool_result"
            | "tool-use"
            | "tool_use"
            | "tool"
            | "function_call"
            | "function_call_output"
            | "input_json_delta"
            | "thinking"
            | "redacted_thinking"
            | "image"
            | "image_url"
            | "document"
            | "attachment"
            | "metadata"
            | "system"
    )
}

fn structured_object_is_text(object: &Map<String, Value>) -> bool {
    let kind = object
        .get("type")
        .or_else(|| object.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        kind.as_str(),
        "text" | "input_text" | "output_text" | "markdown"
    )
}

fn parse_embedded_json_text(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    let structured = (trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']'));
    if !structured {
        return None;
    }
    let value = serde_json::from_str::<Value>(trimmed).ok()?;
    embedded_json_may_hold_message_text(&value).then_some(value)
}

fn embedded_json_may_hold_message_text(value: &Value) -> bool {
    let mut pending = vec![value];
    while let Some(candidate) = pending.pop() {
        match candidate {
            Value::Array(items) => pending.extend(items),
            Value::Object(object) => {
                if object.keys().any(|key| {
                    matches!(
                        key.as_str(),
                        "text"
                            | "content"
                            | "message"
                            | "messages"
                            | "prompt"
                            | "response"
                            | "answer"
                            | "summary"
                            | "value"
                            | "parts"
                            | "items"
                            | "turns"
                    )
                }) {
                    return true;
                }
                pending.extend(object.values());
            }
            _ => {}
        }
    }
    false
}

/// `extract_timestamp`: RFC 3339 strings pass through; numbers and
/// numeric strings are epoch values (seconds, milliseconds or
/// microseconds by magnitude) converted via the `util` helpers. The
/// `message` payload is searched after the line itself.
fn timestamp_of(value: &Value) -> Option<String> {
    let mut candidate = value;
    loop {
        let object = candidate.as_object()?;
        for key in [
            "createdAt",
            "created_at",
            "timestamp",
            "ts",
            "time",
            "date",
            "updatedAt",
            "updated_at",
        ] {
            match object.get(key) {
                Some(Value::String(text)) => {
                    let trimmed = text.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(epoch) = trimmed.parse::<i64>() {
                        if let Some(converted) = epoch_to_rfc3339(epoch) {
                            return Some(converted);
                        }
                    }
                    return Some(text.clone());
                }
                Some(Value::Number(number)) => {
                    if let Some(epoch) = number.as_i64().and_then(epoch_to_rfc3339) {
                        return Some(epoch);
                    }
                }
                _ => {}
            }
        }
        candidate = object.get("message")?;
    }
}

/// `epoch_number_to_rfc3339`: the magnitude picks the unit —
/// ≥10¹⁵ microseconds, ≥10¹⁰ milliseconds, else seconds.
fn epoch_to_rfc3339(value: i64) -> Option<String> {
    if value <= 0 {
        return None;
    }
    let absolute = (value as i128).abs();
    let millis = if absolute >= 1_000_000_000_000_000 {
        value / 1_000
    } else if absolute >= 10_000_000_000 {
        value
    } else {
        value.checked_mul(1_000)?
    };
    u64::try_from(millis).ok().map(util::epoch_ms_rfc3339)
}

/// `extract_native_model`: a declared model field on the line or inside
/// its `message` payload.
fn native_model(value: &Value) -> Option<String> {
    find_string(
        value,
        &[
            "model",
            "modelId",
            "modelID",
            "model_id",
            "modelName",
            "model_name",
            "modelLabel",
            "model_label",
        ],
    )
    .filter(|model| !model.trim().is_empty())
}

/// `extract_conversation_title`: a document's declared title fields.
fn document_title(value: &Value) -> Option<String> {
    find_string(
        value,
        &[
            "thread_name",
            "threadName",
            "title",
            "name",
            "conversationTitle",
            "chatTitle",
            "sessionTitle",
            "summary",
        ],
    )
    .filter(|title| !title.trim().is_empty())
}

/// A recorded working directory: absolute, bounded, free of control
/// characters — the validity half of LicoUp's cwd read.
fn working_directory(value: &Value) -> Option<String> {
    let directory = ["cwd", "workingDirectory", "projectPath"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))?
        .trim();
    if directory.is_empty()
        || directory.len() > 4096
        || !Path::new(directory).is_absolute()
        || directory.chars().any(char::is_control)
    {
        return None;
    }
    Some(directory.to_string())
}

/// `extract_native_session_id` at record granularity: the session keys
/// a line may declare — never a bare `id`, which on a message line is
/// the message's own identity. Session-header lines may use `id`.
fn line_session_id(value: &Value) -> Option<String> {
    find_string(value, LINE_SESSION_KEYS)
        .filter(|id| !id.trim().is_empty())
        .or_else(|| {
            let semantic = normalize_semantic(&native_content_semantic(value));
            SESSION_HEADER_SEMANTICS
                .contains(&semantic.as_str())
                .then(|| find_string(value, &["id"]).filter(|id| !id.trim().is_empty()))
                .flatten()
        })
}

/// `native_session_id_or_layout` for a whole document: the session keys
/// plus the bare `id` a single-document session names itself with.
fn document_session_id(value: &Value) -> Option<String> {
    let mut keys = LINE_SESSION_KEYS.to_vec();
    keys.push("id");
    find_string(value, &keys).filter(|id| !id.trim().is_empty())
}

/// A line's declared record identity (`id`/`uuid`/`messageId`), when it
/// carries one — the `id:` selector stays usable on generic stores.
fn declared_record_id(value: &Value) -> Option<String> {
    find_string(value, &["id", "uuid", "messageId", "message_id"])
        .filter(|id| !id.trim().is_empty())
}

/// `is_conversation_uuid_component`: 36 chars, dashes at 8/13/18/23,
/// hex elsewhere.
fn is_conversation_uuid_component(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

/// The first usage counter object on the line or inside its `message`
/// payload. The raw object is preserved — counter semantics belong to
/// the `TranscriptUsage` consumer, not the codec.
fn usage_object(value: &Value) -> Option<Value> {
    for key in USAGE_KEYS {
        if let Some(usage) = value.get(*key).filter(|usage| usage.is_object()) {
            return Some(usage.clone());
        }
    }
    value.get("message").and_then(|message| {
        USAGE_KEYS
            .iter()
            .find_map(|key| message.get(*key).filter(|usage| usage.is_object()))
            .cloned()
    })
}

// --- text transcript helpers ------------------------------------------

enum RolePrefix {
    User,
    Assistant,
}

/// `user:`/`human:`/`prompt:`/`question:` open user records;
/// `assistant:`/`agent:`/`response:`/`answer:` open assistant records —
/// LicoUp's `looks_like_text_conversation` prefix set.
fn role_prefixed(line: &str) -> Option<RolePrefix> {
    let lower = line.to_ascii_lowercase();
    for prefix in ["user:", "human:", "prompt:", "question:"] {
        if lower.starts_with(prefix) {
            return Some(RolePrefix::User);
        }
    }
    for prefix in ["assistant:", "agent:", "response:", "answer:"] {
        if lower.starts_with(prefix) {
            return Some(RolePrefix::Assistant);
        }
    }
    None
}

/// The text after a `role:` prefix.
fn prefix_body(line: &str) -> &str {
    line.split_once(':')
        .map(|(_, body)| body.trim())
        .unwrap_or_default()
}

/// Append a continuation line to an open text record.
fn append_text(body: &mut Body, line: &str) {
    let text = match body {
        Body::User { text, .. } | Body::Assistant { text, .. } | Body::System { text, .. } => text,
        _ => return,
    };
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(line);
}

// --- sqlite probe -------------------------------------------------------

/// Whether a table name may hold conversation history — the generic
/// filter (rejects sqlite_*/fts/embedding/system tables; accepts
/// chat/conversation/session/history names and the VS Code `ItemTable`
/// key-value store).
pub fn sqlite_table_may_hold_history(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("sqlite") || lower.contains("fts") || lower.contains("embedding") {
        return false;
    }
    lower.contains("chat")
        || lower.contains("conversation")
        || lower.contains("session")
        || lower.contains("history")
        || lower == "itemtable"
}

/// Whether a row key/blob looks like a conversation payload: a key that
/// names a conversation artifact, or row text containing the vocabulary
/// transcripts always carry (roles, messages, prompts).
pub fn sqlite_row_may_hold_history(_table: &str, key: Option<&str>, text: &str) -> bool {
    let key = key.unwrap_or_default().to_ascii_lowercase();
    key.contains("chat")
        || key.contains("conversation")
        || key.contains("session")
        || key.contains("history")
        || key.contains("composer")
        || key.contains("aichat")
        || key.starts_with("bubbleid:")
        || key.starts_with("composerdata:")
        || looks_like_history_text(text)
}

/// `looks_like_history_text` — LicoUp's default row gate.
fn looks_like_history_text(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    lower.contains("assistant")
        || lower.contains("user")
        || lower.contains("prompt")
        || lower.contains("message")
        || lower.contains("conversation")
        || lower.contains("chat")
}

/// Session keys inside one sqlite database — each candidate row's
/// `<table>/<key-or-row-index>` selector, in table-then-row order.
/// Candidate rows pass both filters and yield at least one message, so
/// every selector here decodes to a non-empty conversation.
pub fn sqlite_session_keys(path: &Path) -> Result<Vec<String>> {
    Ok(probe_sqlite(path)?
        .into_iter()
        .map(|row| row.selector)
        .collect())
}

/// Decode one sqlite row's blob into a canonical conversation. `key`
/// is a selector from `sqlite_session_keys`; `None` decodes the first
/// qualifying row (single-session convenience for whole-file decode).
///
/// `record_lines` carries the index of the row field that held each
/// record's JSON payload; `line_count` is the row's kept-field count.
/// The session id is a declared id inside the blob when one exists,
/// else `<file-stem>:<table>:<key-or-index>`.
pub fn decode_sqlite(path: &Path, key: Option<&str>, agent: &str) -> Result<Decoded> {
    let rows = probe_sqlite(path)?;
    let row = match key {
        Some(selector) => rows
            .into_iter()
            .find(|row| row.selector == selector)
            .ok_or_else(|| {
                Error::Usage(format!(
                    "sqlite selector `{selector}` not found in `{}`",
                    path.display()
                ))
            })?,
        None => rows.into_iter().next().ok_or_else(|| Error::Decode {
            path: path.to_path_buf(),
            line: 0,
            message: "the database holds no conversation rows".to_string(),
        })?,
    };
    let mut conversation = Conversation::new(row.session_id);
    conversation.agent = Some(agent.to_string());
    conversation.title = row.title;
    conversation.cwd = row.cwd;
    conversation.created_at = row
        .created_at
        .or_else(|| row.bodies.first().and_then(|(_, ts, _)| ts.clone()));
    conversation.updated_at = row
        .bodies
        .last()
        .and_then(|(_, ts, _)| ts.clone())
        .or_else(|| conversation.created_at.clone());
    let mut record_lines = Vec::with_capacity(row.bodies.len());
    for (field_index, ts, body) in row.bodies {
        conversation.records.push(Record {
            id: None,
            parent: None,
            ts,
            ordinal: 0,
            body,
        });
        record_lines.push(field_index);
    }
    finalize(&mut conversation);
    Ok(Decoded {
        conversation,
        record_lines,
        line_count: row.field_count,
        skipped: Vec::new(),
    })
}

/// One probed row: its selector, decoded bodies and the document-level
/// facts its JSON payload declared.
struct ProbedRow {
    selector: String,
    session_id: String,
    field_count: usize,
    title: Option<String>,
    cwd: Option<String>,
    created_at: Option<String>,
    bodies: Vec<(usize, Option<String>, Body)>,
}

/// `parse_generic_sqlite_sessions`: open read-only, probe each
/// history-candidate table's rows, and keep rows whose text/blob
/// columns parse to conversation JSON — the row is skipped when no
/// field yields a message.
fn probe_sqlite(path: &Path) -> Result<Vec<ProbedRow>> {
    std::fs::metadata(path).map_err(Error::io(path))?;
    let connection = open_sqlite(path)?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let mut out = Vec::new();
    for table in sqlite_history_tables(&connection, path)? {
        let query = format!("SELECT * FROM \"{}\"", table.replace('"', "\"\""));
        let mut statement = connection.prepare(&query).map_err(|e| Error::Decode {
            path: path.to_path_buf(),
            line: 0,
            message: e.to_string(),
        })?;
        let column_names = statement
            .column_names()
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>();
        let mut rows = statement.query([]).map_err(|e| Error::Decode {
            path: path.to_path_buf(),
            line: 0,
            message: e.to_string(),
        })?;
        let mut index = 0usize;
        while let Some(row) = rows.next().map_err(|e| Error::Decode {
            path: path.to_path_buf(),
            line: 0,
            message: e.to_string(),
        })? {
            let row_index = index;
            index = index.saturating_add(1);
            let fields = sqlite_row_fields(row, &column_names).map_err(|e| Error::Decode {
                path: path.to_path_buf(),
                line: 0,
                message: e.to_string(),
            })?;
            let row_text = fields
                .iter()
                .map(|(name, value)| format!("{name}: {value}"))
                .collect::<Vec<_>>()
                .join("\n");
            let row_key = sqlite_row_key(&fields);
            if !sqlite_row_may_hold_history(&table, row_key.as_deref(), &row_text) {
                continue;
            }
            let mut bodies = Vec::<(usize, Option<String>, Body)>::new();
            let mut session = None;
            let mut title = None;
            let mut cwd = None;
            let mut created_at = None;
            for (field_index, (_, value)) in fields.iter().enumerate() {
                let Some(payload) = serde_json::from_str::<Value>(value)
                    .ok()
                    .or_else(|| extract_json_from_text(value))
                else {
                    continue;
                };
                if session.is_none() {
                    session = document_session_id(&payload);
                }
                if title.is_none() {
                    title = document_title(&payload);
                }
                if cwd.is_none() {
                    cwd = working_directory(&payload);
                }
                if created_at.is_none() {
                    created_at = timestamp_of(&payload);
                }
                let mut collected = Vec::<(usize, Option<String>, Body)>::new();
                collect_messages(&payload, &mut collected);
                bodies.extend(
                    collected
                        .into_iter()
                        .map(|(_, ts, body)| (field_index, ts, body)),
                );
            }
            if bodies.is_empty() {
                continue;
            }
            let key = row_key.unwrap_or_else(|| row_index.to_string());
            out.push(ProbedRow {
                selector: format!("{table}/{key}"),
                session_id: session.unwrap_or_else(|| format!("{stem}:{table}:{key}")),
                field_count: fields.len(),
                title,
                cwd,
                created_at,
                bodies,
            });
        }
    }
    Ok(out)
}

/// `open_read_only_connection`: read-only, no-mutex — the probe never
/// writes to a live store. Open failures (including `not a database`)
/// are decode failures.
fn open_sqlite(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| Error::Decode {
        path: path.to_path_buf(),
        line: 0,
        message: e.to_string(),
    })
}

/// History-candidate table names, in `sqlite_master` order.
fn sqlite_history_tables(connection: &Connection, path: &Path) -> Result<Vec<String>> {
    let mut statement = connection
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        .map_err(|e| Error::Decode {
            path: path.to_path_buf(),
            line: 0,
            message: e.to_string(),
        })?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| Error::Decode {
            path: path.to_path_buf(),
            line: 0,
            message: e.to_string(),
        })?;
    let mut names = Vec::new();
    for row in rows {
        let name = row.map_err(|e| Error::Decode {
            path: path.to_path_buf(),
            line: 0,
            message: e.to_string(),
        })?;
        if sqlite_table_may_hold_history(&name) {
            names.push(name);
        }
    }
    Ok(names)
}

/// `sqlite_row_fields`: `(column, text)` pairs for non-empty values —
/// integers/reals stringify, text/blob columns decode lossily.
fn sqlite_row_fields(
    row: &Row<'_>,
    column_names: &[String],
) -> rusqlite::Result<Vec<(String, String)>> {
    let mut fields = Vec::new();
    for (index, name) in column_names.iter().enumerate() {
        let Some(value) = sqlite_value_text(row.get_ref(index)?) else {
            continue;
        };
        if value.trim().is_empty() {
            continue;
        }
        fields.push((name.clone(), value));
    }
    Ok(fields)
}

/// `sqlite_row_key`: the row's declared key column when it has one.
fn sqlite_row_key(fields: &[(String, String)]) -> Option<String> {
    for preferred in [
        "key",
        "id",
        "sessionId",
        "session_id",
        "sessionKey",
        "session_key",
        "conversationId",
    ] {
        if let Some((_, value)) = fields
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(preferred))
        {
            if !value.trim().is_empty() {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

/// `sqlite_value_text`: one column's printable form — blob and text
/// columns alike read as lossy UTF-8 so JSON blobs survive either type.
fn sqlite_value_text(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Null => Some(String::new()),
        ValueRef::Integer(value) => Some(value.to_string()),
        ValueRef::Real(value) => Some(value.to_string()),
        ValueRef::Text(value) | ValueRef::Blob(value) => {
            Some(String::from_utf8_lossy(value).into_owned())
        }
    }
}

/// `extract_json_from_text`: the first `{`/`[` on any line that parses
/// as JSON wins — blobs prefixed with text headers still decode.
fn extract_json_from_text(text: &str) -> Option<Value> {
    for part in text.split('\n') {
        let trimmed = part.trim();
        if let Some(start) = trimmed.find('{') {
            if let Ok(value) = serde_json::from_str::<Value>(&trimmed[start..]) {
                return Some(value);
            }
        }
        if let Some(start) = trimmed.find('[') {
            if let Ok(value) = serde_json::from_str::<Value>(&trimmed[start..]) {
                return Some(value);
            }
        }
    }
    None
}

// --- encode helpers ----------------------------------------------------

/// Emit one assistant `message` line for a run of assistant-side
/// records: text/thinking/toolCall become `content` blocks, a queued
/// `meta{usage}` lands on the message's `usage` field.
fn flush_assistant(pending: &mut Vec<&Record>, usage: &mut Option<Value>, lines: &mut Vec<String>) {
    if pending.is_empty() {
        return;
    }
    let model = pending.iter().find_map(|rec| match &rec.body {
        Body::Assistant {
            model: Some(model), ..
        } => Some(model.clone()),
        _ => None,
    });
    let blocks: Vec<Value> = pending
        .iter()
        .filter_map(|rec| match &rec.body {
            Body::Assistant { text, .. } => Some(json!({"type": "text", "text": text})),
            Body::Reasoning { text } => Some(json!({"type": "thinking", "thinking": text})),
            Body::ToolCall {
                call_id,
                name,
                args,
            } => Some(json!({
                "type": "toolCall",
                "id": call_id,
                "name": name,
                "arguments": args,
            })),
            _ => None,
        })
        .collect();
    lines.push(
        json!({
            "type": "message",
            "timestamp": pending.first().and_then(|rec| rec.ts.clone()),
            "message": {
                "role": "assistant",
                "model": model,
                "content": blocks,
                "usage": usage.take(),
            },
        })
        .to_string(),
    );
    pending.clear();
}

/// A usage payload with no assistant message to ride on still lands in
/// message shape — an empty-content assistant line the decoder reads
/// back as `meta{usage}`.
fn usage_line(record: &Record, usage: &Value) -> String {
    json!({
        "type": "message",
        "timestamp": record.ts,
        "message": {"role": "assistant", "content": [], "usage": usage},
    })
    .to_string()
}

// --- redact helpers ----------------------------------------------------

/// Fields whose string value is message text; `message` joins the set
/// for formats that carry it as a bare string (objects recurse
/// through the generic walk instead).
const REDACT_TEXT_FIELDS: &[&str] = &[
    "text",
    "thinking",
    "content",
    "message",
    "prompt",
    "response",
    "answer",
    "summary",
    "reasoning",
    "output",
    "result",
];

/// Tool-call argument fields — an arguments payload is conversation
/// content too (file bodies, commands), so it is replaced wholesale.
const REDACT_ARGS_FIELDS: &[&str] = &["input", "arguments", "args"];

fn redact_value(value: &mut Value, marker: &str) -> bool {
    match value {
        Value::Object(object) => {
            let mut touched = false;
            for (key, field) in object.iter_mut() {
                let textual = REDACT_TEXT_FIELDS.contains(&key.as_str()) && field.is_string();
                let arguments = REDACT_ARGS_FIELDS.contains(&key.as_str()) && !field.is_null();
                if textual || arguments {
                    *field = Value::String(marker.to_string());
                    touched = true;
                } else {
                    touched |= redact_value(field, marker);
                }
            }
            touched
        }
        Value::Array(items) => {
            let mut touched = false;
            for item in items.iter_mut() {
                touched |= redact_value(item, marker);
            }
            touched
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RecordKind;
    use std::path::PathBuf;

    fn write_temp(name: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("aco-generic-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    const JSONL: &str = concat!(
        // session header: declares identity, cwd and a title-ish field.
        r#"{"type":"session","id":"sess-42","cwd":"/synthetic/proj","timestamp":"2026-01-01T10:00:00Z"}"#,
        "\n",
        // plain role/text message with an epoch-millis timestamp.
        r#"{"role":"user","text":"hello agent","timestamp":1767261601000}"#,
        "\n",
        // assistant message: content blocks fan out thinking+text+toolCall, usage rides along.
        r#"{"type":"message","timestamp":"2026-01-01T10:00:02Z","message":{"role":"assistant","model":"synthetic-model","content":[{"type":"thinking","thinking":"pondering"},{"type":"text","text":"working on it"},{"type":"toolCall","id":"call-1","name":"fake_tool","arguments":{"arg":1}}],"usage":{"promptTokens":10,"completionTokens":5}}}"#,
        "\n",
        // toolResult-shaped message.
        r#"{"type":"message","timestamp":"2026-01-01T10:00:03Z","message":{"role":"toolResult","toolCallId":"call-1","content":"done","isError":false}}"#,
        "\n",
        // a typed native event → a system record.
        r#"{"type":"telemetry","timestamp":"2026-01-01T10:00:04Z","payload":{"x":1}}"#,
        "\n",
        // a message with no text or content → projection skip.
        r#"{"type":"message","message":{"role":"assistant"}}"#,
        "\n",
        // corrupt line.
        "not json at all\n",
        // a different session's line → projection skip.
        r#"{"sessionId":"sess-99","role":"user","text":"foreign session"}"#,
        "\n"
    );

    #[test]
    fn jsonl_decodes_mixed_shapes_with_blocks_usage_and_skips() {
        let path = write_temp("mixed.jsonl", JSONL);
        let decoded = decode_jsonl(&path, "synthetic").unwrap();
        let conv = &decoded.conversation;

        assert_eq!(decoded.line_count, 8);
        assert_eq!(conv.session_id, "sess-42");
        assert_eq!(conv.agent.as_deref(), Some("synthetic"));
        assert_eq!(conv.cwd.as_deref(), Some("/synthetic/proj"));
        assert_eq!(conv.created_at.as_deref(), Some("2026-01-01T10:00:00Z"));
        assert_eq!(conv.updated_at.as_deref(), Some("2026-01-01T10:00:04Z"));

        let kinds: Vec<RecordKind> = conv.records.iter().map(Record::kind).collect();
        assert_eq!(
            kinds,
            vec![
                RecordKind::System,     // the {"type":"session"} header event
                RecordKind::User,       // plain role/text line
                RecordKind::Reasoning,  // thinking block
                RecordKind::Assistant,  // text block
                RecordKind::ToolCall,   // toolCall block
                RecordKind::Meta,       // usage on the assistant line
                RecordKind::ToolResult, // toolResult message
                RecordKind::System      // the telemetry event
            ]
        );
        assert_eq!(decoded.record_lines, vec![0, 1, 2, 2, 2, 2, 3, 4]);

        let skipped: Vec<(usize, Option<&str>, &str)> = decoded
            .skipped
            .iter()
            .map(|s| (s.line, s.native_type.as_deref(), s.reason.as_str()))
            .collect();
        assert_eq!(
            skipped,
            vec![
                (5, Some("message"), "projection"),
                (6, None, "corrupt"),
                (7, Some("user"), "projection"),
            ]
        );

        // Epoch-millis timestamps convert to RFC 3339.
        assert_eq!(
            conv.records[1].ts.as_deref(),
            Some("2026-01-01T10:00:01.000Z")
        );
        match &conv.records[4].body {
            Body::ToolCall {
                call_id,
                name,
                args,
            } => {
                assert_eq!(call_id.as_deref(), Some("call-1"));
                assert_eq!(name, "fake_tool");
                assert_eq!(args["arg"], 1);
            }
            other => panic!("expected tool call, got {other:?}"),
        }
        match &conv.records[5].body {
            Body::Meta { subtype, data } => {
                assert_eq!(subtype, "usage");
                assert_eq!(data["promptTokens"], 10);
            }
            other => panic!("expected usage meta, got {other:?}"),
        }
        match &conv.records[6].body {
            Body::ToolResult {
                call_id, content, ..
            } => {
                assert_eq!(call_id.as_deref(), Some("call-1"));
                assert_eq!(content, "done");
            }
            other => panic!("expected tool result, got {other:?}"),
        }
        // Ordinals are assigned in record order.
        for (i, rec) in conv.records.iter().enumerate() {
            assert_eq!(rec.ordinal, i as u64);
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn json_document_decodes_a_messages_object() {
        let doc = serde_json::json!({
            "id": "doc-session",
            "title": "synthetic conversation",
            "cwd": "/synthetic/doc",
            "createdAt": "2026-02-01T09:00:00Z",
            "messages": [
                {"role": "user", "content": "question one"},
                {"role": "assistant", "content": [{"type": "text", "text": "answer one"}]}
            ]
        });
        let path = write_temp("doc.json", &doc.to_string());
        let decoded = decode_json_document(&path, "synthetic").unwrap();
        let conv = &decoded.conversation;

        assert_eq!(conv.session_id, "doc-session");
        assert_eq!(conv.title.as_deref(), Some("synthetic conversation"));
        assert_eq!(conv.cwd.as_deref(), Some("/synthetic/doc"));
        assert_eq!(conv.created_at.as_deref(), Some("2026-02-01T09:00:00Z"));
        let kinds: Vec<RecordKind> = conv.records.iter().map(Record::kind).collect();
        assert_eq!(kinds, vec![RecordKind::User, RecordKind::Assistant]);
        assert_eq!(decoded.record_lines, vec![0, 1]);
        match &conv.records[0].body {
            Body::User { text, .. } => assert_eq!(text, "question one"),
            other => panic!("expected user, got {other:?}"),
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn json_document_decodes_a_bare_array() {
        let doc = serde_json::json!([
            {"role": "user", "text": "hi"},
            {"role": "agent", "text": "hello", "model": "m-1"},
            {"usage": {"totalTokens": 3}}
        ]);
        let path = write_temp("array.json", &doc.to_string());
        let decoded = decode_json_document(&path, "synthetic").unwrap();
        let kinds: Vec<RecordKind> = decoded
            .conversation
            .records
            .iter()
            .map(Record::kind)
            .collect();
        assert_eq!(
            kinds,
            vec![RecordKind::User, RecordKind::Assistant, RecordKind::Meta]
        );
        assert_eq!(decoded.record_lines, vec![0, 1, 2]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn text_decodes_role_lines_and_paragraphs() {
        let raw = "# Synthetic transcript\n\nuser: first question\ncontinued detail\n\nassistant: an answer\n\nloose paragraph\n\nprompt: second question\nanswer: second answer\n";
        let path = write_temp("notes.md", raw);
        let decoded = decode_text(&path, "synthetic").unwrap();
        let conv = &decoded.conversation;

        assert_eq!(conv.title.as_deref(), Some("Synthetic transcript"));
        let kinds: Vec<RecordKind> = conv.records.iter().map(Record::kind).collect();
        assert_eq!(
            kinds,
            vec![
                RecordKind::User,
                RecordKind::Assistant,
                RecordKind::System,
                RecordKind::User,
                RecordKind::Assistant,
            ]
        );
        assert_eq!(decoded.record_lines, vec![2, 5, 7, 9, 10]);
        match &conv.records[0].body {
            Body::User { text, .. } => assert_eq!(text, "first question\ncontinued detail"),
            other => panic!("expected user, got {other:?}"),
        }
        match &conv.records[2].body {
            Body::System { subtype, text } => {
                assert_eq!(subtype, "transcript");
                assert_eq!(text, "loose paragraph");
            }
            other => panic!("expected transcript paragraph, got {other:?}"),
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn dispatch_picks_by_extension_and_rejects_the_rest() {
        let jsonl = write_temp("dispatch.ndjson", r#"{"role":"user","text":"x"}"#);
        let json = write_temp("dispatch.json", r#"[{"role":"user","text":"x"}]"#);
        let text = write_temp("dispatch.log", "user: x\n");
        for path in [&jsonl, &json, &text] {
            let decoded = dispatch(path, "synthetic").unwrap();
            assert_eq!(decoded.conversation.records.len(), 1);
            assert_eq!(
                decoded.conversation.records[0].kind(),
                RecordKind::User,
                "{path:?}"
            );
        }
        let bogus = write_temp("dispatch.bin", "garbage");
        assert!(matches!(
            dispatch(&bogus, "synthetic"),
            Err(Error::Usage(_))
        ));
        assert!(accepts("jsonl") && accepts("ndjson") && accepts("json"));
        assert!(accepts("md") && accepts("markdown") && accepts("txt") && accepts("log"));
        assert!(!accepts("sqlite") && !accepts("BIN".to_ascii_lowercase().as_str()));
        for path in [&jsonl, &json, &text, &bogus] {
            std::fs::remove_file(path).ok();
        }
    }

    #[test]
    fn session_id_prefers_uuid_directories_then_stem() {
        let uuid_dir =
            Path::new("/store/brain/86990c82-fd1b-4f55-b291-e7b3c6d3d093/transcript.jsonl");
        assert_eq!(session_id(uuid_dir), "86990c82-fd1b-4f55-b291-e7b3c6d3d093");
        assert_eq!(session_id(Path::new("/store/flat/notes.jsonl")), "notes");
    }

    #[test]
    fn redact_line_rewrites_textual_fields_only() {
        let line = serde_json::json!({
            "type": "message",
            "message": {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "secret thought"},
                {"type": "text", "text": "secret text"},
                {"type": "toolCall", "id": "c1", "name": "t", "arguments": {"cmd": "rm"}},
            ]},
        });
        let redacted = redact_line(&line, "<redacted>").unwrap();
        let content = &redacted["message"]["content"];
        assert_eq!(content[0]["thinking"], "<redacted>");
        assert_eq!(content[1]["text"], "<redacted>");
        assert_eq!(content[2]["arguments"], "<redacted>");
        assert_eq!(content[2]["name"], "t");
        assert_eq!(content[2]["id"], "c1");

        // String `content` and `text` fields anywhere are covered.
        let flat = redact_line(
            &serde_json::json!({"role": "user", "content": "secret"}),
            "<redacted>",
        )
        .unwrap();
        assert_eq!(flat["content"], "<redacted>");

        // No text-bearing fields → untouched per contract.
        assert!(redact_line(
            &serde_json::json!({"type": "session", "id": "s1", "cwd": "/p"}),
            "<redacted>"
        )
        .is_none());
    }

    #[test]
    fn encode_jsonl_round_trips_record_kinds() {
        let mut conv = Conversation::new("sess-rt");
        conv.agent = Some("synthetic".into());
        conv.records = vec![
            Record {
                id: None,
                parent: None,
                ts: Some("2026-01-01T10:00:00Z".into()),
                ordinal: 0,
                body: Body::User {
                    text: "question".into(),
                    attachments: Vec::new(),
                },
            },
            Record {
                id: None,
                parent: None,
                ts: Some("2026-01-01T10:00:01Z".into()),
                ordinal: 1,
                body: Body::Reasoning {
                    text: "thinking".into(),
                },
            },
            Record {
                id: None,
                parent: None,
                ts: Some("2026-01-01T10:00:01Z".into()),
                ordinal: 2,
                body: Body::Assistant {
                    text: "answer".into(),
                    model: Some("m-1".into()),
                },
            },
            Record {
                id: None,
                parent: None,
                ts: Some("2026-01-01T10:00:01Z".into()),
                ordinal: 3,
                body: Body::ToolCall {
                    call_id: Some("c1".into()),
                    name: "fake_tool".into(),
                    args: serde_json::json!({"a": 1}),
                },
            },
            Record {
                id: None,
                parent: None,
                ts: Some("2026-01-01T10:00:01Z".into()),
                ordinal: 4,
                body: Body::Meta {
                    subtype: "usage".into(),
                    data: serde_json::json!({"totalTokens": 9}),
                },
            },
            Record {
                id: None,
                parent: None,
                ts: Some("2026-01-01T10:00:02Z".into()),
                ordinal: 5,
                body: Body::ToolResult {
                    call_id: Some("c1".into()),
                    name: Some("fake_tool".into()),
                    content: serde_json::json!("done"),
                    is_error: false,
                },
            },
            Record {
                id: None,
                parent: None,
                ts: Some("2026-01-01T10:00:03Z".into()),
                ordinal: 6,
                body: Body::System {
                    subtype: "mode".into(),
                    text: "mode change".into(),
                },
            },
            Record {
                id: None,
                parent: None,
                ts: None,
                ordinal: 7,
                body: Body::Meta {
                    subtype: "session-index".into(),
                    data: serde_json::json!({"k": 1}),
                },
            },
        ];

        let encoded = encode_jsonl(&conv).unwrap();
        // system is adapted; usage meta is adapted; the other meta drops.
        assert_eq!(encoded.projection.dropped, vec![(7, RecordKind::Meta)]);
        assert_eq!(
            encoded.projection.adapted,
            vec![(4, RecordKind::Meta), (6, RecordKind::System)]
        );

        let path = write_temp("roundtrip.jsonl", &encoded.lines.join("\n"));
        let back = decode_jsonl(&path, "synthetic").unwrap();
        let kinds: Vec<RecordKind> = back.conversation.records.iter().map(Record::kind).collect();
        assert_eq!(
            kinds,
            vec![
                RecordKind::User,
                RecordKind::Reasoning,
                RecordKind::Assistant,
                RecordKind::ToolCall,
                RecordKind::Meta,
                RecordKind::ToolResult,
                RecordKind::System,
            ]
        );
        // The folded assistant message carries model and usage back out.
        match &back.conversation.records[2].body {
            Body::Assistant { text, model } => {
                assert_eq!(text, "answer");
                assert_eq!(model.as_deref(), Some("m-1"));
            }
            other => panic!("expected assistant, got {other:?}"),
        }
        match &back.conversation.records[4].body {
            Body::Meta { subtype, data } => {
                assert_eq!(subtype, "usage");
                assert_eq!(data["totalTokens"], 9);
            }
            other => panic!("expected usage meta, got {other:?}"),
        }
        std::fs::remove_file(&path).ok();
    }

    /// Build a synthetic sqlite store: history-holding tables beside
    /// filtered ones, rows with key/blob JSON payloads, a key-less blob
    /// table, and rows the row filter must reject.
    fn sqlite_fixture(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("fixture.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE chat_fts (x TEXT);
             INSERT INTO chat_fts VALUES ('{\"messages\":[{\"role\":\"user\",\"text\":\"fts row\"}]}');
             CREATE TABLE chat_sessions (key TEXT, value TEXT);
             CREATE TABLE session_data (id TEXT, blob TEXT);
             CREATE TABLE chat_blobs (data TEXT);
             CREATE TABLE metrics (k TEXT, v TEXT);
             INSERT INTO metrics VALUES ('uptime', '42');",
        )
        .unwrap();
        let conv = serde_json::json!({
            "sessionId": "declared-7",
            "title": "sqlite conversation",
            "messages": [
                {"role": "user", "text": "question", "createdAt": "2026-03-01T08:00:00Z"},
                {"role": "agent", "text": "answer", "model": "m-2"}
            ]
        });
        conn.execute(
            "INSERT INTO chat_sessions VALUES ('s1', ?1)",
            [conv.to_string()],
        )
        .unwrap();
        // Passes the table filter but carries no conversation payload.
        conn.execute(
            "INSERT INTO chat_sessions VALUES ('s2', '{\"unrelated\":1}')",
            [],
        )
        .unwrap();
        // A bare messages-array blob; declared id wins over stem:table:key.
        conn.execute(
            "INSERT INTO session_data VALUES ('d1', ?1)",
            [serde_json::json!({
                "messages": [{"role": "assistant", "content": [{"type": "text", "text": "blob reply"}]}]
            })
            .to_string()],
        )
        .unwrap();
        // Key-less row → `table/<row-index>` selector; the blob sits in a
        // text field prefixed by a non-JSON header line.
        conn.execute(
            "INSERT INTO chat_blobs VALUES ('header garbage\n{\"role\":\"user\",\"text\":\"loose\"}')",
            [],
        )
        .unwrap();
        drop(conn);
        path
    }

    #[test]
    fn sqlite_keys_follow_table_then_row_order_and_filters_hold() {
        let dir = tempfile::tempdir().unwrap();
        let path = sqlite_fixture(dir.path());
        let keys = sqlite_session_keys(&path).unwrap();
        assert_eq!(
            keys,
            vec![
                "chat_sessions/s1".to_string(),
                "session_data/d1".to_string(),
                "chat_blobs/0".to_string(),
            ]
        );

        // Table filter: index/system shapes are refused by name.
        assert!(!sqlite_table_may_hold_history("chat_fts"));
        assert!(!sqlite_table_may_hold_history("sqlite_sequence"));
        assert!(!sqlite_table_may_hold_history("vec_embeddings"));
        assert!(!sqlite_table_may_hold_history("metrics"));
        assert!(sqlite_table_may_hold_history("ItemTable"));
        assert!(sqlite_table_may_hold_history("chat_sessions"));

        // Row filter: conversation payloads pass, stray rows do not.
        assert!(sqlite_row_may_hold_history(
            "chat_sessions",
            Some("composerData:abc"),
            "key: composerData:abc"
        ));
        assert!(sqlite_row_may_hold_history(
            "session_data",
            Some("d1"),
            "value: {\"messages\":[{\"role\":\"user\"}]}"
        ));
        assert!(!sqlite_row_may_hold_history(
            "chat_sessions",
            Some("s2"),
            "key: s2\nvalue: {\"unrelated\":1}"
        ));
    }

    #[test]
    fn sqlite_decodes_a_row_into_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = sqlite_fixture(dir.path());
        let decoded = decode_sqlite(&path, Some("chat_sessions/s1"), "synthetic").unwrap();
        let conv = &decoded.conversation;

        // Declared id inside the blob wins over stem:table:key.
        assert_eq!(conv.session_id, "declared-7");
        assert_eq!(conv.agent.as_deref(), Some("synthetic"));
        assert_eq!(conv.title.as_deref(), Some("sqlite conversation"));
        assert_eq!(conv.created_at.as_deref(), Some("2026-03-01T08:00:00Z"));

        let kinds: Vec<RecordKind> = conv.records.iter().map(Record::kind).collect();
        assert_eq!(kinds, vec![RecordKind::User, RecordKind::Assistant]);
        // Both records came from the `value` column — field index 1.
        assert_eq!(decoded.record_lines, vec![1, 1]);
        assert_eq!(decoded.line_count, 2);
        match &conv.records[1].body {
            Body::Assistant { text, model } => {
                assert_eq!(text, "answer");
                assert_eq!(model.as_deref(), Some("m-2"));
            }
            other => panic!("expected assistant, got {other:?}"),
        }
    }

    #[test]
    fn sqlite_selector_round_trip_and_default_decode() {
        let dir = tempfile::tempdir().unwrap();
        let path = sqlite_fixture(dir.path());
        // `None` decodes the first qualifying row.
        let first = decode_sqlite(&path, None, "synthetic").unwrap();
        assert_eq!(first.conversation.session_id, "declared-7");

        // Every enumerated key resolves to a non-empty conversation.
        for key in sqlite_session_keys(&path).unwrap() {
            let decoded = decode_sqlite(&path, Some(&key), "synthetic").unwrap();
            assert!(
                !decoded.conversation.records.is_empty(),
                "selector {key} decoded empty"
            );
        }

        // The key-less blob table decodes under its row-index selector.
        let blob = decode_sqlite(&path, Some("chat_blobs/0"), "synthetic").unwrap();
        assert_eq!(blob.conversation.session_id, "fixture:chat_blobs:0");
        match &blob.conversation.records[0].body {
            Body::User { text, .. } => assert_eq!(text, "loose"),
            other => panic!("expected user, got {other:?}"),
        }

        // Bare-array blob lands the stem:table:key fallback session id.
        let array = decode_sqlite(&path, Some("session_data/d1"), "synthetic").unwrap();
        assert_eq!(array.conversation.session_id, "fixture:session_data:d1");
        assert_eq!(array.conversation.records[0].kind(), RecordKind::Assistant);

        // Unknown selector → Usage; unreadable and non-sqlite files error.
        assert!(matches!(
            decode_sqlite(&path, Some("chat_sessions/nope"), "synthetic"),
            Err(Error::Usage(_))
        ));
        let not_db = dir.path().join("not-a-db.sqlite");
        std::fs::write(&not_db, "plain text").unwrap();
        assert!(matches!(
            decode_sqlite(&not_db, None, "synthetic"),
            Err(Error::Decode { .. })
        ));
        let missing = dir.path().join("missing.db");
        assert!(matches!(
            decode_sqlite(&missing, None, "synthetic"),
            Err(Error::Io { .. })
        ));
    }
}
