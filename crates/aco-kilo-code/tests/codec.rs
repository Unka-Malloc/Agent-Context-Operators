use aco::codec::{Capability, SessionRef};
use aco::model::{Body, Conversation, RecordKind};
use rusqlite::{params, Connection};

/// An OpenCode-shaped kilo store: `session`/`message`/`part` tables.
fn openagent_db(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let path = dir.path().join("kilo.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                parent_id TEXT,
                title TEXT,
                directory TEXT,
                path TEXT,
                model TEXT,
                time_created INTEGER,
                time_updated INTEGER,
                tokens_input INTEGER,
                tokens_output INTEGER,
                tokens_reasoning INTEGER,
                tokens_cache_read INTEGER,
                tokens_cache_write INTEGER
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT,
                time_created INTEGER,
                time_updated INTEGER,
                data TEXT
            );
            CREATE TABLE part (
                id TEXT PRIMARY KEY,
                message_id TEXT,
                session_id TEXT,
                time_created INTEGER,
                data TEXT
            );
            CREATE TABLE account (id TEXT PRIMARY KEY, token TEXT);
            CREATE TABLE session_fts (id TEXT);",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO session VALUES (
                'ses_main', NULL, 'fix login', '/work/p', '/work/p',
                'kilo-auto/free', 1700000000000, 1700000005000,
                100, 20, 0, 10, 0
            )",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO session VALUES ('ses_sub', 'ses_main', 'sub', NULL, NULL, NULL, 1700000001000, 1700000002000, 0, 0, 0, 0, 0)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO message VALUES (
                'msg_u1', 'ses_main', 1700000001000, NULL,
                '{\"role\":\"user\",\"time\":{\"created\":1700000001000}}'
            )",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO message VALUES (
                'msg_a1', 'ses_main', 1700000002000, 1700000004000,
                '{\"role\":\"assistant\",\"parentID\":\"msg_u1\",\"modelID\":\"kilo-auto/free\",\"time\":{\"created\":1700000002000,\"completed\":1700000004000},\"tokens\":{\"input\":10,\"output\":5,\"cache\":{\"read\":2}}}'
            )",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO message VALUES (
                'msg_s1', 'ses_sub', 1700000001000, NULL,
                '{\"role\":\"tool_result\",\"type\":\"tool_result\",\"result\":\"Synthetic result\"}'
            )",
            [],
        )
        .unwrap();
    // Distinct `time_created` values pin the part order the codec inherits
    // from LicoUp (`time_created ASC, id ASC`); equal timestamps would sort
    // by `id` instead.
    for (id, message, session, created, data) in [
        (
            "part_u1",
            "msg_u1",
            "ses_main",
            1700000001500_i64,
            r#"{"type":"text","text":"hello"}"#,
        ),
        (
            "part_r1",
            "msg_a1",
            "ses_main",
            1700000002100,
            r#"{"type":"reasoning","text":"hmm"}"#,
        ),
        (
            "part_t1",
            "msg_a1",
            "ses_main",
            1700000002200,
            r#"{"type":"tool","callID":"c1","tool":"read","state":{"status":"completed","input":{"path":"/a"},"output":"file body"}}"#,
        ),
        (
            "part_x1",
            "msg_a1",
            "ses_main",
            1700000002300,
            r#"{"type":"text","text":"reading"}"#,
        ),
        (
            "part_s1",
            "msg_a1",
            "ses_main",
            1700000002400,
            r#"{"type":"step-finish","tokens":{"input":3,"output":1}}"#,
        ),
    ] {
        connection
            .execute(
                "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, message, session, created, data],
            )
            .unwrap();
    }
    path
}

/// A db with no openagent triple — only a generic `history` table and a
/// rejected `account` table.
fn generic_db(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let path = dir.path().join("other.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE history (
                key TEXT,
                value TEXT
            );
            CREATE TABLE account (id TEXT PRIMARY KEY, token TEXT);
            CREATE TABLE notes (body TEXT);",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO history VALUES ('conv-1', '{\"role\":\"user\",\"text\":\"hi there\"}')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO history VALUES ('conv-1', '{\"role\":\"assistant\",\"text\":\"answer\"}')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO history VALUES ('meta', 'plain metrics text')",
            [],
        )
        .unwrap();
    connection
        .execute("INSERT INTO notes VALUES ('unrelated note')", [])
        .unwrap();
    path
}

#[test]
fn enumerate_openagent_db_emits_session_refs() {
    let dir = tempfile::tempdir().unwrap();
    let db = openagent_db(&dir);
    let root = aco::HistoryRoot {
        path: "{home}/.local/share/kilo",
        kind: "kilo-session-store",
        store: aco::StoreKind::SqliteDb,
    };
    let sessions = aco_kilo_code::codec().enumerate(&root, dir.path()).unwrap();
    let mut ids: Vec<String> = sessions.iter().map(|s| s.id.clone()).collect();
    ids.sort();
    assert_eq!(ids, ["ses_main", "ses_sub"]);
    for session in &sessions {
        assert_eq!(session.path, db);
        assert_eq!(session.key.as_deref(), Some(session.id.as_str()));
    }
}

#[test]
fn decode_session_projects_openagent_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = openagent_db(&dir);
    let session = SessionRef {
        id: "ses_main".to_string(),
        path: db,
        key: Some("ses_main".to_string()),
    };
    let decoded = aco_kilo_code::codec().decode_session(&session).unwrap();
    let conv = &decoded.conversation;
    assert_eq!(conv.session_id, "ses_main");
    assert_eq!(conv.title.as_deref(), Some("fix login"));
    assert_eq!(conv.cwd.as_deref(), Some("/work/p"));
    assert_eq!(conv.created_at.as_deref(), Some("2023-11-14T22:13:20.000Z"));
    let kinds: Vec<RecordKind> = conv.records.iter().map(|r| r.kind()).collect();
    assert_eq!(
        kinds,
        [
            RecordKind::User,
            RecordKind::Reasoning,
            RecordKind::ToolCall,
            RecordKind::ToolResult,
            RecordKind::Assistant,
            RecordKind::Meta, // step-finish usage
            RecordKind::Meta, // message-level tokens usage
            RecordKind::Meta, // session row metadata (model/projectPath)
            RecordKind::Meta, // session usage counters
        ]
    );
    // The reasoning part keeps the provider text.
    match &conv.records[1].body {
        Body::Reasoning { text } => assert_eq!(text, "hmm"),
        other => panic!("expected reasoning, got {other:?}"),
    }
    // The tool part decodes both call and result from `state`.
    match &conv.records[2].body {
        Body::ToolCall {
            call_id,
            name,
            args,
        } => {
            assert_eq!(call_id.as_deref(), Some("c1"));
            assert_eq!(name, "read");
            assert_eq!(args["path"], "/a");
        }
        other => panic!("expected tool-call, got {other:?}"),
    }
    match &conv.records[3].body {
        Body::ToolResult {
            content, is_error, ..
        } => {
            assert_eq!(*content, "file body");
            assert!(!is_error);
        }
        other => panic!("expected tool-result, got {other:?}"),
    }
    // Message-level `tokens` blob → usage meta.
    match &conv.records[6].body {
        Body::Meta { subtype, data } => {
            assert_eq!(subtype, "usage");
            assert_eq!(data["input"], 10);
            assert_eq!(data["cache"]["read"], 2);
        }
        other => panic!("expected usage meta, got {other:?}"),
    }
    // Session-row columns without a canonical home → session meta.
    match &conv.records[7].body {
        Body::Meta { subtype, data } => {
            assert_eq!(subtype, "session");
            assert_eq!(data["projectPath"], "/work/p");
            assert_eq!(data["model"], "kilo-auto/free");
        }
        other => panic!("expected session meta, got {other:?}"),
    }
    // Session token columns → normalized usage meta.
    match &conv.records[8].body {
        Body::Meta { subtype, data } => {
            assert_eq!(subtype, "usage");
            assert_eq!(data["scope"], "session");
            assert_eq!(data["promptTokens"], 110);
            assert_eq!(data["cachedInputTokens"], 10);
            assert_eq!(data["totalTokens"], 130);
        }
        other => panic!("expected session usage meta, got {other:?}"),
    }
    // Step-finish is a lifecycle event the canonical model has no kind
    // for — it surfaces its usage and reports the skipped part.
    assert!(decoded
        .skipped
        .iter()
        .any(|s| s.native_type.as_deref() == Some("step-finish")));
    // Parent id recorded on the assistant-side records.
    assert_eq!(
        conv.records[1].parent.as_deref(),
        Some("msg_u1"),
        "assistant records keep the message's parentID"
    );
}

#[test]
fn decode_session_folds_bare_tool_result_blob() {
    let dir = tempfile::tempdir().unwrap();
    let db = openagent_db(&dir);
    let session = SessionRef {
        id: "ses_sub".to_string(),
        path: db,
        key: Some("ses_sub".to_string()),
    };
    let decoded = aco_kilo_code::codec().decode_session(&session).unwrap();
    let kinds: Vec<RecordKind> = decoded
        .conversation
        .records
        .iter()
        .map(|r| r.kind())
        .collect();
    // The tool_result blob plus the session row's parentId metadata.
    assert_eq!(kinds, [RecordKind::ToolResult, RecordKind::Meta]);
    match &decoded.conversation.records[0].body {
        Body::ToolResult { content, .. } => assert_eq!(*content, "Synthetic result"),
        other => panic!("expected tool-result, got {other:?}"),
    }
    match &decoded.conversation.records[1].body {
        Body::Meta { subtype, data } => {
            assert_eq!(subtype, "session");
            assert_eq!(data["parentId"], "ses_main");
        }
        other => panic!("expected session meta, got {other:?}"),
    }
}

#[test]
fn enumerate_generic_db_uses_probe_groups() {
    let dir = tempfile::tempdir().unwrap();
    generic_db(&dir);
    let root = aco::HistoryRoot {
        path: "{home}/.local/share/kilo",
        kind: "kilo-session-store",
        store: aco::StoreKind::SqliteDb,
    };
    let sessions = aco_kilo_code::codec().enumerate(&root, dir.path()).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "conv-1");
    assert_eq!(sessions[0].key.as_deref(), Some("history:conv-1"));
    let decoded = aco_kilo_code::codec().decode_session(&sessions[0]).unwrap();
    let kinds: Vec<RecordKind> = decoded
        .conversation
        .records
        .iter()
        .map(|r| r.kind())
        .collect();
    assert_eq!(kinds, [RecordKind::User, RecordKind::Assistant]);
}

#[test]
fn probe_rejects_account_fts_embedding_and_sqlite_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("probe.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE account (id TEXT, value TEXT);
             CREATE TABLE control_account (id TEXT, value TEXT);
             CREATE TABLE chat_fts (id TEXT, value TEXT);
             CREATE TABLE vector_embedding (id TEXT, value TEXT);
             CREATE TABLE workspace (id TEXT, value TEXT);
             INSERT INTO account VALUES ('a', '{\"role\":\"user\",\"text\":\"x\"}');
             INSERT INTO chat_fts VALUES ('b', '{\"role\":\"user\",\"text\":\"y\"}');
             INSERT INTO workspace VALUES ('c', '{\"role\":\"user\",\"text\":\"z\"}');",
        )
        .unwrap();
    let root = aco::HistoryRoot {
        path: "{home}/.local/share/kilo",
        kind: "kilo-session-store",
        store: aco::StoreKind::SqliteDb,
    };
    // `workspace` names no history column per the probe rules, so no
    // session refs survive; a db with zero session rows is empty.
    let sessions = aco_kilo_code::codec().enumerate(&root, dir.path()).unwrap();
    assert!(sessions.is_empty());
}

#[test]
fn decode_jsonl_groups_by_native_session() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"sessionId":"s1","role":"user","content":"one"}"#,
            "\n",
            r#"{"sessionId":"s1","role":"assistant","content":"two"}"#,
            "\n",
            r#"{"sessionId":"s2","role":"user","content":"other session"}"#,
            "\n",
            "not-json\n",
        ),
    )
    .unwrap();
    let decoded = aco_kilo_code::codec().decode(&path).unwrap();
    assert_eq!(decoded.conversation.session_id, "s1");
    let kinds: Vec<RecordKind> = decoded
        .conversation
        .records
        .iter()
        .map(|r| r.kind())
        .collect();
    assert_eq!(kinds, [RecordKind::User, RecordKind::Assistant]);
    assert!(decoded
        .skipped
        .iter()
        .any(|s| s.native_type.as_deref() == Some("session:s2")));
    assert!(decoded.skipped.iter().any(|s| s.reason == "corrupt"));
}

#[test]
fn codec_is_read_only() {
    let codec = aco_kilo_code::codec();
    assert!(!codec.capabilities().writable);
    for kind in RecordKind::ALL {
        assert_eq!(codec.capabilities().of(kind), Capability::Unsupported);
    }
    let conv = Conversation::new("s1");
    let err = codec.encode(&conv).unwrap_err();
    assert!(err.to_string().contains("kilocode"));
    assert!(codec
        .store_path(std::path::Path::new("/tmp"), &conv)
        .is_err());
}

#[test]
fn history_comes_from_the_card() {
    let codec = aco_kilo_code::codec();
    let history = codec.history();
    assert_eq!(
        history
            .iter()
            .map(|root| (root.kind, root.path))
            .collect::<Vec<_>>(),
        [
            ("kilo-session-store", "{home}/.local/share/kilo"),
            ("kilo-config", "{home}/.config/kilo")
        ]
    );
    assert_eq!(codec.session_kinds(), &["kilo-session-store"]);
    assert_eq!(codec.id(), "kilocode");
}

#[test]
fn select_session_files_accepts_the_licoup_extension_set() {
    let codec = aco_kilo_code::codec();
    let files = vec![
        std::path::PathBuf::from("a.jsonl"),
        std::path::PathBuf::from("b.ndjson"),
        std::path::PathBuf::from("c.json"),
        std::path::PathBuf::from("d.md"),
        std::path::PathBuf::from("e.txt"),
        std::path::PathBuf::from("f.log"),
        std::path::PathBuf::from("g.sqlite"),
        std::path::PathBuf::from("h.sqlite3"),
        std::path::PathBuf::from("i.db"),
        std::path::PathBuf::from("j.vscdb"),
        std::path::PathBuf::from("k.backup"),
        std::path::PathBuf::from("codebase-external.sqlite"),
        std::path::PathBuf::from("l.png"),
    ];
    let selected = codec.select_session_files(files);
    let names: Vec<String> = selected
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
