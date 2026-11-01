//! Codec-level coverage for aco-codex over a fully synthetic rollout
//! file. Nothing here is real user data.

use aco::{Body, RecordKind};
use std::io::Write;

fn fixture() -> String {
    let lines = [
        serde_json::json!({"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"11111111-2222-3333-4444-555555555555","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/proj","originator":"codex_cli"}}),
        serde_json::json!({"timestamp":"2026-01-01T00:00:01Z","type":"turn_context","payload":{"cwd":"/tmp/proj"}}),
        serde_json::json!({"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"message","id":"msg_u1","role":"user","content":[{"type":"input_text","text":"hello agent"}]}}),
        serde_json::json!({"timestamp":"2026-01-01T00:00:03Z","type":"response_item","payload":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"think about it"}],"encrypted_content":"blob"}}),
        serde_json::json!({"timestamp":"2026-01-01T00:00:04Z","type":"response_item","payload":{"type":"custom_tool_call","id":"ctc_1","call_id":"call_1","name":"exec","input":"echo hi"}}),
        serde_json::json!({"timestamp":"2026-01-01T00:00:05Z","type":"response_item","payload":{"type":"custom_tool_call_output","id":"ctco_1","call_id":"call_1","output":[{"type":"input_text","text":"hi"}]}}),
        serde_json::json!({"timestamp":"2026-01-01T00:00:06Z","type":"response_item","payload":{"type":"message","id":"msg_a1","role":"assistant","content":[{"type":"output_text","text":"done"}]}}),
        serde_json::json!({"timestamp":"2026-01-01T00:00:07Z","type":"event_msg","payload":{"type":"token_count","info":{"total":42}}}),
        serde_json::json!({"timestamp":"2026-01-01T00:00:08Z","type":"response_item","payload":{"type":"message","id":"msg_d1","role":"developer","content":[{"type":"input_text","text":"be good"}]}}),
    ];
    lines
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn write_fixture(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("rollout-2026-01-01T00-00-00-11111111-2222-3333-4444-555555555555.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(fixture().as_bytes()).unwrap();
    path
}

#[test]
fn decode_maps_all_record_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_fixture(dir.path());
    let d = aco_codex::codec().decode(&path).unwrap();
    let kinds: Vec<RecordKind> = d.conversation.records.iter().map(|r| r.kind()).collect();
    assert_eq!(
        kinds,
        vec![
            RecordKind::Meta, // session_meta
            RecordKind::Meta, // turn_context
            RecordKind::User,
            RecordKind::Reasoning,
            RecordKind::ToolCall,
            RecordKind::ToolResult,
            RecordKind::Assistant,
            RecordKind::Meta,   // event:token_count
            RecordKind::System, // developer message
        ]
    );
    assert_eq!(
        d.conversation.session_id,
        "11111111-2222-3333-4444-555555555555"
    );
    assert_eq!(d.conversation.cwd.as_deref(), Some("/tmp/proj"));
    assert_eq!(d.record_lines.len(), 9);
    assert_eq!(d.record_lines[2], 2); // user record ↔ its line index
    match &d.conversation.records[4].body {
        Body::ToolCall { call_id, name, .. } => {
            assert_eq!(call_id.as_deref(), Some("call_1"));
            assert_eq!(name, "exec");
        }
        other => panic!("expected tool-call, got {other:?}"),
    }
}

#[test]
fn encode_decode_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_fixture(dir.path());
    let codec = aco_codex::codec();
    let conv = codec.decode(&path).unwrap().conversation;
    let enc = codec.encode(&conv).unwrap();
    // session_meta + 6 projectable records (3 meta kinds dropped)
    assert_eq!(enc.lines.len(), 7);
    assert_eq!(enc.projection.dropped.len(), 3); // both meta + event meta
    let tmp = dir.path().join("roundtrip.jsonl");
    std::fs::write(&tmp, enc.lines.join("\n")).unwrap();
    let re = codec.decode(&tmp).unwrap();
    let kinds: Vec<RecordKind> = re.conversation.records.iter().map(|r| r.kind()).collect();
    assert_eq!(
        kinds,
        vec![
            RecordKind::Meta, // regenerated session_meta
            RecordKind::User,
            RecordKind::Reasoning,
            RecordKind::ToolCall,
            RecordKind::ToolResult,
            RecordKind::Assistant,
            RecordKind::System,
        ]
    );
}

#[test]
fn redact_scrubs_text() {
    let codec = aco_codex::codec();
    let line: serde_json::Value = serde_json::from_str(
        r#"{"timestamp":"t","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"secret"}]}}"#,
    )
    .unwrap();
    let red = codec.redact_line(&line, "<x>").unwrap();
    assert_eq!(
        red["payload"]["content"][0]["text"].as_str().unwrap(),
        "<x>"
    );
    // Reasoning: encrypted blob must also be cleared.
    let rs: serde_json::Value = serde_json::from_str(
        r#"{"timestamp":"t","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"s"}],"encrypted_content":"blob"}}"#,
    )
    .unwrap();
    let red = codec.redact_line(&rs, "<x>").unwrap();
    assert!(red["payload"]["encrypted_content"].is_null());
    assert_eq!(red["payload"]["summary"][0]["text"], "<x>");
}

#[test]
fn store_path_layout() {
    let home = tempfile::tempdir().unwrap();
    let mut conv = aco::Conversation::new("abcdefab-0000-0000-0000-000000000000");
    conv.cwd = Some("/tmp/proj".into());
    let p = aco_codex::codec().store_path(home.path(), &conv).unwrap();
    let s = p.to_string_lossy();
    assert!(s.contains("/.codex/sessions/"), "{s}");
    assert!(p
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("rollout-"));
    assert!(p
        .file_name()
        .unwrap()
        .to_string_lossy()
        .ends_with("-abcdefab-0000-0000-0000-000000000000.jsonl"));
}
