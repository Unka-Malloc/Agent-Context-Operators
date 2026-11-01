use aco::model::{Body, Conversation, RecordKind};

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("myproj_42.jsonl"),
        concat!(
            r#"{"type":"session","id":"42","timestamp":"2025-01-01T00:00:00Z","cwd":"/work/p"}"#,
            "\n",
            r#"{"type":"session_info","name":"fix login"}"#,
            "\n",
            r#"{"type":"message","timestamp":"2025-01-01T00:00:01Z","message":{"role":"user","content":"hello"}}"#,
            "\n",
            r#"{"type":"message","timestamp":"2025-01-01T00:00:02Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"toolCall","id":"c1","name":"read","arguments":{"path":"/a"}},{"type":"text","text":"reading"}],"usage":{"inputTokens":10,"outputTokens":5}}}"#,
            "\n",
            r#"{"type":"message","timestamp":"2025-01-01T00:00:03Z","message":{"role":"toolResult","toolCallId":"c1","toolName":"read","content":"file body","isError":false}}"#,
            "\n",
        ),
    )
    .unwrap();
    dir
}

#[test]
fn decode_projects_blocks_and_usage() {
    let dir = fixture();
    let path = dir.path().join("myproj_42.jsonl");
    let decoded = aco_pi::codec().decode(&path).unwrap();
    let conv = &decoded.conversation;
    assert_eq!(conv.session_id, "42");
    assert_eq!(conv.title.as_deref(), Some("fix login"));
    assert_eq!(conv.cwd.as_deref(), Some("/work/p"));
    let kinds: Vec<RecordKind> = conv.records.iter().map(|r| r.kind()).collect();
    assert_eq!(
        kinds,
        [
            RecordKind::User,
            RecordKind::Reasoning,
            RecordKind::ToolCall,
            RecordKind::Assistant,
            RecordKind::Meta,
            RecordKind::ToolResult
        ]
    );
    match &conv.records[3].body {
        Body::Assistant { text, .. } => assert_eq!(text, "reading"),
        other => panic!("expected assistant, got {other:?}"),
    }
    assert_eq!(decoded.record_lines, vec![2, 3, 3, 3, 3, 4]);
}

#[test]
fn session_id_prefers_header_over_filename() {
    let dir = fixture();
    let path = dir.path().join("myproj_42.jsonl");
    assert_eq!(aco_pi::codec().session_id(&path), "42");
    let bare = dir.path().join("proj_noheader.jsonl");
    std::fs::write(
        &bare,
        r#"{"type":"message","message":{"role":"user","content":"x"}}"#,
    )
    .unwrap();
    assert_eq!(aco_pi::codec().session_id(&bare), "noheader");
}

#[test]
fn encode_roundtrips_native_shape() {
    let dir = fixture();
    let decoded = aco_pi::codec()
        .decode(&dir.path().join("myproj_42.jsonl"))
        .unwrap();
    let encoded = aco_pi::codec().encode(&decoded.conversation).unwrap();
    assert!(encoded.projection.dropped.is_empty());
    let lines: Vec<serde_json::Value> = encoded
        .lines
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[0]["id"], "42");
    assert_eq!(lines[1]["type"], "session_info");
    assert_eq!(lines[3]["message"]["role"], "assistant");
    let blocks = lines[3]["message"]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "thinking");
    assert_eq!(blocks[1]["type"], "toolCall");
    assert_eq!(blocks[2]["type"], "text");
    assert_eq!(lines[3]["message"]["usage"]["inputTokens"], 10);
    assert_eq!(lines[4]["message"]["role"], "toolResult");
}

#[test]
fn store_path_honors_env_override() {
    let dir = tempfile::tempdir().unwrap();
    let conv = Conversation::new("abc");
    std::env::set_var("PI_CODING_AGENT_SESSION_DIR", dir.path());
    let path = aco_pi::codec().store_path(dir.path(), &conv).unwrap();
    std::env::remove_var("PI_CODING_AGENT_SESSION_DIR");
    assert_eq!(path, dir.path().join("abc.jsonl"));
}

#[test]
fn redact_line_rewrites_text_blocks_only() {
    let line: serde_json::Value = serde_json::from_str(
        r#"{"type":"message","message":{"role":"assistant","content":[{"type":"thinking","thinking":"secret"},{"type":"text","text":"more"}]}}"#,
    )
    .unwrap();
    let out = aco_pi::codec().redact_line(&line, "[redacted]").unwrap();
    assert_eq!(out["message"]["content"][0]["thinking"], "[redacted]");
    assert_eq!(out["message"]["content"][1]["text"], "[redacted]");
    let session: serde_json::Value =
        serde_json::from_str(r#"{"type":"session","id":"1"}"#).unwrap();
    assert!(aco_pi::codec()
        .redact_line(&session, "[redacted]")
        .is_none());
}
