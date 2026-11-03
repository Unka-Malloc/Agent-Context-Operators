use acop::model::{Conversation, RecordKind};
use std::path::PathBuf;

const OC_ID: &str = "12345678-1234-1234-1234-123456789abc";

/// The shared session-grouping rule resolves a line's session from its
/// declared keys or the conversation-uuid directory; the fixture keeps
/// the real `type:session` header + identity-less event lines shape.
fn jsonl_fixture(dir: &tempfile::TempDir) -> PathBuf {
    let session_dir = dir.path().join(OC_ID);
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
    let decoded = acop_opencode::codec().decode(&jsonl_fixture(&dir)).unwrap();
    let conv = &decoded.conversation;
    assert_eq!(conv.session_id, OC_ID);
    assert_eq!(conv.agent.as_deref(), Some("opencode"));
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
fn decode_json_document_and_text() {
    let dir = tempfile::tempdir().unwrap();
    let json = dir.path().join("doc.json");
    std::fs::write(
        &json,
        serde_json::json!({
            "id": "oc-doc",
            "messages": [
                {"role": "user", "content": "question one"},
                {"role": "assistant", "content": [{"type": "text", "text": "answer one"}]},
            ]
        })
        .to_string(),
    )
    .unwrap();
    let decoded = acop_opencode::codec().decode(&json).unwrap();
    assert_eq!(decoded.conversation.session_id, "oc-doc");
    let kinds: Vec<RecordKind> = decoded
        .conversation
        .records
        .iter()
        .map(|r| r.kind())
        .collect();
    assert_eq!(kinds, [RecordKind::User, RecordKind::Assistant]);

    let txt = dir.path().join("notes.txt");
    std::fs::write(&txt, "user: first\n\nassistant: second\n").unwrap();
    let decoded = acop_opencode::codec().decode(&txt).unwrap();
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
    let codec = acop_opencode::codec();
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
fn xdg_data_root_resolves_through_the_environment() {
    let codec = acop_opencode::codec();
    let dir = tempfile::tempdir().unwrap();
    let root = codec
        .history()
        .into_iter()
        .find(|root| root.path == "{xdg_data}/opencode")
        .expect("the card declares the xdg data-store root");
    std::env::set_var("XDG_DATA_HOME", dir.path());
    let expanded = codec.expand_root(&root, dir.path());
    std::env::remove_var("XDG_DATA_HOME");
    assert_eq!(expanded, dir.path().join("opencode"));
}

#[test]
fn encode_store_path_and_redact_pass_through() {
    let dir = tempfile::tempdir().unwrap();
    let codec = acop_opencode::codec();

    let decoded = codec.decode(&jsonl_fixture(&dir)).unwrap();
    let encoded = codec.encode(&decoded.conversation).unwrap();
    assert!(!encoded.lines.is_empty());

    let conv = Conversation::new("sess-1");
    let path = codec.store_path(dir.path(), &conv).unwrap();
    assert_eq!(path, dir.path().join(".local/share/opencode/sess-1.jsonl"));

    let line: serde_json::Value = serde_json::from_str(
        r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"secret"}]}}"#,
    )
    .unwrap();
    let out = codec.redact_line(&line, "[redacted]").unwrap();
    assert_eq!(out["message"]["content"][0]["text"], "[redacted]");
}

#[test]
fn history_comes_from_the_card() {
    let codec = acop_opencode::codec();
    let history = codec.history();
    assert!(history.iter().any(|root| {
        root.kind == "opencode-data-store" && root.path == "{home}/.local/share/opencode"
    }));
    // Config roots are inventory, never sessions.
    assert!(history.iter().any(|root| root.kind == "opencode-config"));
    assert_eq!(codec.session_kinds(), &["opencode-data-store"]);
    assert_eq!(codec.id(), "opencode");
}

/// Container files enumerate one `SessionRef` per stored session key
/// through `acop::generic::sqlite_session_keys`.
#[test]
fn sqlite_file_enumerates_session_keys() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("opencode.db");
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE history (key TEXT, value TEXT);
             INSERT INTO history VALUES ('conv-1', '{\"role\":\"user\",\"text\":\"hi\"}');
             INSERT INTO history VALUES ('conv-1', '{\"role\":\"assistant\",\"text\":\"hello\"}');",
        )
        .unwrap();
    let sessions = acop_opencode::codec().sessions_in_file(&db).unwrap();
    assert!(!sessions.is_empty());
    assert!(sessions.iter().all(|s| s.key.is_some() && s.path == db));
}

#[test]
fn archive_session_flips_time_archived_on_session_rows() {
    let home = tempfile::tempdir().unwrap();
    let db = home.path().join("opencode.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, time_archived INTEGER)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO session_v2 (id) VALUES ('ses_abc')", [])
            .unwrap();
    }
    let codec = acop_opencode::codec();
    let session = acop::codec::SessionRef {
        id: "ses_abc".into(),
        path: db.clone(),
        key: Some("session_v2/ses_abc".into()),
        parent: None,
    };
    assert!(codec.archive_session(&session, home.path(), false).unwrap());
    let conn = rusqlite::Connection::open(&db).unwrap();
    let archived: Option<i64> = conn
        .query_row(
            "SELECT time_archived FROM session_v2 WHERE id = 'ses_abc'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(archived.is_some(), "time_archived was set");

    // File-backed sessions and foreign tables report no marker.
    let file_session = acop::codec::SessionRef {
        id: "x".into(),
        path: home.path().join("x.jsonl"),
        key: None,
        parent: None,
    };
    assert!(!codec
        .archive_session(&file_session, home.path(), false)
        .unwrap());
    let other_table = acop::codec::SessionRef {
        key: Some("message/1".into()),
        ..session.clone()
    };
    assert!(!codec
        .archive_session(&other_table, home.path(), false)
        .unwrap());
}
