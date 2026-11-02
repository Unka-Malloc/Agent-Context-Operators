//! The `claude` codec: `~/.claude/projects/<cwd-slug>/<session>.jsonl`
//! session transcripts.
//!
//! Native record shape: one JSON object per line, a `type` tag per line.
//! `user`/`assistant` lines carry a `message.content` part array (or a bare
//! string for user text); each part fans out into its own canonical record
//! — text, thinking, tool_use, tool_result, image. `system` lines carry
//! `{subtype, content}`; every other line type (last-prompt, mode,
//! permission-mode, ai-title, file-history-snapshot, …) decodes as `meta`
//! so an exported document never drops store state silently.

use std::path::{Path, PathBuf};

use acop::codec::SessionRef;
use acop::{
    Attachment, Body, Capability, CapabilityMatrix, Codec, Conversation, Decoded, Encoded, Error,
    Record, Result, Skipped,
};
use serde_json::{json, Value};

fn extension_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

const CAPS: CapabilityMatrix = CapabilityMatrix::new(
    true,
    [
        Capability::Native,      // user
        Capability::Native,      // assistant
        Capability::Native,      // reasoning (thinking parts)
        Capability::Native,      // tool-call (tool_use parts)
        Capability::Native,      // tool-result (tool_result parts)
        Capability::Native,      // system
        Capability::Unsupported, // meta
    ],
);

const SESSION_KINDS: &[&str] = &["claude-project-transcripts"];

pub struct ClaudeCodec;

pub const CODEC: ClaudeCodec = ClaudeCodec;

/// The codec as a trait object for registries.
pub fn codec() -> &'static dyn Codec {
    &CODEC
}

impl Codec for ClaudeCodec {
    fn id(&self) -> &'static str {
        "claude"
    }

    /// Store locations come from the `a4aa-claude` card — the descriptor
    /// stays the single authority; `a4aa`'s history vocabulary is `acop`'s
    /// own (`a4aa::card` re-exports `acop::store`), so the projection is
    /// the declaration itself.
    fn history(&self) -> Vec<acop::store::HistoryRoot> {
        a4aa_claude::card().history.to_vec()
    }

    fn transcript_usage(&self) -> Option<acop::TranscriptUsage> {
        a4aa_claude::card().usage.transcript
    }

    fn session_kinds(&self) -> &'static [&'static str] {
        SESSION_KINDS
    }

    fn capabilities(&self) -> &'static CapabilityMatrix {
        &CAPS
    }

    /// `<session-uuid>.jsonl` → `<session-uuid>`.
    fn session_id(&self, path: &Path) -> String {
        path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// `.json` artifacts are never claude transcripts — a `sessions[]`
    /// document decodes through the shared lane, anything else through
    /// the generic document decoder.
    fn decode(&self, path: &Path) -> Result<Decoded> {
        if extension_of(path) == "json" {
            return acop::generic::decode_json_document(path, "claude");
        }
        let text = std::fs::read_to_string(path).map_err(Error::io(path))?;
        let mut conv = Conversation::new(self.session_id(path));
        conv.agent = Some("claude".into());
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
            let ty = line.get("type").and_then(Value::as_str).unwrap_or("");
            let ts = line
                .get("timestamp")
                .and_then(Value::as_str)
                .map(String::from);
            let id = line.get("uuid").and_then(Value::as_str).map(String::from);
            let parent = line
                .get("parentUuid")
                .and_then(Value::as_str)
                .map(String::from);

            if let Some(session) = line.get("sessionId").and_then(Value::as_str) {
                conv.session_id = session.to_string();
            }
            conv.cwd = line
                .get("cwd")
                .and_then(Value::as_str)
                .map(String::from)
                .or(conv.cwd);
            if conv.created_at.is_none() {
                conv.created_at = ts.clone();
            }
            conv.updated_at = ts.clone().or(conv.updated_at);

            let bodies = match ty {
                "user" => message_bodies(&line, true),
                "assistant" => message_bodies(&line, false),
                "system" => vec![Body::System {
                    subtype: line
                        .get("subtype")
                        .and_then(Value::as_str)
                        .unwrap_or("system")
                        .to_string(),
                    text: match line.get("content") {
                        Some(Value::String(s)) => s.clone(),
                        Some(v) => v.to_string(),
                        None => String::new(),
                    },
                }],
                "" => {
                    skipped.push(Skipped {
                        line: idx,
                        native_type: None,
                        reason: "untyped line".into(),
                    });
                    continue;
                }
                other => vec![Body::Meta {
                    subtype: other.to_string(),
                    data: line.clone(),
                }],
            };

            if bodies.is_empty() {
                skipped.push(Skipped {
                    line: idx,
                    native_type: Some(ty.to_string()),
                    reason: "no content parts".into(),
                });
                continue;
            }
            for body in bodies {
                let ordinal = conv.records.len() as u64;
                conv.records.push(Record {
                    id: id.clone(),
                    parent: parent.clone(),
                    ts: ts.clone(),
                    ordinal,
                    body,
                });
                record_lines.push(idx);
            }
        }
        if conv.records.is_empty()
            && !skipped.is_empty()
            && matches!(extension_of(path).as_str(), "jsonl" | "ndjson")
        {
            // A `.jsonl` artifact without claude `type` lines is a
            // generic transcript hosted under the claude adapter.
            return acop::generic::decode_jsonl(path, "claude");
        }
        Ok(Decoded {
            conversation: conv,
            record_lines,
            line_count,
            skipped,
        })
    }

    /// A `.json` document holding `sessions[]` items or a `.jsonl`
    /// transcript declaring several session groups enumerates one ref
    /// per unit; everything else stays one session per file.
    fn sessions_in_file(&self, path: &Path) -> Result<Vec<SessionRef>> {
        match extension_of(path).as_str() {
            "json" => {
                let refs = acop::generic::json_document_session_refs(path);
                if !refs.is_empty() {
                    return Ok(refs);
                }
            }
            "jsonl" | "ndjson" => {
                let refs = acop::generic::jsonl_session_refs(path);
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

    /// `key = Some` selects one container unit — a `sessions[]` item or
    /// a declared session group; `key = None` decodes the file.
    fn decode_session(&self, session: &SessionRef) -> Result<Decoded> {
        match session.key.as_deref() {
            None => self.decode(&session.path),
            Some(key) => match extension_of(&session.path).as_str() {
                "json" => acop::generic::decode_json_document_session(&session.path, key, "claude"),
                "jsonl" | "ndjson" => {
                    acop::generic::decode_jsonl_session(&session.path, key, "claude")
                }
                _ => self.decode(&session.path),
            },
        }
    }

    fn encode(&self, conv: &Conversation) -> Result<Encoded> {
        let (kept, projection) = self.project(conv);
        let now = acop::util::now_rfc3339();
        let mut lines = Vec::with_capacity(kept.len());
        let mut last_uuid: Option<String> = None;
        for rec in kept {
            let ts = rec.ts.clone().unwrap_or_else(|| now.clone());
            let uuid = uuid::Uuid::new_v4().to_string();
            let parent = last_uuid.clone();
            let line = match &rec.body {
                Body::User {
                    text, attachments, ..
                } => {
                    let mut content = vec![json!({"type": "text", "text": text})];
                    for att in attachments {
                        content.push(att.data.clone());
                    }
                    envelope(
                        "user",
                        &uuid,
                        &parent,
                        conv,
                        &ts,
                        json!({"role": "user", "content": content}),
                    )
                }
                Body::Assistant { text, model } => envelope(
                    "assistant",
                    &uuid,
                    &parent,
                    conv,
                    &ts,
                    json!({
                        "role": "assistant",
                        "model": model.clone().unwrap_or_else(|| "unknown".into()),
                        "content": [{"type": "text", "text": text}],
                    }),
                ),
                Body::Reasoning { text } => envelope(
                    "assistant",
                    &uuid,
                    &parent,
                    conv,
                    &ts,
                    json!({
                        "role": "assistant",
                        "model": "unknown",
                        "content": [{"type": "thinking", "thinking": text}],
                    }),
                ),
                Body::ToolCall {
                    call_id,
                    name,
                    args,
                } => envelope(
                    "assistant",
                    &uuid,
                    &parent,
                    conv,
                    &ts,
                    json!({
                        "role": "assistant",
                        "model": "unknown",
                        "content": [{
                            "type": "tool_use",
                            "id": call_id.clone().unwrap_or_else(|| format!(
                                "toolu_{}",
                                uuid::Uuid::new_v4().simple()
                            )),
                            "name": name,
                            "input": args,
                        }],
                    }),
                ),
                Body::ToolResult {
                    call_id,
                    content,
                    is_error,
                    ..
                } => envelope(
                    "user",
                    &uuid,
                    &parent,
                    conv,
                    &ts,
                    json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": call_id,
                            "content": content,
                            "is_error": is_error,
                        }],
                    }),
                ),
                Body::System { subtype, text } => json!({
                    "type": "system",
                    "subtype": subtype,
                    "content": text,
                    "uuid": uuid,
                    "sessionId": conv.session_id,
                    "timestamp": ts,
                }),
                Body::Meta { .. } => continue, // filtered by projection already
            };
            last_uuid = Some(uuid);
            lines.push(line.to_string());
        }
        Ok(Encoded { lines, projection })
    }

    /// `~/.claude/projects/<cwd-slug>/<session_id>.jsonl` where the slug is
    /// the session cwd with every `/` replaced by `-` (leading `-` kept):
    /// `/Users/x/proj` → `-Users-x-proj`.
    fn store_path(&self, home: &Path, conv: &Conversation) -> Result<PathBuf> {
        let slug = conv
            .cwd
            .as_deref()
            .map(|cwd| cwd.replace('/', "-"))
            .unwrap_or_else(|| "-acop-imported".to_string());
        Ok(home
            .join(".claude/projects")
            .join(slug)
            .join(format!("{}.jsonl", conv.session_id)))
    }

    fn redact_line(&self, line: &Value, marker: &str) -> Option<Value> {
        let mut line = line.clone();
        let parts = line
            .get_mut("message")?
            .get_mut("content")?
            .as_array_mut()?;
        let mut replaced = false;
        for part in parts.iter_mut() {
            for key in ["text", "thinking"] {
                if part.get(key).is_some() {
                    part[key] = json!(marker);
                    replaced = true;
                }
            }
            match part.get("type").and_then(Value::as_str) {
                Some("tool_use") if part.get("input").is_some() => {
                    part["input"] = json!(marker);
                    replaced = true;
                }
                Some("tool_result") if part.get("content").is_some() => {
                    part["content"] = json!(marker);
                    replaced = true;
                }
                _ => {}
            }
        }
        if replaced {
            Some(line)
        } else {
            None
        }
    }
}

/// The `user`/`assistant`/`tool_result`-user envelope every emitted record
/// line shares.
fn envelope(
    ty: &str,
    uuid: &str,
    parent: &Option<String>,
    conv: &Conversation,
    ts: &str,
    message: Value,
) -> Value {
    json!({
        "type": ty,
        "uuid": uuid,
        "parentUuid": parent,
        "sessionId": conv.session_id,
        "timestamp": ts,
        "cwd": conv.cwd,
        "userType": "external",
        "isSidechain": false,
        "message": message,
    })
}

/// Fan one `user`/`assistant` line's `message.content` out into canonical
/// bodies. Content may be a part array or a bare string (user text). Image
/// parts fold into the message's first `User` record; when no text part
/// exists they land on a `User` record with empty text.
fn message_bodies(line: &Value, user: bool) -> Vec<Body> {
    let empty = Value::Null;
    let message = line.get("message").unwrap_or(&empty);
    let model = message
        .get("model")
        .and_then(Value::as_str)
        .map(String::from);
    let parts: Vec<Value> = match message.get("content") {
        Some(Value::Array(parts)) => parts.clone(),
        // A string content field can itself be an embedded JSON message
        // (`"{\"type\":\"text\",\"text\":…}"`) — decode it like the
        // generic extractor does.
        Some(Value::String(s)) => {
            let text =
                acop::generic::extract_text(&Value::String(s.clone())).unwrap_or_else(|| s.clone());
            vec![json!({"type": "text", "text": text})]
        }
        _ => Vec::new(),
    };

    let mut pending: Vec<Attachment> = parts
        .iter()
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("image"))
        .map(|p| Attachment {
            kind: "image".into(),
            media_type: p
                .pointer("/source/media_type")
                .and_then(Value::as_str)
                .map(String::from),
            data: p.clone(),
        })
        .collect();
    let has_text = parts
        .iter()
        .any(|p| p.get("type").and_then(Value::as_str) == Some("text"));

    let mut bodies = Vec::new();
    for part in &parts {
        let ptype = part.get("type").and_then(Value::as_str).unwrap_or("");
        match ptype {
            "text" if user => bodies.push(Body::User {
                text: part_text(part, "text"),
                model: None,
                attachments: std::mem::take(&mut pending),
            }),
            "text" => bodies.push(Body::Assistant {
                text: part_text(part, "text"),
                model: model.clone(),
            }),
            "thinking" => bodies.push(Body::Reasoning {
                text: part_text(part, "thinking"),
            }),
            "tool_use" => bodies.push(Body::ToolCall {
                call_id: part.get("id").and_then(Value::as_str).map(String::from),
                name: part
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string(),
                args: part.get("input").cloned().unwrap_or(Value::Null),
            }),
            "tool_result" => bodies.push(Body::ToolResult {
                call_id: part
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .map(String::from),
                name: None,
                content: part.get("content").cloned().unwrap_or(Value::Null),
                is_error: part
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }),
            "image" if user => {
                if !has_text && !pending.is_empty() {
                    bodies.push(Body::User {
                        text: String::new(),
                        model: None,
                        attachments: std::mem::take(&mut pending),
                    });
                }
                // otherwise folded onto the text part's User record
            }
            "image" => bodies.push(Body::Meta {
                subtype: "part:image".into(),
                data: part.clone(),
            }),
            other => bodies.push(Body::Meta {
                subtype: format!("part:{other}"),
                data: part.clone(),
            }),
        }
    }
    // `message.usage` rides the message blob — a trailing `meta{usage}`
    // record the consumer folds onto the message that produced it.
    if let Some(usage) = message.get("usage").filter(|u| u.is_object()) {
        bodies.push(Body::Meta {
            subtype: "usage".into(),
            data: usage.clone(),
        });
    }
    bodies
}

fn part_text(part: &Value, key: &str) -> String {
    part.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use acop::RecordKind;

    fn write_temp(name: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("acop-claude-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// Synthetic transcript: user text+image, assistant thinking+text+
    /// tool_use, user tool_result, one system and one unmapped line.
    const TRANSCRIPT: &str = concat!(
        r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"sess-1","timestamp":"2026-01-01T10:00:00Z","cwd":"/synthetic/proj","userType":"external","isSidechain":false,"message":{"role":"user","content":[{"type":"text","text":"hello agent"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}}]}}"#,
        "\n",
        r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"sess-1","timestamp":"2026-01-01T10:00:01Z","message":{"role":"assistant","model":"synthetic-model","content":[{"type":"thinking","thinking":"pondering"},{"type":"text","text":"working on it"},{"type":"tool_use","id":"toolu_1","name":"fake_tool","input":{"arg":1}}]}}"#,
        "\n",
        r#"{"type":"user","uuid":"u2","parentUuid":"a1","sessionId":"sess-1","timestamp":"2026-01-01T10:00:02Z","cwd":"/synthetic/proj","userType":"external","isSidechain":false,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"done","is_error":false}]}}"#,
        "\n",
        r#"{"type":"system","subtype":"stop_hook_summary","content":"hook ran","uuid":"s1","sessionId":"sess-1","timestamp":"2026-01-01T10:00:03Z"}"#,
        "\n",
        r#"{"type":"mode","sessionId":"sess-1","mode":"normal"}"#,
        "\n"
    );

    #[test]
    fn decode_fans_out_content_parts() {
        let path = write_temp("decode-fanout.jsonl", TRANSCRIPT);
        let decoded = CODEC.decode(&path).unwrap();
        let conv = &decoded.conversation;

        assert_eq!(decoded.line_count, 5);
        assert_eq!(decoded.record_lines, vec![0, 1, 1, 1, 2, 3, 4]);
        assert_eq!(conv.session_id, "sess-1");
        assert_eq!(conv.cwd.as_deref(), Some("/synthetic/proj"));
        assert_eq!(conv.created_at.as_deref(), Some("2026-01-01T10:00:00Z"));
        assert_eq!(conv.updated_at.as_deref(), Some("2026-01-01T10:00:03Z"));

        let kinds: Vec<RecordKind> = conv.records.iter().map(Record::kind).collect();
        assert_eq!(
            kinds,
            vec![
                RecordKind::User,
                RecordKind::Reasoning,
                RecordKind::Assistant,
                RecordKind::ToolCall,
                RecordKind::ToolResult,
                RecordKind::System,
                RecordKind::Meta,
            ]
        );

        // uuid / parentUuid propagate onto every fanned record.
        assert_eq!(conv.records[0].id.as_deref(), Some("u1"));
        assert_eq!(conv.records[0].parent, None);
        for rec in &conv.records[1..4] {
            assert_eq!(rec.id.as_deref(), Some("a1"));
            assert_eq!(rec.parent.as_deref(), Some("u1"));
        }

        // The image part folded onto the message's User record.
        match &conv.records[0].body {
            Body::User {
                text,
                model: None,
                attachments,
            } => {
                assert_eq!(text, "hello agent");
                assert_eq!(attachments.len(), 1);
                assert_eq!(attachments[0].kind, "image");
                assert_eq!(attachments[0].media_type.as_deref(), Some("image/png"));
            }
            other => panic!("expected user body, got {other:?}"),
        }
        match &conv.records[3].body {
            Body::ToolCall {
                call_id,
                name,
                args,
            } => {
                assert_eq!(call_id.as_deref(), Some("toolu_1"));
                assert_eq!(name, "fake_tool");
                assert_eq!(args["arg"], 1);
            }
            other => panic!("expected tool call, got {other:?}"),
        }
        match &conv.records[4].body {
            Body::ToolResult {
                call_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(call_id.as_deref(), Some("toolu_1"));
                assert_eq!(content, "done");
                assert!(!is_error);
            }
            other => panic!("expected tool result, got {other:?}"),
        }
        match &conv.records[6].body {
            Body::Meta { subtype, .. } => assert_eq!(subtype, "mode"),
            other => panic!("expected meta, got {other:?}"),
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn decode_rejects_malformed_lines() {
        let path = write_temp("decode-bad.jsonl", "{\"type\":\"user\"\nnot json\n");
        assert!(matches!(CODEC.decode(&path), Err(Error::Decode { .. })));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn encode_decode_round_trip_preserves_kinds() {
        let path = write_temp("roundtrip-src.jsonl", TRANSCRIPT);
        let decoded = CODEC.decode(&path).unwrap();
        let encoded = CODEC.encode(&decoded.conversation).unwrap();

        // Meta is unsupported: dropped from the wire, reported honestly.
        assert_eq!(encoded.lines.len(), 6);
        assert_eq!(encoded.projection.dropped.len(), 1);
        assert_eq!(encoded.projection.dropped[0].1, RecordKind::Meta);

        let back_path = write_temp("roundtrip-back.jsonl", &encoded.lines.join("\n"));
        let back = CODEC.decode(&back_path).unwrap();
        let kinds: Vec<RecordKind> = back.conversation.records.iter().map(Record::kind).collect();
        assert_eq!(
            kinds,
            vec![
                RecordKind::User,
                RecordKind::Reasoning,
                RecordKind::Assistant,
                RecordKind::ToolCall,
                RecordKind::ToolResult,
                RecordKind::System,
            ]
        );

        // Encoded lines chain fresh uuids through parentUuid.
        let first: Value = serde_json::from_str(&encoded.lines[0]).unwrap();
        let second: Value = serde_json::from_str(&encoded.lines[1]).unwrap();
        assert_eq!(first["type"], "user");
        assert_eq!(first["parentUuid"], Value::Null);
        assert_eq!(second["parentUuid"], first["uuid"]);
        assert_eq!(second["type"], "assistant");

        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&back_path).ok();
    }

    #[test]
    fn redact_line_marks_textual_parts() {
        let line = json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "secret thought"},
                {"type": "tool_use", "id": "toolu_9", "name": "t", "input": {"cmd": "x"}},
            ]},
        });
        let redacted = CODEC.redact_line(&line, "<redacted>").unwrap();
        assert_eq!(redacted["message"]["content"][0]["thinking"], "<redacted>");
        assert_eq!(redacted["message"]["content"][1]["input"], "<redacted>");
        assert_eq!(redacted["message"]["content"][1]["name"], "t");

        // Nothing redactable: no message.content array.
        assert!(CODEC
            .redact_line(&json!({"type": "mode", "mode": "normal"}), "<redacted>")
            .is_none());
        // Bare-string user content is not an array: untouched per contract.
        assert!(CODEC
            .redact_line(
                &json!({"type": "user", "message": {"role": "user", "content": "hi"}}),
                "<redacted>"
            )
            .is_none());
    }

    #[test]
    fn store_path_slugs_cwd() {
        let mut conv = Conversation::new("sess-abc");
        conv.cwd = Some("/Users/x/proj".into());
        assert_eq!(
            CODEC.store_path(Path::new("/home/u"), &conv).unwrap(),
            Path::new("/home/u/.claude/projects/-Users-x-proj/sess-abc.jsonl")
        );
        conv.cwd = None;
        assert_eq!(
            CODEC.store_path(Path::new("/home/u"), &conv).unwrap(),
            Path::new("/home/u/.claude/projects/-acop-imported/sess-abc.jsonl")
        );
    }

    #[test]
    fn session_id_is_the_file_stem() {
        assert_eq!(
            CODEC.session_id(Path::new(
                "/p/-Users-x/86990c82-fd1b-4f55-b291-e7b3c6d3d093.jsonl"
            )),
            "86990c82-fd1b-4f55-b291-e7b3c6d3d093"
        );
    }

    #[test]
    fn capabilities_match_the_native_vocabulary() {
        assert_eq!(CODEC.id(), "claude");
        assert_eq!(CODEC.session_kinds(), &["claude-project-transcripts"]);
        let caps = CODEC.capabilities();
        assert!(caps.writable);
        for kind in [
            RecordKind::User,
            RecordKind::Assistant,
            RecordKind::Reasoning,
            RecordKind::ToolCall,
            RecordKind::ToolResult,
            RecordKind::System,
        ] {
            assert_eq!(caps.of(kind), Capability::Native, "{kind:?}");
        }
        assert_eq!(caps.of(RecordKind::Meta), Capability::Unsupported);
        assert_eq!(codec().id(), CODEC.id());
    }
}
