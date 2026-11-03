use acop::model::{Conversation, RecordKind};
use std::path::PathBuf;

const HM_ID: &str = "12345678-1234-1234-1234-123456789abc";

/// Hermes transcripts sit under a conversation-uuid directory; the
/// layout supplies every line's session group and the `session`
/// header redeclares the same id.
fn jsonl_fixture(dir: &tempfile::TempDir) -> PathBuf {
    let session_dir = dir.path().join(HM_ID);
    std::fs::create_dir_all(&session_dir).unwrap();
    let path = session_dir.join("transcript.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"type":"session","id":"12345678-1234-1234-1234-123456789abc","cwd":"/work/proj","timestamp":"2026-01-01T10:00:00Z"}"#,
            "\n",
            r#"{"role":"user","text":"hello agent","timestamp":"2026-01-01T10:00:01Z"}"#,
            "\n",
            r#"{"type":"message","timestamp":"2026-01-01T10:00:02Z","message":{"role":"assistant","model":"m-1","content":[{"type":"thinking","thinking":"pondering"},{"type":"text","text":"working on it"}],"usage":{"promptTokens":10,"completionTokens":5}}}"#,
            "\n",
            "not json\n",
        ),
    )
    .unwrap();
    path
}

#[test]
fn decode_jsonl_through_the_generic_lane() {
    let dir = tempfile::tempdir().unwrap();
    let decoded = acop_hermes::codec().decode(&jsonl_fixture(&dir)).unwrap();
    let conv = &decoded.conversation;
    assert_eq!(conv.session_id, HM_ID);
    assert_eq!(conv.agent.as_deref(), Some("hermes"));
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
fn decode_json_document_and_markdown() {
    let dir = tempfile::tempdir().unwrap();
    let json = dir.path().join("doc.json");
    std::fs::write(
        &json,
        serde_json::json!({
            "id": "hm-doc",
            "title": "synthetic conversation",
            "messages": [
                {"role": "user", "content": "question one"},
                {"role": "assistant", "content": [{"type": "text", "text": "answer one"}]},
            ]
        })
        .to_string(),
    )
    .unwrap();
    let decoded = acop_hermes::codec().decode(&json).unwrap();
    assert_eq!(decoded.conversation.session_id, "hm-doc");
    assert_eq!(
        decoded.conversation.title.as_deref(),
        Some("synthetic conversation")
    );
    let kinds: Vec<RecordKind> = decoded
        .conversation
        .records
        .iter()
        .map(|r| r.kind())
        .collect();
    assert_eq!(kinds, [RecordKind::User, RecordKind::Assistant]);

    let md = dir.path().join("notes.md");
    std::fs::write(&md, "# Title\n\nuser: first\n\nassistant: second\n").unwrap();
    let decoded = acop_hermes::codec().decode(&md).unwrap();
    assert_eq!(decoded.conversation.title.as_deref(), Some("Title"));
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
    let codec = acop_hermes::codec();
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
    let codec = acop_hermes::codec();

    let decoded = codec.decode(&jsonl_fixture(&dir)).unwrap();
    let encoded = codec.encode(&decoded.conversation).unwrap();
    assert!(!encoded.lines.is_empty());

    let conv = Conversation::new("sess-1");
    let path = codec.store_path(dir.path(), &conv).unwrap();
    assert_eq!(path, dir.path().join(".hermes/sess-1.jsonl"));

    let line: serde_json::Value = serde_json::from_str(
        r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"secret"}]}}"#,
    )
    .unwrap();
    let out = codec.redact_line(&line, "[redacted]").unwrap();
    assert_eq!(out["message"]["content"][0]["text"], "[redacted]");
}

#[test]
fn history_comes_from_the_card() {
    let codec = acop_hermes::codec();
    let history = codec.history();
    assert_eq!(
        history
            .iter()
            .map(|root| (root.kind, root.path))
            .collect::<Vec<_>>(),
        [("hermes-profile-store", "{home}/.hermes")]
    );
    assert_eq!(codec.session_kinds(), &["hermes-profile-store"]);
    assert_eq!(codec.id(), "hermes");
}

/// Container files enumerate one `SessionRef` per stored session key
/// through `acop::generic::sqlite_session_keys`.
#[test]
fn sqlite_file_enumerates_session_keys() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("store.sqlite3");
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE history (key TEXT, value TEXT);
             INSERT INTO history VALUES ('conv-1', '{\"role\":\"user\",\"text\":\"hi\"}');
             INSERT INTO history VALUES ('conv-1', '{\"role\":\"assistant\",\"text\":\"hello\"}');",
        )
        .unwrap();
    let sessions = acop_hermes::codec().sessions_in_file(&db).unwrap();
    assert!(!sessions.is_empty());
    assert!(sessions.iter().all(|s| s.key.is_some() && s.path == db));
}
