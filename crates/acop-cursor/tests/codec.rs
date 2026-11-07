use std::path::{Path, PathBuf};

use acop::codec::SessionRef;
use acop::model::{Body, Conversation, Record, RecordKind};
use acop::store::{HistoryRoot, StoreKind};

fn root(kind: &'static str) -> HistoryRoot {
    HistoryRoot {
        path: "{home}/fixture",
        kind,
        store: StoreKind::Directory,
    }
}

// ---------------------------------------------------------------------------
// Fixture builders (synthetic stores only)
// ---------------------------------------------------------------------------

/// `(bubble-id, type, text)` — `type` 1 is user, 0/2 agent.
type BubbleSpec<'a> = (&'a str, i64, &'a str);
/// `(composer-id, composer-json, bubbles)`.
type ComposerSpec<'a> = (&'a str, &'a str, &'a [BubbleSpec<'a>]);

/// `cursorDiskKV` store: `composerData:<id>` headers + `bubbleId:` rows.
fn vscdb_fixture(dir: &Path, composers: &[ComposerSpec<'_>]) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let db = dir.join("state.vscdb");
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch("CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB)")
        .unwrap();
    for (composer_id, composer_json, bubbles) in composers {
        connection
            .execute(
                "INSERT INTO cursorDiskKV VALUES (?1, ?2)",
                rusqlite::params![format!("composerData:{composer_id}"), composer_json],
            )
            .unwrap();
        for (bubble_id, bubble_type, text) in *bubbles {
            let bubble = serde_json::json!({
                "bubbleId": bubble_id,
                "type": bubble_type,
                "createdAt": 1_773_798_000_000i64,
                "text": text,
            });
            connection
                .execute(
                    "INSERT INTO cursorDiskKV VALUES (?1, ?2)",
                    rusqlite::params![
                        format!("bubbleId:{composer_id}:{bubble_id}"),
                        serde_json::to_vec(&bubble).unwrap()
                    ],
                )
                .unwrap();
        }
    }
    db
}

/// `blobs`+`meta` CLI chat store with a `meta.json` sibling.
fn cli_store_fixture(dir: &Path, session: &str, blobs: &[serde_json::Value]) -> PathBuf {
    let chat = dir.join("chats/deadbeef").join(session);
    std::fs::create_dir_all(&chat).unwrap();
    let db = chat.join("store.db");
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE blobs (id TEXT, data BLOB); CREATE TABLE meta (key TEXT, value TEXT)",
        )
        .unwrap();
    let meta_json = serde_json::json!({
        "agentId": session,
        "name": "Fixture chat",
        "createdAt": "1792521191005",
        "model": "composer-1.5",
    })
    .to_string();
    let hex: String = meta_json.bytes().map(|b| format!("{b:02x}")).collect();
    connection
        .execute("INSERT INTO meta VALUES ('0', ?1)", [hex])
        .unwrap();
    for (index, blob) in blobs.iter().enumerate() {
        connection
            .execute(
                "INSERT INTO blobs VALUES (?1, ?2)",
                rusqlite::params![format!("{index:03}"), serde_json::to_vec(blob).unwrap()],
            )
            .unwrap();
    }
    drop(connection);
    std::fs::write(
        chat.join("meta.json"),
        serde_json::json!({
            "schemaVersion": "1",
            "createdAtMs": 1_792_521_191_005i64,
            "updatedAtMs": 1_792_522_525_734i64,
            "hasConversation": true,
            "cwd": "/work/fixture-project",
        })
        .to_string(),
    )
    .unwrap();
    db
}

/// OpenAgent schema: `session`/`message`/`part` tables.
fn openagent_fixture(dir: &Path) -> PathBuf {
    let db = dir.join("openagent.db");
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, title TEXT, directory TEXT,
                time_created INTEGER, time_updated INTEGER, parent_id TEXT,
                tokens_input INTEGER, tokens_output INTEGER);
             CREATE TABLE message (id TEXT, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO session VALUES ('s1', 'root session', '/work/proj', 1_700_000_000_000,
                1_700_000_600_000, NULL, 10, 5)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO session VALUES ('s2', 'child task', '/work/proj', 1_700_000_001_000,
                1_700_000_601_000, 's1', 3, 2)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO session VALUES ('s3', 'empty draft', NULL, NULL, NULL, NULL, NULL, NULL)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO message VALUES ('m1', 's1', 1_700_000_000_000,
                '{\"role\":\"user\",\"time\":{\"created\":1700000000000}}')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO part VALUES ('m1', 's1', 1, '{\"type\":\"text\",\"text\":\"hello there\"}')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO message VALUES ('m2', 's1', 1_700_000_100_000,
                '{\"role\":\"assistant\",\"time\":{\"created\":1700000100000}}')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO part VALUES ('m2', 's1', 2,
                '{\"type\":\"tool\",\"tool\":\"bash\",\"callID\":\"call-1\",\"state\":{\"status\":\"completed\",\"input\":{\"cmd\":\"ls\"},\"output\":\"files\"}}')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO part VALUES ('m2', 's1', 3, '{\"type\":\"text\",\"text\":\"done\"}')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO message VALUES ('m9', 's2', 1_700_000_001_000,
                '{\"role\":\"user\",\"time\":{\"created\":1700000001000}}')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO part VALUES ('m9', 's2', 1, '{\"type\":\"text\",\"text\":\"child work\"}')",
            [],
        )
        .unwrap();
    db
}

fn transcript_fixture(dir: &Path, session: &str, lines: &[&str]) -> PathBuf {
    let transcript_dir = dir
        .join("projects/fixture-proj/agent-transcripts")
        .join(session);
    std::fs::create_dir_all(&transcript_dir).unwrap();
    let path = transcript_dir.join(format!("{session}.jsonl"));
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    std::fs::write(
        dir.join("projects/fixture-proj/.workspace-trusted"),
        r#"{"workspacePath":"/work/trusted-proj"}"#,
    )
    .unwrap();
    path
}

// ---------------------------------------------------------------------------
// Roots and file selection
// ---------------------------------------------------------------------------

#[test]
fn history_roots_cover_card_and_platform_storage_variants() {
    let roots = acop_cursor::codec().history();
    let kinds: Vec<(&str, &str)> = roots.iter().map(|r| (r.kind, r.path)).collect();
    assert_eq!(
        kinds
            .iter()
            .filter(|(kind, _)| *kind == "cursor-agents")
            .count(),
        1
    );
    // The card's macOS roots plus the codec-declared appdata/xdg variants.
    assert!(kinds.contains(&(
        "cursor-workspace-storage",
        "{home}/Library/Application Support/Cursor/User/workspaceStorage"
    )));
    assert!(kinds.contains(&(
        "cursor-workspace-storage",
        "{appdata}/Cursor/User/workspaceStorage"
    )));
    assert!(kinds.contains(&(
        "cursor-global-storage",
        "{xdg_config}/Cursor/User/globalStorage"
    )));
    assert!(kinds.contains(&("cursor-cli-chats", "{home}/.cursor/chats")));
    assert!(kinds.contains(&("cursor-cli-projects", "{home}/.cursor/projects")));
    // The card declares no persisted usage counters.
    assert!(acop_cursor::codec().transcript_usage().is_none());
}

#[test]
fn expand_root_resolves_appdata_and_xdg_tokens() {
    // Keep the assertions deterministic when the host exports these.
    std::env::remove_var("APPDATA");
    std::env::remove_var("XDG_CONFIG_HOME");
    let home = Path::new("/fixture-home");
    let roots = acop_cursor::codec().history();
    let xdg = roots
        .iter()
        .find(|r| r.path.starts_with("{xdg_config}/"))
        .unwrap();
    assert_eq!(
        acop_cursor::codec().expand_root(xdg, home),
        PathBuf::from("/fixture-home/.config/Cursor/User/workspaceStorage")
    );
    let appdata = roots
        .iter()
        .find(|r| r.path.starts_with("{appdata}/"))
        .unwrap();
    assert_eq!(
        acop_cursor::codec().expand_root(appdata, home),
        PathBuf::from("/fixture-home/AppData/Roaming/Cursor/User/workspaceStorage")
    );
}

#[test]
fn select_session_files_filters_extensions_and_meta() {
    let files: Vec<PathBuf> = [
        "a.jsonl",
        "b.ndjson",
        "c.json",
        "d.sqlite",
        "e.sqlite3",
        "f.db",
        "g.vscdb",
        "meta.json",
        "notes.md",
        "store.db-wal",
        "store.db-shm",
        "log.txt",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    let selected = acop_cursor::codec().select_session_files(files);
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
            "d.sqlite",
            "e.sqlite3",
            "f.db",
            "g.vscdb"
        ]
    );
}

// ---------------------------------------------------------------------------
// IDE composer store (cursorDiskKV container)
// ---------------------------------------------------------------------------

#[test]
fn enumerate_emits_one_session_ref_per_composer_row() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("workspaceStorage/aaaa");
    std::fs::create_dir_all(&ws).unwrap();
    let db = vscdb_fixture(
        &ws,
        &[
            (
                "composer-one",
                r#"{"composerId":"composer-one","name":"First","createdAt":1773798000000,
                    "lastUpdatedAt":1773798100000,
                    "modelConfig":{"modelName":"grok-4"},
                    "fullConversationHeadersOnly":[{"bubbleId":"b1"},{"bubbleId":"b2"}]}"#,
                &[("b1", 1, "hi"), ("b2", 2, "answer")],
            ),
            // A draft with no header list and no bubbles is not a session.
            ("composer-draft", r#"{"composerId":"composer-draft"}"#, &[]),
        ],
    );
    let refs = acop_cursor::codec()
        .enumerate(&root("cursor-workspace-storage"), dir.path())
        .unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(
        refs[0],
        SessionRef {
            id: "composer-one".to_string(),
            path: db,
            key: Some("composerData:composer-one".to_string()),
            parent: None,
        }
    );
}

#[test]
fn decode_session_reads_composer_bubbles() {
    let dir = tempfile::tempdir().unwrap();
    let db = vscdb_fixture(
        dir.path(),
        &[(
            "composer-1",
            r#"{"composerId":"composer-1","name":"Titled chat","createdAt":1773798000000,
                "lastUpdatedAt":1773798100000,"modelConfig":{"modelName":"default"},
                "workspaceIdentifier":{"uri":{"path":"/work/composer-proj"}},
                "fullConversationHeadersOnly":[{"bubbleId":"u1"},{"bubbleId":"a1"}]}"#,
            &[("u1", 1, "please scan"), ("a1", 2, "scanning now")],
        )],
    );
    // A usage-carrying bubble: tokenCount projects to a usage meta.
    {
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection
            .execute(
                "UPDATE cursorDiskKV SET value=?2 WHERE key=?1",
                rusqlite::params![
                    "bubbleId:composer-1:a1",
                    serde_json::to_vec(&serde_json::json!({
                        "bubbleId": "a1",
                        "type": 2,
                        "createdAt": 1_773_798_050_000i64,
                        "text": "scanning now",
                        "modelInfo": {"modelName": "grok-4.5"},
                        "tokenCount": {"inputTokens": 120, "outputTokens": 40},
                    }))
                    .unwrap()
                ],
            )
            .unwrap();
    }
    let session = SessionRef {
        id: "composer-1".to_string(),
        path: db,
        key: Some("composerData:composer-1".to_string()),
        parent: None,
    };
    let decoded = acop_cursor::codec().decode_session(&session).unwrap();
    let conv = &decoded.conversation;
    assert_eq!(conv.session_id, "composer-1");
    assert_eq!(conv.title.as_deref(), Some("Titled chat"));
    assert_eq!(conv.cwd.as_deref(), Some("/work/composer-proj"));
    assert_eq!(conv.created_at.as_deref(), Some("2026-03-18T01:40:00.000Z"));
    let kinds: Vec<RecordKind> = conv.records.iter().map(|r| r.kind()).collect();
    assert_eq!(
        kinds,
        [
            RecordKind::Meta,
            RecordKind::User,
            RecordKind::Assistant,
            RecordKind::Meta
        ]
    );
    match &conv.records[0].body {
        Body::Meta { subtype, .. } => assert_eq!(subtype, "session-agent"),
        other => panic!("expected session-facts meta, got {other:?}"),
    }
    match &conv.records[2].body {
        Body::Assistant { text, model } => {
            assert_eq!(text, "scanning now");
            assert_eq!(model.as_deref(), Some("grok-4.5"));
        }
        other => panic!("expected assistant, got {other:?}"),
    }
    match &conv.records[3].body {
        Body::Meta { subtype, data } => {
            assert_eq!(subtype, "usage");
            assert_eq!(data["usage"]["inputTokens"], 120);
        }
        other => panic!("expected usage meta, got {other:?}"),
    }
}

#[test]
fn delegated_composer_carries_lineage_meta() {
    let dir = tempfile::tempdir().unwrap();
    let db = vscdb_fixture(
        dir.path(),
        &[
            (
                "child-composer",
                r#"{"composerId":"child-composer","name":"Child",
                "subagentInfo":{"parentComposerId":"parent-composer","subagentTypeName":"explore"},
                "fullConversationHeadersOnly":[{"bubbleId":"c1"}]}"#,
                &[("c1", 1, "task work")],
            ),
            // The parent row must exist: an unresolvable parent pointer
            // leaves the session a plain root, never a delegated marker.
            (
                "parent-composer",
                r#"{"composerId":"parent-composer","name":"Parent"}"#,
                &[],
            ),
        ],
    );
    let session = SessionRef {
        id: "child-composer".to_string(),
        path: db,
        key: Some("composerData:child-composer".to_string()),
        parent: None,
    };
    let decoded = acop_cursor::codec().decode_session(&session).unwrap();
    match &decoded.conversation.records[0].body {
        Body::Meta { subtype, data } => {
            assert_eq!(subtype, "subagent-lineage");
            assert_eq!(data["parentSessionId"], "parent-composer");
            assert_eq!(data["subagentTypeName"], "explore");
        }
        other => panic!("expected lineage meta, got {other:?}"),
    }
}

#[test]
fn decode_multi_session_container_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = vscdb_fixture(
        dir.path(),
        &[
            (
                "one",
                r#"{"composerId":"one","fullConversationHeadersOnly":[{"bubbleId":"x"}]}"#,
                &[("x", 1, "a")],
            ),
            (
                "two",
                r#"{"composerId":"two","fullConversationHeadersOnly":[{"bubbleId":"y"}]}"#,
                &[("y", 1, "b")],
            ),
        ],
    );
    let err = acop_cursor::codec().decode(&db).unwrap_err();
    assert!(matches!(err, acop::Error::Usage(_)), "got {err:?}");

    // A container with exactly one stored session decodes as that session.
    let single = vscdb_fixture(
        &dir.path().join("single"),
        &[(
            "only",
            r#"{"composerId":"only","fullConversationHeadersOnly":[{"bubbleId":"z"}]}"#,
            &[("z", 1, "lone")],
        )],
    );
    let decoded = acop_cursor::codec().decode(&single).unwrap();
    assert_eq!(decoded.conversation.session_id, "only");
}

// ---------------------------------------------------------------------------
// CLI chat store (store.db blobs + meta.json)
// ---------------------------------------------------------------------------

#[test]
fn enumerate_finds_store_db_sessions_and_skips_meta_json() {
    let dir = tempfile::tempdir().unwrap();
    let db = cli_store_fixture(
        dir.path(),
        "aaaa-bbbb",
        &[serde_json::json!({"role": "user", "content": "hi"})],
    );
    let refs = acop_cursor::codec()
        .enumerate(&root("cursor-cli-chats"), dir.path())
        .unwrap();
    assert_eq!(
        refs,
        vec![SessionRef {
            id: "aaaa-bbbb".to_string(),
            path: db,
            key: None,
            parent: None,
        }]
    );
}

#[test]
fn decode_cli_store_maps_blob_roles_and_meta() {
    let dir = tempfile::tempdir().unwrap();
    let db = cli_store_fixture(
        dir.path(),
        "session-uuid",
        &[
            serde_json::json!({"role": "system", "content": "you are an agent"}),
            serde_json::json!({"role": "user", "content": "list files"}),
            serde_json::json!({
                "role": "assistant",
                "id": "msg_1",
                "content": [
                    {"type": "reasoning", "text": "thinking"},
                    {"type": "tool-call", "toolCallId": "c1", "toolName": "ls", "args": {"dir": "/w"}},
                    {"type": "text", "text": "listing"}
                ],
            }),
            serde_json::json!({
                "role": "tool",
                "content": [{"type": "tool-result", "toolCallId": "c1", "toolName": "ls", "result": "a.rs"}],
            }),
            serde_json::json!({"sessionId": "session-uuid", "configOptions": {"mode": "agent"}}),
        ],
    );
    let decoded = acop_cursor::codec().decode(&db).unwrap();
    let conv = &decoded.conversation;
    assert_eq!(conv.session_id, "session-uuid");
    assert_eq!(conv.title.as_deref(), Some("Fixture chat"));
    assert_eq!(conv.cwd.as_deref(), Some("/work/fixture-project"));
    assert_eq!(conv.created_at.as_deref(), Some("2026-10-20T18:33:11.005Z"));
    let kinds: Vec<RecordKind> = conv.records.iter().map(|r| r.kind()).collect();
    assert_eq!(
        kinds,
        [
            RecordKind::Meta, // store-model
            RecordKind::System,
            RecordKind::User,
            RecordKind::Reasoning,
            RecordKind::ToolCall,
            RecordKind::Assistant,
            RecordKind::ToolResult,
            RecordKind::Meta, // session-config
        ]
    );
    match &conv.records[4].body {
        Body::ToolCall {
            call_id,
            name,
            args,
        } => {
            assert_eq!(call_id.as_deref(), Some("c1"));
            assert_eq!(name, "ls");
            assert_eq!(args["dir"], "/w");
        }
        other => panic!("expected tool call, got {other:?}"),
    }
    match &conv.records[6].body {
        Body::ToolResult {
            call_id, content, ..
        } => {
            assert_eq!(call_id.as_deref(), Some("c1"));
            assert_eq!(content, &serde_json::json!("a.rs"));
        }
        other => panic!("expected tool result, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// OpenAgent container
// ---------------------------------------------------------------------------

#[test]
fn openagent_store_enumerates_rows_and_decodes_them() {
    let dir = tempfile::tempdir().unwrap();
    let db = openagent_fixture(dir.path());
    let refs = acop_cursor::codec()
        .enumerate(&root("cursor-agents"), &db)
        .unwrap();
    // s3 has no messages — not an enumerated session.
    let mut ids: Vec<&str> = refs.iter().map(|r| r.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["s1", "s2"]);
    let s1 = refs.iter().find(|r| r.id == "s1").unwrap();
    assert_eq!(s1.key.as_deref(), Some("s1"));

    let decoded = acop_cursor::codec().decode_session(s1).unwrap();
    let conv = &decoded.conversation;
    assert_eq!(conv.session_id, "s1");
    assert_eq!(conv.title.as_deref(), Some("root session"));
    assert_eq!(conv.cwd.as_deref(), Some("/work/proj"));
    let kinds: Vec<RecordKind> = conv.records.iter().map(|r| r.kind()).collect();
    assert_eq!(
        kinds,
        [
            RecordKind::Meta, // session usage from token columns
            RecordKind::User,
            RecordKind::ToolCall,
            RecordKind::ToolResult,
            RecordKind::Assistant,
        ]
    );
    match &conv.records[0].body {
        Body::Meta { subtype, data } => {
            assert_eq!(subtype, "usage");
            assert_eq!(data["promptTokens"], 10);
        }
        other => panic!("expected usage meta, got {other:?}"),
    }
    match &conv.records[2].body {
        Body::ToolCall {
            call_id,
            name,
            args,
        } => {
            assert_eq!(call_id.as_deref(), Some("call-1"));
            assert_eq!(name, "bash");
            assert_eq!(args["cmd"], "ls");
        }
        other => panic!("expected tool call, got {other:?}"),
    }
}

#[test]
fn openagent_child_session_carries_lineage() {
    let dir = tempfile::tempdir().unwrap();
    let db = openagent_fixture(dir.path());
    let session = SessionRef {
        id: "s2".to_string(),
        path: db,
        key: Some("s2".to_string()),
        parent: None,
    };
    let decoded = acop_cursor::codec().decode_session(&session).unwrap();
    match &decoded.conversation.records[0].body {
        Body::Meta { subtype, data } => {
            assert_eq!(subtype, "subagent-lineage");
            assert_eq!(data["parentSessionId"], "s1");
        }
        other => panic!("expected lineage meta, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Transcript files
// ---------------------------------------------------------------------------

#[test]
fn decode_transcript_projects_roles_blocks_and_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let path = transcript_fixture(
        dir.path(),
        "11111111-2222-3333-4444-555555555555",
        &[
            r#"{"role":"user","message":{"content":[{"type":"text","text":"hi agent"}]}}"#,
            r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"read","input":{"path":"/a"}},{"type":"text","text":"reading it"}]}}"#,
            r#"{"type":"status","status":"completed"}"#,
            r#"{"type":"turn_ended","status":"error","error":"usage limit"}"#,
        ],
    );
    let decoded = acop_cursor::codec().decode(&path).unwrap();
    let conv = &decoded.conversation;
    assert_eq!(conv.session_id, "11111111-2222-3333-4444-555555555555");
    assert_eq!(conv.cwd.as_deref(), Some("/work/trusted-proj"));
    let kinds: Vec<RecordKind> = conv.records.iter().map(|r| r.kind()).collect();
    assert_eq!(
        kinds,
        [
            RecordKind::User,
            RecordKind::ToolCall,
            RecordKind::Assistant,
            RecordKind::System,
            RecordKind::System,
        ]
    );
    match &conv.records[1].body {
        Body::ToolCall { name, args, .. } => {
            assert_eq!(name, "read");
            assert_eq!(args["path"], "/a");
        }
        other => panic!("expected tool call, got {other:?}"),
    }
    match &conv.records[3].body {
        Body::System { subtype, .. } => assert_eq!(subtype, "status"),
        other => panic!("expected system, got {other:?}"),
    }
}

#[test]
fn delegated_transcript_uses_task_identity_and_parent() {
    let dir = tempfile::tempdir().unwrap();
    let conversation_dir = dir.path().join("projects/p/agent-transcripts/conv-uuid");
    let task_dir = conversation_dir.join("subagents");
    std::fs::create_dir_all(&task_dir).unwrap();
    let task = task_dir.join("task-uuid.jsonl");
    std::fs::write(
        &task,
        r#"{"role":"user","message":{"content":[{"type":"text","text":"task prompt"}]}}"#,
    )
    .unwrap();
    let decoded = acop_cursor::codec().decode(&task).unwrap();
    assert_eq!(decoded.conversation.session_id, "task-uuid");
    match &decoded.conversation.records[0].body {
        Body::Meta { subtype, data } => {
            assert_eq!(subtype, "subagent-lineage");
            assert_eq!(data["parentSessionId"], "conv-uuid");
        }
        other => panic!("expected lineage meta, got {other:?}"),
    }
    let refs = acop_cursor::codec()
        .enumerate(&root("cursor-cli-projects"), &dir.path().join("projects"))
        .unwrap();
    let ids: Vec<&str> = refs.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["task-uuid"]);
}

#[test]
fn decode_json_document_collects_message_arrays() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    std::fs::write(
        &path,
        r#"{"sessionId":"doc-1","messages":[
            {"role":"user","message":{"content":[{"type":"text","text":"q"}]}},
            {"role":"assistant","message":{"content":[{"type":"text","text":"a"}]}}
        ]}"#,
    )
    .unwrap();
    let decoded = acop_cursor::codec().decode(&path).unwrap();
    assert_eq!(decoded.conversation.session_id, "doc-1");
    let kinds: Vec<RecordKind> = decoded
        .conversation
        .records
        .iter()
        .map(|r| r.kind())
        .collect();
    assert_eq!(kinds, [RecordKind::User, RecordKind::Assistant]);
}

// ---------------------------------------------------------------------------
// Encode / redact / store_path
// ---------------------------------------------------------------------------

#[test]
fn encode_emits_transcript_lines_with_honest_projection() {
    let mut conv = Conversation::new("s");
    for (i, body) in [
        Body::User {
            text: "go".to_string(),
            model: None,
            attachments: Vec::new(),
        },
        Body::Reasoning {
            text: "hmm".to_string(),
        },
        Body::ToolCall {
            call_id: Some("c1".to_string()),
            name: "read".to_string(),
            args: serde_json::json!({"path": "/a"}),
        },
        Body::Assistant {
            text: "done".to_string(),
            model: None,
        },
        Body::ToolResult {
            call_id: Some("c1".to_string()),
            name: Some("read".to_string()),
            content: serde_json::json!("file body"),
            is_error: false,
        },
        Body::System {
            subtype: "status".to_string(),
            text: "done".to_string(),
        },
        Body::Meta {
            subtype: "usage".to_string(),
            data: serde_json::json!({}),
        },
    ]
    .into_iter()
    .enumerate()
    {
        conv.records.push(Record {
            id: None,
            parent: None,
            ts: None,
            ordinal: i as u64,
            body,
        });
    }
    let encoded = acop_cursor::codec().encode(&conv).unwrap();
    // system + meta cannot ride in a transcript.
    assert_eq!(
        encoded.projection.dropped,
        [(5, RecordKind::System), (6, RecordKind::Meta)]
    );
    assert!(!encoded.projection.adapted.is_empty());
    let lines: Vec<serde_json::Value> = encoded
        .lines
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["role"], "user");
    assert_eq!(lines[1]["role"], "assistant");
    let blocks = lines[1]["message"]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "reasoning");
    assert_eq!(blocks[1]["type"], "tool_use");
    assert_eq!(blocks[2]["type"], "text");
    assert_eq!(lines[2]["role"], "tool");
    assert_eq!(lines[2]["message"]["content"][0]["type"], "tool-result");
}

#[test]
fn redact_line_rewrites_transcript_text_and_payloads() {
    let line: serde_json::Value = serde_json::from_str(
        r#"{"role":"assistant","message":{"content":[{"type":"text","text":"secret"},{"type":"tool_use","name":"x","input":{"a":1}}]}}"#,
    )
    .unwrap();
    let out = acop_cursor::codec()
        .redact_line(&line, "[redacted]")
        .unwrap();
    assert_eq!(out["message"]["content"][0]["text"], "[redacted]");
    assert_eq!(out["message"]["content"][1]["input"], "[redacted]");

    let lifecycle: serde_json::Value =
        serde_json::from_str(r#"{"type":"turn_ended","status":"error","error":"oops"}"#).unwrap();
    let out = acop_cursor::codec()
        .redact_line(&lifecycle, "[redacted]")
        .unwrap();
    assert_eq!(out["error"], "[redacted]");

    // Container rows are not file lines — redaction cannot touch them.
    let row_selector = ["composerData", "x"].join(":");
    let row = serde_json::json!({ "key": row_selector, "value": "{}" });
    assert!(acop_cursor::codec()
        .redact_line(&row, "[redacted]")
        .is_none());
}

#[test]
fn store_path_lands_in_agents_transcript_layout() {
    let dir = tempfile::tempdir().unwrap();
    let conv = Conversation::new("new-session-id");
    let path = acop_cursor::codec().store_path(dir.path(), &conv).unwrap();
    assert_eq!(
        path,
        dir.path()
            .join(".cursor/agents/new-session-id/new-session-id.jsonl")
    );
}

#[test]
fn decode_rejects_unrecognized_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("random.db");
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch("CREATE TABLE something (a TEXT)")
        .unwrap();
    drop(connection);
    let err = acop_cursor::codec().decode(&db).unwrap_err();
    assert!(matches!(err, acop::Error::Usage(_)), "got {err:?}");
}
