use acop::model::{Conversation, RecordKind};
use std::path::PathBuf;

fn jsonl_fixture(dir: &tempfile::TempDir) -> PathBuf {
    let path = dir.path().join("transcript.ndjson");
    std::fs::write(
        &path,
        concat!(
            r#"{"type":"session","id":"claw-42","cwd":"/work/proj","timestamp":"2026-01-01T10:00:00Z"}"#,
            "\n",
            r#"{"sessionId":"claw-42","role":"user","text":"hello agent","timestamp":"2026-01-01T10:00:01Z"}"#,
            "\n",
            r#"{"sessionId":"claw-42","type":"message","timestamp":"2026-01-01T10:00:02Z","message":{"role":"assistant","model":"m-1","content":[{"type":"thinking","thinking":"pondering"},{"type":"text","text":"working on it"}],"usage":{"promptTokens":10,"completionTokens":5}}}"#,
            "\n",
            "not json\n",
        ),
    )
    .unwrap();
    path
}

#[test]
fn decode_ndjson_through_the_generic_lane() {
    let dir = tempfile::tempdir().unwrap();
    let decoded = acop_openclaw::codec().decode(&jsonl_fixture(&dir)).unwrap();
    let conv = &decoded.conversation;
    assert_eq!(conv.session_id, "claw-42");
    assert_eq!(conv.agent.as_deref(), Some("openclaw"));
    assert_eq!(conv.cwd.as_deref(), Some("/work/proj"));
    let kinds: Vec<RecordKind> = conv.records.iter().map(|r| r.kind()).collect();
    assert_eq!(
        kinds,
        [
            RecordKind::System, // the {"type":"session"} header event
            RecordKind::User,
            RecordKind::Reasoning,
            RecordKind::Assistant,
            RecordKind::Meta, // usage on the assistant line
        ]
    );
    assert!(decoded.skipped.iter().any(|s| s.reason == "corrupt"));
}

#[test]
fn decode_json_document_and_log() {
    let dir = tempfile::tempdir().unwrap();
    let json = dir.path().join("doc.json");
    std::fs::write(
        &json,
        serde_json::json!([
            {"role": "user", "text": "hi"},
            {"role": "agent", "text": "hello", "model": "m-1"},
        ])
        .to_string(),
    )
    .unwrap();
    let decoded = acop_openclaw::codec().decode(&json).unwrap();
    let kinds: Vec<RecordKind> = decoded
        .conversation
        .records
        .iter()
        .map(|r| r.kind())
        .collect();
    assert_eq!(kinds, [RecordKind::User, RecordKind::Assistant]);

    let log = dir.path().join("run.log");
    std::fs::write(&log, "user: first\n\nassistant: second\n").unwrap();
    let decoded = acop_openclaw::codec().decode(&log).unwrap();
    let kinds: Vec<RecordKind> = decoded
        .conversation
        .records
        .iter()
        .map(|r| r.kind())
        .collect();
    assert_eq!(kinds, [RecordKind::User, RecordKind::Assistant]);
}

#[test]
fn select_session_files_accepts_the_licoup_extension_set() {
    let codec = acop_openclaw::codec();
    let files = vec![
        PathBuf::from("a.jsonl"),
        PathBuf::from("b.ndjson"),
        PathBuf::from("c.json"),
        PathBuf::from("d.md"),
        PathBuf::from("e.txt"),
        PathBuf::from("f.log"),
        PathBuf::from("g.sqlite"),
        PathBuf::from("h.sqlite3"),
        PathBuf::from("i.db"),
        PathBuf::from("j.vscdb"),
        PathBuf::from("k.jsonl.backup"),
        PathBuf::from("codebase-external.sqlite"),
        PathBuf::from("l.png"),
    ];
    let names: Vec<String> = codec
        .select_session_files(files)
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        [
            "a.jsonl",
            "b.ndjson",
            "c.json",
            "d.md",
            "e.txt",
            "f.log",
            "g.sqlite",
            "h.sqlite3",
            "i.db"
        ]
    );
}

#[test]
fn encode_store_path_and_redact_pass_through() {
    let dir = tempfile::tempdir().unwrap();
    let codec = acop_openclaw::codec();

    let decoded = codec.decode(&jsonl_fixture(&dir)).unwrap();
    let encoded = codec.encode(&decoded.conversation).unwrap();
    assert!(!encoded.lines.is_empty());

    let conv = Conversation::new("sess-1");
    let path = codec.store_path(dir.path(), &conv).unwrap();
    assert_eq!(path, dir.path().join(".openclaw/sess-1.jsonl"));

    let line: serde_json::Value = serde_json::from_str(
        r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"secret"}]}}"#,
    )
    .unwrap();
    let out = codec.redact_line(&line, "[redacted]").unwrap();
    assert_eq!(out["message"]["content"][0]["text"], "[redacted]");
}

#[test]
fn history_comes_from_the_card() {
    let codec = acop_openclaw::codec();
    let history = codec.history();
    assert_eq!(
        history
            .iter()
            .map(|root| (root.kind, root.path))
            .collect::<Vec<_>>(),
        [("openclaw-store", "{home}/.openclaw")]
    );
    assert_eq!(codec.session_kinds(), &["openclaw-store"]);
    assert_eq!(codec.id(), "openclaw");
}

/// Container files enumerate one `SessionRef` per stored session key
/// through `acop::generic::sqlite_session_keys`.
#[test]
fn sqlite_file_enumerates_session_keys() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("store.db");
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE history (key TEXT, value TEXT);
             INSERT INTO history VALUES ('conv-1', '{\"role\":\"user\",\"text\":\"hi\"}');
             INSERT INTO history VALUES ('conv-1', '{\"role\":\"assistant\",\"text\":\"hello\"}');",
        )
        .unwrap();
    let sessions = acop_openclaw::codec().sessions_in_file(&db).unwrap();
    assert!(!sessions.is_empty());
    assert!(sessions.iter().all(|s| s.key.is_some() && s.path == db));
}
