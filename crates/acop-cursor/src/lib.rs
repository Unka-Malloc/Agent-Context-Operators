//! acop codec: the `cursor` conversation store.
//!
//! Cursor persists conversations in four shapes:
//!
//! - **IDE composer store** — `state.vscdb` under
//!   `User/{workspaceStorage,globalStorage}`: one `cursorDiskKV` table holds
//!   every conversation of the workspace. `composerData:<id>` rows carry the
//!   session header (title, times, model config, workspace URI, delegated
//!   `subagentInfo`), `bubbleId:<id>:<bubble>` rows the messages. One file
//!   holds many sessions, so `enumerate` emits one [`SessionRef`] per
//!   composer row with `key = Some("composerData:<id>")`.
//! - **CLI chat store** — `~/.cursor/chats/<hash>/<session>/store.db`: one
//!   `blobs` table of AI-SDK-shaped message objects (`role`, `content`
//!   blocks: `text`, `reasoning`, `tool-call`, `tool-result`) plus a `meta`
//!   table of hex-encoded JSON session facts; `meta.json` beside the store
//!   records the real `createdAtMs`/`updatedAtMs`/`cwd`. One file is one
//!   session (`key: None`).
//! - **CLI project transcripts** —
//!   `~/.cursor/projects/<project>/agent-transcripts/<session>/<session>.jsonl`,
//!   with delegated task transcripts under `<session>/subagents/<task>.jsonl`:
//!   `{"role","message":{"content":[...]}}` lines carrying `text` and
//!   `tool_use` blocks. Records hold no timestamps and no session field —
//!   identity and lineage come from the path layout.
//! - **OpenAgent sqlite** — `session`/`message`/`part` tables (the
//!   OpenCode-shaped schema Cursor's bundled agent stores use): one file
//!   holds many sessions, enumerated as `key = Some(<session-id>)`.
//!
//! `cursor-agents` (`~/.cursor/agents`) is declared by the card but today
//! holds only `.md` subagent definitions — enumerated generically so any
//! transcript/database landing there is still read.
//!
//! Encoding targets the JSONL transcript layout — the only format Cursor
//! itself writes as plain files. Container stores are not writable through
//! this codec, and `redact_line` is file-level only: for container rows it
//! returns `None` (redacting a `cursorDiskKV`/`blobs` row would mean a SQL
//! rewrite, which the line-oriented edit operator does not perform).

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use acop::codec::{Capability, CapabilityMatrix, Codec, Decoded, Encoded, SessionRef, Skipped};
use acop::model::{Attachment, Body, Conversation, Record, RecordKind};
use acop::store::{HistoryRoot, StoreKind};
use acop::{Error, Result};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

/// The codec instance acop's registry hands out.
pub struct CursorCodec;

/// The one codec instance for the registry.
pub fn codec() -> &'static dyn Codec {
    &CursorCodec
}

const SESSION_KINDS: &[&str] = &[
    "cursor-agents",
    "cursor-cli-chats",
    "cursor-cli-projects",
    "cursor-workspace-storage",
    "cursor-global-storage",
];

static CAPABILITIES: CapabilityMatrix = CapabilityMatrix::new(
    true,
    [
        Capability::Adapted, // user — text blocks carry; attachments have no native block
        Capability::Native,  // assistant
        Capability::Adapted, // reasoning — `reasoning` content block (store.db shape)
        Capability::Adapted, // tool-call — `tool_use` block carries name+input, not call_id
        Capability::Adapted, // tool-result — `tool`/`tool-result` row shape (store.db)
        Capability::Unsupported, // system — lifecycle rows are a fixed vocabulary
        Capability::Unsupported, // meta — transcripts persist no usage/meta records
    ],
);

/// File extensions Cursor stores conversation artifacts under.
const SESSION_EXTENSIONS: &[&str] = &[
    "jsonl", "ndjson", "json", "sqlite", "sqlite3", "db", "vscdb",
];
const SQLITE_EXTENSIONS: &[&str] = &["sqlite", "sqlite3", "db", "vscdb"];
/// Directory inside a `cursor-cli-projects` project that holds transcripts.
const TRANSCRIPTS_DIRECTORY: &str = "agent-transcripts";
/// Directory inside a conversation directory holding delegated tasks.
const DELEGATED_DIRECTORY: &str = "subagents";
/// The IDE store file name — the only conversation artifact under the
/// `User/*Storage` trees.
const IDE_STORE_FILE: &str = "state.vscdb";
/// `globalStorage/state.vscdb` sits one level down, `workspaceStorage/<id>/`
/// two: deeper walks would descend into bundled agent-CLI installs.
const IDE_STORE_WALK_DEPTH: usize = 2;

impl Codec for CursorCodec {
    fn id(&self) -> &'static str {
        "cursor"
    }

    fn history(&self) -> Vec<HistoryRoot> {
        let mut roots = a4aa_cursor::card().history.to_vec();
        // Cursor also keeps the same IDE stores under the Windows
        // (`{appdata}`) and Linux (`{xdg_config}`) homes; the card declares
        // only the macOS `{home}/Library/Application Support` pair. These
        // are genuinely Cursor's own conversation stores, so the codec
        // declares them itself.
        roots.extend([
            HistoryRoot {
                path: "{appdata}/Cursor/User/workspaceStorage",
                kind: "cursor-workspace-storage",
                store: StoreKind::Directory,
            },
            HistoryRoot {
                path: "{appdata}/Cursor/User/globalStorage",
                kind: "cursor-global-storage",
                store: StoreKind::Directory,
            },
            HistoryRoot {
                path: "{xdg_config}/Cursor/User/workspaceStorage",
                kind: "cursor-workspace-storage",
                store: StoreKind::Directory,
            },
            HistoryRoot {
                path: "{xdg_config}/Cursor/User/globalStorage",
                kind: "cursor-global-storage",
                store: StoreKind::Directory,
            },
        ]);
        roots
    }

    fn transcript_usage(&self) -> Option<acop::TranscriptUsage> {
        a4aa_cursor::card().usage.transcript
    }

    fn session_kinds(&self) -> &'static [&'static str] {
        SESSION_KINDS
    }

    fn capabilities(&self) -> &'static CapabilityMatrix {
        &CAPABILITIES
    }

    /// `{appdata}` and `{xdg_config}` resolve through the host environment
    /// the way the IDE's own tooling resolves them; everything else is
    /// `{home}`-relative.
    fn expand_root(&self, root: &HistoryRoot, home: &Path) -> PathBuf {
        if let Some(rest) = root.path.strip_prefix("{appdata}/") {
            let base = std::env::var("APPDATA")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("AppData/Roaming"));
            return base.join(rest);
        }
        if let Some(rest) = root.path.strip_prefix("{xdg_config}/") {
            let base = std::env::var("XDG_CONFIG_HOME")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"));
            return base.join(rest);
        }
        acop::util::expand_home(root.path, home)
    }

    /// `store.db` keeps its session id in the parent directory; every other
    /// layout names the file after the conversation.
    fn session_id(&self, path: &Path) -> String {
        if path.file_name().and_then(|n| n.to_str()) == Some("store.db") {
            if let Some(id) = path
                .parent()
                .and_then(|dir| dir.file_name())
                .and_then(|name| name.to_str())
                .filter(|name| !name.trim().is_empty())
            {
                return id.to_string();
            }
        }
        path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Cursor session artifacts: JSONL/NDJSON/JSON transcripts and
    /// SQLite-family containers. `meta.json` is session *metadata* beside
    /// `store.db`, never a session document itself.
    fn select_session_files(&self, files: Vec<PathBuf>) -> Vec<PathBuf> {
        files
            .into_iter()
            .filter(|p| {
                let ext = p
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase())
                    .unwrap_or_default();
                SESSION_EXTENSIONS.contains(&ext.as_str())
                    && p.file_name().and_then(|n| n.to_str()) != Some("meta.json")
            })
            .collect()
    }

    /// Container stores (`state.vscdb` composers, openagent databases) emit
    /// one [`SessionRef`] per stored row; plain files emit `key: None`.
    fn enumerate(&self, root: &HistoryRoot, path: &Path) -> Result<Vec<SessionRef>> {
        if root.store != StoreKind::Directory {
            return Err(Error::Usage(format!(
                "cursor store kind {:?} is not a directory enumeration",
                root.store
            )));
        }
        let files = self.candidate_files(root, path)?;
        let mut sessions = Vec::new();
        for file in files {
            if is_sqlite_file(&file) {
                sessions.extend(container_session_refs(&file));
            } else {
                sessions.push(SessionRef {
                    id: self.session_id(&file),
                    path: file,
                    key: None,
                    parent: None,
                });
            }
        }
        Ok(sessions)
    }

    /// Container contract: a discovered sqlite store enumerates its
    /// session rows (`cursorDiskKV` composers, OpenAgent `session` rows,
    /// a `blobs` chat, or probed `table/key` groups); every other file
    /// stays one session per file.
    fn sessions_in_file(&self, path: &Path) -> Result<Vec<SessionRef>> {
        if is_sqlite_file(path) {
            return Ok(container_session_refs(path));
        }
        let ext = file_extension(path);
        if ext == "json" {
            let refs = acop::generic::json_document_session_refs(path);
            if !refs.is_empty() {
                return Ok(refs);
            }
        }
        if matches!(ext.as_str(), "jsonl" | "ndjson") {
            let refs = acop::generic::jsonl_session_refs(path);
            if !refs.is_empty() {
                return Ok(refs);
            }
        }
        Ok(vec![SessionRef {
            id: self.session_id(path),
            path: path.to_path_buf(),
            key: None,
            parent: None,
        }])
    }

    /// `key = Some` reads one row out of a container store;
    /// `key = None` decodes the file itself.
    fn decode_session(&self, session: &SessionRef) -> Result<Decoded> {
        let Some(key) = session.key.as_deref() else {
            return self.decode(&session.path);
        };
        match file_extension(&session.path).as_str() {
            "json" => {
                return acop::generic::decode_json_document_session(&session.path, key, "cursor")
            }
            "jsonl" | "ndjson" => {
                return acop::generic::decode_jsonl_session(&session.path, key, "cursor")
            }
            _ => {}
        }
        let connection = open_read_only(&session.path).ok_or_else(|| Error::Decode {
            path: session.path.clone(),
            line: 0,
            message: "sqlite store does not open read-only".into(),
        })?;
        if let Some(composer_id) = key.strip_prefix("composerData:") {
            let known = composer_rows(&connection)
                .into_iter()
                .map(|composer| composer.id)
                .collect();
            return decode_composer_session(
                &connection,
                &session.path,
                composer_id,
                Some(key),
                &known,
            );
        }
        if let Some(decoded) =
            acop::generic::openagent::decode(&connection, &session.path, key, "cursor")?
        {
            return Ok(decoded);
        }
        acop::generic::decode_sqlite(&session.path, Some(key), "cursor")
    }

    /// Single-file decode: JSONL/NDJSON transcripts, whole-document JSON,
    /// and single-session SQLite stores. A recognized multi-session
    /// container is refused by name — `enumerate` addresses its rows.
    fn decode(&self, path: &Path) -> Result<Decoded> {
        if is_sqlite_file(path) {
            return decode_sqlite_file(path);
        }
        let ext = file_extension(path);
        match ext.as_str() {
            // `.json` documents decode through the shared lane —
            // `sessions[]` containers and single-session documents
            // alike are generic semantics, not cursor format knowledge.
            "json" => acop::generic::decode_json_document(path, "cursor"),
            _ => decode_transcript(path),
        }
    }

    /// Emit the JSONL transcript shape — `{"role",…,"message":{"content"}}`
    /// lines. Assistant-side runs (text, reasoning, tool calls) share one
    /// message line the way Cursor writes them.
    fn encode(&self, conversation: &Conversation) -> Result<Encoded> {
        let (kept, projection) = self.project(conversation);
        let mut lines = Vec::new();
        let mut pending: Vec<&Record> = Vec::new();
        for rec in kept {
            match rec.kind() {
                RecordKind::Assistant | RecordKind::Reasoning | RecordKind::ToolCall => {
                    pending.push(rec)
                }
                RecordKind::User => {
                    flush_assistant(&mut pending, &mut lines);
                    let mut content = vec![json!({"type": "text", "text": rec.body.text()})];
                    if let Body::User { attachments, .. } = &rec.body {
                        for attachment in attachments {
                            content.push(json!({
                                "type": "attachment",
                                "kind": attachment.kind,
                                "data": attachment.data,
                            }));
                        }
                    }
                    lines
                        .push(json!({"role": "user", "message": {"content": content}}).to_string());
                }
                RecordKind::ToolResult => {
                    flush_assistant(&mut pending, &mut lines);
                    if let Body::ToolResult {
                        call_id,
                        name,
                        content,
                        is_error,
                    } = &rec.body
                    {
                        lines.push(
                            json!({
                                "role": "tool",
                                "message": {"content": [{
                                    "type": "tool-result",
                                    "toolCallId": call_id,
                                    "toolName": name,
                                    "result": content,
                                    "isError": is_error,
                                }]},
                            })
                            .to_string(),
                        );
                    }
                }
                RecordKind::System | RecordKind::Meta => {}
            }
        }
        flush_assistant(&mut pending, &mut lines);
        Ok(Encoded { lines, projection })
    }

    /// Imports land under the declared `cursor-agents` root in the CLI
    /// transcript layout: one directory per conversation holding
    /// `<session>/<session>.jsonl`.
    fn store_path(&self, home: &Path, conversation: &Conversation) -> Result<PathBuf> {
        let root = self
            .history()
            .into_iter()
            .find(|r| r.kind == "cursor-agents")
            .map(|r| self.expand_root(&r, home))
            .ok_or_else(|| Error::Usage("cursor-agents root undeclared".to_string()))?;
        Ok(root
            .join(&conversation.session_id)
            .join(format!("{}.jsonl", conversation.session_id)))
    }

    /// Rewrite the textual content of one transcript line. Container rows
    /// (`cursorDiskKV`, `blobs`, openagent tables) are not file lines —
    /// `None` hands them back to the edit operator untouched.
    fn redact_line(&self, line: &Value, marker: &str) -> Option<Value> {
        let mut out = line.clone();
        let mut touched = false;
        if let Some(content) = out
            .get_mut("message")
            .and_then(|message| message.get_mut("content"))
        {
            touched |= redact_content(content, marker);
        }
        for key in ["text", "status", "error"] {
            if out.get(key).and_then(Value::as_str).is_some() {
                out[key] = json!(marker);
                touched = true;
            }
        }
        touched.then_some(out)
    }
}

impl CursorCodec {
    /// The files one root contributes to enumeration.
    fn candidate_files(&self, root: &HistoryRoot, path: &Path) -> Result<Vec<PathBuf>> {
        if path.is_file() {
            return Ok(vec![path.to_path_buf()]);
        }
        match root.kind {
            // Only `state.vscdb` inside the IDE storage trees is a
            // conversation store; the walk stays bounded.
            "cursor-workspace-storage" | "cursor-global-storage" => Ok(collect_named_files(
                path,
                IDE_STORE_FILE,
                IDE_STORE_WALK_DEPTH,
            )),
            // Only `agent-transcripts/` inside each project holds
            // conversations — `mcps/`, `terminals/`, `assets/` are tool
            // bookkeeping under the same root.
            "cursor-cli-projects" => {
                let mut files = Vec::new();
                if let Ok(entries) = std::fs::read_dir(path) {
                    for entry in entries.flatten() {
                        if entry.file_type().is_ok_and(|t| t.is_dir()) {
                            let transcripts = entry.path().join(TRANSCRIPTS_DIRECTORY);
                            files.extend(acop::store::collect_files(&transcripts)?);
                        }
                    }
                }
                Ok(self.select_session_files(files))
            }
            _ => Ok(self.select_session_files(acop::store::collect_files(path)?)),
        }
    }
}

// ---------------------------------------------------------------------------
// Store enumeration
// ---------------------------------------------------------------------------

fn file_extension(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default()
}

fn is_sqlite_file(path: &Path) -> bool {
    SQLITE_EXTENSIONS.contains(&file_extension(path).as_str())
}

/// Bounded recursive walk for files of one exact name.
fn collect_named_files(dir: &Path, name: &str, max_depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((d, depth)) = stack.pop() {
        if depth > max_depth {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push((p, depth + 1));
            } else if p.file_name().and_then(|n| n.to_str()) == Some(name) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// A SQLite file under a session root: recognized container schemas emit
/// one [`SessionRef`] per stored session; the single-session CLI store and
/// unrecognizable schemas keep file-level addressing.
fn container_session_refs(path: &Path) -> Vec<SessionRef> {
    let Some(connection) = open_read_only(path) else {
        return Vec::new();
    };
    if sqlite_table_exists(&connection, "cursorDiskKV") {
        return composer_session_refs(&connection, path);
    }
    if let Some(refs) = acop::generic::openagent::session_refs(&connection, path) {
        return refs;
    }
    if sqlite_table_exists(&connection, "blobs") {
        // One `store.db` is one chat: file-level addressing.
        return vec![SessionRef {
            id: cli_store_session_id(path),
            path: path.to_path_buf(),
            key: None,
            parent: None,
        }];
    }
    // Unknown sqlite layouts probe the generic row-grouping rules —
    // `ItemTable` key/value stores and friends still enumerate.
    acop::generic::sqlite_session_refs(path).unwrap_or_default()
}

/// The session id a `store.db` carries: its parent directory name.
fn cli_store_session_id(path: &Path) -> String {
    path.parent()
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "cursor-cli-session".to_string())
}

// ---------------------------------------------------------------------------
// SQLite helpers
// ---------------------------------------------------------------------------

fn open_read_only(path: &Path) -> Option<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

fn sqlite_table_exists(connection: &Connection, table: &str) -> bool {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1 LIMIT 1",
            [table],
            |_| Ok(()),
        )
        .is_ok()
}

fn sqlite_value_text(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Null => None,
        ValueRef::Integer(value) => Some(value.to_string()),
        ValueRef::Real(value) => Some(value.to_string()),
        ValueRef::Text(value) | ValueRef::Blob(value) => {
            Some(String::from_utf8_lossy(value).into_owned())
        }
    }
}

/// Epoch number → RFC 3339 by magnitude: µs ≥ 1e15, ms ≥ 1e10,
/// else seconds. Non-positive values carry no time.
fn epoch_to_rfc3339(value: i64) -> Option<String> {
    if value <= 0 {
        return None;
    }
    let absolute = (value as i128).abs();
    let millis = if absolute >= 1_000_000_000_000_000 {
        (value as i128 / 1_000) as i64
    } else if absolute >= 10_000_000_000 {
        value
    } else {
        value.saturating_mul(1000)
    };
    u64::try_from(millis).ok().map(acop::util::epoch_ms_rfc3339)
}

fn epoch_value_to_rfc3339(value: &Value) -> Option<String> {
    value
        .as_i64()
        .or_else(|| {
            value
                .as_str()
                .and_then(|text| text.trim().parse::<i64>().ok())
        })
        .and_then(epoch_to_rfc3339)
}

// ---------------------------------------------------------------------------
// IDE composer store (`state.vscdb` → `cursorDiskKV`)
// ---------------------------------------------------------------------------

/// Enumeration-level composer row: only identity and whether it carries
/// bubbles. Rich metadata is re-read from the row JSON at decode time.
struct ComposerMeta {
    id: String,
    key: String,
    bubble_ids: Vec<String>,
    parent: Option<String>,
}

/// One [`SessionRef`] per composer that carries at least one message
/// bubble. Composers without bubbles are drafts Cursor never populated.
fn composer_session_refs(connection: &Connection, path: &Path) -> Vec<SessionRef> {
    composer_rows(connection)
        .into_iter()
        .filter(|composer| {
            !composer.bubble_ids.is_empty()
                || !composer_bubble_ids(connection, &composer.id).is_empty()
        })
        .map(|composer| SessionRef {
            id: composer.id.clone(),
            path: path.to_path_buf(),
            key: Some(composer.key),
            parent: composer.parent,
        })
        .collect()
}

/// `composerData:*` rows → session headers. The key-range predicate is
/// Cursor's own prefix scheme (`:` < `;` orders the range exactly).
fn composer_rows(connection: &Connection) -> Vec<ComposerMeta> {
    let Ok(mut statement) = connection.prepare(
        "SELECT key, value FROM cursorDiskKV
         WHERE key >= 'composerData:' AND key < 'composerData;'",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok((
            sqlite_value_text(row.get_ref(0)?),
            sqlite_value_text(row.get_ref(1)?),
        ))
    }) else {
        return Vec::new();
    };
    let mut composers = Vec::new();
    for (key, value) in rows.flatten() {
        let (Some(key), Some(value)) = (key, value) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<Value>(&value) else {
            continue;
        };
        let id = json
            .get("composerId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .or_else(|| {
                key.strip_prefix("composerData:")
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
            });
        let Some(id) = id else { continue };
        let bubble_ids = json
            .get("fullConversationHeadersOnly")
            .and_then(Value::as_array)
            .map(|headers| {
                headers
                    .iter()
                    .filter_map(|header| {
                        header
                            .get("bubbleId")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|v| !v.is_empty())
                            .map(str::to_string)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let parent = json
            .pointer("/subagentInfo/parentComposerId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string);
        composers.push(ComposerMeta {
            id,
            key,
            bubble_ids,
            parent,
        });
    }
    composers
}

fn composer_model_from_config(json: &Value) -> String {
    if let Some(selected) = json
        .pointer("/modelConfig/selectedModels")
        .and_then(Value::as_array)
    {
        for item in selected {
            let Some(model_id) = item
                .get("modelId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
            else {
                continue;
            };
            if !model_id.eq_ignore_ascii_case("default") {
                return model_id.to_string();
            }
        }
    }
    json.pointer("/modelConfig/modelName")
        .and_then(Value::as_str)
        .or_else(|| json.get("modelName").and_then(Value::as_str))
        .or_else(|| json.get("model").and_then(Value::as_str))
        .map(normalize_model_name)
        .unwrap_or_else(|| "cursor-auto".to_string())
}

fn normalize_model_name(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("default") {
        "cursor-auto".to_string()
    } else {
        trimmed.to_string()
    }
}

/// The project directory a composer records as `workspaceIdentifier.uri` —
/// `path`/`fsPath`/`external` (`file://`) forms, bounded so a residual
/// root-level record can never become a project directory.
fn composer_workspace_path(value: &Value) -> Option<String> {
    let uri = value.get("workspaceIdentifier")?.get("uri")?;
    ["path", "fsPath", "external"]
        .iter()
        .filter_map(|key| uri.get(*key).and_then(Value::as_str))
        .filter_map(decode_workspace_location)
        .find_map(|candidate| bounded_workspace(&candidate))
}

fn decode_workspace_location(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("file://") {
        // A local file URI keeps the leading separator; a URI naming a
        // remote host is not a local project directory.
        let path = rest.strip_prefix('/')?;
        return Some(decode_percent_escapes(&format!("/{path}")));
    }
    if trimmed.contains("://") {
        return None;
    }
    Some(trimmed.to_string())
}

fn decode_percent_escapes(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| value.to_string())
}

/// A recorded project directory must be a bounded absolute path — never
/// the filesystem root, the home directory or one of its ancestors.
fn bounded_workspace(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = Path::new(trimmed);
    if !acop::util::recorded_absolute(trimmed) || path.parent().is_none() {
        return None;
    }
    if let Some(home) = std::env::home_dir() {
        // The home directory and every ancestor of it hold the user's
        // whole personal tree — never a project directory.
        if home.starts_with(path) {
            return None;
        }
    }
    Some(trimmed.to_string())
}

/// Bubble keys of one composer — the fallback when the composer row keeps
/// no `fullConversationHeadersOnly` list.
fn composer_bubble_ids(connection: &Connection, composer_id: &str) -> Vec<String> {
    let prefix = format!("bubbleId:{composer_id}:");
    let upper = format!("bubbleId:{composer_id};");
    let Ok(mut statement) =
        connection.prepare("SELECT key FROM cursorDiskKV WHERE key >= ?1 AND key < ?2")
    else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map([&prefix, &upper], |row| {
        Ok(sqlite_value_text(row.get_ref(0)?))
    }) else {
        return Vec::new();
    };
    rows.flatten()
        .flatten()
        .filter_map(|key| {
            key.strip_prefix(&prefix)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        })
        .collect()
}

fn disk_kv_json(connection: &Connection, key: &str) -> Option<Value> {
    let value = connection
        .query_row(
            "SELECT value FROM cursorDiskKV WHERE key = ?1 LIMIT 1",
            [key],
            |row| Ok(sqlite_value_text(row.get_ref(0)?)),
        )
        .ok()
        .flatten()?;
    serde_json::from_str(&value).ok()
}

/// Decode one composer row plus its `bubbleId:*` messages.
///
/// Container "lines": 0 is the composer row, bubbles number from 1 — the
/// record-line map stays honest without inventing file lines.
fn decode_composer_session(
    connection: &Connection,
    path: &Path,
    composer_id: &str,
    key: Option<&str>,
    known_composers: &std::collections::BTreeSet<String>,
) -> Result<Decoded> {
    let row_key = key
        .map(str::to_string)
        .unwrap_or_else(|| format!("composerData:{composer_id}"));
    let composer = disk_kv_json(connection, &row_key).ok_or_else(|| Error::Decode {
        path: path.to_path_buf(),
        line: 0,
        message: format!("composer row `{row_key}` missing or not JSON"),
    })?;
    let id = composer
        .get("composerId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| composer_id.to_string());
    let model = composer_model_from_config(&composer);
    let mut conversation = Conversation::new(&id);
    conversation.agent = Some("cursor".to_string());
    conversation.title = composer
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    conversation.created_at =
        epoch_value_to_rfc3339(composer.get("createdAt").unwrap_or(&Value::Null));
    conversation.updated_at = epoch_value_to_rfc3339(
        composer
            .get("lastUpdatedAt")
            .or_else(|| composer.get("updatedAt"))
            .unwrap_or(&Value::Null),
    );
    conversation.cwd = composer_workspace_path(&composer);

    let mut records = Vec::new();
    let mut record_lines = Vec::new();
    let mut skipped = Vec::new();

    // Delegated lineage is part of session ancestry — a meta record keeps
    // it without collapsing the session into its parent. A parent pointer
    // the store cannot resolve (the parent composer row is gone) leaves
    // the session a plain root, never a delegated marker.
    let mut session_facts = serde_json::Map::new();
    session_facts.insert("model".to_string(), json!(model));
    let parent = composer
        .get("subagentInfo")
        .and_then(|info| info.get("parentComposerId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty() && known_composers.contains(*v))
        .map(str::to_string);
    if let Some(parent) = parent {
        let info = composer.get("subagentInfo").unwrap_or(&Value::Null);
        session_facts.insert("parentSessionId".to_string(), json!(parent));
        if let Some(kind) = info.get("subagentTypeName").and_then(Value::as_str) {
            session_facts.insert("subagentTypeName".to_string(), json!(kind));
        }
        if let Some(title) = composer.get("name").and_then(Value::as_str) {
            session_facts.insert("subagentTitle".to_string(), json!(title));
        }
        push_record(
            &mut records,
            &mut record_lines,
            0,
            None,
            Body::Meta {
                subtype: "subagent-lineage".to_string(),
                data: Value::Object(session_facts),
            },
        );
    } else {
        push_record(
            &mut records,
            &mut record_lines,
            0,
            None,
            Body::Meta {
                subtype: "session-agent".to_string(),
                data: Value::Object(session_facts),
            },
        );
    }

    let mut bubble_ids = composer
        .get("fullConversationHeadersOnly")
        .and_then(Value::as_array)
        .map(|headers| {
            headers
                .iter()
                .filter_map(|h| {
                    h.get("bubbleId")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(str::to_string)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if bubble_ids.is_empty() {
        bubble_ids = composer_bubble_ids(connection, &id);
    }
    for (index, bubble_id) in bubble_ids.iter().enumerate() {
        let line = index + 1;
        let Some(bubble) = disk_kv_json(connection, &format!("bubbleId:{id}:{bubble_id}")) else {
            skipped.push(Skipped {
                line,
                native_type: Some("bubbleId".to_string()),
                reason: "corrupt".to_string(),
            });
            continue;
        };
        let before = records.len();
        project_bubble(
            &mut records,
            &mut record_lines,
            line,
            &bubble,
            bubble_id,
            &model,
        );
        if records.len() == before {
            skipped.push(Skipped {
                line,
                native_type: Some("bubbleId".to_string()),
                reason: "projection".to_string(),
            });
        }
    }
    if records.is_empty() {
        return Err(Error::Decode {
            path: path.to_path_buf(),
            line: 0,
            message: format!("composer `{id}` holds no decodable messages"),
        });
    }
    conversation.records = records;
    Ok(Decoded {
        conversation,
        record_lines,
        line_count: bubble_ids.len() + 1,
        skipped,
    })
}

/// One `bubbleId:` blob → user/assistant records plus a usage meta.
fn project_bubble(
    records: &mut Vec<Record>,
    record_lines: &mut Vec<usize>,
    line: usize,
    bubble: &Value,
    bubble_id: &str,
    fallback_model: &str,
) {
    let role = match bubble.get("type").and_then(Value::as_i64) {
        Some(1) => "user",
        Some(0) | Some(2) => "agent",
        _ => match bubble
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            r if r.contains("user") || r.contains("human") => "user",
            r if r.contains("assistant") || r.contains("agent") || r.contains("model") => "agent",
            _ => return,
        },
    };
    let text = bubble
        .get("text")
        .or_else(|| bubble.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // A bubble's own model name wins; the "default" placeholder defers
    // to the composer's declared model (selectedModels first).
    let model = bubble
        .pointer("/modelInfo/modelName")
        .and_then(Value::as_str)
        .or_else(|| bubble.get("modelName").and_then(Value::as_str))
        .or_else(|| bubble.get("model").and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("default"))
        .map(str::to_string)
        .unwrap_or_else(|| fallback_model.to_string());
    let usage = bubble.get("tokenCount").filter(|u| u.is_object());
    let attachments = bubble_attachments(bubble);
    if text.trim().is_empty() && usage.is_none() && attachments.is_empty() {
        return;
    }
    let ts = epoch_value_to_rfc3339(bubble.get("createdAt").unwrap_or(&Value::Null));
    let id = bubble
        .get("bubbleId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| bubble_id.to_string());
    let body = if role == "user" {
        Body::User {
            text: text.clone(),
            model: Some(model.clone()),
            attachments,
        }
    } else {
        Body::Assistant {
            text: text.clone(),
            model: Some(model.clone()),
        }
    };
    records.push(Record {
        id: Some(id),
        parent: None,
        ts: ts.clone(),
        ordinal: records.len() as u64,
        body,
    });
    record_lines.push(line);
    if let Some(usage) = usage {
        push_record(
            records,
            record_lines,
            line,
            ts,
            Body::Meta {
                subtype: "usage".to_string(),
                data: json!({"usage": usage, "model": model}),
            },
        );
    }
}

/// Non-text payload keys Cursor attaches to a bubble.
fn bubble_attachments(bubble: &Value) -> Vec<Attachment> {
    [
        "attachments",
        "attachedFiles",
        "attachedFileCodeChunks",
        "fileSelections",
        "images",
        "imageAttachments",
    ]
    .iter()
    .filter_map(|key| bubble.get(*key).map(|value| (*key, value)))
    .flat_map(|(kind, value)| match value {
        Value::Array(items) => items
            .iter()
            .map(|item| (kind, item.clone()))
            .collect::<Vec<_>>(),
        Value::Null => Vec::new(),
        other => vec![(kind, other.clone())],
    })
    .map(|(kind, data)| Attachment {
        kind: kind.to_string(),
        media_type: data
            .get("mediaType")
            .or_else(|| data.get("mimeType"))
            .and_then(Value::as_str)
            .map(str::to_string),
        data,
    })
    .collect()
}

// ---------------------------------------------------------------------------
// CLI chat store (`~/.cursor/chats/<hash>/<session>/store.db`)
// ---------------------------------------------------------------------------

/// Decode one `store.db`: the `blobs` table's AI-SDK-shaped message
/// objects plus session facts from `meta` and the sibling `meta.json`.
fn decode_cli_store(connection: &Connection, path: &Path) -> Result<Decoded> {
    let mut conversation = Conversation::new(cli_store_session_id(path));
    conversation.agent = Some("cursor".to_string());
    let mut record_lines = Vec::new();
    if let Some(meta) = cli_store_metadata(connection) {
        conversation.created_at = meta.created_at;
        conversation.updated_at = meta.updated_at;
        conversation.title = meta.name;
        if let Some(model) = meta.model.filter(|m| !m.trim().is_empty()) {
            // The store's own model record stays visible as metadata.
            conversation
                .records
                .push(meta_record(0, "store-model", json!({"model": model})));
            record_lines.push(0);
        }
    }
    if let Some(meta_json) = cli_meta_json(path) {
        conversation.created_at = meta_json
            .get("createdAtMs")
            .and_then(epoch_value_to_rfc3339)
            .or(conversation.created_at);
        conversation.updated_at = meta_json
            .get("updatedAtMs")
            .and_then(epoch_value_to_rfc3339)
            .or(conversation.updated_at);
        conversation.cwd = meta_json
            .get("cwd")
            .and_then(Value::as_str)
            .and_then(bounded_workspace)
            .or(conversation.cwd);
    }

    let Ok(mut statement) = connection.prepare("SELECT id, data FROM blobs ORDER BY id ASC") else {
        return Err(Error::Decode {
            path: path.to_path_buf(),
            line: 0,
            message: "store.db carries no readable `blobs` table".into(),
        });
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<Vec<u8>>>(1)?,
        ))
    }) else {
        return Err(Error::Decode {
            path: path.to_path_buf(),
            line: 0,
            message: "store.db `blobs` rows do not read".into(),
        });
    };
    let mut skipped = Vec::new();
    let mut blob_count = 0usize;
    for (index, row) in rows.flatten().enumerate() {
        blob_count = index + 1;
        let line = index + 1;
        let (_, data) = row;
        let Some(data) = data else {
            skipped.push(Skipped {
                line,
                native_type: Some("blob".to_string()),
                reason: "corrupt".to_string(),
            });
            continue;
        };
        let Ok(raw) = std::str::from_utf8(&data) else {
            skipped.push(Skipped {
                line,
                native_type: Some("blob".to_string()),
                reason: "corrupt".to_string(),
            });
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(raw) else {
            skipped.push(Skipped {
                line,
                native_type: Some("blob".to_string()),
                reason: "corrupt".to_string(),
            });
            continue;
        };
        let before = conversation.records.len();
        project_store_blob(&mut conversation, &mut record_lines, line, &value);
        if conversation.records.len() == before {
            skipped.push(Skipped {
                line,
                native_type: value
                    .get("role")
                    .or_else(|| value.get("type"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                reason: "projection".to_string(),
            });
        }
    }
    for (i, rec) in conversation.records.iter_mut().enumerate() {
        rec.ordinal = i as u64;
    }
    Ok(Decoded {
        conversation,
        record_lines,
        line_count: blob_count + 1,
        skipped,
    })
}

/// Session facts recovered from the `meta` table.
struct CliMeta {
    created_at: Option<String>,
    updated_at: Option<String>,
    model: Option<String>,
    name: Option<String>,
}

/// The store's `meta` table holds hex-encoded (or plain) JSON session
/// facts — first row with any of `createdAt`/`updatedAt`/`model` wins.
fn cli_store_metadata(connection: &Connection) -> Option<CliMeta> {
    if !sqlite_table_exists(connection, "meta") {
        return None;
    }
    let mut statement = connection
        .prepare("SELECT value FROM meta ORDER BY key ASC")
        .ok()?;
    let rows = statement
        .query_map([], |row| row.get::<_, Option<String>>(0))
        .ok()?;
    for value in rows.flatten().flatten() {
        let Some(json) = decode_cli_meta_value(&value) else {
            continue;
        };
        let created_at = json
            .get("createdAt")
            .and_then(epoch_value_to_rfc3339)
            .or_else(|| json.get("createdAtMs").and_then(epoch_value_to_rfc3339));
        let updated_at = json
            .get("updatedAt")
            .and_then(epoch_value_to_rfc3339)
            .or_else(|| json.get("updatedAtMs").and_then(epoch_value_to_rfc3339));
        let model = json
            .get("model")
            .or_else(|| json.get("modelId"))
            .and_then(Value::as_str)
            .map(normalize_model_name);
        let name = json
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty() && *v != "New Agent")
            .map(str::to_string);
        if created_at.is_some() || updated_at.is_some() || model.is_some() || name.is_some() {
            return Some(CliMeta {
                created_at,
                updated_at,
                model,
                name,
            });
        }
    }
    None
}

/// `meta.json` — the real creation/update times and working directory
/// Cursor writes beside `store.db`.
fn cli_meta_json(path: &Path) -> Option<Value> {
    let meta_path = path.parent()?.join("meta.json");
    let raw = std::fs::read_to_string(meta_path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn decode_cli_meta_value(value: &str) -> Option<Value> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed).ok();
    }
    if trimmed.len() % 2 == 0 && trimmed.chars().all(|ch| ch.is_ascii_hexdigit()) {
        let bytes = (0..trimmed.len())
            .step_by(2)
            .filter_map(|index| u8::from_str_radix(&trimmed[index..index + 2], 16).ok())
            .collect::<Vec<_>>();
        let text = String::from_utf8(bytes).ok()?;
        return serde_json::from_str(&text).ok();
    }
    None
}

/// One `blobs` row → records. Shapes are AI-SDK messages:
/// `{"role","content","id","providerOptions"}` with `content` a string or
/// `[{type:text|reasoning|tool-call|tool-result,…}]`, plus a
/// `{"sessionId","configOptions"}` session record.
fn project_store_blob(
    conversation: &mut Conversation,
    record_lines: &mut Vec<usize>,
    line: usize,
    value: &Value,
) {
    let role = value
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    // Config/metadata rows carry `sessionId`/`configOptions` and no role;
    // a transcript message may carry `sessionId` alongside its role.
    if value.get("configOptions").is_some() || (value.get("sessionId").is_some() && role.is_empty())
    {
        conversation
            .records
            .push(meta_record(line, "session-config", value.clone()));
        record_lines.push(line);
        return;
    }
    // Message envelopes wrap payload as {"role","message":{"content":[…]}}.
    let id = value
        .get("id")
        .or_else(|| value.pointer("/message/id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let content = value
        .get("content")
        .or_else(|| value.get("message").and_then(|m| m.get("content")));
    let push = |conversation: &mut Conversation, record_lines: &mut Vec<usize>, body: Body| {
        let ordinal = conversation.records.len() as u64;
        conversation.records.push(Record {
            id: id.clone(),
            parent: None,
            ts: None,
            ordinal,
            body,
        });
        record_lines.push(line);
    };
    match role.as_str() {
        "system" => {
            if let Some(text) = content_text(content) {
                push(
                    conversation,
                    record_lines,
                    Body::System {
                        subtype: "system".to_string(),
                        text,
                    },
                );
            }
        }
        "user" => match content {
            Some(Value::Array(blocks)) => {
                let mut text_parts = Vec::new();
                let mut attachments = Vec::new();
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") | None => {
                            if let Some(text) = block
                                .get("text")
                                .or_else(|| block.get("content"))
                                .and_then(Value::as_str)
                            {
                                text_parts.push(text.to_string());
                            }
                        }
                        Some(other) => attachments.push(Attachment {
                            kind: other.to_string(),
                            media_type: block
                                .get("mediaType")
                                .or_else(|| block.get("mimeType"))
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            data: block.clone(),
                        }),
                    }
                }
                let text = text_parts.join("\n");
                if !text.trim().is_empty() || !attachments.is_empty() {
                    push(
                        conversation,
                        record_lines,
                        Body::User {
                            text,
                            model: None,
                            attachments,
                        },
                    );
                }
            }
            _ => {
                if let Some(text) = content_text(content) {
                    push(
                        conversation,
                        record_lines,
                        Body::User {
                            text,
                            model: None,
                            attachments: Vec::new(),
                        },
                    );
                }
            }
        },
        "assistant" => {
            if let Some(blocks) = content.and_then(Value::as_array) {
                for block in blocks {
                    match block
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                    {
                        "reasoning" | "thinking" => {
                            if let Some(text) = non_empty(block.get("text")) {
                                push(conversation, record_lines, Body::Reasoning { text });
                            }
                        }
                        "tool-call" | "tool_use" => {
                            push(
                                conversation,
                                record_lines,
                                Body::ToolCall {
                                    call_id: block
                                        .get("toolCallId")
                                        .or_else(|| block.get("id"))
                                        .and_then(Value::as_str)
                                        .map(str::to_string),
                                    name: block
                                        .get("toolName")
                                        .or_else(|| block.get("name"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("tool")
                                        .to_string(),
                                    args: block
                                        .get("args")
                                        .or_else(|| block.get("input"))
                                        .cloned()
                                        .unwrap_or(Value::Null),
                                },
                            );
                        }
                        "tool-result" | "tool_result" => {
                            push(conversation, record_lines, tool_result_body(block));
                        }
                        _ => {
                            if let Some(text) =
                                non_empty(block.get("text").or_else(|| block.get("content")))
                            {
                                push(
                                    conversation,
                                    record_lines,
                                    Body::Assistant { text, model: None },
                                );
                            }
                        }
                    }
                }
            } else if let Some(text) = content_text(content) {
                push(
                    conversation,
                    record_lines,
                    Body::Assistant { text, model: None },
                );
            }
        }
        "tool" => {
            if let Some(blocks) = content.and_then(Value::as_array) {
                for block in blocks {
                    push(conversation, record_lines, tool_result_body(block));
                }
            } else if let Some(text) = content_text(content) {
                push(
                    conversation,
                    record_lines,
                    Body::ToolResult {
                        call_id: None,
                        name: None,
                        content: json!(text),
                        is_error: false,
                    },
                );
            }
        }
        _ => {}
    }
}

fn tool_result_body(block: &Value) -> Body {
    Body::ToolResult {
        call_id: block
            .get("toolCallId")
            .or_else(|| block.get("tool_use_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        name: block
            .get("toolName")
            .or_else(|| block.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string),
        content: block
            .get("result")
            .or_else(|| block.get("output"))
            .or_else(|| block.get("experimental_content"))
            .or_else(|| block.get("content"))
            .cloned()
            .unwrap_or(Value::Null),
        is_error: block
            .get("isError")
            .or_else(|| block.get("is_error"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

fn content_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Array(blocks) => {
            let text: String = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.trim().is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn non_empty(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string)
}

fn meta_record(line: usize, subtype: &str, data: Value) -> Record {
    Record {
        id: None,
        parent: None,
        ts: None,
        ordinal: line as u64,
        body: Body::Meta {
            subtype: subtype.to_string(),
            data,
        },
    }
}

fn push_record(
    records: &mut Vec<Record>,
    record_lines: &mut Vec<usize>,
    line: usize,
    ts: Option<String>,
    body: Body,
) {
    records.push(Record {
        id: None,
        parent: None,
        ts,
        ordinal: records.len() as u64,
        body,
    });
    record_lines.push(line);
}

fn decode_transcript(path: &Path) -> Result<Decoded> {
    let file = File::open(path).map_err(Error::io(path))?;
    let mut conversation = Conversation::new(transcript_session_id(path));
    conversation.agent = Some("cursor".to_string());
    conversation.cwd = transcript_workspace(path).or(conversation.cwd);
    let mut record_lines = Vec::new();
    let mut skipped = Vec::new();
    let mut line_count = 0;

    // A transcript under `subagents/` is a delegated task: identity is the
    // task's own file stem, ancestry is the owning conversation directory.
    if let Some(parent) = delegated_parent(path) {
        conversation.records.push(meta_record(
            0,
            "subagent-lineage",
            json!({"parentSessionId": parent, "taskId": transcript_session_id(path)}),
        ));
        record_lines.push(0);
    }

    for (index, line) in BufReader::new(file).lines().enumerate() {
        line_count = index + 1;
        let Ok(line) = line else { continue };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            skipped.push(Skipped {
                line: index,
                native_type: None,
                reason: "corrupt".to_string(),
            });
            continue;
        };
        let before = conversation.records.len();
        project_transcript_line(&mut conversation, &mut record_lines, index, &value);
        if conversation.records.len() == before {
            skipped.push(Skipped {
                line: index,
                native_type: value
                    .get("type")
                    .or_else(|| value.get("role"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                reason: "projection".to_string(),
            });
        }
    }
    for (i, rec) in conversation.records.iter_mut().enumerate() {
        rec.ordinal = i as u64;
    }
    Ok(Decoded {
        conversation,
        record_lines,
        line_count,
        skipped,
    })
}

/// Transcript session identity: `agent-transcripts/<id>/<id>.jsonl`,
/// `subagents/<task>.jsonl` and `store.db` all name the conversation in
/// the path; everywhere else the file stem is the name.
fn transcript_session_id(path: &Path) -> String {
    if path.file_name().and_then(|n| n.to_str()) == Some("store.db") {
        return cli_store_session_id(path);
    }
    path.file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "cursor-session".to_string())
}

/// The conversation a `subagents/` transcript belongs to: the directory
/// that owns the `subagents/` directory.
fn delegated_parent(path: &Path) -> Option<String> {
    let mut ancestors = path.ancestors().skip(1);
    loop {
        let ancestor = ancestors.next()?;
        if ancestor.file_name().and_then(|n| n.to_str()) == Some(DELEGATED_DIRECTORY) {
            return ancestor
                .parent()
                .and_then(|dir| dir.file_name())
                .and_then(|name| name.to_str())
                .map(str::to_string);
        }
        ancestor.parent()?;
    }
}

/// The trusted workspace recorded at the owning project root —
/// `.workspace-trusted` holds `{"workspacePath": …}`.
fn transcript_workspace(path: &Path) -> Option<String> {
    let project = path
        .ancestors()
        .find(|ancestor| {
            ancestor.file_name().and_then(|n| n.to_str()) == Some(TRANSCRIPTS_DIRECTORY)
        })?
        .parent()?;
    let trusted = project.join(".workspace-trusted");
    if !trusted.is_file() {
        return None;
    }
    let raw = std::fs::read_to_string(&trusted).ok()?;
    let value = serde_json::from_str::<Value>(&raw).ok()?;
    bounded_workspace(value.get("workspacePath").and_then(Value::as_str)?)
}

/// One transcript line → records. `{"role","message":{"content":[…]}}`
/// maps to user/assistant bodies; `{"type":…}` lifecycle rows map to
/// system records.
fn project_transcript_line(
    conversation: &mut Conversation,
    record_lines: &mut Vec<usize>,
    line: usize,
    value: &Value,
) {
    let role = value
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    match role.as_str() {
        "user" | "assistant" | "system" | "tool" => {
            project_store_blob(conversation, record_lines, line, value)
        }
        _ => {
            // Lifecycle rows: {"type":"status"|"turn_ended"|…, "status", "error"}.
            let kind = value.get("type").and_then(Value::as_str);
            let text = value
                .get("error")
                .or_else(|| value.get("status"))
                .or_else(|| value.get("text"))
                .and_then(Value::as_str);
            if let (Some(kind), Some(text)) = (kind, text) {
                let ordinal = conversation.records.len() as u64;
                conversation.records.push(Record {
                    id: None,
                    parent: None,
                    ts: None,
                    ordinal,
                    body: Body::System {
                        subtype: kind.to_string(),
                        text: text.to_string(),
                    },
                });
                record_lines.push(line);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SQLite file routing for `decode` outside enumeration
// ---------------------------------------------------------------------------

/// A raw `.vscdb`/`.sqlite` path: decode it when it physically holds one
/// session (`store.db` blobs, or a container with exactly one stored
/// session); a recognized multi-session container is refused by name.
fn decode_sqlite_file(path: &Path) -> Result<Decoded> {
    let connection = open_read_only(path).ok_or_else(|| Error::Decode {
        path: path.to_path_buf(),
        line: 0,
        message: "sqlite store does not open read-only".into(),
    })?;
    if sqlite_table_exists(&connection, "cursorDiskKV") {
        let composers = composer_rows(&connection)
            .into_iter()
            .filter(|composer| {
                !composer.bubble_ids.is_empty()
                    || !composer_bubble_ids(&connection, &composer.id).is_empty()
            })
            .collect::<Vec<_>>();
        let known = composers
            .iter()
            .map(|composer| composer.id.clone())
            .collect();
        return match composers.len() {
            1 => decode_composer_session(
                &connection,
                path,
                &composers[0].id,
                Some(&composers[0].key),
                &known,
            ),
            n => Err(Error::Usage(format!(
                "{} is a cursor container store holding {n} sessions; enumerate it",
                path.display()
            ))),
        };
    }
    if let Some(sessions) = acop::generic::openagent::session_refs(&connection, path) {
        return match sessions.len() {
            1 => acop::generic::openagent::decode(
                &connection,
                path,
                sessions[0].key.as_deref().unwrap_or(&sessions[0].id),
                "cursor",
            )?
            .ok_or_else(|| {
                Error::Usage(format!(
                    "{} holds an openagent schema but no session row matched",
                    path.display()
                ))
            }),
            n => Err(Error::Usage(format!(
                "{} is an openagent container store holding {n} sessions; enumerate it",
                path.display()
            ))),
        };
    }
    if sqlite_table_exists(&connection, "blobs") {
        return decode_cli_store(&connection, path);
    }
    match acop::generic::sqlite_session_keys(path)?.len() {
        1 => acop::generic::decode_sqlite(path, None, "cursor"),
        n => Err(Error::Usage(format!(
            "{} is a cursor container store holding {n} sessions; enumerate it",
            path.display()
        ))),
    }
}

// ---------------------------------------------------------------------------
// Encode helpers + redaction internals
// ---------------------------------------------------------------------------

/// Emit one assistant `message` line for a run of assistant-side records:
/// text/reasoning/tool calls become `content` blocks.
fn flush_assistant(pending: &mut Vec<&Record>, lines: &mut Vec<String>) {
    if pending.is_empty() {
        return;
    }
    let blocks: Vec<Value> = pending
        .iter()
        .map(|rec| match &rec.body {
            Body::Reasoning { text } => json!({"type": "reasoning", "text": text}),
            Body::ToolCall { name, args, .. } => {
                json!({"type": "tool_use", "name": name, "input": args})
            }
            _ => json!({"type": "text", "text": rec.body.text()}),
        })
        .collect();
    lines.push(json!({"role": "assistant", "message": {"content": blocks}}).to_string());
    pending.clear();
}

/// Rewrite the textual fields inside a `content` value — a string, or
/// blocks' `text`/`thinking` fields plus tool payloads.
fn redact_content(content: &mut Value, marker: &str) -> bool {
    match content {
        Value::String(_) => {
            *content = json!(marker);
            true
        }
        Value::Array(blocks) => {
            let mut touched = false;
            for block in blocks {
                for key in ["text", "thinking"] {
                    if block.get(key).and_then(Value::as_str).is_some() {
                        block[key] = json!(marker);
                        touched = true;
                    }
                }
                for key in ["input", "args", "result", "output"] {
                    if block.get(key).is_some_and(|v| !v.is_null()) {
                        block[key] = json!(marker);
                        touched = true;
                    }
                }
            }
            touched
        }
        _ => false,
    }
}
