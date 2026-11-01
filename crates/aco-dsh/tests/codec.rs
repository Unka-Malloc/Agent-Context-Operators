//! Synthetic dsh session-store fixtures — no real session data.
use aco::model::Body;
use aco::RecordKind;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

fn header(id: &str, extra: Value) -> Value {
    let mut row = json!({ "type": "session", "version": 4, "id": id });
    row.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    row
}

fn event(seq: u64, kind: &str, data: Value) -> String {
    json!({ "seq": seq, "time": seq * 1000, "type": kind, "data": data }).to_string()
}

fn write_session(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

fn fixture_lines() -> Vec<String> {
    vec![
        header("sess-1", json!({ "cwd": "/work/proj", "createdAt": 1_700_000_000_000_i64, "inheritedEventCount": 1 })).to_string(),
        event(0, "user/message", json!({ "id": "u-inherited", "role": "user", "content": [{"type":"text","text":"ancestry row"}] })),
        event(1, "session/title", json!({ "title": "Fixture session" })),
        event(2, "user/message", json!({ "id": "u1", "role": "user", "content": [{"type":"text","text":"first"},{"type":"text","text":"second"}] })),
        event(3, "agent/inbox/spliced", json!({ "inserted": [
            {"id":"u1","role":"user","content":[{"type":"text","text":"dup — claimed"}]},
            {"id":"u2","role":"user","content":[{"type":"text","text":"spliced task"}]},
        ] })),
        event(4, "assistant/message", json!({
            "message": { "id": "m1", "source": {"model":"deepseek-v4"}, "content": [
                {"type":"reasoning","text":"thinking"},
                {"type":"text","text":"answer"},
                {"type":"tool-call","id":"c1","name":"read_file","input":{"path":"a.rs"}},
            ]},
            "turn": 1, "step": 2,
            "usage": {"inputTokens":10,"outputTokens":5,"totalTokens":15},
        })),
        event(5, "tool/call", json!({ "callId": "c1", "name": "read_file", "input": {"path":"a.rs"} })),
        event(6, "tool/call", json!({ "callId": "c2", "name": "shell", "input": {"cmd":"ls"} })),
        event(7, "tool/result", json!({ "callId": "c2", "message": {"content":"done"} })),
    ]
}

#[test]
fn decode_fold_covers_blocks_dedupe_and_ancestry() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("proj/sess-1");
    let path = write_session(&dir, "session.jsonl", &fixture_lines());

    let decoded = aco_dsh::codec().decode(&path).unwrap();
    let conv = &decoded.conversation;
    assert_eq!(conv.session_id, "sess-1");
    assert_eq!(conv.title.as_deref(), Some("Fixture session"));
    assert_eq!(conv.cwd.as_deref(), Some("/work/proj"));
    assert_eq!(conv.agent.as_deref(), Some("dsh"));

    let kinds: Vec<RecordKind> = conv.records.iter().map(|r| r.kind()).collect();
    // header meta, title meta, user, spliced user + splice meta,
    // reasoning + assistant + tool-call + usage meta, tool-call, tool-result.
    assert_eq!(
        kinds,
        [
            RecordKind::Meta,
            RecordKind::Meta,
            RecordKind::User,
            RecordKind::User,
            RecordKind::Meta,
            RecordKind::Reasoning,
            RecordKind::Assistant,
            RecordKind::ToolCall,
            RecordKind::Meta,
            RecordKind::ToolCall,
            RecordKind::ToolResult,
        ]
    );
    // Ancestry (seq < inherited) dropped; the claimed splice insert renders
    // once under its `user/message` row — not again as a user record (the
    // raw splice row stays in its Meta for fidelity).
    assert!(!conv.records.iter().any(|r| r.text().contains("ancestry")));
    assert!(!conv
        .records
        .iter()
        .any(|r| r.kind() == RecordKind::User && r.text().contains("dup — claimed")));
    assert!(conv
        .records
        .iter()
        .any(|r| r.kind() == RecordKind::User && r.text().contains("spliced task")));
    // The duplicated tool/call row went to `skipped`.
    assert_eq!(decoded.skipped.len(), 1);
    assert_eq!(decoded.skipped[0].native_type.as_deref(), Some("tool/call"));
    // usage meta preserved for transcript accounting.
    let usage = conv
        .records
        .iter()
        .find_map(|r| match &r.body {
            Body::Meta { subtype, data } if subtype == "usage" => Some(data.clone()),
            _ => None,
        })
        .expect("usage meta");
    assert_eq!(usage["usage"]["totalTokens"], 15);
}

#[test]
fn generation_selection_keeps_only_newest_per_directory() {
    let codec = aco_dsh::codec();
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("proj/sess-1");
    let other = tmp.path().join("proj/sess-2");
    let files = vec![
        write_session(&dir, "session.jsonl", &fixture_lines()),
        write_session(&dir, "session.v2.jsonl", &fixture_lines()),
        write_session(&dir, "session.v10.jsonl", &fixture_lines()),
        write_session(&dir, "session.v02.jsonl", &fixture_lines()),
        write_session(&dir, "notes.txt", &["not a session".to_string()]),
        write_session(&other, "session.v3.jsonl", &fixture_lines()),
    ];
    let selected = codec.select_session_files(files);
    assert_eq!(selected.len(), 2);
    assert!(selected.iter().any(|p| p.ends_with("session.v10.jsonl")));
    assert!(selected.iter().any(|p| p.ends_with("session.v3.jsonl")));
    // Header-read session id resolves through the newest artifact.
    assert_eq!(codec.session_id(selected.last().unwrap()), "sess-1");
}

#[test]
fn zstd_container_decodes_like_plain() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("proj/sess-1");
    fs::create_dir_all(&dir).unwrap();
    let payload = fixture_lines().join("\n") + "\n";
    let path = dir.join("session.v2.jsonl.zstd");
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 0).unwrap();
    std::io::Write::write_all(&mut encoder, payload.as_bytes()).unwrap();
    // Concatenated frames decode as one stream — write a second frame to
    // prove the reader handles the container form.
    let frame = encoder.finish().unwrap();
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 0).unwrap();
    std::io::Write::write_all(&mut encoder, payload.as_bytes()).unwrap();
    let mut bytes = frame;
    bytes.extend(encoder.finish().unwrap());
    fs::write(&path, bytes).unwrap();

    // Two concatenated copies of the same log: rows after the first log's
    // end are just more rows — decode must not fail on the second frame.
    let decoded = aco_dsh::codec().decode(&path);
    assert!(decoded.is_ok(), "{:?}", decoded.err());
}

#[test]
fn other_format_versions_refused_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("proj/sess-9");
    let path = write_session(
        &dir,
        "session.jsonl",
        &[json!({ "type": "session", "version": 3, "id": "sess-9" }).to_string()],
    );
    let err = aco_dsh::codec().decode(&path).unwrap_err();
    assert!(err.to_string().contains("format version 3"), "{err}");
}

#[test]
fn encode_roundtrip_and_store_path() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("proj/sess-1");
    let path = write_session(&dir, "session.jsonl", &fixture_lines());
    let codec = aco_dsh::codec();
    let conv = codec.decode(&path).unwrap().conversation;

    let encoded = codec.encode(&conv).unwrap();
    // Meta rows are store-internal — encode drops them with a report.
    assert!(encoded
        .projection
        .dropped
        .iter()
        .all(|(_, k)| *k == RecordKind::Meta));
    let rewritten = tmp.path().join("out/session.v9.jsonl");
    fs::create_dir_all(rewritten.parent().unwrap()).unwrap();
    fs::write(&rewritten, encoded.lines.join("\n")).unwrap();
    let again = codec.decode(&rewritten).unwrap().conversation;
    assert_eq!(again.session_id, conv.session_id);
    let kinds: Vec<_> = again.records.iter().map(|r| r.kind()).collect();
    assert!(kinds.contains(&RecordKind::User));
    assert!(kinds.contains(&RecordKind::Reasoning));
    assert!(kinds.contains(&RecordKind::ToolCall));

    let home = tmp.path();
    let target = codec.store_path(home, &conv).unwrap();
    assert_eq!(target, home.join(".dsh/sessions/proj/sess-1/session.jsonl"));
}
