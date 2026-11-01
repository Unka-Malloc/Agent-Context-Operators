//! aco codec: the `copilot` (GitHub Copilot) conversation store.
//!
//! The declared store root is `~/.copilot` (`COPILOT_HOME` relocates it the
//! way copilot's own tooling honors). Sessions live three ways:
//!
//! - `session-state/<id>/events.jsonl` — the CLI's per-session event log
//!   (pre-1.0.11 builds wrote flat `session-state/<id>.jsonl` files). A
//!   `session.start` row carries `data.sessionId`; `user.message` and
//!   `assistant.message` rows carry `data.content`; `tool.execution_start`
//!   / `tool.execution_complete` rows are the tool call/result pair; every
//!   other `*.message`-style or lifecycle `type` projects through the
//!   semantic kind map — unknown event rows are preserved as meta records
//!   rather than dropped.
//! - `session-store.db` — a SQLite index (`sessions` table: `id`, `cwd`,
//!   `summary`, `created_at`, `updated_at`). It indexes the same sessions
//!   the event logs carry, so directory scans do not re-emit its rows; an
//!   index row with no `session-state/<id>/events.jsonl` on disk has no
//!   locally resumable content and is skipped, the store's own rule. A
//!   `turns` table (user message + final answer per turn) exists beside it
//!   but is only a partial projection of the transcript — it is not read.
//! - VS Code `state.vscdb` stores — `ItemTable(key, value)` rows where the
//!   `github.copilot-chat.chatSessions` key holds a
//!   `{"chatSessions":[…]}` document, one item per session. Enumerating the
//!   database emits one `(path, key)` ref per item; `decode_session` reads
//!   the item's `messages` array.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use aco::codec::{Capability, CapabilityMatrix, Codec, Decoded, Encoded, SessionRef, Skipped};
use aco::model::{Attachment, Body, Conversation, Record, RecordKind};
use aco::store::{HistoryRoot, StoreKind};
use aco::{Error, Result};
use serde_json::{json, Value};

/// The codec instance aco's registry hands out.
pub struct CopilotCodec;

const CAPABILITIES: CapabilityMatrix = CapabilityMatrix::new(
    true,
    [
        Capability::Native,      // user — `user.message` rows
        Capability::Native,      // assistant — `assistant.message` rows
        Capability::Adapted,     // reasoning — folds into `assistant.message` data.reasoningText
        Capability::Native,      // tool-call — `tool.execution_start` rows / `toolRequests`
        Capability::Native,      // tool-result — `tool.execution_complete` rows
        Capability::Unsupported, // system — the transcript writes no harness-event row
        Capability::Adapted,     // meta — usage rides inside `assistant.message` data
    ],
);

/// The `ItemTable` key under which VS Code state stores keep the
/// copilot-chat sessions document.
const CHAT_SESSIONS_KEY: &str = "github.copilot-chat.chatSessions";
/// `SessionRef::key` prefix selecting one `chatSessions[i]` item.
const CHAT_SESSION_KEY_PREFIX: &str = "github.copilot-chat.chatSessions#";
/// `SessionRef::key` prefix selecting one `sessions`-table row id — the
/// row's content lives in `session-state/<id>/events.jsonl`.
const SESSION_INDEX_KEY_PREFIX: &str = "sessions:";
/// LicoUp's `accepts_file` set for copilot stores — transcripts and the
/// SQLite containers sessions live in.
const ACCEPTED_EXTENSIONS: &[&str] = &[
    "jsonl", "ndjson", "json", "sqlite", "sqlite3", "db", "vscdb",
];
/// Deepest container nesting the usage/model readers descend.
const MAX_METADATA_DEPTH: usize = 4;

/// One codec instance for the registry.
pub fn codec() -> &'static dyn Codec {
    &CopilotCodec
}

/// The card's declared history surface, projected onto the unified
/// `aco::store` vocabulary: `a4aa-copilot` 0.4.1 ships the old a4aa
/// `HistoryRoot`/`StoreKind` types — structurally identical but distinct.
/// Rebind to the unified aco::store types after a4aa republishes; transitional projection.
fn roots() -> Vec<HistoryRoot> {
    a4aa_copilot::card()
        .history
        .iter()
        .map(|root| HistoryRoot {
            path: root.path,
            kind: root.kind,
            store: store_kind(root.store),
        })
        .collect()
}

/// Same transitional projection for the card's transcript-usage surface.
fn usage_surface() -> Option<aco::store::TranscriptUsage> {
    a4aa_copilot::card()
        .usage
        .transcript
        .as_ref()
        .map(|usage| aco::store::TranscriptUsage {
            root: usage.root,
            record: usage.record,
            container: usage.container,
            input: usage.input,
            cached: usage.cached,
            output: usage.output,
            total: usage.total,
        })
}

/// Map the published card's `StoreKind` onto the unified vocabulary by
/// name — the types are structurally identical but distinct until a4aa
/// republishes.
fn store_kind(store: impl serde::Serialize) -> StoreKind {
    serde_json::to_value(store)
        .ok()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or(StoreKind::Other)
}

impl Codec for CopilotCodec {
    fn id(&self) -> &'static str {
        "copilot"
    }

    fn history(&self) -> Vec<HistoryRoot> {
        roots()
    }

    fn transcript_usage(&self) -> Option<aco::store::TranscriptUsage> {
        usage_surface()
    }

    fn session_kinds(&self) -> &'static [&'static str] {
        &["copilot-session-store"]
    }

    fn capabilities(&self) -> &'static CapabilityMatrix {
        &CAPABILITIES
    }

    /// `COPILOT_HOME` relocates the `.copilot` tree — the same override
    /// copilot's own tooling resolves.
    fn expand_root(&self, root: &HistoryRoot, home: &Path) -> PathBuf {
        if let Some(copilot_home) = std::env::var("COPILOT_HOME")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from)
        {
            if let Some(relative) = root.path.strip_prefix("{home}/.copilot") {
                return copilot_home.join(relative.trim_start_matches('/'));
            }
        }
        aco::util::expand_home(root.path, home)
    }

    /// The durable session id is the `session.start` row's `data.sessionId`,
    /// with the session-state directory name as the format's own fallback
    /// for `events.jsonl` artifacts.
    fn session_id(&self, path: &Path) -> String {
        transcript_session_id(path)
            .or_else(|| session_dir_name(path))
            .or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "copilot-session".to_string())
    }

    /// The extension set LicoUp's copilot catalog accepts — transcripts
    /// and SQLite containers; a non-transcript `.json` document decodes
    /// to nothing on its own.
    fn select_session_files(&self, files: Vec<PathBuf>) -> Vec<PathBuf> {
        files
            .into_iter()
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| ACCEPTED_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
                    .unwrap_or(false)
            })
            .collect()
    }

    /// File stores emit one ref per accepted file; a `SqliteDb` root (or
    /// any database file met during a directory walk) enumerates its
    /// session-bearing rows instead. The `sessions`-table index is only
    /// consulted when the database itself is the addressed store — under a
    /// directory walk its rows name event logs already enumerated, or
    /// sessions with no locally resumable content.
    fn enumerate(&self, root: &HistoryRoot, path: &Path) -> Result<Vec<SessionRef>> {
        if root.store == StoreKind::SqliteDb {
            return self.sessions_in_file(path);
        }
        let files = if path.is_file() {
            vec![path.to_path_buf()]
        } else {
            aco::store::collect_files(path)?
        };
        let selected = if path.is_file() {
            files
        } else {
            self.select_session_files(files)
        };
        let mut sessions = Vec::new();
        for file in selected {
            if sqlite_extension(&file) {
                // A stray or foreign database under a walked directory
                // skips itself; an explicitly addressed one reports its
                // own error.
                sessions.extend(enumerate_sqlite(&file, path.is_file()).unwrap_or_default());
            } else {
                sessions.push(SessionRef {
                    id: self.session_id(&file),
                    path: file,
                    key: None,
                });
            }
        }
        Ok(sessions)
    }

    /// Row-level sessions a container file holds — the hook a host that
    /// walks files itself uses to reach `chatSessions`/`sessions` rows.
    fn sessions_in_file(&self, path: &Path) -> Result<Vec<SessionRef>> {
        if sqlite_extension(path) {
            return enumerate_sqlite(path, true);
        }
        Ok(vec![SessionRef {
            id: self.session_id(path),
            path: path.to_path_buf(),
            key: None,
        }])
    }

    /// File refs decode their transcript; row refs read their container:
    /// a `chatSessions#<i>` item's `messages` array projects like a
    /// transcript, a `sessions:<id>` index row resolves to its
    /// `session-state/<id>/events.jsonl`.
    fn decode_session(&self, session: &SessionRef) -> Result<Decoded> {
        match session.key.as_deref() {
            None => self.decode(&session.path),
            Some(key) if key.starts_with(CHAT_SESSION_KEY_PREFIX) => {
                let index: usize = key[CHAT_SESSION_KEY_PREFIX.len()..]
                    .parse()
                    .map_err(|_| Error::Usage(format!("bad copilot row key `{key}`")))?;
                let connection = open_read_only(&session.path)?;
                let document =
                    chat_sessions_document(&connection, &session.path)?.ok_or_else(|| {
                        Error::Usage(format!(
                            "{}: `{CHAT_SESSIONS_KEY}` row is absent",
                            session.path.display()
                        ))
                    })?;
                let item = document
                    .get("chatSessions")
                    .and_then(Value::as_array)
                    .and_then(|items| items.get(index))
                    .ok_or_else(|| {
                        Error::Usage(format!(
                            "{}: `chatSessions[{index}]` is absent",
                            session.path.display()
                        ))
                    })?;
                Ok(project_chat_session(item, index))
            }
            Some(key) if key.starts_with(SESSION_INDEX_KEY_PREFIX) => {
                let id = &key[SESSION_INDEX_KEY_PREFIX.len()..];
                let events = session
                    .path
                    .parent()
                    .map(|base| base.join("session-state").join(id).join("events.jsonl"))
                    .filter(|events| events.is_file())
                    .ok_or_else(|| {
                        Error::Usage(format!(
                            "{}: session `{id}` has no locally resumable event log",
                            session.path.display()
                        ))
                    })?;
                self.decode(&events)
            }
            Some(key) => Err(Error::Usage(format!(
                "unrecognized copilot row key `{key}`"
            ))),
        }
    }

    fn decode(&self, path: &Path) -> Result<Decoded> {
        if sqlite_extension(path) {
            let sessions = enumerate_sqlite(path, true)?;
            return match sessions.len() {
                1 => self.decode_session(&sessions[0]),
                n => Err(Error::Usage(format!(
                    "{}: sqlite store holds {n} copilot sessions — address one by id",
                    path.display()
                ))),
            };
        }
        let raw = std::fs::read_to_string(path).map_err(Error::io(path))?;
        if let Some(items) = session_document(&raw) {
            return match items.len() {
                1 => Ok(project_chat_session(&items[0], 0)),
                n => Err(Error::Usage(format!(
                    "{}: document holds {n} copilot sessions — address one by id",
                    path.display()
                ))),
            };
        }
        decode_transcript(path, &raw)
    }

    fn encode(&self, conversation: &Conversation) -> Result<Encoded> {
        let (kept, projection) = self.project(conversation);
        let mut lines = Vec::new();
        lines.push(
            json!({
                "type": "session.start",
                "data": {
                    "sessionId": conversation.session_id,
                    "cwd": conversation.cwd,
                },
                "timestamp": conversation.created_at,
            })
            .to_string(),
        );
        let mut pending: Vec<&Record> = Vec::new();
        for rec in kept {
            match rec.kind() {
                // Each assistant record opens a new message group; reasoning
                // decoded ahead of it joins the group, tool requests and
                // usage fold into it.
                RecordKind::Assistant => {
                    if pending.iter().any(|r| r.kind() == RecordKind::Assistant) {
                        flush_assistant(&mut pending, &mut lines);
                    }
                    pending.push(rec);
                }
                RecordKind::Reasoning | RecordKind::ToolCall => pending.push(rec),
                RecordKind::Meta => {
                    if is_usage_meta(rec) {
                        pending.push(rec);
                    } else {
                        flush_assistant(&mut pending, &mut lines);
                        lines.push(meta_row(rec));
                    }
                }
                RecordKind::User => {
                    flush_assistant(&mut pending, &mut lines);
                    if let Body::User { text, attachments } = &rec.body {
                        lines.push(
                            json!({
                                "type": "user.message",
                                "data": {
                                    "messageId": rec.id,
                                    "content": text,
                                    "attachments": encode_attachments(attachments),
                                },
                                "timestamp": rec.ts,
                            })
                            .to_string(),
                        );
                    }
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
                            json!({
                                "type": "tool.execution_complete",
                                "data": {
                                    "toolCallId": call_id,
                                    "toolName": name,
                                    "result": content,
                                    "success": !is_error,
                                },
                                "timestamp": rec.ts,
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

    /// Imported sessions land in the CLI session layout:
    /// `<store>/session-state/<id>/events.jsonl`.
    fn store_path(&self, home: &Path, conversation: &Conversation) -> Result<PathBuf> {
        let root = self
            .history()
            .into_iter()
            .find(|r| r.kind == "copilot-session-store")
            .map(|r| self.expand_root(&r, home))
            .ok_or_else(|| aco::Error::Usage("copilot session root undeclared".to_string()))?;
        let id = conversation
            .session_id
            .replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '_', "-");
        Ok(root.join("session-state").join(id).join("events.jsonl"))
    }

    /// Rewrite the textual payload a row carries: message `content` /
    /// `transformedContent` / `reasoningText`, tool `arguments` /
    /// `toolRequests`, and result `result` / `output` payloads. Structural
    /// rows (`session.start`, lifecycle events) hold no conversation text.
    fn redact_line(&self, line: &Value, marker: &str) -> Option<Value> {
        let mut out = line.clone();
        let data = out.get_mut("data")?;
        let mut touched = false;
        for key in [
            "content",
            "reasoningText",
            "text",
            "thinking",
            "summary",
            "error",
        ] {
            if data.get(key).and_then(Value::as_str).is_some() {
                data[key] = json!(marker);
                touched = true;
            }
        }
        // `transformedContent` is an array of typed parts when structured —
        // rewrite whichever shape the row persists.
        for key in [
            "transformedContent",
            "arguments",
            "input",
            "args",
            "result",
            "output",
            "message",
        ] {
            if data.get(key).is_some_and(|v| !v.is_null()) {
                data[key] = json!(marker);
                touched = true;
            }
        }
        if let Some(requests) = data.get_mut("toolRequests").and_then(Value::as_array_mut) {
            for request in requests {
                for key in ["arguments", "input", "args"] {
                    if request.get(key).is_some_and(|v| !v.is_null()) {
                        request[key] = json!(marker);
                        touched = true;
                    }
                }
            }
        }
        touched.then_some(out)
    }
}

// ---------------------------------------------------------------------------
// Transcript decode — `events.jsonl` rows, ported from LicoUp's
// `parse_copilot_transcript_session`.
// ---------------------------------------------------------------------------

/// The semantic buckets the native `type` field maps onto — LicoUp's
/// `HistoryMessageKind`, kept verbatim so event-type routing does not
/// drift between the two readers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SemanticKind {
    Text,
    ToolCall,
    ToolResult,
    Reasoning,
    Metadata,
    Error,
    Event,
}

fn message_kind(value: &str) -> SemanticKind {
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
        return SemanticKind::Text;
    }
    if semantic.contains("reasoning")
        || semantic.contains("analysis")
        || semantic.contains("thinking")
    {
        return SemanticKind::Reasoning;
    }
    if semantic.contains("error")
        || semantic.contains("failure")
        || semantic.contains("failed")
        || semantic.contains("exception")
    {
        return SemanticKind::Error;
    }
    if semantic == "metadata"
        || matches!(
            semantic.as_str(),
            "image" | "image-url" | "document" | "attachment" | "input-json-delta"
        )
    {
        return SemanticKind::Metadata;
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
        return SemanticKind::ToolResult;
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
        return SemanticKind::ToolCall;
    }
    SemanticKind::Event
}

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

fn decode_transcript(path: &Path, raw: &str) -> Result<Decoded> {
    let fallback_id = session_dir_name(path)
        .or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "copilot-session".to_string());
    let mut conv = Conversation::new(fallback_id);
    conv.agent = Some("copilot".to_string());
    let mut records = Vec::new();
    let mut record_lines = Vec::new();
    let mut skipped = Vec::new();
    let mut line_count = 0;
    // A call id carried by an assistant message's `toolRequests` is seen:
    // its `tool.execution_start` row is the same call, not a new record.
    let mut seen_call_ids = BTreeSet::new();

    for (index, line) in raw.lines().enumerate() {
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
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        let data = value.get("data").unwrap_or(&value);
        let ts = extract_timestamp(&value);
        if event_type == "session.start" {
            // Lifecycle header: seeds identity, never a record.
            if let Some(id) = data
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
            {
                conv.session_id = id.to_string();
            }
            conv.cwd = data
                .get("cwd")
                .and_then(Value::as_str)
                .or_else(|| data.pointer("/context/cwd").and_then(Value::as_str))
                .or_else(|| data.get("workingDirectory").and_then(Value::as_str))
                .filter(|cwd| !cwd.trim().is_empty())
                .map(str::to_string)
                .or(conv.cwd);
            conv.created_at = ts.clone().or(conv.created_at);
            continue;
        }
        let before = records.len();
        match message_kind(event_type) {
            SemanticKind::Text => match event_type {
                "user.message" => {
                    let text = data
                        .get("content")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| extract_text(data))
                        .unwrap_or_default();
                    let attachments = user_attachments(data);
                    if !text.trim().is_empty() || !attachments.is_empty() {
                        push(
                            &mut records,
                            &mut record_lines,
                            index,
                            data.get("messageId")
                                .or_else(|| data.get("id"))
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            ts.clone(),
                            Body::User { text, attachments },
                        );
                    }
                }
                "assistant.message" => {
                    if let Some(text) = data
                        .get("reasoningText")
                        .and_then(Value::as_str)
                        .filter(|t| !t.trim().is_empty())
                    {
                        push(
                            &mut records,
                            &mut record_lines,
                            index,
                            data.get("messageId")
                                .or_else(|| data.get("id"))
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            ts.clone(),
                            Body::Reasoning {
                                text: text.to_string(),
                            },
                        );
                    }
                    let text = data
                        .get("content")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| extract_text(data))
                        .unwrap_or_default();
                    if !text.trim().is_empty() {
                        push(
                            &mut records,
                            &mut record_lines,
                            index,
                            data.get("messageId")
                                .or_else(|| data.get("id"))
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            ts.clone(),
                            Body::Assistant {
                                text,
                                model: prefer_model(native_model(data), native_model(&value)),
                            },
                        );
                    }
                    if let Some(requests) = data.get("toolRequests").and_then(Value::as_array) {
                        for request in requests {
                            let call_id = request
                                .get("toolCallId")
                                .or_else(|| request.get("id"))
                                .and_then(Value::as_str)
                                .map(str::to_string);
                            if let Some(id) = &call_id {
                                seen_call_ids.insert(id.clone());
                            }
                            push(
                                &mut records,
                                &mut record_lines,
                                index,
                                call_id.clone(),
                                ts.clone(),
                                Body::ToolCall {
                                    call_id,
                                    name: structured_name(request).unwrap_or("tool").to_string(),
                                    args: request
                                        .get("arguments")
                                        .or_else(|| request.get("input"))
                                        .cloned()
                                        .unwrap_or(Value::Null),
                                },
                            );
                        }
                    }
                }
                _ => {}
            },
            SemanticKind::Reasoning => {
                if let Some(text) = extract_text(data) {
                    push(
                        &mut records,
                        &mut record_lines,
                        index,
                        data.get("id")
                            .or_else(|| data.get("messageId"))
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        ts.clone(),
                        Body::Reasoning { text },
                    );
                }
            }
            SemanticKind::ToolCall => {
                let call_id = data
                    .get("toolCallId")
                    .or_else(|| data.get("tool_call_id"))
                    .or_else(|| data.get("id"))
                    .or_else(|| data.get("messageId"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if call_id
                    .as_deref()
                    .is_some_and(|id| seen_call_ids.contains(id))
                {
                    skipped.push(Skipped {
                        line: index,
                        native_type: Some(event_type.to_string()),
                        reason: "call already carried by its assistant message".into(),
                    });
                    continue;
                }
                if let Some(id) = &call_id {
                    seen_call_ids.insert(id.clone());
                }
                push(
                    &mut records,
                    &mut record_lines,
                    index,
                    call_id.clone(),
                    ts.clone(),
                    Body::ToolCall {
                        call_id,
                        name: structured_name(data).unwrap_or("tool").to_string(),
                        args: data
                            .get("arguments")
                            .or_else(|| data.get("input"))
                            .or_else(|| data.get("args"))
                            .cloned()
                            .unwrap_or(Value::Null),
                    },
                );
            }
            SemanticKind::ToolResult => {
                push(
                    &mut records,
                    &mut record_lines,
                    index,
                    data.get("toolCallId")
                        .or_else(|| data.get("tool_call_id"))
                        .or_else(|| data.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    ts.clone(),
                    Body::ToolResult {
                        call_id: data
                            .get("toolCallId")
                            .or_else(|| data.get("tool_call_id"))
                            .or_else(|| data.get("id"))
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        name: structured_name(data).map(str::to_string),
                        content: data
                            .get("result")
                            .or_else(|| data.get("output"))
                            .or_else(|| data.get("content"))
                            .or_else(|| data.get("message"))
                            .cloned()
                            .unwrap_or_else(|| data.clone()),
                        is_error: data
                            .get("isError")
                            .or_else(|| data.get("is_error"))
                            .or_else(|| data.get("error").filter(|e| e.is_boolean()))
                            .and_then(Value::as_bool)
                            .or_else(|| data.get("success").and_then(Value::as_bool).map(|s| !s))
                            .unwrap_or(false),
                    },
                );
            }
            SemanticKind::Error => {
                push(
                    &mut records,
                    &mut record_lines,
                    index,
                    data.get("id")
                        .or_else(|| data.get("messageId"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    ts.clone(),
                    Body::System {
                        subtype: "error".to_string(),
                        text: extract_text(data).unwrap_or_else(|| data.to_string()),
                    },
                );
            }
            // Metadata and lifecycle/event rows are preserved whole — the
            // canonical transcript never silently loses persisted rows.
            SemanticKind::Metadata | SemanticKind::Event => {
                push(
                    &mut records,
                    &mut record_lines,
                    index,
                    None,
                    ts.clone(),
                    Body::Meta {
                        subtype: event_type.to_string(),
                        data: value.clone(),
                    },
                );
            }
        }
        if before == records.len() {
            let native_type = (!event_type.is_empty()).then(|| event_type.to_string());
            // `user.message`/`assistant.message` rows with no text carry no
            // record (empty assistant turns happen); every other unprojected
            // row is reported.
            if event_type != "user.message" && event_type != "assistant.message" {
                skipped.push(Skipped {
                    line: index,
                    native_type,
                    reason: "projection".to_string(),
                });
            }
        }
        if let Some(usage) = extract_token_usage(data).or_else(|| extract_token_usage(&value)) {
            push(
                &mut records,
                &mut record_lines,
                index,
                None,
                ts.clone(),
                Body::Meta {
                    subtype: "usage".to_string(),
                    data: usage,
                },
            );
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

/// One `chatSessions` item's `messages` array, projected the way a
/// transcript decodes. Message-array positions stand in for line numbers:
/// `line_count` is `messages.len() + 1` (index 0 is the item itself).
fn project_chat_session(item: &Value, index: usize) -> Decoded {
    let session_id = chat_session_id(item).unwrap_or_else(|| format!("chatSessions-{index}"));
    let mut conv = Conversation::new(session_id);
    conv.agent = Some("copilot".to_string());
    conv.title = find_string(
        item,
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
    .filter(|t| !t.trim().is_empty());
    conv.cwd = find_string(item, &["cwd", "workingDirectory", "projectPath"])
        .filter(|c| !c.trim().is_empty());
    conv.created_at = session_time(item, &["createdAt", "created_at"]);
    conv.updated_at = session_time(
        item,
        &[
            "updatedAt",
            "updated_at",
            "lastActivity",
            "lastActivityAt",
            "timestamp",
            "time",
        ],
    )
    .or(conv.created_at.clone());

    let empty = Vec::new();
    let messages = item
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut records = Vec::new();
    let mut record_lines = Vec::new();
    let mut skipped = Vec::new();
    for (position, message) in messages.iter().enumerate() {
        let line = position + 1;
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let ts = extract_timestamp(message);
        let text = message
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| extract_text(message))
            .unwrap_or_default();
        let before = records.len();
        match role.as_str() {
            "user" | "human" if !text.trim().is_empty() => {
                push(
                    &mut records,
                    &mut record_lines,
                    line,
                    message
                        .get("id")
                        .or_else(|| message.get("messageId"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    ts.clone(),
                    Body::User {
                        text,
                        attachments: Vec::new(),
                    },
                );
            }
            "assistant" | "agent" | "model" | "ai" if !text.trim().is_empty() => {
                push(
                    &mut records,
                    &mut record_lines,
                    line,
                    message
                        .get("id")
                        .or_else(|| message.get("messageId"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    ts.clone(),
                    Body::Assistant {
                        text,
                        model: native_model(message),
                    },
                );
            }
            _ => {}
        }
        if before == records.len() && !role.is_empty() {
            skipped.push(Skipped {
                line,
                native_type: Some(format!("role:{role}")),
                reason: "projection".to_string(),
            });
        }
        if let Some(usage) = extract_token_usage(message) {
            push(
                &mut records,
                &mut record_lines,
                line,
                None,
                ts.clone(),
                Body::Meta {
                    subtype: "usage".to_string(),
                    data: usage,
                },
            );
        }
    }
    conv.updated_at = records
        .last()
        .and_then(|r| r.ts.clone())
        .or(conv.updated_at);
    conv.created_at = conv
        .created_at
        .or_else(|| records.first().and_then(|r| r.ts.clone()));
    for (ordinal, rec) in records.iter_mut().enumerate() {
        rec.ordinal = ordinal as u64;
    }
    conv.records = records;
    Decoded {
        conversation: conv,
        record_lines,
        line_count: messages.len() + 1,
        skipped,
    }
}

// ---------------------------------------------------------------------------
// SQLite containers — ported from LicoUp's `copilot_vscode_*_catalog` and
// `read_copilot_sessions`.
// ---------------------------------------------------------------------------

/// Open a store read-only the way the native readers do.
fn open_read_only(path: &Path) -> Result<rusqlite::Connection> {
    rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_URI
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| {
        Error::Usage(format!(
            "{}: cannot open copilot sqlite store: {e}",
            path.display()
        ))
    })
}

fn table_exists(connection: &rusqlite::Connection, table: &str) -> bool {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1 COLLATE NOCASE LIMIT 1",
            [table],
            |_| Ok(()),
        )
        .is_ok()
}

/// Enumerate the session-bearing rows one database holds. Two recognized
/// layouts, both ported from LicoUp's copilot readers:
///
/// - `ItemTable(key, value)` — VS Code state stores; the
///   `github.copilot-chat.chatSessions` key's document carries one
///   `chatSessions[i]` item per session.
/// - `sessions(id, …)` — the CLI's `session-store.db` index; a row is a
///   session only when `session-state/<id>/events.jsonl` exists beside the
///   database. `include_index_rows` gates this layout: inside a walked
///   root the event logs enumerate directly, so index rows would
///   duplicate them.
fn enumerate_sqlite(path: &Path, include_index_rows: bool) -> Result<Vec<SessionRef>> {
    let connection = open_read_only(path)?;
    if table_exists(&connection, "ItemTable") {
        let Some(document) = chat_sessions_document(&connection, path)? else {
            return Ok(Vec::new());
        };
        let Some(items) = document.get("chatSessions").and_then(Value::as_array) else {
            return Err(Error::Usage(format!(
                "{}: `{CHAT_SESSIONS_KEY}` payload is not a sessions document",
                path.display()
            )));
        };
        return Ok(items
            .iter()
            .enumerate()
            .map(|(index, item)| SessionRef {
                id: chat_session_id(item).unwrap_or_else(|| format!("chatSessions-{index}")),
                path: path.to_path_buf(),
                key: Some(format!("{CHAT_SESSION_KEY_PREFIX}{index}")),
            })
            .collect());
    }
    if table_exists(&connection, "sessions") {
        if !include_index_rows {
            return Ok(Vec::new());
        }
        let mut statement = connection
            .prepare("SELECT id FROM sessions ORDER BY id ASC")
            .map_err(|_| {
                Error::Usage(format!(
                    "{}: `sessions` table layout unrecognized",
                    path.display()
                ))
            })?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|_| Error::Usage(format!("{}: `sessions` rows unreadable", path.display())))?;
        let mut sessions = Vec::new();
        for row in rows {
            let Ok(id) = row else { continue };
            let Some(base) = path.parent() else { continue };
            if !base
                .join("session-state")
                .join(&id)
                .join("events.jsonl")
                .is_file()
            {
                continue;
            }
            sessions.push(SessionRef {
                key: Some(format!("{SESSION_INDEX_KEY_PREFIX}{id}")),
                id,
                path: path.to_path_buf(),
            });
        }
        return Ok(sessions);
    }
    Err(Error::Usage(format!(
        "{}: unrecognized copilot sqlite layout",
        path.display()
    )))
}

/// The `ItemTable` sessions document: `key`/`value` columns are required
/// (schema check), and the `value` blob may be TEXT or BLOB.
fn chat_sessions_document(connection: &rusqlite::Connection, path: &Path) -> Result<Option<Value>> {
    let mut statement = connection
        .prepare("PRAGMA table_info(ItemTable)")
        .map_err(|_| sqlite_schema_error(path))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| sqlite_schema_error(path))?
        .filter_map(std::result::Result::ok)
        .map(|name| name.to_ascii_lowercase())
        .collect::<std::collections::HashSet<_>>();
    if !columns.contains("key") || !columns.contains("value") {
        return Err(sqlite_schema_error(path));
    }
    let mut statement = connection
        .prepare("SELECT value FROM ItemTable WHERE key = ?1 COLLATE NOCASE LIMIT 1")
        .map_err(|_| sqlite_read_error(path))?;
    let mut rows = statement
        .query([CHAT_SESSIONS_KEY])
        .map_err(|_| sqlite_read_error(path))?;
    let Some(row) = rows.next().map_err(|_| sqlite_read_error(path))? else {
        return Ok(None);
    };
    let bytes = match row.get_ref(0).map_err(|_| sqlite_read_error(path))? {
        rusqlite::types::ValueRef::Text(bytes) | rusqlite::types::ValueRef::Blob(bytes) => bytes,
        _ => {
            return Err(Error::Usage(format!(
                "{}: `{CHAT_SESSIONS_KEY}` payload is not a JSON document",
                path.display()
            )))
        }
    };
    serde_json::from_slice(bytes).map(Some).map_err(|_| {
        Error::Usage(format!(
            "{}: `{CHAT_SESSIONS_KEY}` payload is not valid JSON",
            path.display()
        ))
    })
}

fn sqlite_schema_error(path: &Path) -> Error {
    Error::Usage(format!(
        "{}: sqlite schema is not a copilot session store",
        path.display()
    ))
}

fn sqlite_read_error(path: &Path) -> Error {
    Error::Usage(format!(
        "{}: copilot session rows unreadable",
        path.display()
    ))
}

// ---------------------------------------------------------------------------
// Field extraction — generic-name ports of LicoUp's history helpers.
// ---------------------------------------------------------------------------

fn push(
    records: &mut Vec<Record>,
    lines: &mut Vec<usize>,
    line: usize,
    id: Option<String>,
    ts: Option<String>,
    body: Body,
) {
    records.push(Record {
        id,
        parent: None,
        ts,
        ordinal: 0,
        body,
    });
    lines.push(line);
}

/// `session.start` → `data.sessionId`, scanned across the file: the header
/// is not guaranteed to be the first row.
fn transcript_session_id(path: &Path) -> Option<String> {
    let reader = BufReader::new(File::open(path).ok()?);
    reader
        .lines()
        .map_while(std::result::Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .find_map(|value| session_start_id(&value).map(str::to_string))
}

fn session_start_id(value: &Value) -> Option<&str> {
    if value.get("type").and_then(Value::as_str) != Some("session.start") {
        return None;
    }
    value
        .get("data")
        .and_then(|data| data.get("sessionId"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

/// `events.jsonl` artifacts take their session id from the session-state
/// directory name when the header is absent.
fn session_dir_name(path: &Path) -> Option<String> {
    if path.file_name().and_then(|n| n.to_str()) != Some("events.jsonl") {
        return None;
    }
    path.parent()
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .map(str::to_string)
}

fn sqlite_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| ["sqlite", "sqlite3", "db", "vscdb"].contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// A whole-file JSON document holding a sessions array
/// (`{"chatSessions":[…]}` and friends) — the same containers the
/// ItemTable document carries.
fn session_document(raw: &str) -> Option<Vec<Value>> {
    let trimmed = raw.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let value = serde_json::from_str::<Value>(trimmed).ok()?;
    let object = value.as_object()?;
    for key in ["chatSessions", "sessions", "conversations", "chats"] {
        if let Some(items) = object.get(key).and_then(Value::as_array) {
            return Some(items.clone());
        }
    }
    None
}

/// The session's own identifier on a `chatSessions` item — the store's
/// recorded ids, `id` last.
fn chat_session_id(item: &Value) -> Option<String> {
    find_string(
        item,
        &[
            "sessionId",
            "session_id",
            "conversationId",
            "conversation_id",
            "chatId",
            "chat_id",
            "threadId",
            "thread_id",
            "id",
        ],
    )
    .filter(|value| !value.trim().is_empty())
}

/// Timestamp keys native records carry, descending one `message` level —
/// the same key set LicoUp's `extract_timestamp` reads.
fn extract_timestamp(value: &Value) -> Option<String> {
    find_string(
        value,
        &[
            "createdAt",
            "updatedAt",
            "timestamp",
            "time",
            "date",
            "created_at",
            "updated_at",
        ],
    )
}

/// Session time on a `chatSessions` item: RFC 3339 strings pass through,
/// epoch numbers (s / ms / µs) are converted.
fn session_time(item: &Value, keys: &[&str]) -> Option<String> {
    let object = item.as_object()?;
    for key in keys {
        let Some(value) = object.get(*key) else {
            continue;
        };
        if let Some(text) = value.as_str() {
            if let Ok(epoch) = text.trim().parse::<i64>() {
                if let Some(converted) = epoch_to_rfc3339(epoch) {
                    return Some(converted);
                }
            }
            return Some(text.to_string());
        }
        if let Some(epoch) = value.as_i64() {
            if let Some(converted) = epoch_to_rfc3339(epoch) {
                return Some(converted);
            }
        }
    }
    None
}

/// Epoch number → RFC 3339. Unit follows magnitude the way the native
/// readers resolve it: ≥1e15 is microseconds, ≥1e10 milliseconds, else
/// seconds.
fn epoch_to_rfc3339(value: i64) -> Option<String> {
    if value <= 0 {
        return None;
    }
    let ms = if value >= 1_000_000_000_000_000 {
        value / 1_000
    } else if value >= 10_000_000_000 {
        value
    } else {
        value.checked_mul(1_000)?
    };
    Some(aco::util::epoch_ms_rfc3339(ms as u64))
}

/// First string/int a record carries under any of `keys`, descending one
/// `message` level — the shape `role`, `timestamp`, `title` and friends
/// share in native history.
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

/// The tool's recorded name on a structured row — the native fields the
/// store writes, in the order LicoUp's `structured_name` reads them.
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
}

/// Bounded text descent over the generic carriers a native payload uses.
/// Objects typed as tool/metadata blocks are structure, not text; the
/// embedded-JSON probe LicoUp runs for its generic adapters is not ported
/// — copilot payloads carry text in string fields.
fn extract_text(value: &Value) -> Option<String> {
    let mut pending = vec![value];
    let mut parts = Vec::new();
    while let Some(candidate) = pending.pop() {
        match candidate {
            Value::String(text) if !text.trim().is_empty() => {
                parts.push(text.clone());
            }
            Value::Array(items) => pending.extend(items.iter().rev()),
            Value::Object(object) => {
                if object_is_tool_or_metadata(object) {
                    continue;
                }
                let child = if object_is_text(object) {
                    object.get("text").or_else(|| object.get("content"))
                } else {
                    [
                        "text", "content", "message", "messages", "prompt", "response", "answer",
                        "summary", "value", "parts", "items", "turns",
                    ]
                    .iter()
                    .find_map(|key| object.get(*key).filter(|v| value_may_have_text(v)))
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

/// The model a record names: its own metadata containers only (the record,
/// `data`, `message`, `modelInfo`, `request`, `response`, usage objects —
/// never session state or content text), first concrete id wins, a
/// remembered placeholder stands as fallback — the port of LicoUp's
/// `extract_native_model`/`recorded_usage_model` over generic field names.
fn native_model(value: &Value) -> Option<String> {
    const MODEL_KEYS: &[&str] = &[
        "model",
        "modelId",
        "modelID",
        "model_id",
        "modelName",
        "model_name",
        "modelLabel",
        "model_label",
    ];
    const CONTAINER_KEYS: &[&str] = &[
        "usage",
        "tokenUsage",
        "token_usage",
        "usageMetadata",
        "modelInfo",
        "message",
        "response",
        "data",
        "payload",
        "request",
        "metadata",
    ];
    let mut objects = vec![value];
    let mut fallback = None::<String>;
    for _ in 0..=MAX_METADATA_DEPTH {
        let mut next = Vec::new();
        for object in objects {
            for key in MODEL_KEYS {
                let Some(model) = object.get(*key).and_then(Value::as_str) else {
                    continue;
                };
                let model = model.trim();
                if model.is_empty() || has_structured_selector_syntax(model) {
                    continue;
                }
                if is_concrete_model(model) {
                    return Some(model.to_string());
                }
                fallback.get_or_insert_with(|| model.to_string());
            }
            for key in CONTAINER_KEYS {
                if let Some(child) = object.get(*key).filter(|v| v.is_object()) {
                    next.push(child);
                }
            }
        }
        objects = next;
        if objects.is_empty() {
            break;
        }
    }
    fallback
}

/// The recorded model on the payload wins over one on the event envelope —
/// LicoUp's `prefer_recorded_model`: concrete primary, then concrete
/// related, then whatever was recorded.
fn prefer_model(primary: Option<String>, related: Option<String>) -> Option<String> {
    if primary.as_deref().is_some_and(is_concrete_model) {
        primary
    } else if related.as_deref().is_some_and(is_concrete_model) {
        related
    } else {
        primary.or(related)
    }
}

fn is_concrete_model(id: &str) -> bool {
    !is_placeholder_id(id) && !has_structured_selector_syntax(id)
}

fn is_placeholder_id(id: &str) -> bool {
    matches!(
        id.rsplit('/')
            .next()
            .unwrap_or(id)
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "" | "default" | "auto" | "unknown" | "unspecified" | "others"
    )
}

/// A serialized container or a field tail is not a model id.
fn has_structured_selector_syntax(raw: &str) -> bool {
    let raw = raw.trim();
    raw.starts_with(['{', '[', '"'])
}

/// The token counters a record carries — a bounded port of LicoUp's
/// `extract_token_usage` over generic field names. The usage container is
/// the record itself or its first nested `usage`/`data`/`payload`-style
/// object with explicit counters; additive cache reads/writes fold into
/// the prompt, reasoning counters into the completion, and a reported
/// total stands over the sum. The uncounted-counter refusals LicoUp
/// applies are not ported: a malformed counter reads as absent.
fn extract_token_usage(value: &Value) -> Option<Value> {
    let usage = usage_container(value)?;
    let prompt_base = first_count(usage, PROMPT_FIELDS);
    let cache_read = first_count(usage, CACHE_READ_FIELDS).unwrap_or(0);
    let cache_write = first_count(usage, CACHE_WRITE_FIELDS).unwrap_or(0);
    let completion_base = first_count(usage, COMPLETION_FIELDS);
    let completion = completion_base.unwrap_or(0)
        + first_count(usage, REASONING_FIELDS).unwrap_or(0)
        + first_count(usage, TOOL_USE_FIELDS).unwrap_or(0);
    let reported_total = first_count(usage, TOTAL_FIELDS);
    let cached = first_count(usage, CACHED_FIELDS)
        .unwrap_or(0)
        .min(prompt_base.unwrap_or(0))
        + cache_read;
    let explicit = prompt_base.is_some()
        || cache_read > 0
        || cache_write > 0
        || completion_base.is_some()
        || reported_total.is_some();
    if !explicit {
        return None;
    }
    let prompt = prompt_base.unwrap_or(0) + cache_read + cache_write;
    Some(json!({
        "promptTokens": prompt,
        "cachedInputTokens": cached.min(prompt),
        "completionTokens": completion,
        "totalTokens": reported_total.unwrap_or(prompt + completion),
        "source": "explicit"
    }))
}

/// The first object carrying explicit counter fields: the record itself,
/// then its usage containers level by level — matching the native
/// reader's "first child with counts wins" descent.
fn usage_container(value: &Value) -> Option<&Value> {
    let mut objects = vec![value];
    for _ in 0..=MAX_METADATA_DEPTH {
        if let Some(object) = objects.iter().find(|o| has_counter_fields(o)) {
            return Some(object);
        }
        let mut next = Vec::new();
        for object in objects {
            if let Value::Object(map) = object {
                for key in [
                    "usage",
                    "tokenUsage",
                    "token_usage",
                    "usageMetadata",
                    "usage_metadata",
                    "tokenCount",
                    "token_count",
                    "responseUsage",
                    "response_usage",
                    "tokens",
                    "data",
                    "payload",
                    "status",
                ] {
                    if let Some(child) = map.get(key).filter(|v| v.is_object()) {
                        next.push(child);
                    }
                }
            }
        }
        if next.is_empty() {
            return None;
        }
        objects = next;
    }
    None
}

fn has_counter_fields(value: &Value) -> bool {
    PROMPT_FIELDS
        .iter()
        .chain(COMPLETION_FIELDS)
        .chain(TOTAL_FIELDS)
        .chain(CACHED_FIELDS)
        .chain(CACHE_READ_FIELDS)
        .chain(CACHE_WRITE_FIELDS)
        .any(|key| counter_value(value.get(*key)).is_some())
}

fn first_count(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| counter_value(value.get(*key)))
}

/// A counter reads as a non-negative integer; numeric strings count too.
fn counter_value(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(number) => number
            .as_u64()
            .or_else(|| number.as_f64().and_then(|v| (v >= 0.0).then_some(v as u64))),
        Value::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    }
}

const PROMPT_FIELDS: &[&str] = &[
    "promptTokens",
    "prompt_tokens",
    "inputTokens",
    "input_tokens",
    "totalInputTokens",
    "total_input_tokens",
    "promptTokenCount",
    "prompt_token_count",
    "inputOther",
    "input_other",
    "input",
];
const CACHED_FIELDS: &[&str] = &[
    "cachedInputTokens",
    "cached_input_tokens",
    "inputCachedTokens",
    "input_cached_tokens",
    "totalCachedTokens",
    "total_cached_tokens",
    "cachedContentTokenCount",
    "cached_content_token_count",
];
const CACHE_READ_FIELDS: &[&str] = &[
    "cacheReadInputTokens",
    "cache_read_input_tokens",
    "cacheReadTokens",
    "cache_read_tokens",
    "cacheRead",
    "cache_read",
    "inputCacheRead",
    "input_cache_read",
];
const CACHE_WRITE_FIELDS: &[&str] = &[
    "cacheCreationInputTokens",
    "cache_creation_input_tokens",
    "cacheWriteInputTokens",
    "cache_write_input_tokens",
    "cacheWriteTokens",
    "cache_write_tokens",
    "cacheWrite",
    "cache_write",
    "inputCacheWriteTokens",
    "input_cache_write_tokens",
    "inputCacheCreation",
    "input_cache_creation",
];
const COMPLETION_FIELDS: &[&str] = &[
    "completionTokens",
    "completion_tokens",
    "outputTokens",
    "output_tokens",
    "responseTokens",
    "response_tokens",
    "totalOutputTokens",
    "total_output_tokens",
    "candidatesTokenCount",
    "candidates_token_count",
    "output",
];
const REASONING_FIELDS: &[&str] = &[
    "reasoningTokens",
    "reasoning_tokens",
    "thoughtsTokenCount",
    "thoughts_token_count",
    "totalThoughtTokens",
    "total_thought_tokens",
];
const TOOL_USE_FIELDS: &[&str] = &["totalToolUseTokens", "total_tool_use_tokens"];
const TOTAL_FIELDS: &[&str] = &[
    "totalTokens",
    "total_tokens",
    "totalTokenCount",
    "total_token_count",
    "total",
];

fn user_attachments(data: &Value) -> Vec<Attachment> {
    data.get("attachments")
        .and_then(Value::as_array)
        .map(|attachments| {
            attachments
                .iter()
                .map(|attachment| Attachment {
                    kind: attachment
                        .get("type")
                        .or_else(|| attachment.get("kind"))
                        .and_then(Value::as_str)
                        .unwrap_or("attachment")
                        .to_string(),
                    media_type: attachment
                        .get("mediaType")
                        .or_else(|| attachment.get("mimeType"))
                        .or_else(|| attachment.get("contentType"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    data: attachment.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn encode_attachments(attachments: &[Attachment]) -> Value {
    if attachments.is_empty() {
        return Value::Null;
    }
    Value::Array(
        attachments
            .iter()
            .map(|attachment| attachment.data.clone())
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Encode helpers.
// ---------------------------------------------------------------------------

fn is_usage_meta(rec: &Record) -> bool {
    matches!(&rec.body, Body::Meta { subtype, .. } if subtype == "usage")
}

/// Emit one `assistant.message` row for a run of assistant-side records:
/// text carries `data.content`, reasoning joins `data.reasoningText`, tool
/// calls land in `data.toolRequests`, and a trailing usage meta becomes
/// `data.usage` — the way the transcript persists them. A group with no
/// assistant record renders its parts standalone (`tool.execution_start`
/// rows, an `assistant.reasoning` row, a bare usage row).
fn flush_assistant(pending: &mut Vec<&Record>, lines: &mut Vec<String>) {
    if pending.is_empty() {
        return;
    }
    let ts = pending.first().and_then(|r| r.ts.clone());
    let mut data = json!({});
    let mut assistant = false;
    for rec in pending.iter() {
        match &rec.body {
            Body::Assistant { text, model } => {
                data["content"] = json!(text);
                if let Some(model) = model {
                    data["model"] = json!(model);
                }
                if let Some(id) = &rec.id {
                    data["messageId"] = json!(id);
                }
                assistant = true;
            }
            Body::Reasoning { text } => {
                let existing = data["reasoningText"].as_str().unwrap_or_default();
                data["reasoningText"] = if existing.is_empty() {
                    json!(text)
                } else {
                    json!(format!("{existing}\n{text}"))
                };
            }
            Body::ToolCall {
                call_id,
                name,
                args,
            } => {
                if !data["toolRequests"].is_array() {
                    data["toolRequests"] = json!([]);
                }
                data["toolRequests"].as_array_mut().unwrap().push(json!({
                    "toolCallId": call_id,
                    "name": name,
                    "arguments": args,
                }));
            }
            Body::Meta { data: usage, .. } => {
                data["usage"] = usage.clone();
            }
            _ => {}
        }
    }
    if assistant {
        lines.push(
            json!({
                "type": "assistant.message",
                "data": data,
                "timestamp": ts,
            })
            .to_string(),
        );
    } else {
        for rec in pending.iter() {
            match &rec.body {
                Body::Reasoning { text } => lines.push(
                    json!({
                        "type": "assistant.reasoning",
                        "data": {"content": text},
                        "timestamp": rec.ts,
                    })
                    .to_string(),
                ),
                Body::ToolCall {
                    call_id,
                    name,
                    args,
                } => lines.push(
                    json!({
                        "type": "tool.execution_start",
                        "data": {
                            "toolCallId": call_id,
                            "toolName": name,
                            "arguments": args,
                        },
                        "timestamp": rec.ts,
                    })
                    .to_string(),
                ),
                Body::Meta { subtype, data } => lines.push(
                    json!({
                        "type": subtype,
                        "data": data,
                        "timestamp": rec.ts,
                    })
                    .to_string(),
                ),
                _ => {}
            }
        }
    }
    pending.clear();
}

/// A meta record back into a native row: a preserved row re-emits
/// verbatim; a bare meta wraps its data under its subtype tag.
fn meta_row(rec: &Record) -> String {
    if let Body::Meta { subtype, data } = &rec.body {
        if data.get("type").and_then(Value::as_str).is_some() {
            return data.to_string();
        }
        return json!({
            "type": subtype,
            "data": data,
            "timestamp": rec.ts,
        })
        .to_string();
    }
    unreachable!("meta_row on non-meta record")
}
