//! Synthetic kimi-code wire fixtures — no real session data.
use acop::model::{Body, Conversation, RecordKind};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// `<root>/wd_project/<session>/agents/<agent>/wire.jsonl` plus a session
/// `state.json` — the layout the store keeps on disk.
fn wire_fixture(root: &Path, session: &str, agent: &str, state: Option<Value>) -> PathBuf {
    let session_dir = root.join("wd_project").join(session);
    let agent_dir = session_dir.join("agents").join(agent);
    fs::create_dir_all(&agent_dir).unwrap();
    if let Some(state) = state {
        fs::write(session_dir.join("state.json"), state.to_string()).unwrap();
    }
    agent_dir.join("wire.jsonl")
}

fn fixture_lines() -> Vec<String> {
    vec![
        r#"{"type":"metadata","protocol_version":1}"#.to_string(),
        r#"{"type":"turn.prompt","turnId":"turn-1","time":"2026-07-10T00:00:00Z","input":"Review the fixture"}"#.to_string(),
        r#"{"type":"context.append_loop_event","turnId":"turn-1","time":"2026-07-10T00:00:01Z","event":{"type":"content.part","step":1,"part":{"type":"think","think":"hmm ","summary":["s1"]}}}"#.to_string(),
        r#"{"type":"context.append_loop_event","turnId":"turn-1","time":"2026-07-10T00:00:01Z","event":{"type":"content.part","step":1,"part":{"type":"think","think":"more"}}}"#.to_string(),
        r#"{"type":"context.append_loop_event","turnId":"turn-1","time":"2026-07-10T00:00:02Z","event":{"type":"tool.call","callId":"c1","name":"exec","arguments":{"command":"ls"}}}"#.to_string(),
        r#"{"type":"context.append_loop_event","turnId":"turn-1","time":"2026-07-10T00:00:03Z","event":{"type":"tool.result","callId":"c1","name":"exec","result":"done"}}"#.to_string(),
        r#"{"type":"context.append_loop_event","turnId":"turn-1","time":"2026-07-10T00:00:04Z","event":{"type":"content.part","step":2,"part":{"type":"text","text":"Final "}}}"#.to_string(),
        r#"{"type":"context.append_loop_event","turnId":"turn-1","time":"2026-07-10T00:00:04Z","event":{"type":"content.part","step":2,"part":{"type":"text","text":"answer"}}}"#.to_string(),
        r#"{"type":"usage.record","time":1780912801000,"model":"kimi-code/kimi-for-coding","usageScope":"turn","usage":{"inputOther":100,"inputCacheRead":20,"inputCacheCreation":5,"output":30}}"#.to_string(),
        r#"{"type":"usage.record","usageScope":"session","usage":{"inputOther":9999,"output":9999}}"#.to_string(),
        r#"{"type":"mystery.row","x":1}"#.to_string(),
        "not json".to_string(),
    ]
}

#[test]
fn decode_projects_every_wire_kind() {
    let tmp = tempfile::tempdir().unwrap();
    let path = wire_fixture(
        tmp.path(),
        "session-1",
        "main",
        Some(json!({
            "title": "Synthetic Kimi session",
            "workDir": "/workspace/kimi-project",
            "createdAt": "2026-07-09T00:00:00Z",
        })),
    );
    fs::write(&path, fixture_lines().join("\n") + "\n").unwrap();

    let decoded = aco_kimi_code::codec().decode(&path).unwrap();
    let conv = &decoded.conversation;
    assert_eq!(conv.session_id, "session-1");
    assert_eq!(conv.agent.as_deref(), Some("kimicode"));
    assert_eq!(conv.title.as_deref(), Some("Synthetic Kimi session"));
    assert_eq!(conv.cwd.as_deref(), Some("/workspace/kimi-project"));
    assert_eq!(conv.created_at.as_deref(), Some("2026-07-09T00:00:00Z"));

    let kinds: Vec<RecordKind> = conv.records.iter().map(|r| r.kind()).collect();
    assert_eq!(
        kinds,
        [
            RecordKind::Meta,      // metadata row
            RecordKind::User,      // turn.prompt
            RecordKind::Reasoning, // merged think run
            RecordKind::Meta,      // reasoning-summary
            RecordKind::ToolCall,
            RecordKind::ToolResult,
            RecordKind::Assistant, // merged text run "Final answer"
            RecordKind::Meta,      // usage.record (turn scope)
        ]
    );
    assert_eq!(decoded.record_lines, vec![0, 1, 2, 2, 4, 5, 6, 8]);
    match &conv.records[2].body {
        Body::Reasoning { text } => assert_eq!(text, "hmm more"),
        other => panic!("expected reasoning, got {other:?}"),
    }
    match &conv.records[6].body {
        Body::Assistant { text, .. } => assert_eq!(text, "Final answer"),
        other => panic!("expected assistant, got {other:?}"),
    }
    let usage = conv
        .records
        .iter()
        .find_map(|r| match &r.body {
            Body::Meta { subtype, data } if subtype == "usage" => Some(data.clone()),
            _ => None,
        })
        .expect("usage meta");
    assert_eq!(usage["usage"]["promptTokens"], 125);
    assert_eq!(usage["usage"]["cachedInputTokens"], 20);
    assert_eq!(usage["usage"]["completionTokens"], 30);
    assert_eq!(usage["usage"]["totalTokens"], 155);
    assert_eq!(usage["model"], "kimi-code/kimi-for-coding");

    // Session-scoped usage emits nothing; the unknown type and the corrupt
    // line land in `skipped`.
    assert_eq!(decoded.skipped.len(), 2);
    assert_eq!(
        decoded.skipped[0].native_type.as_deref(),
        Some("mystery.row")
    );
    assert_eq!(decoded.skipped[0].reason, "projection");
    assert_eq!(decoded.skipped[1].reason, "corrupt");
}

#[test]
fn session_id_encodes_agent_ancestry() {
    let codec = aco_kimi_code::codec();
    let main = Path::new("/x/.kimi-code/wd/session-9/agents/main/wire.jsonl");
    let delegated = Path::new("/x/.kimi-code/wd/session-9/agents/agent-0/wire.jsonl");
    assert_eq!(codec.session_id(main), "session-9");
    assert_eq!(codec.session_id(delegated), "session-9:agent-0");
}

#[test]
fn select_session_files_accepts_only_agents_wire() {
    let codec = aco_kimi_code::codec();
    let files: Vec<PathBuf> = [
        "wd/s1/agents/main/wire.jsonl",
        "wd/s1/agents/agent-0/wire.jsonl",
        "wd/s1/agents/main/other.jsonl",
        "wd/s1/wire.jsonl",
        "wd/s1/agents/main/wire.txt",
        "wd/s1/state.json",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    let selected = codec.select_session_files(files);
    assert_eq!(selected.len(), 2);
    assert!(selected
        .iter()
        .all(|p| p.ends_with("wire.jsonl") && p.components().any(|c| c.as_os_str() == "agents")));
}

#[test]
fn context_append_message_is_fallback_only() {
    let tmp = tempfile::tempdir().unwrap();
    let codec = aco_kimi_code::codec();

    // No turn.prompt: the buffered append_message becomes the user turn.
    let orphan = wire_fixture(tmp.path(), "session-fallback", "main", None);
    fs::write(
        &orphan,
        concat!(
            r#"{"type":"context.append_message","time":"2026-07-10T09:59:59Z","message":{"role":"user","content":"Fallback prompt"}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","time":"2026-07-10T10:00:00Z","event":{"type":"content.part","step":1,"part":{"type":"text","text":"Reply"}}}"#,
            "\n",
        ),
    )
    .unwrap();
    let decoded = codec.decode(&orphan).unwrap();
    let kinds: Vec<RecordKind> = decoded
        .conversation
        .records
        .iter()
        .map(|r| r.kind())
        .collect();
    // The open text run flushes before end-of-file fallbacks release —
    // the same emission order the wire parser produces.
    assert_eq!(kinds, [RecordKind::Assistant, RecordKind::User]);
    assert_eq!(decoded.record_lines, vec![1, 0]);

    // A real prompt wins: the buffered row is dropped entirely.
    let primary = wire_fixture(tmp.path(), "session-primary", "main", None);
    fs::write(
        &primary,
        concat!(
            r#"{"type":"context.append_message","message":{"role":"user","content":"Shadowed"}}"#,
            "\n",
            r#"{"type":"turn.prompt","input":"Primary prompt"}"#,
            "\n",
        ),
    )
    .unwrap();
    let decoded = codec.decode(&primary).unwrap();
    let texts: Vec<String> = decoded
        .conversation
        .records
        .iter()
        .map(|r| r.text())
        .collect();
    assert_eq!(texts, ["Primary prompt"]);
}

#[test]
fn encode_roundtrips_wire_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let codec = aco_kimi_code::codec();
    let path = wire_fixture(tmp.path(), "session-1", "main", None);
    fs::write(&path, fixture_lines().join("\n") + "\n").unwrap();
    let conv = codec.decode(&path).unwrap().conversation;

    let encoded = codec.encode(&conv).unwrap();
    assert!(encoded.projection.dropped.is_empty());
    // Meta rows ride the metadata envelope — adapted, not dropped.
    assert!(encoded
        .projection
        .adapted
        .iter()
        .all(|(_, k)| *k == RecordKind::Meta));
    let lines: Vec<Value> = encoded
        .lines
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["type"], "metadata");
    assert_eq!(lines[1]["type"], "turn.prompt");
    assert_eq!(lines[1]["input"], "Review the fixture");
    assert_eq!(lines[2]["event"]["part"]["type"], "think");
    assert_eq!(lines[4]["event"]["type"], "tool.call");
    assert_eq!(lines[4]["event"]["callId"], "c1");
    assert_eq!(lines[5]["event"]["type"], "tool.result");
    assert_eq!(lines[6]["event"]["part"]["type"], "text");
    assert_eq!(lines[7]["type"], "usage.record");
    assert_eq!(lines[7]["usage"]["totalTokens"], 155);

    let rewritten = wire_fixture(tmp.path(), "session-2", "main", None);
    fs::write(&rewritten, encoded.lines.join("\n") + "\n").unwrap();
    let again = codec.decode(&rewritten).unwrap();
    assert_eq!(again.conversation.session_id, "session-2");
    let kinds: Vec<RecordKind> = again
        .conversation
        .records
        .iter()
        .map(|r| r.kind())
        .collect();
    assert!(kinds.contains(&RecordKind::User));
    assert!(kinds.contains(&RecordKind::Reasoning));
    assert!(kinds.contains(&RecordKind::ToolCall));
    assert!(kinds.contains(&RecordKind::ToolResult));
    assert!(kinds.contains(&RecordKind::Assistant));
}

#[test]
fn expand_root_and_store_path_honor_kimi_code_home() {
    let codec = aco_kimi_code::codec();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let kimi_home = tmp.path().join("kimi-elsewhere");
    let root = codec
        .history()
        .into_iter()
        .find(|r| r.kind == "kimi-session-store")
        .unwrap();

    std::env::set_var("KIMI_CODE_HOME", &kimi_home);
    assert_eq!(codec.expand_root(&root, &home), kimi_home);
    let mut conv = Conversation::new("sess-7");
    conv.cwd = Some("/work/proj".to_string());
    let target = codec.store_path(&home, &conv).unwrap();
    assert_eq!(target, kimi_home.join("proj/sess-7/agents/main/wire.jsonl"));
    std::env::remove_var("KIMI_CODE_HOME");

    assert_eq!(codec.expand_root(&root, &home), home.join(".kimi-code"));
}

#[test]
fn redact_line_rewrites_content_only() {
    let codec = aco_kimi_code::codec();

    let prompt: Value =
        serde_json::from_str(r#"{"type":"turn.prompt","input":"secret prompt","turnId":"t1"}"#)
            .unwrap();
    let out = codec.redact_line(&prompt, "[redacted]").unwrap();
    assert_eq!(out["input"], "[redacted]");
    assert_eq!(out["turnId"], "t1");

    let think: Value = serde_json::from_str(
        r#"{"type":"context.append_loop_event","turnId":"t1","event":{"type":"content.part","step":1,"part":{"type":"think","think":"private"}}}"#,
    )
    .unwrap();
    let out = codec.redact_line(&think, "[redacted]").unwrap();
    assert_eq!(out["event"]["part"]["think"], "[redacted]");
    assert_eq!(out["event"]["part"]["type"], "think");

    let call: Value = serde_json::from_str(
        r#"{"type":"context.append_loop_event","event":{"type":"tool.call","name":"exec","arguments":{"command":"secret","flags":["-x"]}}}"#,
    )
    .unwrap();
    let out = codec.redact_line(&call, "[redacted]").unwrap();
    assert_eq!(out["event"]["arguments"]["command"], "[redacted]");
    assert_eq!(out["event"]["arguments"]["flags"][0], "[redacted]");
    assert_eq!(out["event"]["name"], "exec");

    // Usage/metadata rows carry no conversation text.
    let usage: Value =
        serde_json::from_str(r#"{"type":"usage.record","usage":{"inputOther":1,"output":2}}"#)
            .unwrap();
    assert!(codec.redact_line(&usage, "[redacted]").is_none());
}
