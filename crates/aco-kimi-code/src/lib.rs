//! `aco-kimi-code` — the Kimi Code conversation-store codec.
//!
//! Kimi Code keeps each conversation as a session directory holding one
//! `state.json` (title, `workDir`, per-agent facts) plus one wire log per
//! agent at `agents/<id>/wire.jsonl` under `~/.kimi-code`; `KIMI_CODE_HOME`
//! relocates the whole `.kimi-code` tree the way the CLI's own tooling
//! honors. Every wire row is one JSON object whose `type` drives the
//! projection: `turn.prompt` carries user input, `context.append_message`
//! buffers fallback messages for stores that log prompts there,
//! `context.append_loop_event` wraps the model's loop events —
//! `content.part` text/think fragments, `tool.call`, `tool.result` — and
//! `usage.record`/`StatusUpdate` rows carry token counters.
//!
//! Text and think fragments stream in pieces, so the decoder accumulates
//! them and flushes one record per contiguous run, regrouped by
//! `turn`\n`step`: a new part kind or a group change ends the open run.
//! Delegated subagents are sibling wire logs whose session id encodes the
//! ancestry as `<session>:<agent>`.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use aco::codec::SessionRef;
use aco::codec::{Capability, CapabilityMatrix, Codec, Decoded, Encoded, Skipped};
use aco::model::{Body, Conversation, Record};
use aco::store::HistoryRoot;
use aco::{Error, Result};
use serde_json::{json, Map, Value};

const SESSION_KINDS: &[&str] = &["kimi-session-store"];

static CAPABILITIES: CapabilityMatrix = CapabilityMatrix::new(
    true,
    [
        Capability::Native,      // user — `turn.prompt`
        Capability::Native,      // assistant — `content.part` text runs
        Capability::Native,      // reasoning — `content.part` think runs
        Capability::Native,      // tool-call — `tool.call`
        Capability::Native,      // tool-result — `tool.result`
        Capability::Unsupported, // system — the wire records no harness rows
        Capability::Adapted,     // meta — `usage.record`/`metadata` rows
    ],
);

/// The one codec instance aco's registry hands out.
pub struct KimiCodeCodec;

impl KimiCodeCodec {
    pub fn new() -> Self {
        Self
    }
}

impl Default for KimiCodeCodec {
    fn default() -> Self {
        Self::new()
    }
}

/// The registered codec instance.
pub fn codec() -> &'static dyn Codec {
    &KimiCodeCodec
}

fn extension_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

impl Codec for KimiCodeCodec {
    fn id(&self) -> &'static str {
        "kimicode"
    }

    fn history(&self) -> Vec<HistoryRoot> {
        a4aa_kimi_code::card().history.to_vec()
    }

    fn transcript_usage(&self) -> Option<aco::TranscriptUsage> {
        a4aa_kimi_code::card().usage.transcript
    }

    fn session_kinds(&self) -> &'static [&'static str] {
        SESSION_KINDS
    }

    fn capabilities(&self) -> &'static CapabilityMatrix {
        &CAPABILITIES
    }

    /// The wire path carries identity: `<session>/agents/<agent>/wire.jsonl`,
    /// `main` is the conversation itself and any other agent slot is a
    /// delegated subagent rendered `<session>:<agent>`.
    fn session_id(&self, path: &Path) -> String {
        native_session_id(path)
    }

    /// `KIMI_CODE_HOME` relocates the `.kimi-code` tree — every root under
    /// `{home}/.kimi-code` re-resolves beneath it, the tree root itself
    /// becomes the override.
    fn expand_root(&self, root: &HistoryRoot, home: &Path) -> PathBuf {
        if root.kind == "kimi-session-store" {
            if let Some(kimi_home) = std::env::var("KIMI_CODE_HOME")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(PathBuf::from)
            {
                let expanded = aco::util::expand_home(root.path, home);
                let native = home.join(".kimi-code");
                if let Ok(relative) = expanded.strip_prefix(&native) {
                    return kimi_home.join(relative);
                }
                return kimi_home;
            }
        }
        aco::util::expand_home(root.path, home)
    }

    /// Only `agents/<id>/wire.jsonl` artifacts are session rows — the same
    /// acceptance the wire catalog applies (`*.jsonl` named `wire.jsonl`
    /// beneath an `agents` component).
    fn select_session_files(&self, files: Vec<PathBuf>) -> Vec<PathBuf> {
        files
            .into_iter()
            .filter(|path| {
                path.extension().and_then(|e| e.to_str()) == Some("jsonl")
                    && path.file_name().and_then(|n| n.to_str()) == Some("wire.jsonl")
                    && path
                        .components()
                        .any(|component| component.as_os_str() == "agents")
            })
            .collect()
    }

    /// `.json` artifacts decode through the shared document lane;
    /// kimi wire transcripts are `.jsonl`.
    fn decode(&self, path: &Path) -> Result<Decoded> {
        if extension_of(path) == "json" {
            return aco::generic::decode_json_document(path, "kimicode");
        }
        let file = File::open(path).map_err(Error::io(path))?;
        let mut conv = Conversation::new(native_session_id(path));
        conv.agent = Some("kimicode".to_string());
        apply_session_state(path, &mut conv);

        let mut records = Vec::new();
        let mut record_lines = Vec::new();
        let mut skipped = Vec::new();
        let mut line_count = 0;
        let mut saw_user = false;
        let mut saw_agent = false;
        let mut fallback_user: Vec<(usize, Record)> = Vec::new();
        let mut fallback_agent: Vec<(usize, Record)> = Vec::new();
        let mut assistant = Accum::default();
        let mut reasoning = ReasoningAccum::default();

        for (index, line) in BufReader::new(file).lines().enumerate() {
            line_count = index + 1;
            let Ok(line) = line else { continue };
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
            let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
            match kind {
                "turn.prompt" => {
                    flush_assistant(&mut records, &mut record_lines, &mut assistant);
                    flush_reasoning(&mut records, &mut record_lines, &mut reasoning);
                    if let Some(text) = value.get("input").and_then(extract_text) {
                        push(
                            &mut records,
                            &mut record_lines,
                            index,
                            timestamp(&value),
                            Body::User {
                                text,
                                model: None,
                                attachments: Vec::new(),
                            },
                        );
                        saw_user = true;
                    }
                }
                // Stores that log prompts through the context buffer keep them
                // here as fallbacks: released at the first usage row or end of
                // file when no primary event of that side ever appeared.
                "context.append_message" => {
                    let mut message = value.get("message").cloned().unwrap_or(Value::Null);
                    if let Some(object) = message.as_object_mut() {
                        if !object.contains_key("time") {
                            if let Some(time) = value.get("time") {
                                object.insert("time".to_string(), time.clone());
                            }
                        }
                    }
                    let role = message
                        .get("role")
                        .and_then(Value::as_str)
                        .unwrap_or("user");
                    let ts = timestamp(&message);
                    if let Some(text) = extract_text(&message) {
                        match role {
                            "user" | "human" => fallback_user.push((
                                index,
                                record(
                                    ts,
                                    Body::User {
                                        text,
                                        model: None,
                                        attachments: Vec::new(),
                                    },
                                ),
                            )),
                            "agent" | "assistant" | "model" | "ai" => fallback_agent
                                .push((index, record(ts, Body::Assistant { text, model: None }))),
                            _ => {}
                        }
                    }
                }
                "context.append_loop_event" => {
                    let event = value.get("event").unwrap_or(&value);
                    let semantic = event.get("type").and_then(Value::as_str).unwrap_or("");
                    let ts = timestamp(&value).or_else(|| timestamp(event));
                    match semantic {
                        "content.part" => {
                            let part = event.get("part").unwrap_or(event);
                            let group = content_group(&value, event);
                            match part.get("type").and_then(Value::as_str) {
                                Some("text") => {
                                    flush_reasoning(
                                        &mut records,
                                        &mut record_lines,
                                        &mut reasoning,
                                    );
                                    if assistant
                                        .group
                                        .as_deref()
                                        .is_some_and(|active| active != group)
                                    {
                                        flush_assistant(
                                            &mut records,
                                            &mut record_lines,
                                            &mut assistant,
                                        );
                                    }
                                    if let Some(text) = part.get("text").and_then(extract_text) {
                                        if !text.is_empty() {
                                            if assistant.text.is_empty() {
                                                assistant.line = index;
                                                assistant.ts = ts;
                                                assistant.group = Some(group);
                                            }
                                            assistant.text.push_str(&text);
                                            saw_agent = true;
                                        }
                                    }
                                }
                                Some("think") => {
                                    flush_assistant(
                                        &mut records,
                                        &mut record_lines,
                                        &mut assistant,
                                    );
                                    if reasoning
                                        .group
                                        .as_deref()
                                        .is_some_and(|active| active != group)
                                    {
                                        flush_reasoning(
                                            &mut records,
                                            &mut record_lines,
                                            &mut reasoning,
                                        );
                                    }
                                    if reasoning.text.is_empty() {
                                        reasoning.line = index;
                                        reasoning.ts = ts;
                                        reasoning.group = Some(group);
                                    }
                                    if let Some(text) = part
                                        .get("think")
                                        .or_else(|| part.get("text"))
                                        .and_then(extract_text)
                                    {
                                        reasoning.text.push_str(&text);
                                    }
                                    for key in ["summary", "reasoningSummary", "reasoning_summary"]
                                    {
                                        if let Some(summary) = part.get(key) {
                                            reasoning.summaries.push(summary.clone());
                                        }
                                    }
                                }
                                other => skipped.push(Skipped {
                                    line: index,
                                    native_type: Some(format!(
                                        "content.part:{}",
                                        other.unwrap_or("")
                                    )),
                                    reason: "projection".to_string(),
                                }),
                            }
                        }
                        "tool.call" | "tool.result" => {
                            flush_assistant(&mut records, &mut record_lines, &mut assistant);
                            flush_reasoning(&mut records, &mut record_lines, &mut reasoning);
                            let body = if semantic == "tool.call" {
                                Body::ToolCall {
                                    call_id: call_id(event),
                                    name: event
                                        .get("name")
                                        .or_else(|| event.get("tool"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("tool")
                                        .to_string(),
                                    args: event
                                        .get("arguments")
                                        .or_else(|| event.get("input"))
                                        .or_else(|| event.get("args"))
                                        .cloned()
                                        .unwrap_or(Value::Null),
                                }
                            } else {
                                Body::ToolResult {
                                    call_id: call_id(event),
                                    name: event
                                        .get("name")
                                        .and_then(Value::as_str)
                                        .map(str::to_string),
                                    content: event
                                        .get("result")
                                        .or_else(|| event.get("message"))
                                        .or_else(|| event.get("content"))
                                        .cloned()
                                        .unwrap_or(Value::Null),
                                    is_error: event
                                        .get("isError")
                                        .or_else(|| event.get("error"))
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false),
                                }
                            };
                            push(&mut records, &mut record_lines, index, ts, body);
                        }
                        _ => skipped.push(Skipped {
                            line: index,
                            native_type: Some(format!("loop:{semantic}")),
                            reason: "projection".to_string(),
                        }),
                    }
                }
                "usage.record" | "StatusUpdate" | "status.update" | "status_update" => {
                    if !saw_user {
                        release(&mut records, &mut record_lines, &mut fallback_user);
                    }
                    if !saw_agent {
                        release(&mut records, &mut record_lines, &mut fallback_agent);
                    }
                    flush_assistant(&mut records, &mut record_lines, &mut assistant);
                    flush_reasoning(&mut records, &mut record_lines, &mut reasoning);
                    if let Some(body) = usage_meta(&value, kind) {
                        push(
                            &mut records,
                            &mut record_lines,
                            index,
                            timestamp(&value),
                            body,
                        );
                    }
                }
                // Store-internal rows stay as metadata — the transcript never
                // silently loses a persisted row type it names.
                "metadata" | "session_meta" => {
                    push(
                        &mut records,
                        &mut record_lines,
                        index,
                        timestamp(&value),
                        Body::Meta {
                            subtype: kind.to_string(),
                            data: value.clone(),
                        },
                    );
                }
                other => skipped.push(Skipped {
                    line: index,
                    native_type: if other.is_empty() {
                        None
                    } else {
                        Some(other.to_string())
                    },
                    reason: "projection".to_string(),
                }),
            }
        }
        flush_assistant(&mut records, &mut record_lines, &mut assistant);
        flush_reasoning(&mut records, &mut record_lines, &mut reasoning);
        if !saw_user {
            release(&mut records, &mut record_lines, &mut fallback_user);
        }
        if !saw_agent {
            release(&mut records, &mut record_lines, &mut fallback_agent);
        }
        conv.updated_at = records
            .last()
            .and_then(|r| r.ts.clone())
            .or(conv.updated_at.clone())
            .or(conv.created_at.clone());
        conv.created_at = conv
            .created_at
            .clone()
            .or_else(|| records.first().and_then(|r| r.ts.clone()));
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

    /// Emit wire-shaped rows: prompts and loop events. `usage` meta rows go
    /// back as `usage.record`; other meta rows ride the store's `metadata`
    /// envelope with their subtype carried through.
    /// A `.json` document holding `sessions[]` items or a `.jsonl`
    /// transcript declaring several session groups enumerates one ref
    /// per unit; everything else stays one session per file.
    fn sessions_in_file(&self, path: &Path) -> Result<Vec<SessionRef>> {
        match extension_of(path).as_str() {
            "json" => {
                let refs = aco::generic::json_document_session_refs(path);
                if !refs.is_empty() {
                    return Ok(refs);
                }
            }
            "jsonl" | "ndjson" => {
                let refs = aco::generic::jsonl_session_refs(path);
                if !refs.is_empty() {
                    return Ok(refs);
                }
            }
            _ => {}
        }
        Ok(vec![SessionRef {
            id: self.session_id(path),
            path: path.to_path_buf(),
            key: None,
            parent: None,
        }])
    }

    /// `key = Some` selects one container unit; `key = None` decodes
    /// the file.
    fn decode_session(&self, session: &SessionRef) -> Result<Decoded> {
        match session.key.as_deref() {
            None => self.decode(&session.path),
            Some(key) => match extension_of(&session.path).as_str() {
                "json" => {
                    aco::generic::decode_json_document_session(&session.path, key, "kimicode")
                }
                "jsonl" | "ndjson" => {
                    aco::generic::decode_jsonl_session(&session.path, key, "kimicode")
                }
                _ => self.decode(&session.path),
            },
        }
    }

    fn encode(&self, conversation: &Conversation) -> Result<Encoded> {
        let (kept, projection) = self.project(conversation);
        let mut lines = Vec::with_capacity(kept.len());
        for rec in kept {
            let line = match &rec.body {
                Body::User { text, .. } => json!({
                    "type": "turn.prompt",
                    "time": rec.ts,
                    "input": text,
                }),
                Body::Assistant { text, .. } => loop_event(
                    &rec.ts,
                    json!({"type": "content.part", "part": {"type": "text", "text": text}}),
                ),
                Body::Reasoning { text } => loop_event(
                    &rec.ts,
                    json!({"type": "content.part", "part": {"type": "think", "think": text}}),
                ),
                Body::ToolCall {
                    call_id,
                    name,
                    args,
                } => loop_event(&rec.ts, {
                    let mut event = json!({"type": "tool.call", "name": name, "arguments": args});
                    if let Some(id) = call_id {
                        event["callId"] = json!(id);
                    }
                    event
                }),
                Body::ToolResult {
                    call_id,
                    name,
                    content,
                    is_error,
                } => loop_event(&rec.ts, {
                    let mut event = json!({
                        "type": "tool.result",
                        "name": name,
                        "result": content,
                        "isError": is_error,
                    });
                    if let Some(id) = call_id {
                        event["callId"] = json!(id);
                    }
                    event
                }),
                Body::Meta { subtype, data } if subtype == "usage" => json!({
                    "type": "usage.record",
                    "time": rec.ts,
                    "model": data.get("model"),
                    "usageScope": data.get("usageScope").cloned().unwrap_or(json!("turn")),
                    "usage": data.get("usage").cloned().unwrap_or(Value::Null),
                }),
                Body::Meta { subtype, data } => json!({
                    "type": "metadata",
                    "subtype": subtype,
                    "data": data,
                }),
                _ => continue,
            };
            lines.push(line.to_string());
        }
        Ok(Encoded { lines, projection })
    }

    /// Imported sessions land as the `main` agent's wire log under a session
    /// directory of their own — the session id's `:<agent>` suffix, when an
    /// imported id keeps one, selects the agent slot the way the store does.
    fn store_path(&self, home: &Path, conversation: &Conversation) -> Result<PathBuf> {
        let root = self
            .history()
            .into_iter()
            .find(|r| r.kind == "kimi-session-store")
            .map(|r| self.expand_root(&r, home))
            .ok_or_else(|| Error::Usage("kimi session root undeclared".to_string()))?;
        let (session, agent) = conversation
            .session_id
            .split_once(':')
            .unwrap_or((conversation.session_id.as_str(), "main"));
        let project = conversation
            .cwd
            .as_deref()
            .and_then(|cwd| Path::new(cwd).file_name())
            .and_then(|name| name.to_str())
            .map(|name| name.replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '_', "-"))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "imported".to_string());
        Ok(root
            .join(project)
            .join(session)
            .join("agents")
            .join(agent)
            .join("wire.jsonl"))
    }

    /// Rewrite the text one wire row carries. Structural tags (`type`,
    /// `role`, names) stay; content keys — `input`, `text`, `think`,
    /// `content`, `result`, `arguments`, `summary` — lose every string they
    /// held.
    fn redact_line(&self, line: &Value, marker: &str) -> Option<Value> {
        let kind = line.get("type")?.as_str()?;
        let mut row = line.clone();
        let touched = match kind {
            "turn.prompt" => row
                .get_mut("input")
                .map(|input| match input {
                    Value::String(_) => {
                        *input = json!(marker);
                        true
                    }
                    other => redact_text_fields(other, marker),
                })
                .unwrap_or(false),
            "context.append_message" => row
                .get_mut("message")
                .map(|m| redact_text_fields(m, marker))
                .unwrap_or(false),
            "context.append_loop_event" => {
                let event = if row.get("event").is_some() {
                    row.get_mut("event")?
                } else {
                    &mut row
                };
                redact_text_fields(event, marker)
            }
            _ => false,
        };
        touched.then_some(row)
    }
}

/// Text-bearing keys inside wire payloads; everything a matching key holds
/// is message content, so its strings are all redactable.
const TEXT_KEYS: &[&str] = &[
    "text",
    "think",
    "content",
    "input",
    "prompt",
    "message",
    "result",
    "output",
    "arguments",
    "args",
    "summary",
    "response",
    "answer",
];

/// Accumulated text-part run: flushes as one assistant record.
#[derive(Default)]
struct Accum {
    text: String,
    line: usize,
    ts: Option<String>,
    group: Option<String>,
}

/// Accumulated think-part run plus any provider summaries it carried.
#[derive(Default)]
struct ReasoningAccum {
    text: String,
    summaries: Vec<Value>,
    line: usize,
    ts: Option<String>,
    group: Option<String>,
}

fn record(ts: Option<String>, body: Body) -> Record {
    Record {
        id: None,
        parent: None,
        ts,
        ordinal: 0,
        body,
    }
}

fn push(
    records: &mut Vec<Record>,
    lines: &mut Vec<usize>,
    line: usize,
    ts: Option<String>,
    body: Body,
) {
    records.push(record(ts, body));
    lines.push(line);
}

/// Emit one assistant record for the open text run, if any.
fn flush_assistant(records: &mut Vec<Record>, lines: &mut Vec<usize>, accum: &mut Accum) {
    if accum.text.is_empty() {
        return;
    }
    push(
        records,
        lines,
        accum.line,
        accum.ts.take(),
        Body::Assistant {
            text: std::mem::take(&mut accum.text),
            model: None,
        },
    );
    accum.group = None;
}

/// Emit one reasoning record for the open think run; provider summaries
/// ride beside it as a `reasoning-summary` meta row on the same line.
fn flush_reasoning(records: &mut Vec<Record>, lines: &mut Vec<usize>, accum: &mut ReasoningAccum) {
    if accum.text.is_empty() && accum.summaries.is_empty() {
        return;
    }
    let text = std::mem::take(&mut accum.text);
    let summaries = std::mem::take(&mut accum.summaries);
    push(
        records,
        lines,
        accum.line,
        accum.ts.take(),
        Body::Reasoning { text },
    );
    if !summaries.is_empty() {
        push(
            records,
            lines,
            accum.line,
            None,
            Body::Meta {
                subtype: "reasoning-summary".to_string(),
                data: json!({ "summary": summaries }),
            },
        );
    }
    accum.group = None;
}

/// Move buffered `context.append_message` records into the transcript at
/// their release point, keeping each one's own source line.
fn release(records: &mut Vec<Record>, lines: &mut Vec<usize>, buffered: &mut Vec<(usize, Record)>) {
    for (line, rec) in buffered.drain(..) {
        records.push(rec);
        lines.push(line);
    }
}

/// `<session>/agents/<agent>/wire.jsonl` → `<session>` or `<session>:<agent>`.
fn native_session_id(path: &Path) -> String {
    let agent = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|v| v.to_str())
        .unwrap_or("main");
    let session = path
        .ancestors()
        .nth(3)
        .and_then(Path::file_name)
        .and_then(|v| v.to_str())
        .unwrap_or("session");
    if agent == "main" {
        session.to_string()
    } else {
        format!("{session}:{agent}")
    }
}

/// Session-level facts live beside the wire logs in `<session>/state.json`.
fn session_state(path: &Path) -> Option<Value> {
    let root = path.ancestors().nth(3)?;
    let raw = std::fs::read_to_string(root.join("state.json")).ok()?;
    serde_json::from_str(&raw).ok()
}

fn apply_session_state(path: &Path, conv: &mut Conversation) {
    let Some(state) = session_state(path) else {
        return;
    };
    conv.title = find_string(
        &state,
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
    .filter(|t| !t.trim().is_empty())
    .or(conv.title.take());
    // `workDir` is honored only as the absolute path the session recorded.
    conv.cwd = state
        .get("workDir")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| {
            !v.is_empty() && v.len() <= 4096 && !v.contains('\0') && Path::new(v).is_absolute()
        })
        .map(str::to_string)
        .or(conv.cwd.take());
    conv.created_at = state
        .get("createdAt")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(conv.created_at.take());
    conv.updated_at = state
        .get("updatedAt")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(conv.updated_at.take());
}

/// Contiguous-run key: fragments of one model step share the enclosing
/// `turn` and the event's `step`; a new step starts a new run.
fn content_group(value: &Value, event: &Value) -> String {
    let turn = find_string(value, &["turnId", "turn_id"])
        .or_else(|| find_string(event, &["turnId", "turn_id"]))
        .unwrap_or_default();
    let step = find_string(event, &["step", "stepId", "step_id"]).unwrap_or_default();
    format!("{turn}\n{step}")
}

fn call_id(event: &Value) -> Option<String> {
    event
        .get("callId")
        .or_else(|| event.get("call_id"))
        .or_else(|| event.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// First string (or integer rendered as one) under any of `keys`, following
/// the wire's `message` nesting the way the source parser does.
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

/// Event timestamps: the wire stamps rows under `time`-family keys, as an
/// RFC 3339 string or epoch milliseconds — normalized to RFC 3339.
fn timestamp(value: &Value) -> Option<String> {
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
    .map(|raw| {
        raw.trim()
            .parse::<u64>()
            .ok()
            .map(aco::util::epoch_ms_rfc3339)
            .unwrap_or(raw)
    })
}

/// The text a payload carries: a plain string, joined across arrays, or
/// lifted out of the wire's content-carrying keys. Typed parts yield their
/// own `text`/`content` first.
fn extract_text(value: &Value) -> Option<String> {
    extract_text_depth(value, 0)
}

fn extract_text_depth(value: &Value, depth: u8) -> Option<String> {
    if depth > 32 {
        return None;
    }
    match value {
        Value::String(text) => non_empty(text),
        Value::Array(items) => {
            let text: String = items
                .iter()
                .filter_map(|item| extract_text_depth(item, depth + 1))
                .collect::<Vec<_>>()
                .join("\n");
            non_empty(&text)
        }
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("text") {
                return object
                    .get("text")
                    .or_else(|| object.get("content"))
                    .and_then(|v| extract_text_depth(v, depth + 1));
            }
            [
                "text", "content", "message", "messages", "prompt", "response", "answer",
                "summary", "value", "parts", "items", "turns",
            ]
            .iter()
            .find_map(|key| {
                object
                    .get(*key)
                    .and_then(|v| extract_text_depth(v, depth + 1))
            })
        }
        _ => None,
    }
}

fn non_empty(text: &str) -> Option<String> {
    (!text.trim().is_empty()).then(|| text.to_string())
}

/// The settled usage one usage/status row carries — normalized to the
/// session's own `{promptTokens, cachedInputTokens, completionTokens,
/// totalTokens}` shape. Session-scoped or zero-total rows report nothing.
fn usage_meta(value: &Value, native_type: &str) -> Option<Body> {
    let scope = find_string(value, &["usageScope", "usage_scope"]);
    if scope
        .as_deref()
        .is_some_and(|s| !s.eq_ignore_ascii_case("turn"))
    {
        return None;
    }
    let usage = token_usage(value)?;
    if usage
        .get("totalTokens")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        == 0
    {
        return None;
    }
    let mut data = Map::new();
    data.insert("usage".to_string(), usage);
    data.insert(
        "usageScope".to_string(),
        json!(scope.unwrap_or_else(|| "turn".to_string())),
    );
    data.insert("sourceEventType".to_string(), json!(native_type));
    if let Some(model) = find_string(value, &["model"]) {
        data.insert("model".to_string(), json!(model));
    }
    Some(Body::Meta {
        subtype: "usage".to_string(),
        data: Value::Object(data),
    })
}

/// Normalize the wire's token counters. `inputOther`/`input_cache_*` are
/// the uncached and cached input buckets — prompt totals sum them unless an
/// explicit `promptTokens`/`inputTokens` total is already reported.
fn token_usage(value: &Value) -> Option<Value> {
    let usage = value
        .get("usage")
        .or_else(|| value.get("token_usage"))
        .filter(|u| u.is_object())?;
    let explicit_prompt = count(
        usage,
        &[
            "promptTokens",
            "prompt_tokens",
            "inputTokens",
            "input_tokens",
        ],
    );
    let base = count(usage, &["inputOther", "input_other", "input"]);
    let cached = count(
        usage,
        &[
            "inputCacheRead",
            "input_cache_read",
            "cachedInputTokens",
            "cached_tokens",
        ],
    )
    .unwrap_or(0);
    let creation = count(
        usage,
        &[
            "inputCacheCreation",
            "input_cache_creation",
            "cacheCreation",
            "cache_creation",
        ],
    )
    .unwrap_or(0);
    let completion = count(
        usage,
        &[
            "output",
            "outputTokens",
            "output_tokens",
            "completionTokens",
            "completion_tokens",
        ],
    )
    .unwrap_or(0);
    let prompt = explicit_prompt.or_else(|| base.map(|b| b + cached + creation))?;
    let total =
        count(usage, &["totalTokens", "total_tokens", "total"]).unwrap_or(prompt + completion);
    Some(json!({
        "promptTokens": prompt,
        "cachedInputTokens": cached,
        "completionTokens": completion,
        "totalTokens": total,
    }))
}

fn count(object: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .find_map(|key| object.get(*key))
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
}

fn loop_event(ts: &Option<String>, event: Value) -> Value {
    json!({"type": "context.append_loop_event", "time": ts, "event": event})
}

/// Descend a payload; every value a text key holds is content and loses all
/// its strings, while structural fields stay untouched.
fn redact_text_fields(value: &mut Value, marker: &str) -> bool {
    let mut touched = false;
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if TEXT_KEYS.contains(&key.as_str()) {
                    touched |= redact_strings(child, marker);
                } else {
                    touched |= redact_text_fields(child, marker);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                touched |= redact_text_fields(item, marker);
            }
        }
        _ => {}
    }
    touched
}

/// Replace every string a value subtree carries.
fn redact_strings(value: &mut Value, marker: &str) -> bool {
    match value {
        Value::String(_) => {
            *value = json!(marker);
            true
        }
        Value::Array(items) => {
            let mut touched = false;
            for item in items {
                touched |= redact_strings(item, marker);
            }
            touched
        }
        Value::Object(map) => {
            let mut touched = false;
            for (_, child) in map.iter_mut() {
                touched |= redact_strings(child, marker);
            }
            touched
        }
        _ => false,
    }
}
