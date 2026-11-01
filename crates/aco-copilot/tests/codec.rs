use aco::model::{Body, Conversation, Record, RecordKind};
use aco::store::{HistoryRoot, StoreKind};

const TRANSCRIPT: &str = concat!(
    r#"{"type":"session.start","data":{"sessionId":"copilot-1","cwd":"/work/p"},"timestamp":"2025-01-01T00:00:00Z"}"#,
    "\n",
    r#"{"type":"user.message","data":{"messageId":"u1","content":"inspect routing"},"timestamp":"2025-01-01T00:00:01Z"}"#,
    "\n",
    r#"{"type":"assistant.message","data":{"messageId":"a1","content":"","reasoningText":"thinking","toolRequests":[{"toolCallId":"c1","name":"readFile","arguments":{"path":"/a"}}],"model":"copilot-model"},"timestamp":"2025-01-01T00:00:02Z"}"#,
    "\n",
    r#"{"type":"tool.execution_start","data":{"toolCallId":"c1","toolName":"readFile","arguments":{"path":"/a"}},"timestamp":"2025-01-01T00:00:03Z"}"#,
    "\n",
    r#"{"type":"tool.execution_complete","data":{"toolCallId":"c1","toolName":"readFile","result":"file body","success":true},"timestamp":"2025-01-01T00:00:04Z"}"#,
    "\n",
    r#"{"type":"assistant.message","data":{"messageId":"a2","content":"done","model":"copilot-model","usage":{"input_tokens":20,"output_tokens":6}},"timestamp":"2025-01-01T00:00:05Z"}"#,
    "\n",
    r#"{"type":"session.shutdown","data":{"reason":"exit"},"timestamp":"2025-01-01T00:00:06Z"}"#,
    "\n",
);

fn transcript_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let session_dir = dir.path().join("session-state").join("copilot-1");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(session_dir.join("events.jsonl"), TRANSCRIPT).unwrap();
    dir
}

fn directory_root() -> HistoryRoot {
    HistoryRoot {
        path: "{home}/.copilot",
        kind: "copilot-session-store",
        store: StoreKind::Directory,
    }
}

fn sqlite_root() -> HistoryRoot {
    HistoryRoot {
        path: "{home}/.copilot/session-store.db",
        kind: "copilot-session-store",
        store: StoreKind::SqliteDb,
    }
}

#[test]
fn decode_projects_transcript_rows() {
    let dir = transcript_dir();
    let path = dir.path().join("session-state/copilot-1/events.jsonl");
    let decoded = aco_copilot::codec().decode(&path).unwrap();
    let conv = &decoded.conversation;
    assert_eq!(conv.session_id, "copilot-1");
    assert_eq!(conv.cwd.as_deref(), Some("/work/p"));
    assert_eq!(conv.created_at.as_deref(), Some("2025-01-01T00:00:00Z"));
    assert_eq!(conv.updated_at.as_deref(), Some("2025-01-01T00:00:06Z"));
    let kinds: Vec<RecordKind> = conv.records.iter().map(Record::kind).collect();
    assert_eq!(
        kinds,
        [
            RecordKind::User,
            RecordKind::Reasoning,
            RecordKind::ToolCall,
            RecordKind::ToolResult,
            RecordKind::Assistant,
            RecordKind::Meta,
            RecordKind::Meta,
        ]
    );
    assert_eq!(decoded.record_lines, vec![1, 2, 2, 4, 5, 5, 6]);
    match &conv.records[2].body {
        Body::ToolCall {
            call_id,
            name,
            args,
        } => {
            assert_eq!(call_id.as_deref(), Some("c1"));
            assert_eq!(name, "readFile");
            assert_eq!(args["path"], "/a");
        }
        other => panic!("expected tool call, got {other:?}"),
    }
    match &conv.records[4].body {
        Body::Assistant { text, model } => {
            assert_eq!(text, "done");
            assert_eq!(model.as_deref(), Some("copilot-model"));
        }
        other => panic!("expected assistant, got {other:?}"),
    }
    match &conv.records[5].body {
        Body::Meta { subtype, data } => {
            assert_eq!(subtype, "usage");
            assert_eq!(data["promptTokens"], 20);
            assert_eq!(data["completionTokens"], 6);
            assert_eq!(data["totalTokens"], 26);
        }
        other => panic!("expected usage meta, got {other:?}"),
    }
    // The tool.execution_start row for c1 was already carried by the
    // assistant message's toolRequests — reported, not double-counted.
    assert_eq!(decoded.skipped.len(), 1);
    assert_eq!(
        decoded.skipped[0].native_type.as_deref(),
        Some("tool.execution_start")
    );
}

#[test]
fn session_id_prefers_header_then_directory_name() {
    let dir = transcript_dir();
    let events = dir.path().join("session-state/copilot-1/events.jsonl");
    assert_eq!(aco_copilot::codec().session_id(&events), "copilot-1");
    // No header: the session-state directory name is the id.
    let bare_dir = dir.path().join("session-state/idx-9");
    std::fs::create_dir_all(&bare_dir).unwrap();
    let bare = bare_dir.join("events.jsonl");
    std::fs::write(&bare, r#"{"type":"user.message","data":{"content":"x"}}"#).unwrap();
    assert_eq!(aco_copilot::codec().session_id(&bare), "idx-9");
    // A loose transcript file falls back to its stem.
    let loose = dir.path().join("abc-session.jsonl");
    std::fs::write(&loose, r#"{"type":"user.message","data":{"content":"x"}}"#).unwrap();
    assert_eq!(aco_copilot::codec().session_id(&loose), "abc-session");
}

#[test]
fn select_session_files_accepts_the_licoup_extension_set() {
    let files: Vec<std::path::PathBuf> = [
        "events.jsonl",
        "log.ndjson",
        "export.json",
        "state.vscdb",
        "session-store.db",
        "backup.sqlite3",
        "archive.sqlite",
        "workspace.yaml",
    ]
    .into_iter()
    .map(std::path::PathBuf::from)
    .collect();
    let selected = aco_copilot::codec().select_session_files(files);
    assert_eq!(
        selected,
        [
            "events.jsonl",
            "log.ndjson",
            "export.json",
            "state.vscdb",
            "session-store.db",
            "backup.sqlite3",
            "archive.sqlite"
        ]
        .into_iter()
        .map(std::path::PathBuf::from)
        .collect::<Vec<_>>()
    );
}

#[test]
fn encode_roundtrips_native_rows() {
    let dir = transcript_dir();
    let decoded = aco_copilot::codec()
        .decode(&dir.path().join("session-state/copilot-1/events.jsonl"))
        .unwrap();
    let encoded = aco_copilot::codec().encode(&decoded.conversation).unwrap();
    assert!(encoded.projection.dropped.is_empty());
    // reasoning, usage meta and the preserved shutdown row are adapted.
    assert_eq!(encoded.projection.adapted.len(), 3);
    let lines: Vec<serde_json::Value> = encoded
        .lines
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["type"], "session.start");
    assert_eq!(lines[0]["data"]["sessionId"], "copilot-1");
    assert_eq!(lines[1]["type"], "user.message");
    assert_eq!(lines[1]["data"]["content"], "inspect routing");
    // Reasoning and the tool call decoded ahead of any assistant record
    // render as standalone rows — the transcript's own event types.
    assert_eq!(lines[2]["type"], "assistant.reasoning");
    assert_eq!(lines[2]["data"]["content"], "thinking");
    assert_eq!(lines[3]["type"], "tool.execution_start");
    assert_eq!(lines[3]["data"]["toolCallId"], "c1");
    assert_eq!(lines[3]["data"]["toolName"], "readFile");
    assert_eq!(lines[4]["type"], "tool.execution_complete");
    assert_eq!(lines[4]["data"]["result"], "file body");
    assert_eq!(lines[4]["data"]["success"], true);
    assert_eq!(lines[5]["type"], "assistant.message");
    assert_eq!(lines[5]["data"]["content"], "done");
    assert_eq!(lines[5]["data"]["model"], "copilot-model");
    assert_eq!(lines[5]["data"]["usage"]["promptTokens"], 20);
    // The lifecycle row decodes to Meta and re-emits verbatim.
    assert_eq!(lines[6]["type"], "session.shutdown");
    assert_eq!(lines[6]["data"]["reason"], "exit");
}

#[test]
fn store_path_lands_in_session_state_layout() {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("COPILOT_HOME", dir.path());
    let conv = Conversation::new("imported-1");
    let path = aco_copilot::codec().store_path(dir.path(), &conv).unwrap();
    std::env::remove_var("COPILOT_HOME");
    assert_eq!(
        path,
        dir.path()
            .join("session-state")
            .join("imported-1")
            .join("events.jsonl")
    );
}

#[test]
fn enumerate_directory_root_finds_transcripts_and_store_rows() {
    let dir = tempfile::tempdir().unwrap();
    let session_dir = dir.path().join("session-state/copilot-1");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(session_dir.join("events.jsonl"), TRANSCRIPT).unwrap();
    // A VS Code-style state store beside the transcripts.
    let db = dir.path().join("state.vscdb");
    {
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection
            .execute(
                "CREATE TABLE ItemTable (key TEXT NOT NULL, value TEXT NOT NULL)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
                [
                    "github.copilot-chat.chatSessions",
                    r#"{"chatSessions":[{"id":"copilot-chat-1","createdAt":"2025-01-01T00:00:00Z","updatedAt":"2025-01-02T00:00:00Z","cwd":"/proj","title":"Chat One","messages":[{"role":"user","content":"hello"},{"role":"assistant","content":"answer","model":"m1"}]},{"messages":[{"role":"user","content":"idless prompt"}]}]}"#,
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO ItemTable (key, value) VALUES ('storage', '{}')",
                [],
            )
            .unwrap();
    }
    let sessions = aco_copilot::codec()
        .enumerate(&directory_root(), dir.path())
        .unwrap();
    assert_eq!(sessions.len(), 3);
    assert!(sessions
        .iter()
        .any(|s| s.key.is_none() && s.id == "copilot-1" && s.path.ends_with("events.jsonl")));
    let by_key: Vec<_> = sessions.iter().filter(|s| s.key.is_some()).collect();
    assert_eq!(by_key.len(), 2);
    assert_eq!(
        by_key[0].key.as_deref(),
        Some("github.copilot-chat.chatSessions#0")
    );
    assert_eq!(by_key[0].id, "copilot-chat-1");
    // The idless item gets a positional id inside its store.
    assert_eq!(by_key[1].id, "chatSessions-1");
}

#[test]
fn decode_session_reads_chat_sessions_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.vscdb");
    {
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection
            .execute(
                "CREATE TABLE ItemTable (key TEXT NOT NULL, value TEXT NOT NULL)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
                [
                    "github.copilot-chat.chatSessions",
                    r#"{"chatSessions":[{"id":"copilot-chat-1","createdAt":"2025-01-01T00:00:00Z","updatedAt":"2025-01-02T00:00:00Z","cwd":"/proj","title":"Chat One","messages":[{"role":"user","content":"hello"},{"role":"assistant","content":"answer","model":"m1"},{"role":"system","content":"bookkeeping"}]}]}"#,
                ],
            )
            .unwrap();
    }
    let sessions = aco_copilot::codec().enumerate(&sqlite_root(), &db).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].key.as_deref(),
        Some("github.copilot-chat.chatSessions#0")
    );
    let decoded = aco_copilot::codec().decode_session(&sessions[0]).unwrap();
    let conv = &decoded.conversation;
    assert_eq!(conv.session_id, "copilot-chat-1");
    assert_eq!(conv.title.as_deref(), Some("Chat One"));
    assert_eq!(conv.cwd.as_deref(), Some("/proj"));
    assert_eq!(conv.created_at.as_deref(), Some("2025-01-01T00:00:00Z"));
    assert_eq!(conv.updated_at.as_deref(), Some("2025-01-02T00:00:00Z"));
    let kinds: Vec<RecordKind> = conv.records.iter().map(Record::kind).collect();
    assert_eq!(kinds, [RecordKind::User, RecordKind::Assistant]);
    match &conv.records[1].body {
        Body::Assistant { text, model } => {
            assert_eq!(text, "answer");
            assert_eq!(model.as_deref(), Some("m1"));
        }
        other => panic!("expected assistant, got {other:?}"),
    }
    assert_eq!(decoded.record_lines, vec![1, 2]);
    assert_eq!(decoded.line_count, 4);
    assert_eq!(decoded.skipped.len(), 1);
    assert_eq!(
        decoded.skipped[0].native_type.as_deref(),
        Some("role:system")
    );
}

#[test]
fn index_rows_point_at_session_state_event_logs() {
    let dir = transcript_dir();
    // The CLI index beside the session-state tree.
    let db = dir.path().join("session-store.db");
    {
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection
            .execute(
                "CREATE TABLE sessions (id TEXT NOT NULL, cwd TEXT, summary TEXT, created_at TEXT, updated_at TEXT)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO sessions (id, updated_at) VALUES ('copilot-1', '2025-01-01T00:00:05Z')",
                [],
            )
            .unwrap();
        // A remote session with no local event log is not a session here.
        connection
            .execute("INSERT INTO sessions (id) VALUES ('remote-2')", [])
            .unwrap();
    }
    // Under a directory root the index adds nothing the event logs do not
    // already enumerate — it is an index, not content.
    let sessions = aco_copilot::codec()
        .enumerate(&directory_root(), dir.path())
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].key.is_none());
    // Addressed directly, the store enumerates one resumable row and its
    // key resolves to the session's event log.
    let sessions = aco_copilot::codec().enumerate(&sqlite_root(), &db).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "copilot-1");
    assert_eq!(sessions[0].key.as_deref(), Some("sessions:copilot-1"));
    let decoded = aco_copilot::codec().decode_session(&sessions[0]).unwrap();
    assert_eq!(decoded.conversation.session_id, "copilot-1");
    assert_eq!(decoded.conversation.records.len(), 7);
}

#[test]
fn unrecognized_sqlite_layout_reports_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("other.db");
    {
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection
            .execute("CREATE TABLE unrelated (id TEXT)", [])
            .unwrap();
    }
    assert!(aco_copilot::codec().enumerate(&sqlite_root(), &db).is_err());
}

#[test]
fn redact_line_rewrites_textual_payload() {
    let line: serde_json::Value = serde_json::from_str(
        r#"{"type":"assistant.message","data":{"messageId":"a1","content":"secret answer","reasoningText":"hidden thinking","toolRequests":[{"toolCallId":"c1","name":"bash","arguments":{"command":"rm -rf"}}]}}"#,
    )
    .unwrap();
    let out = aco_copilot::codec()
        .redact_line(&line, "[redacted]")
        .unwrap();
    assert_eq!(out["data"]["content"], "[redacted]");
    assert_eq!(out["data"]["reasoningText"], "[redacted]");
    assert_eq!(out["data"]["toolRequests"][0]["arguments"], "[redacted]");
    assert_eq!(out["data"]["toolRequests"][0]["name"], "bash");

    let tool: serde_json::Value = serde_json::from_str(
        r#"{"type":"tool.execution_complete","data":{"toolCallId":"c1","result":"private output","success":true}}"#,
    )
    .unwrap();
    let out = aco_copilot::codec()
        .redact_line(&tool, "[redacted]")
        .unwrap();
    assert_eq!(out["data"]["result"], "[redacted]");
    assert_eq!(out["data"]["success"], true);

    // A lifecycle row holds no conversation text.
    let start: serde_json::Value =
        serde_json::from_str(r#"{"type":"session.start","data":{"sessionId":"s1","cwd":"/w"}}"#)
            .unwrap();
    assert!(aco_copilot::codec()
        .redact_line(&start, "[redacted]")
        .is_none());

    // `transformedContent` persists as an array of typed parts — the whole
    // payload is rewritten, whatever its shape.
    let transformed: serde_json::Value = serde_json::from_str(
        r#"{"type":"user.message","data":{"content":"","transformedContent":[{"type":"text","text":"secret prompt"}]}}"#,
    )
    .unwrap();
    let out = aco_copilot::codec()
        .redact_line(&transformed, "[redacted]")
        .unwrap();
    assert_eq!(out["data"]["transformedContent"], "[redacted]");
}
