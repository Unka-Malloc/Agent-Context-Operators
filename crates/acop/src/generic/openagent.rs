//! The OpenAgent-shaped store family (`session`/`message`/`part`
//! tables) shared by OpenCode, Kilo Code and Cursor's bundled agent
//! databases. One [`SessionRef`] per `session` row; decoding projects
//! each `message.data` envelope through its `part` rows — the same
//! container semantics every codec reuses instead of reimplementing.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::codec::{Decoded, SessionRef, Skipped};
use crate::model::{Body, Conversation, Record};
use crate::Result;
pub fn open_read_only(path: &Path) -> Option<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

fn table_exists(connection: &Connection, table: &str) -> bool {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1 LIMIT 1",
            [table],
            |_| Ok(()),
        )
        .is_ok()
}

fn table_columns(
    connection: &Connection,
    table: &str,
) -> std::result::Result<std::collections::HashSet<String>, rusqlite::Error> {
    // Table names reaching this helper are compile-time constants or
    // schema-derived (never user input).
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let collected = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect();
    collected
}

fn sqlite_value_text(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Null => Some(String::new()),
        ValueRef::Integer(value) => Some(value.to_string()),
        ValueRef::Real(value) => Some(value.to_string()),
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
            Some(String::from_utf8_lossy(bytes).into_owned())
        }
    }
}

pub fn schema(connection: &Connection) -> bool {
    ["session", "message", "part"]
        .iter()
        .all(|table| table_exists(connection, table))
}

pub fn session_refs(connection: &Connection, db: &Path) -> Option<Vec<SessionRef>> {
    if !schema(connection) {
        return None;
    }
    let columns = table_columns(connection, "session").ok()?;
    if !columns.contains("id") {
        return None;
    }
    let order = if columns.contains("time_updated") {
        "ORDER BY time_updated DESC, id ASC"
    } else {
        "ORDER BY id ASC"
    };
    let parent = if columns.contains("parent_id") {
        "parent_id"
    } else {
        "NULL"
    };
    let mut statement = connection
        .prepare(&format!("SELECT id, {parent} FROM session {order}"))
        .ok()?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                sqlite_value_text(row.get_ref(0)?),
                sqlite_value_text(row.get_ref(1)?),
            ))
        })
        .ok()?;
    let mut refs = Vec::new();
    for row in rows {
        // Row errors abort enumeration like the source reader; the
        // caller then falls back to the generic probe.
        let (id, parent) = row.ok()?;
        let Some(id) = id.filter(|id| !id.trim().is_empty()) else {
            continue;
        };
        let parent = parent.filter(|parent| !parent.trim().is_empty());
        // A `session` row with no `message` rows is not a conversation —
        // the source reader filters it out of enumeration.
        let has_messages = table_columns(connection, "message")
            .map(|columns| columns.contains("session_id"))
            .unwrap_or(false)
            && connection
                .query_row(
                    "SELECT 1 FROM message WHERE session_id=?1 LIMIT 1",
                    [id.as_str()],
                    |_| Ok(()),
                )
                .is_ok();
        if !has_messages {
            continue;
        }
        refs.push(SessionRef {
            path: db.to_path_buf(),
            key: Some(id.clone()),
            id,
            parent,
        });
    }
    Some(refs)
}

/// Session-row metadata the projection can use when the schema happens
/// to carry it. `id` is the only required column; narrower Kilo builds
/// lack `agent`, `model` and the token columns, which project as `NULL`.
const OPENAGENT_SESSION_COLUMNS: [&str; 15] = [
    "id",
    "title",
    "directory",
    "path",
    "agent",
    "model",
    "time_created",
    "time_updated",
    "tokens_input",
    "tokens_output",
    "tokens_reasoning",
    "tokens_cache_read",
    "tokens_cache_write",
    "parent_id",
    "time_archived",
];

struct SessionMeta {
    id: String,
    title: Option<String>,
    directory: Option<String>,
    path: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
    usage: Option<Value>,
    parent_id: Option<String>,
    archived: bool,
}

fn openagent_session_meta(connection: &Connection, session_id: &str) -> Option<SessionMeta> {
    let columns = table_columns(connection, "session").ok()?;
    if !columns.contains("id") {
        return None;
    }
    let projection = OPENAGENT_SESSION_COLUMNS
        .iter()
        .map(|column| {
            if columns.contains(*column) {
                format!("\"{column}\"")
            } else {
                "NULL".to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    connection
        .query_row(
            &format!("SELECT {projection} FROM session WHERE id=?1"),
            [session_id],
            |row| {
                Ok(SessionMeta {
                    id: sqlite_value_text(row.get_ref(0)?).unwrap_or_default(),
                    title: sqlite_value_text(row.get_ref(1)?)
                        .filter(|value| !value.trim().is_empty()),
                    directory: sqlite_value_text(row.get_ref(2)?)
                        .as_deref()
                        .and_then(bounded_workspace),
                    path: sqlite_value_text(row.get_ref(3)?),
                    agent: sqlite_value_text(row.get_ref(4)?),
                    model: sqlite_value_text(row.get_ref(5)?),
                    created_at: row
                        .get::<_, Option<i64>>(6)?
                        .and_then(epoch_number_to_rfc3339),
                    updated_at: row
                        .get::<_, Option<i64>>(7)?
                        .and_then(epoch_number_to_rfc3339),
                    usage: session_usage(
                        row.get::<_, Option<i64>>(8)?,
                        row.get::<_, Option<i64>>(9)?,
                        row.get::<_, Option<i64>>(10)?,
                        row.get::<_, Option<i64>>(11)?,
                        row.get::<_, Option<i64>>(12)?,
                    ),
                    parent_id: sqlite_value_text(row.get_ref(13)?)
                        .filter(|value| !value.trim().is_empty()),
                    archived: row
                        .get::<_, Option<i64>>(14)?
                        .map(|value| value != 0)
                        .unwrap_or(false),
                })
            },
        )
        .ok()
}

/// One `message` row: id, column timestamps, and the `data` JSON blob.
struct MessageRow {
    id: String,
    created_ms: Option<i64>,
    updated_ms: Option<i64>,
    data: Value,
}

fn openagent_messages(connection: &Connection, session_id: &str) -> Vec<MessageRow> {
    let Ok(columns) = table_columns(connection, "message") else {
        return Vec::new();
    };
    if !["id", "session_id", "data"]
        .iter()
        .all(|column| columns.contains(*column))
    {
        return Vec::new();
    }
    let created = if columns.contains("time_created") {
        "time_created"
    } else {
        "NULL"
    };
    let updated = if columns.contains("time_updated") {
        "time_updated"
    } else {
        "NULL"
    };
    let order = if columns.contains("time_created") {
        "time_created ASC, id ASC"
    } else {
        "id ASC"
    };
    let query = format!(
        "SELECT id, {created}, {updated}, data FROM message WHERE session_id=?1 ORDER BY {order}"
    );
    let mut statement = match connection.prepare(&query) {
        Ok(statement) => statement,
        Err(_) => return Vec::new(),
    };
    let rows = match statement.query_map([session_id], |row| {
        Ok((
            sqlite_value_text(row.get_ref(0)?),
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<i64>>(2)?,
            sqlite_value_text(row.get_ref(3)?),
        ))
    }) {
        Ok(rows) => rows,
        Err(_) => return Vec::new(),
    };
    let mut messages = Vec::new();
    for row in rows {
        // Same contract as the source reader: one malformed row aborts
        // the session's message list rather than silently slicing it.
        let Ok((id, created_ms, updated_ms, data)) = row else {
            return Vec::new();
        };
        let (Some(id), Some(data)) = (id, data) else {
            continue;
        };
        let data = serde_json::from_str::<Value>(&data).unwrap_or(Value::Null);
        messages.push(MessageRow {
            id,
            created_ms,
            updated_ms,
            data,
        });
    }
    messages
}

/// `part` rows grouped by `message_id`, ordered `time_created ASC, id
/// ASC` (narrowing gracefully on absent columns) — the content blocks
/// one message unfolds into.
fn openagent_parts(connection: &Connection, session_id: &str) -> HashMap<String, Vec<Value>> {
    let mut out = HashMap::new();
    let Ok(columns) = table_columns(connection, "part") else {
        return out;
    };
    if !["message_id", "session_id", "data"]
        .iter()
        .all(|column| columns.contains(*column))
    {
        return out;
    }
    let order = match (columns.contains("time_created"), columns.contains("id")) {
        (true, true) => "time_created ASC, id ASC",
        (true, false) => "time_created ASC",
        (false, true) => "id ASC",
        (false, false) => "rowid ASC",
    };
    let query = format!("SELECT message_id, data FROM part WHERE session_id=?1 ORDER BY {order}");
    let mut statement = match connection.prepare(&query) {
        Ok(statement) => statement,
        Err(_) => return out,
    };
    let rows = match statement.query_map([session_id], |row| {
        Ok((
            sqlite_value_text(row.get_ref(0)?),
            sqlite_value_text(row.get_ref(1)?),
        ))
    }) {
        Ok(rows) => rows,
        Err(_) => return out,
    };
    for row in rows {
        let Ok((message_id, data)) = row else {
            return out;
        };
        let (Some(message_id), Some(data)) = (message_id, data) else {
            continue;
        };
        let value = serde_json::from_str::<Value>(&data).unwrap_or_else(|_| Value::String(data));
        out.entry(message_id).or_insert_with(Vec::new).push(value);
    }
    out
}

pub fn decode(
    connection: &Connection,
    _db: &Path,
    session_id: &str,
    agent: &str,
) -> Result<Option<Decoded>> {
    if !schema(connection) {
        return Ok(None);
    }
    let Some(meta) = openagent_session_meta(connection, session_id) else {
        return Ok(None);
    };
    let messages = openagent_messages(connection, &meta.id);
    let mut parts = openagent_parts(connection, &meta.id);
    let title = meta.title.clone();
    let mut conv = Conversation::new(meta.id.clone());
    conv.agent = Some(agent.to_string());
    conv.title = meta.title;
    conv.cwd = meta.directory.clone().or_else(|| meta.path.clone());
    conv.created_at = meta.created_at;
    conv.updated_at = meta.updated_at;

    let mut records = Vec::new();
    let mut record_lines = Vec::new();
    let mut skipped = Vec::new();
    let line_count = messages.len();

    // Session-level store metadata leads the conversation: a child
    // session's lineage names its parent outright; a root session that
    // still declares agent/model/project facts carries a
    // `session-agent` record instead.
    let mut session_meta = serde_json::Map::new();
    if let Some(agent_name) = meta.agent.as_ref().filter(|v| !v.trim().is_empty()) {
        session_meta.insert("agent".to_string(), Value::String(agent_name.clone()));
    }
    if let Some(model) = meta.model.as_ref().filter(|v| !v.trim().is_empty()) {
        session_meta.insert("model".to_string(), Value::String(model.clone()));
    }
    if let Some(path) = meta.path.as_ref().filter(|v| !v.trim().is_empty()) {
        session_meta.insert("projectPath".to_string(), Value::String(path.clone()));
    }
    if meta.archived {
        session_meta.insert("archived".to_string(), Value::Bool(true));
    }
    // A parent pointer the store cannot resolve (the parent session row
    // is gone) leaves the session a plain root, never a delegated marker.
    let parent = meta.parent_id.filter(|parent| {
        connection
            .query_row("SELECT 1 FROM session WHERE id=?1", [parent], |_| Ok(()))
            .is_ok()
    });
    if let Some(parent) = parent {
        session_meta.insert("parentSessionId".to_string(), Value::String(parent));
        if let Some(title) = title.as_ref().filter(|v| !v.trim().is_empty()) {
            session_meta.insert("subagentTitle".to_string(), Value::String(title.clone()));
        }
        records.push(Record {
            id: None,
            parent: None,
            ts: None,
            ordinal: 0,
            body: Body::Meta {
                subtype: "subagent-lineage".to_string(),
                data: Value::Object(session_meta),
            },
        });
        record_lines.push(0);
    } else if !session_meta.is_empty() {
        records.push(Record {
            id: None,
            parent: None,
            ts: None,
            ordinal: 0,
            body: Body::Meta {
                subtype: "session-agent".to_string(),
                data: Value::Object(session_meta),
            },
        });
        record_lines.push(0);
    }
    if let Some(usage) = meta.usage {
        records.push(Record {
            id: None,
            parent: None,
            ts: conv.updated_at.clone(),
            ordinal: 0,
            body: Body::Meta {
                subtype: "usage".to_string(),
                data: usage,
            },
        });
        record_lines.push(0);
    }

    for (index, message) in messages.into_iter().enumerate() {
        let data = &message.data;
        let role = extract_role(data);
        let ts = json_time(data, "created")
            .or_else(|| message.created_ms.and_then(epoch_number_to_rfc3339))
            .or_else(|| json_time(data, "completed"))
            .or_else(|| message.updated_ms.and_then(epoch_number_to_rfc3339));
        let model = message_model(data).or_else(|| session_model_label(meta.model.as_deref()));
        let parent = find_string(data, &["parentID", "parentId", "parent_id"]);
        let blocks = parts.remove(&message.id).unwrap_or_default();
        let record_id = |block: usize, total: usize| {
            if total == 1 {
                Some(message.id.clone())
            } else {
                Some(format!("{}:{index}:{block}", message.id))
            }
        };
        let before = records.len();
        if blocks.is_empty() {
            // A message without parts still decodes its own blob —
            // e.g. a bare `{"role":"tool_result","result":…}` row.
            project_envelope(
                data,
                &role,
                model.as_deref(),
                &mut records,
                &mut record_lines,
                &mut skipped,
                index,
                &ts,
                Some(message.id.clone()),
                parent.clone(),
            );
        } else {
            let total = blocks.len();
            for (block_index, part) in blocks.iter().enumerate() {
                project_part(
                    part,
                    &role,
                    model.as_deref(),
                    &mut records,
                    &mut record_lines,
                    &mut skipped,
                    index,
                    &ts,
                    record_id(block_index, total),
                    parent.clone(),
                );
            }
        }
        if before == records.len() {
            skipped.push(Skipped {
                line: index,
                native_type: Some(format!("message:{role}")),
                reason: "projection".to_string(),
            });
        }
        // Token counters ride on the message blob (`tokens`/`usage`/…),
        // independent of which parts projected.
        if let Some(usage) = usage_container(data) {
            push_record(
                &mut records,
                &mut record_lines,
                index,
                ts.clone(),
                Body::Meta {
                    subtype: "usage".to_string(),
                    data: usage.clone(),
                },
            );
        }
    }

    conv.updated_at = records
        .iter()
        .rev()
        .find_map(|record| record.ts.clone())
        .or(conv.updated_at)
        .or(conv.created_at.clone());
    finish(&mut conv, records);
    Ok(Some(Decoded {
        conversation: conv,
        record_lines,
        line_count,
        skipped,
    }))
}

// ---------------------------------------------------------------------------
// Message/part projection.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn project_part(
    part: &Value,
    outer_role: &str,
    model: Option<&str>,
    records: &mut Vec<Record>,
    record_lines: &mut Vec<usize>,
    skipped: &mut Vec<Skipped>,
    line: usize,
    ts: &Option<String>,
    id: Option<String>,
    parent: Option<String>,
) {
    let semantic = native_semantic(part);
    match semantic_kind(&semantic) {
        PartKind::Text => {
            let Some(text) = extract_text(part) else {
                skipped.push(Skipped {
                    line,
                    native_type: Some(semantic),
                    reason: "projection".to_string(),
                });
                return;
            };
            let body = if matches!(outer_role, "user" | "human") {
                Body::User {
                    text,
                    model: model.map(str::to_string),
                    attachments: Vec::new(),
                }
            } else {
                Body::Assistant {
                    text,
                    model: model.map(str::to_string),
                }
            };
            push_full(records, record_lines, line, ts.clone(), body, id, parent);
        }
        PartKind::Reasoning => {
            let Some(text) = find_string(part, &["text", "thinking", "reasoning"])
                .or_else(|| extract_text(part))
            else {
                skipped.push(Skipped {
                    line,
                    native_type: Some(semantic),
                    reason: "projection".to_string(),
                });
                return;
            };
            push_full(
                records,
                record_lines,
                line,
                ts.clone(),
                Body::Reasoning { text },
                id,
                parent,
            );
        }
        PartKind::ToolCall => {
            let call_id = part_call_id(part).or_else(|| id.clone());
            let name = find_string(
                part,
                &[
                    "tool",
                    "name",
                    "toolName",
                    "tool_name",
                    "functionName",
                    "function_name",
                ],
            )
            .unwrap_or_else(|| "tool".to_string());
            let state = part.get("state").cloned().unwrap_or(Value::Null);
            let args = state
                .get("input")
                .cloned()
                .or_else(|| part.get("input").cloned())
                .or_else(|| part.get("args").cloned())
                .or_else(|| part.get("arguments").cloned())
                .unwrap_or_else(|| serde_json::json!({}));
            push_full(
                records,
                record_lines,
                line,
                ts.clone(),
                Body::ToolCall {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    args,
                },
                id.clone(),
                parent.clone(),
            );
            // OpenCode tool parts carry the result in `state` — a
            // completed/errored invocation is also the tool's result.
            let status = state.get("status").and_then(Value::as_str).unwrap_or("");
            let output = state
                .get("output")
                .cloned()
                .or_else(|| state.get("error").cloned());
            if let Some(content) = output {
                push_full(
                    records,
                    record_lines,
                    line,
                    ts.clone(),
                    Body::ToolResult {
                        call_id,
                        name: Some(name),
                        content,
                        is_error: status == "error",
                    },
                    id,
                    parent,
                );
            }
        }
        PartKind::ToolResult => {
            let content = part
                .get("result")
                .or_else(|| part.get("output"))
                .or_else(|| part.get("content"))
                .cloned()
                .unwrap_or_else(|| part.clone());
            push_full(
                records,
                record_lines,
                line,
                ts.clone(),
                Body::ToolResult {
                    call_id: part_call_id(part).or_else(|| id.clone()),
                    name: find_string(
                        part,
                        &["tool", "name", "toolName", "tool_name", "functionName"],
                    ),
                    content,
                    is_error: part
                        .get("isError")
                        .or_else(|| part.get("is_error"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                },
                id,
                parent,
            );
        }
        PartKind::Metadata => {
            push_full(
                records,
                record_lines,
                line,
                ts.clone(),
                Body::Meta {
                    subtype: semantic,
                    data: part.clone(),
                },
                id,
                parent,
            );
        }
        PartKind::Error => {
            let text = extract_text(part).unwrap_or_else(|| {
                find_string(part, &["error", "message", "title"])
                    .unwrap_or_else(|| "native agent reported an error".to_string())
            });
            push_full(
                records,
                record_lines,
                line,
                ts.clone(),
                Body::System {
                    subtype: "error".to_string(),
                    text,
                },
                id,
                parent,
            );
        }
        PartKind::Event => {
            // Lifecycle parts carrying token counters still surface them.
            if let Some(usage) = usage_container(part) {
                push_record(
                    records,
                    record_lines,
                    line,
                    ts.clone(),
                    Body::Meta {
                        subtype: "usage".to_string(),
                        data: usage.clone(),
                    },
                );
            }
            skipped.push(Skipped {
                line,
                native_type: Some(semantic),
                reason: "projection".to_string(),
            });
        }
    }
}

/// Project a bare message/envelope JSON value (no `part` rows): role
/// and text extraction decide the record; tool-result envelopes decode
/// as tool results.
#[allow(clippy::too_many_arguments)]
fn project_envelope(
    data: &Value,
    role: &str,
    model: Option<&str>,
    records: &mut Vec<Record>,
    record_lines: &mut Vec<usize>,
    skipped: &mut Vec<Skipped>,
    line: usize,
    ts: &Option<String>,
    id: Option<String>,
    parent: Option<String>,
) {
    let semantic = native_semantic(data);
    match semantic_kind(&semantic) {
        PartKind::ToolCall => project_part(
            data,
            role,
            model,
            records,
            record_lines,
            skipped,
            line,
            ts,
            id,
            parent,
        ),
        PartKind::ToolResult => project_part(
            data,
            role,
            model,
            records,
            record_lines,
            skipped,
            line,
            ts,
            id,
            parent,
        ),
        _ => {
            let Some(text) = extract_text(data) else {
                return;
            };
            let body = match role {
                "user" | "human" => Body::User {
                    text,
                    model: model.map(str::to_string),
                    attachments: Vec::new(),
                },
                "assistant" | "agent" => Body::Assistant {
                    text,
                    model: model.map(str::to_string),
                },
                other => Body::System {
                    subtype: other.to_string(),
                    text,
                },
            };
            push_full(records, record_lines, line, ts.clone(), body, id, parent);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PartKind {
    Text,
    Reasoning,
    ToolCall,
    ToolResult,
    Metadata,
    Error,
    Event,
}

/// LicoUp `history_message_kind_from_semantic`, folded onto canonical
/// kinds: text-ish semantics stay text, `reasoning|analysis|thinking`
/// → reasoning, error semantics → error, attachment/metadata → meta,
/// `tool|function` + result markers → tool result, `tool|function` →
/// tool call, everything else → event (skipped).
fn semantic_kind(semantic: &str) -> PartKind {
    let semantic = normalize_semantic(semantic);
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
        return PartKind::Text;
    }
    if semantic.contains("reasoning")
        || semantic.contains("analysis")
        || semantic.contains("thinking")
    {
        return PartKind::Reasoning;
    }
    if semantic.contains("error")
        || semantic.contains("failure")
        || semantic.contains("failed")
        || semantic.contains("exception")
    {
        return PartKind::Error;
    }
    if semantic == "metadata"
        || matches!(
            semantic.as_str(),
            "image" | "image-url" | "document" | "attachment" | "input-json-delta" | "file"
        )
    {
        return PartKind::Metadata;
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
        return PartKind::ToolResult;
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
        return PartKind::ToolCall;
    }
    PartKind::Event
}

/// LicoUp `normalize_history_message_semantic` — lowercase alnum runs
/// joined by `-`.
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

/// A block's self-declared type — `type`/`kind`/`role`/`eventType`/
/// `event_type`, first present.
fn native_semantic(value: &Value) -> String {
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

fn part_call_id(part: &Value) -> Option<String> {
    find_string(
        part,
        &["callID", "callId", "call_id", "toolCallId", "tool_call_id"],
    )
}

/// LicoUp `extract_role`: user/human → `user`, assistant/agent/model/ai
/// → `agent`, tool → `tool`, else the raw role (default `system`).
fn extract_role(value: &Value) -> String {
    let role = find_string(value, &["role", "author", "speaker", "type", "source"])
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

/// LicoUp `find_string`: key lookup descending through `message` nests;
/// strings and integers both resolve.
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

fn json_time(value: &Value, key: &str) -> Option<String> {
    value
        .get("time")
        .and_then(|time| time.get(key))
        .and_then(epoch_value_to_rfc3339)
}

fn message_model(value: &Value) -> Option<String> {
    find_string(value, &["modelID", "modelId", "model_id", "modelName"])
        .or_else(|| {
            value.get("model").and_then(|model| match model {
                Value::String(text) => Some(text.clone()),
                Value::Object(_) => find_string(model, &["modelID", "modelId", "id", "name"]),
                _ => None,
            })
        })
        .filter(|model| !model.trim().is_empty())
}

/// A `model` session column may be a bare id or a JSON
/// `{providerID, modelID}` object.
fn session_model_label(raw: Option<&str>) -> Option<String> {
    let raw = raw?.trim().trim_matches('"');
    if raw.starts_with('{') {
        return serde_json::from_str::<Value>(raw)
            .ok()
            .and_then(|value| find_string(&value, &["modelID", "modelId", "id", "name"]));
    }
    (!raw.is_empty()).then(|| raw.to_string())
}

/// The first usage container a blob carries — the keys the source's
/// usage reader probes on message `data`.
fn usage_container(value: &Value) -> Option<&Value> {
    for key in [
        "usage",
        "tokens",
        "tokenUsage",
        "token_usage",
        "usageMetadata",
        "usage_metadata",
        "responseUsage",
        "response_usage",
        "tokenCount",
        "gen_ai.usage",
    ] {
        if let Some(usage) = value.get(key).filter(|usage| usage.is_object()) {
            return Some(usage);
        }
    }
    None
}

/// LicoUp `openagent_usage_from_columns`: session token columns → the
/// normalized counter object (`source: openagent-sqlite`).
fn session_usage(
    input: Option<i64>,
    output: Option<i64>,
    reasoning: Option<i64>,
    cache_read: Option<i64>,
    cache_write: Option<i64>,
) -> Option<Value> {
    let cached_input_tokens = cache_read.unwrap_or(0).max(0);
    let prompt_tokens = [input, cache_read, cache_write]
        .into_iter()
        .flatten()
        .filter(|value| *value > 0)
        .sum::<i64>();
    let completion_tokens = [output, reasoning]
        .into_iter()
        .flatten()
        .filter(|value| *value > 0)
        .sum::<i64>();
    let total_tokens = prompt_tokens + completion_tokens;
    if total_tokens <= 0 {
        return None;
    }
    Some(serde_json::json!({
        "scope": "session",
        "promptTokens": prompt_tokens,
        "cachedInputTokens": cached_input_tokens,
        "completionTokens": completion_tokens,
        "totalTokens": total_tokens,
        "source": "openagent-sqlite"
    }))
}

/// LicoUp `extract_text`: depth-first walk over `text|content|message|
/// messages|prompt|response|answer|summary|value|parts|items|turns`,
/// skipping tool/metadata-typed objects and joining survivors on `\n`.
fn extract_text(value: &Value) -> Option<String> {
    let mut pending = vec![value];
    let mut parts = Vec::new();
    while let Some(candidate) = pending.pop() {
        match candidate {
            Value::String(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(decoded) = parse_embedded_json(trimmed) {
                    if let Some(text) = extract_text_shallow(&decoded) {
                        if !text.trim().is_empty() {
                            parts.push(text);
                            continue;
                        }
                    }
                }
                parts.push(trimmed.to_string());
            }
            Value::Array(items) => pending.extend(items.iter().rev()),
            Value::Object(object) => {
                if object_is_tool_or_metadata(object) {
                    continue;
                }
                let child = if object_is_text(object) {
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

/// Same walk without embedded-JSON re-decode — the recursive step of
/// [`extract_text`].
fn extract_text_shallow(value: &Value) -> Option<String> {
    let mut pending = vec![value];
    let mut parts = Vec::new();
    while let Some(candidate) = pending.pop() {
        match candidate {
            Value::String(text) => {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
            Value::Array(items) => pending.extend(items.iter().rev()),
            Value::Object(object) => {
                if object_is_tool_or_metadata(object) {
                    continue;
                }
                let child = [
                    "text", "content", "message", "messages", "prompt", "response", "answer",
                    "summary", "value", "parts", "items", "turns",
                ]
                .iter()
                .find_map(|key| object.get(*key).filter(|value| value_may_have_text(value)));
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

fn object_is_tool_or_metadata(object: &serde_json::Map<String, Value>) -> bool {
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

fn object_is_text(object: &serde_json::Map<String, Value>) -> bool {
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

/// A text field may itself be serialized JSON carrying deeper text.
fn parse_embedded_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    let looks_like_json = (trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']'));
    if !looks_like_json {
        return None;
    }
    let value = serde_json::from_str::<Value>(trimmed).ok()?;
    embedded_may_hold_text(&value).then_some(value)
}

fn embedded_may_hold_text(value: &Value) -> bool {
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

fn epoch_number_to_rfc3339(value: i64) -> Option<String> {
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
    let millis = u64::try_from(millis).ok()?;
    Some(crate::util::epoch_ms_rfc3339(millis))
}

fn epoch_value_to_rfc3339(value: &Value) -> Option<String> {
    value
        .as_i64()
        .or_else(|| {
            value
                .as_str()
                .and_then(|text| text.trim().parse::<i64>().ok())
        })
        .and_then(epoch_number_to_rfc3339)
}

/// LicoUp `bounded_project_workspace`, minus the platform unbounded-root
/// policy: a recorded directory must trim to an absolute path.
fn bounded_workspace(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || !Path::new(trimmed).is_absolute() {
        return None;
    }
    Some(trimmed.to_string())
}

fn push_record(
    records: &mut Vec<Record>,
    lines: &mut Vec<usize>,
    line: usize,
    ts: Option<String>,
    body: Body,
) {
    push_full(records, lines, line, ts, body, None, None);
}

fn push_full(
    records: &mut Vec<Record>,
    lines: &mut Vec<usize>,
    line: usize,
    ts: Option<String>,
    body: Body,
    id: Option<String>,
    parent: Option<String>,
) {
    records.push(Record {
        id,
        parent,
        ts,
        ordinal: 0,
        body,
    });
    lines.push(line);
}

fn finish(conv: &mut Conversation, mut records: Vec<Record>) {
    for (ordinal, record) in records.iter_mut().enumerate() {
        record.ordinal = ordinal as u64;
    }
    conv.records = records;
}
