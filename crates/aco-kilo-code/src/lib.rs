//! aco codec: the `kilocode` (Kilo Code) conversation store.
//!
//! Kilo Code persists sessions in an OpenCode-shaped SQLite database —
//! `~/.local/share/kilo/kilo.db`, declared by the a4aa card as the
//! `kilo-session-store` root with [`StoreKind::SqliteDb`]. Three tables
//! hold a conversation:
//!
//! - `session` — one row per session: `id`, `parent_id`, `title`,
//!   `directory`, `path`, `agent`, `model`, `time_created`,
//!   `time_updated`, `time_archived`, `tokens_input`, `tokens_output`,
//!   `tokens_reasoning`, `tokens_cache_read`, `tokens_cache_write`.
//!   Narrower builds lack trailing columns; missing ones project as
//!   `NULL` the way the source reader does.
//! - `message` — one row per turn: `id`, `session_id`, `time_created`,
//!   `time_updated`, `data` (JSON blob carrying `role`,
//!   `time.{created,completed}`, `parentID`, `modelID`, `tokens`).
//! - `part` — one row per content block: `message_id`, `session_id`,
//!   `data` (JSON blob typed `text` | `reasoning` | `tool` | `file` |
//!   `step-start` | `step-finish` | `snapshot` | `patch` | `agent` |
//!   `retry` | `compaction` | …).
//!
//! A database that lacks the `session`/`message`/`part` triple falls
//! back to the generic schema probe the source catalog uses: tables
//! whose names suggest history (`chat|conversation|session|history` or
//! `ItemTable` — never `sqlite*`/`fts`/`embedding`/`account`) are scanned
//! row by row, rows whose text mentions conversation markers group into
//! sessions by their key column, and JSON-bearing fields decode as
//! message envelopes.
//!
//! The store is read-only for ACO: Kilo Code has no file session format
//! to write, so [`Codec::encode`] and [`Codec::store_path`] report
//! [`Error::Usage`] and the capability matrix stays
//! [`CapabilityMatrix::READ_ONLY`].

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use aco::codec::{Codec, Decoded, Encoded, SessionRef, Skipped};
use aco::model::{Body, Conversation, Record};
use aco::store::{HistoryRoot, StoreKind};
use aco::{CapabilityMatrix, Error, Result};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

const CAPABILITIES: CapabilityMatrix = CapabilityMatrix::READ_ONLY;

/// Extensions LicoUp's `kilo-code` history adapter accepts as
/// session-shaped artifacts under the store root.
const ACCEPTED_EXTENSIONS: &[&str] = &[
    "jsonl", "ndjson", "json", "md", "txt", "log", "sqlite", "sqlite3", "db",
];

/// Extensions that open as SQLite containers for [`Codec::enumerate`].
const SQLITE_EXTENSIONS: &[&str] = &["sqlite", "sqlite3", "db"];

/// File names LicoUp excludes before extension matching.
fn rejected_file_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.ends_with(".backup") || name == "codebase-external.sqlite")
        .unwrap_or(false)
}

fn extension_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// The codec instance aco's registry hands out.
pub struct KiloCodeCodec;

/// One codec instance for the registry.
pub fn codec() -> &'static dyn Codec {
    &KiloCodeCodec
}

impl Codec for KiloCodeCodec {
    fn id(&self) -> &'static str {
        "kilocode"
    }

    fn history(&self) -> Vec<HistoryRoot> {
        a4aa_kilo_code::card().history.to_vec()
    }

    /// The card declares no transcript-usage surface (`transcript: None`
    /// upstream); session/message token counters still decode as `usage`
    /// meta records.
    fn transcript_usage(&self) -> Option<aco::TranscriptUsage> {
        a4aa_kilo_code::card().usage.transcript
    }

    fn session_kinds(&self) -> &'static [&'static str] {
        &["kilo-session-store"]
    }

    fn capabilities(&self) -> &'static CapabilityMatrix {
        &CAPABILITIES
    }

    /// LicoUp's `accepts_file` set for the `kilo-code` adapter.
    fn select_session_files(&self, files: Vec<PathBuf>) -> Vec<PathBuf> {
        files
            .into_iter()
            .filter(|p| {
                !rejected_file_name(p) && ACCEPTED_EXTENSIONS.contains(&extension_of(p).as_str())
            })
            .collect()
    }

    /// Container contract: a host that discovers files itself still
    /// reaches row-level sessions — a database file enumerates its
    /// session rows, an accepted file stays one session per file.
    fn sessions_in_file(&self, path: &Path) -> Result<Vec<SessionRef>> {
        if SQLITE_EXTENSIONS.contains(&extension_of(path).as_str()) && !rejected_file_name(path) {
            return Ok(enumerate_database(path));
        }
        Ok(vec![SessionRef {
            id: self.session_id(path),
            path: path.to_path_buf(),
            key: None,
        }])
    }

    /// `SqliteDb` roots enumerate one [`SessionRef`] per session-bearing
    /// row: OpenCode `session` rows keyed by `id`, or generic-probe row
    /// groups keyed `table:group`. Other store kinds keep the file
    /// enumeration (accepted files, one session per file).
    fn enumerate(&self, root: &HistoryRoot, path: &Path) -> Result<Vec<SessionRef>> {
        if root.store != StoreKind::SqliteDb {
            let files = if path.is_file() {
                vec![path.to_path_buf()]
            } else {
                aco::store::collect_files(path)?
            };
            return Ok(self
                .select_session_files(files)
                .into_iter()
                .map(|p| SessionRef {
                    id: self.session_id(&p),
                    path: p,
                    key: None,
                })
                .collect());
        }
        let mut out = Vec::new();
        for db in database_files(path) {
            out.extend(enumerate_database(&db));
        }
        Ok(out)
    }

    /// `key = Some(session id)` decodes that `session` row; `key =
    /// Some("table:group")` decodes a generic-probe row group; `key =
    /// None` falls through to [`Codec::decode`] on the path.
    fn decode_session(&self, session: &SessionRef) -> Result<Decoded> {
        match &session.key {
            Some(key) => decode_container_session(&session.path, key),
            None => self.decode(&session.path),
        }
    }

    /// Decode one accepted artifact: `jsonl`/`ndjson` line sessions,
    /// `json` documents, `md`/`txt`/`log` text transcripts, or a sqlite
    /// database file. A database holding several sessions cannot become
    /// one canonical conversation — the caller must name a session.
    fn decode(&self, path: &Path) -> Result<Decoded> {
        match extension_of(path).as_str() {
            "jsonl" | "ndjson" => decode_jsonl(path),
            "json" => decode_json_document(path),
            "md" | "txt" | "log" => decode_text(path),
            "sqlite" | "sqlite3" | "db" => {
                let sessions = enumerate_database(path);
                match sessions.len() {
                    1 => self.decode_session(&sessions[0]),
                    0 => Err(Error::Usage(format!(
                        "no kilocode sessions found in `{}`",
                        path.display()
                    ))),
                    n => Err(Error::Usage(format!(
                        "`{}` holds {n} kilocode sessions; select one by id",
                        path.display()
                    ))),
                }
            }
            _ => Err(Error::Usage(format!(
                "`{}` is not a kilocode session artifact (accepted: {})",
                path.display(),
                ACCEPTED_EXTENSIONS.join(", ")
            ))),
        }
    }

    /// Kilo sessions live in a sqlite store — there is no kilocode file
    /// session format to encode canonical records into.
    fn encode(&self, _conversation: &Conversation) -> Result<Encoded> {
        Err(Error::Usage(
            "kilocode keeps sessions in a sqlite store (`kilo.db`); no file \
             session format exists to encode into — transform into a \
             file-based agent store instead"
                .to_string(),
        ))
    }

    /// Same read-only boundary as [`Codec::encode`]: no writable file
    /// layout exists for imported conversations.
    fn store_path(&self, _home: &Path, _conversation: &Conversation) -> Result<PathBuf> {
        Err(Error::Usage(
            "kilocode keeps sessions in a sqlite store (`kilo.db`); no file \
             session format exists for imports"
                .to_string(),
        ))
    }

    /// Rewrite the text-bearing fields of a JSON session/event line
    /// (accepted `jsonl` artifacts); anything else holds no redactable
    /// text for `--redact`.
    fn redact_line(&self, line: &Value, marker: &str) -> Option<Value> {
        let mut out = line.clone();
        let mut touched = false;
        // Content blocks first — `thinking`/`text` payloads.
        for key in ["content", "parts"] {
            if let Some(blocks) = out.get_mut(key).and_then(Value::as_array_mut) {
                for block in blocks {
                    for field in ["text", "thinking", "reasoning"] {
                        if block.get(field).and_then(Value::as_str).is_some() {
                            block[field] = Value::String(marker.to_string());
                            touched = true;
                        }
                    }
                }
            }
        }
        for field in ["text", "content", "prompt", "response", "summary"] {
            if out.get(field).and_then(Value::as_str).is_some() {
                out[field] = Value::String(marker.to_string());
                touched = true;
            }
        }
        touched.then_some(out)
    }
}

// ---------------------------------------------------------------------------
// SQLite plumbing.
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

fn table_exists(connection: &Connection, table: &str) -> bool {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1 LIMIT 1",
            [table],
            |_| Ok(()),
        )
        .is_ok()
}

fn table_columns(
    connection: &Connection,
    table: &str,
) -> std::result::Result<std::collections::HashSet<String>, rusqlite::Error> {
    // Table names reaching this helper are compile-time constants or
    // schema-derived (never user input).
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let collected = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect();
    collected
}

fn sqlite_value_text(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Null => Some(String::new()),
        ValueRef::Integer(value) => Some(value.to_string()),
        ValueRef::Real(value) => Some(value.to_string()),
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
            Some(String::from_utf8_lossy(bytes).into_owned())
        }
    }
}

/// Candidate database files under a `SqliteDb` root: the file itself
/// when the root points at one, else every `sqlite`/`sqlite3`/`db` file
/// below the root.
fn database_files(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return vec![path.to_path_buf()];
    }
    aco::store::collect_files(path)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| !rejected_file_name(p) && SQLITE_EXTENSIONS.contains(&extension_of(p).as_str()))
        .collect()
}

/// Enumerate session-bearing rows in one database: the OpenCode
/// `session` table when the full `session`/`message`/`part` schema is
/// present, else the generic probe's row groups.
fn enumerate_database(db: &Path) -> Vec<SessionRef> {
    let Some(connection) = open_read_only(db) else {
        return Vec::new();
    };
    if let Some(refs) = openagent_session_refs(&connection, db) {
        return refs;
    }
    generic_session_refs(&connection, db)
}

fn decode_container_session(db: &Path, key: &str) -> Result<Decoded> {
    let connection = open_read_only(db).ok_or_else(|| Error::Decode {
        path: db.to_path_buf(),
        line: 0,
        message: "cannot open sqlite database".to_string(),
    })?;
    if let Some(decoded) = decode_openagent_session(&connection, db, key)? {
        return Ok(decoded);
    }
    let (table, group) = key.split_once(':').ok_or_else(|| {
        Error::Usage(format!(
            "kilocode session `{key}` not found in `{}`",
            db.display()
        ))
    })?;
    decode_generic_session(&connection, db, key, table, group)
}

// ---------------------------------------------------------------------------
// OpenCode-shaped schema: `session` / `message` / `part`.
// ---------------------------------------------------------------------------

fn openagent_schema(connection: &Connection) -> bool {
    ["session", "message", "part"]
        .iter()
        .all(|table| table_exists(connection, table))
}

fn openagent_session_refs(connection: &Connection, db: &Path) -> Option<Vec<SessionRef>> {
    if !openagent_schema(connection) {
        return None;
    }
    let columns = table_columns(connection, "session").ok()?;
    if !columns.contains("id") {
        return None;
    }
    let order = if columns.contains("time_updated") {
        "ORDER BY time_updated DESC, id ASC"
    } else {
        "ORDER BY id ASC"
    };
    let mut statement = connection
        .prepare(&format!("SELECT id FROM session {order}"))
        .ok()?;
    let rows = statement
        .query_map([], |row| Ok(sqlite_value_text(row.get_ref(0)?)))
        .ok()?;
    let mut refs = Vec::new();
    for row in rows {
        // Row errors abort enumeration like the source reader; the
        // caller then falls back to the generic probe.
        let id = row.ok()?;
        let Some(id) = id.filter(|id| !id.trim().is_empty()) else {
            continue;
        };
        refs.push(SessionRef {
            path: db.to_path_buf(),
            key: Some(id.clone()),
            id,
        });
    }
    Some(refs)
}

/// Session-row metadata the projection can use when the schema happens
/// to carry it. `id` is the only required column; narrower Kilo builds
/// lack `agent`, `model` and the token columns, which project as `NULL`.
const OPENAGENT_SESSION_COLUMNS: [&str; 15] = [
    "id",
    "title",
    "directory",
    "path",
    "agent",
    "model",
    "time_created",
    "time_updated",
    "tokens_input",
    "tokens_output",
    "tokens_reasoning",
    "tokens_cache_read",
    "tokens_cache_write",
    "parent_id",
    "time_archived",
];

struct SessionMeta {
    id: String,
    title: Option<String>,
    directory: Option<String>,
    path: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
    usage: Option<Value>,
    parent_id: Option<String>,
    archived: bool,
}

fn openagent_session_meta(connection: &Connection, session_id: &str) -> Option<SessionMeta> {
    let columns = table_columns(connection, "session").ok()?;
    if !columns.contains("id") {
        return None;
    }
    let projection = OPENAGENT_SESSION_COLUMNS
        .iter()
        .map(|column| {
            if columns.contains(*column) {
                format!("\"{column}\"")
            } else {
                "NULL".to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    connection
        .query_row(
            &format!("SELECT {projection} FROM session WHERE id=?1"),
            [session_id],
            |row| {
                Ok(SessionMeta {
                    id: sqlite_value_text(row.get_ref(0)?).unwrap_or_default(),
                    title: sqlite_value_text(row.get_ref(1)?)
                        .filter(|value| !value.trim().is_empty()),
                    directory: sqlite_value_text(row.get_ref(2)?)
                        .as_deref()
                        .and_then(bounded_workspace),
                    path: sqlite_value_text(row.get_ref(3)?),
                    agent: sqlite_value_text(row.get_ref(4)?),
                    model: sqlite_value_text(row.get_ref(5)?),
                    created_at: row
                        .get::<_, Option<i64>>(6)?
                        .and_then(epoch_number_to_rfc3339),
                    updated_at: row
                        .get::<_, Option<i64>>(7)?
                        .and_then(epoch_number_to_rfc3339),
                    usage: session_usage(
                        row.get::<_, Option<i64>>(8)?,
                        row.get::<_, Option<i64>>(9)?,
                        row.get::<_, Option<i64>>(10)?,
                        row.get::<_, Option<i64>>(11)?,
                        row.get::<_, Option<i64>>(12)?,
                    ),
                    parent_id: sqlite_value_text(row.get_ref(13)?)
                        .filter(|value| !value.trim().is_empty()),
                    archived: row
                        .get::<_, Option<i64>>(14)?
                        .map(|value| value != 0)
                        .unwrap_or(false),
                })
            },
        )
        .ok()
}

/// One `message` row: id, column timestamps, and the `data` JSON blob.
struct MessageRow {
    id: String,
    created_ms: Option<i64>,
    updated_ms: Option<i64>,
    data: Value,
}

fn openagent_messages(connection: &Connection, session_id: &str) -> Vec<MessageRow> {
    let Ok(columns) = table_columns(connection, "message") else {
        return Vec::new();
    };
    if !["id", "session_id", "data"]
        .iter()
        .all(|column| columns.contains(*column))
    {
        return Vec::new();
    }
    let created = if columns.contains("time_created") {
        "time_created"
    } else {
        "NULL"
    };
    let updated = if columns.contains("time_updated") {
        "time_updated"
    } else {
        "NULL"
    };
    let order = if columns.contains("time_created") {
        "time_created ASC, id ASC"
    } else {
        "id ASC"
    };
    let query = format!(
        "SELECT id, {created}, {updated}, data FROM message WHERE session_id=?1 ORDER BY {order}"
    );
    let mut statement = match connection.prepare(&query) {
        Ok(statement) => statement,
        Err(_) => return Vec::new(),
    };
    let rows = match statement.query_map([session_id], |row| {
        Ok((
            sqlite_value_text(row.get_ref(0)?),
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<i64>>(2)?,
            sqlite_value_text(row.get_ref(3)?),
        ))
    }) {
        Ok(rows) => rows,
        Err(_) => return Vec::new(),
    };
    let mut messages = Vec::new();
    for row in rows {
        // Same contract as the source reader: one malformed row aborts
        // the session's message list rather than silently slicing it.
        let Ok((id, created_ms, updated_ms, data)) = row else {
            return Vec::new();
        };
        let (Some(id), Some(data)) = (id, data) else {
            continue;
        };
        let data = serde_json::from_str::<Value>(&data).unwrap_or(Value::Null);
        messages.push(MessageRow {
            id,
            created_ms,
            updated_ms,
            data,
        });
    }
    messages
}

/// `part` rows grouped by `message_id`, ordered `time_created ASC, id
/// ASC` (narrowing gracefully on absent columns) — the content blocks
/// one message unfolds into.
fn openagent_parts(connection: &Connection, session_id: &str) -> HashMap<String, Vec<Value>> {
    let mut out = HashMap::new();
    let Ok(columns) = table_columns(connection, "part") else {
        return out;
    };
    if !["message_id", "session_id", "data"]
        .iter()
        .all(|column| columns.contains(*column))
    {
        return out;
    }
    let order = match (columns.contains("time_created"), columns.contains("id")) {
        (true, true) => "time_created ASC, id ASC",
        (true, false) => "time_created ASC",
        (false, true) => "id ASC",
        (false, false) => "rowid ASC",
    };
    let query = format!("SELECT message_id, data FROM part WHERE session_id=?1 ORDER BY {order}");
    let mut statement = match connection.prepare(&query) {
        Ok(statement) => statement,
        Err(_) => return out,
    };
    let rows = match statement.query_map([session_id], |row| {
        Ok((
            sqlite_value_text(row.get_ref(0)?),
            sqlite_value_text(row.get_ref(1)?),
        ))
    }) {
        Ok(rows) => rows,
        Err(_) => return out,
    };
    for row in rows {
        let Ok((message_id, data)) = row else {
            return out;
        };
        let (Some(message_id), Some(data)) = (message_id, data) else {
            continue;
        };
        let value = serde_json::from_str::<Value>(&data).unwrap_or_else(|_| Value::String(data));
        out.entry(message_id).or_insert_with(Vec::new).push(value);
    }
    out
}

fn decode_openagent_session(
    connection: &Connection,
    _db: &Path,
    session_id: &str,
) -> Result<Option<Decoded>> {
    if !openagent_schema(connection) {
        return Ok(None);
    }
    let Some(meta) = openagent_session_meta(connection, session_id) else {
        return Ok(None);
    };
    let messages = openagent_messages(connection, &meta.id);
    let mut parts = openagent_parts(connection, &meta.id);
    let mut conv = Conversation::new(meta.id.clone());
    conv.agent = Some("kilocode".to_string());
    conv.title = meta.title;
    conv.cwd = meta.directory;
    conv.created_at = meta.created_at;
    conv.updated_at = meta.updated_at;

    let mut records = Vec::new();
    let mut record_lines = Vec::new();
    let mut skipped = Vec::new();
    let line_count = messages.len();

    for (index, message) in messages.into_iter().enumerate() {
        let data = &message.data;
        let role = extract_role(data);
        let ts = json_time(data, "created")
            .or_else(|| message.created_ms.and_then(epoch_number_to_rfc3339))
            .or_else(|| json_time(data, "completed"))
            .or_else(|| message.updated_ms.and_then(epoch_number_to_rfc3339));
        let model = message_model(data).or_else(|| session_model_label(meta.model.as_deref()));
        let parent = find_string(data, &["parentID", "parentId", "parent_id"]);
        let blocks = parts.remove(&message.id).unwrap_or_default();
        let record_id = |block: usize, total: usize| {
            if total == 1 {
                Some(message.id.clone())
            } else {
                Some(format!("{}:{index}:{block}", message.id))
            }
        };
        let before = records.len();
        if blocks.is_empty() {
            // A message without parts still decodes its own blob —
            // e.g. a bare `{"role":"tool_result","result":…}` row.
            project_envelope(
                data,
                &role,
                model.as_deref(),
                &mut records,
                &mut record_lines,
                &mut skipped,
                index,
                &ts,
                Some(message.id.clone()),
                parent.clone(),
            );
        } else {
            let total = blocks.len();
            for (block_index, part) in blocks.iter().enumerate() {
                project_part(
                    part,
                    &role,
                    model.as_deref(),
                    &mut records,
                    &mut record_lines,
                    &mut skipped,
                    index,
                    &ts,
                    record_id(block_index, total),
                    parent.clone(),
                );
            }
        }
        if before == records.len() {
            skipped.push(Skipped {
                line: index,
                native_type: Some(format!("message:{role}")),
                reason: "projection".to_string(),
            });
        }
        // Token counters ride on the message blob (`tokens`/`usage`/…),
        // independent of which parts projected.
        if let Some(usage) = usage_container(data) {
            push_record(
                &mut records,
                &mut record_lines,
                index,
                ts.clone(),
                Body::Meta {
                    subtype: "usage".to_string(),
                    data: usage.clone(),
                },
            );
        }
    }

    // Session-level store metadata the conversation fields cannot hold.
    let mut session_meta = serde_json::Map::new();
    if let Some(parent) = meta.parent_id {
        session_meta.insert("parentId".to_string(), Value::String(parent));
    }
    if let Some(path) = meta.path.filter(|value| !value.trim().is_empty()) {
        session_meta.insert("projectPath".to_string(), Value::String(path));
    }
    if let Some(agent) = meta.agent.filter(|value| !value.trim().is_empty()) {
        session_meta.insert("nativeAgent".to_string(), Value::String(agent));
    }
    if let Some(model) = meta.model.filter(|value| !value.trim().is_empty()) {
        session_meta.insert("model".to_string(), Value::String(model));
    }
    if meta.archived {
        session_meta.insert("archived".to_string(), Value::Bool(true));
    }
    if !session_meta.is_empty() {
        push_record(
            &mut records,
            &mut record_lines,
            line_count,
            conv.updated_at.clone(),
            Body::Meta {
                subtype: "session".to_string(),
                data: Value::Object(session_meta),
            },
        );
    }
    if let Some(usage) = meta.usage {
        push_record(
            &mut records,
            &mut record_lines,
            line_count,
            conv.updated_at.clone(),
            Body::Meta {
                subtype: "usage".to_string(),
                data: usage,
            },
        );
    }
    conv.updated_at = records
        .iter()
        .rev()
        .find_map(|record| record.ts.clone())
        .or(conv.updated_at)
        .or(conv.created_at.clone());
    finish(&mut conv, records);
    Ok(Some(Decoded {
        conversation: conv,
        record_lines,
        line_count,
        skipped,
    }))
}

// ---------------------------------------------------------------------------
// Generic sqlite probe fallback.
// ---------------------------------------------------------------------------

/// LicoUp `sqlite_table_may_hold_history` for `kilo-code`: `sqlite*` /
/// `fts` / `embedding` / `account` names never hold history; the default
/// arm keeps `chat|conversation|session|history` names and `ItemTable`.
fn table_may_hold_history(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("sqlite") || lower.contains("fts") || lower.contains("embedding") {
        return false;
    }
    if lower.contains("account") || lower.contains("control_account") {
        return false;
    }
    lower.contains("chat")
        || lower.contains("conversation")
        || lower.contains("session")
        || lower.contains("history")
        || lower == "itemtable"
}

/// LicoUp `sqlite_row_may_hold_history` for `kilo-code`.
fn row_may_hold_history(table: &str, row_text: &str) -> bool {
    if table.to_ascii_lowercase().contains("account") {
        return false;
    }
    let lower = row_text.to_ascii_lowercase();
    lower.contains("assistant")
        || lower.contains("user")
        || lower.contains("prompt")
        || lower.contains("message")
        || lower.contains("conversation")
        || lower.contains("chat")
}

fn candidate_tables(connection: &Connection) -> Vec<String> {
    let mut statement = match connection
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
    {
        Ok(statement) => statement,
        Err(_) => return Vec::new(),
    };
    let rows = match statement.query_map([], |row| row.get::<_, String>(0)) {
        Ok(rows) => rows,
        Err(_) => return Vec::new(),
    };
    rows.flatten()
        .filter(|name| table_may_hold_history(name))
        .collect()
}

/// One scanned row: ordinal position in the table scan plus its
/// non-empty `column → text` fields.
struct RowData {
    index: usize,
    fields: Vec<(String, String)>,
}

/// Group a probed table's history rows by session identity: the row's
/// own key column when it has one, else `table:row-index` — the same
/// derivation LicoUp applies.
fn session_groups(connection: &Connection, table: &str) -> Vec<(String, Vec<RowData>)> {
    let query = format!("SELECT * FROM \"{}\"", table.replace('"', "\"\""));
    let mut statement = match connection.prepare(&query) {
        Ok(statement) => statement,
        Err(_) => return Vec::new(),
    };
    let column_names = statement
        .column_names()
        .iter()
        .map(|name| name.to_string())
        .collect::<Vec<_>>();
    let mut rows = match statement.query([]) {
        Ok(rows) => rows,
        Err(_) => return Vec::new(),
    };
    let mut groups: Vec<(String, Vec<RowData>)> = Vec::new();
    let mut index = 0usize;
    loop {
        let row = match rows.next() {
            Ok(Some(row)) => row,
            Ok(None) => break,
            Err(_) => return Vec::new(),
        };
        let row_index = index;
        index = index.saturating_add(1);
        let mut fields = Vec::<(String, String)>::new();
        for (column, name) in column_names.iter().enumerate() {
            let Ok(reference) = row.get_ref(column) else {
                return Vec::new();
            };
            let Some(value) = sqlite_value_text(reference) else {
                continue;
            };
            if value.trim().is_empty() {
                continue;
            }
            fields.push((name.clone(), value));
        }
        let row_text = fields
            .iter()
            .map(|(name, value)| format!("{name}: {value}"))
            .collect::<Vec<_>>()
            .join("\n");
        let key = row_key(&fields);
        if !row_may_hold_history(table, &row_text) {
            continue;
        }
        let group_id = key.unwrap_or_else(|| format!("{table}:{row_index}"));
        let data = RowData {
            index: row_index,
            fields,
        };
        if let Some((_, group)) = groups.iter_mut().find(|(id, _)| *id == group_id) {
            group.push(data);
        } else {
            groups.push((group_id, vec![data]));
        }
    }
    groups
}

/// LicoUp `sqlite_row_key`: the first present of the preferred identity
/// column names, case-insensitive.
fn row_key(fields: &[(String, String)]) -> Option<String> {
    for preferred in [
        "key",
        "id",
        "sessionId",
        "session_id",
        "sessionKey",
        "session_key",
        "conversationId",
    ] {
        if let Some((_, value)) = fields
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(preferred))
        {
            if !value.trim().is_empty() {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

fn generic_session_refs(connection: &Connection, db: &Path) -> Vec<SessionRef> {
    let mut out = Vec::new();
    for table in candidate_tables(connection) {
        for (group, _) in session_groups(connection, &table) {
            out.push(SessionRef {
                id: group.clone(),
                path: db.to_path_buf(),
                key: Some(format!("{table}:{group}")),
            });
        }
    }
    out
}

fn decode_generic_session(
    connection: &Connection,
    db: &Path,
    key: &str,
    table: &str,
    group: &str,
) -> Result<Decoded> {
    if !table_may_hold_history(table) {
        return Err(Error::Usage(format!(
            "kilocode session `{key}` names a non-history table `{table}`"
        )));
    }
    let groups = session_groups(connection, table);
    let Some((_, rows)) = groups.into_iter().find(|(id, _)| id.as_str() == group) else {
        return Err(Error::Usage(format!(
            "kilocode session `{key}` not found in `{}`",
            db.display()
        )));
    };
    let mut conv = Conversation::new(group.to_string());
    conv.agent = Some("kilocode".to_string());
    let mut records = Vec::new();
    let mut record_lines = Vec::new();
    let mut skipped = Vec::new();
    let line_count = rows.len();
    for row in &rows {
        let before = records.len();
        for (_, value) in &row.fields {
            let Some(json) = serde_json::from_str::<Value>(value)
                .ok()
                .or_else(|| extract_json_from_text(value))
            else {
                continue;
            };
            collect_records_from_value(
                &json,
                &mut records,
                &mut record_lines,
                &mut skipped,
                row.index,
            );
        }
        if before == records.len() {
            // The row carries no decodable JSON payload — surface it as a
            // store-metadata record the way the source's "record" card does.
            let fields = row
                .fields
                .iter()
                .map(|(name, value)| (name.clone(), Value::String(value.clone())))
                .collect::<serde_json::Map<_, _>>();
            push_record(
                &mut records,
                &mut record_lines,
                row.index,
                None,
                Body::Meta {
                    subtype: "row".to_string(),
                    data: Value::Object(fields),
                },
            );
        }
    }
    finish(&mut conv, records);
    Ok(Decoded {
        conversation: conv,
        record_lines,
        line_count,
        skipped,
    })
}

/// Recursively lift message-shaped values out of a JSON field — the
/// source's `collect_messages_from_value` container walk.
fn collect_records_from_value(
    value: &Value,
    records: &mut Vec<Record>,
    record_lines: &mut Vec<usize>,
    skipped: &mut Vec<Skipped>,
    line: usize,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                if value_is_container(item) {
                    collect_records_from_value(item, records, record_lines, skipped, line);
                } else {
                    let before = records.len();
                    project_json_envelope(item, records, record_lines, skipped, line);
                    if before == records.len() {
                        collect_records_from_value(item, records, record_lines, skipped, line);
                    }
                }
            }
        }
        Value::Object(object) => {
            let before = records.len();
            for key in [
                "messages",
                "conversation",
                "conversations",
                "transcript",
                "turns",
                "items",
                "entries",
                "sessions",
                "chats",
                "chatSessions",
            ] {
                if let Some(child) = object.get(key) {
                    collect_records_from_value(child, records, record_lines, skipped, line);
                }
            }
            if records.len() == before {
                project_json_envelope(value, records, record_lines, skipped, line);
            }
        }
        _ => {}
    }
}

fn value_is_container(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let has_container = [
        "messages",
        "conversation",
        "conversations",
        "transcript",
        "turns",
        "entries",
        "sessions",
        "chats",
        "chatSessions",
    ]
    .iter()
    .any(|key| object.contains_key(*key));
    let has_direct_text = [
        "text", "content", "prompt", "response", "answer", "summary", "value",
    ]
    .iter()
    .any(|key| object.contains_key(*key));
    has_container && !has_direct_text
}

// ---------------------------------------------------------------------------
// Message projection: native blobs → canonical records.
// ---------------------------------------------------------------------------

/// One `part` blob (or a bare message blob) → canonical records, using
/// the source's semantic vocabulary: `text`-ish blocks keep the outer
/// role, `reasoning`/`thinking` → reasoning, tool semantics → tool
/// calls/results, `error` → system, `file`/`image`/`attachment`/
/// `metadata` → meta, a usage-bearing part → usage meta; anything else
/// is reported skipped as a projection drop.
#[allow(clippy::too_many_arguments)]
fn project_part(
    part: &Value,
    outer_role: &str,
    model: Option<&str>,
    records: &mut Vec<Record>,
    record_lines: &mut Vec<usize>,
    skipped: &mut Vec<Skipped>,
    line: usize,
    ts: &Option<String>,
    id: Option<String>,
    parent: Option<String>,
) {
    let semantic = native_semantic(part);
    match semantic_kind(&semantic) {
        PartKind::Text => {
            let Some(text) = extract_text(part) else {
                skipped.push(Skipped {
                    line,
                    native_type: Some(semantic),
                    reason: "projection".to_string(),
                });
                return;
            };
            let body = if matches!(outer_role, "user" | "human") {
                Body::User {
                    text,
                    attachments: Vec::new(),
                }
            } else {
                Body::Assistant {
                    text,
                    model: model.map(str::to_string),
                }
            };
            push_full(records, record_lines, line, ts.clone(), body, id, parent);
        }
        PartKind::Reasoning => {
            let Some(text) = find_string(part, &["text", "thinking", "reasoning"])
                .or_else(|| extract_text(part))
            else {
                skipped.push(Skipped {
                    line,
                    native_type: Some(semantic),
                    reason: "projection".to_string(),
                });
                return;
            };
            push_full(
                records,
                record_lines,
                line,
                ts.clone(),
                Body::Reasoning { text },
                id,
                parent,
            );
        }
        PartKind::ToolCall => {
            let call_id = part_call_id(part).or_else(|| id.clone());
            let name = find_string(
                part,
                &[
                    "tool",
                    "name",
                    "toolName",
                    "tool_name",
                    "functionName",
                    "function_name",
                ],
            )
            .unwrap_or_else(|| "tool".to_string());
            let state = part.get("state").cloned().unwrap_or(Value::Null);
            let args = state
                .get("input")
                .cloned()
                .or_else(|| part.get("input").cloned())
                .or_else(|| part.get("args").cloned())
                .or_else(|| part.get("arguments").cloned())
                .unwrap_or_else(|| serde_json::json!({}));
            push_full(
                records,
                record_lines,
                line,
                ts.clone(),
                Body::ToolCall {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    args,
                },
                id.clone(),
                parent.clone(),
            );
            // OpenCode tool parts carry the result in `state` — a
            // completed/errored invocation is also the tool's result.
            let status = state.get("status").and_then(Value::as_str).unwrap_or("");
            let output = state
                .get("output")
                .cloned()
                .or_else(|| state.get("error").cloned());
            if let Some(content) = output {
                push_full(
                    records,
                    record_lines,
                    line,
                    ts.clone(),
                    Body::ToolResult {
                        call_id,
                        name: Some(name),
                        content,
                        is_error: status == "error",
                    },
                    id,
                    parent,
                );
            }
        }
        PartKind::ToolResult => {
            let content = part
                .get("result")
                .or_else(|| part.get("output"))
                .or_else(|| part.get("content"))
                .cloned()
                .unwrap_or_else(|| part.clone());
            push_full(
                records,
                record_lines,
                line,
                ts.clone(),
                Body::ToolResult {
                    call_id: part_call_id(part).or_else(|| id.clone()),
                    name: find_string(
                        part,
                        &["tool", "name", "toolName", "tool_name", "functionName"],
                    ),
                    content,
                    is_error: part
                        .get("isError")
                        .or_else(|| part.get("is_error"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                },
                id,
                parent,
            );
        }
        PartKind::Metadata => {
            push_full(
                records,
                record_lines,
                line,
                ts.clone(),
                Body::Meta {
                    subtype: semantic,
                    data: part.clone(),
                },
                id,
                parent,
            );
        }
        PartKind::Error => {
            let text = extract_text(part).unwrap_or_else(|| {
                find_string(part, &["error", "message", "title"])
                    .unwrap_or_else(|| "native agent reported an error".to_string())
            });
            push_full(
                records,
                record_lines,
                line,
                ts.clone(),
                Body::System {
                    subtype: "error".to_string(),
                    text,
                },
                id,
                parent,
            );
        }
        PartKind::Event => {
            // Lifecycle parts carrying token counters still surface them.
            if let Some(usage) = usage_container(part) {
                push_record(
                    records,
                    record_lines,
                    line,
                    ts.clone(),
                    Body::Meta {
                        subtype: "usage".to_string(),
                        data: usage.clone(),
                    },
                );
            }
            skipped.push(Skipped {
                line,
                native_type: Some(semantic),
                reason: "projection".to_string(),
            });
        }
    }
}

/// Project a bare message/envelope JSON value (no `part` rows): role
/// and text extraction decide the record; tool-result envelopes decode
/// as tool results.
#[allow(clippy::too_many_arguments)]
fn project_envelope(
    data: &Value,
    role: &str,
    model: Option<&str>,
    records: &mut Vec<Record>,
    record_lines: &mut Vec<usize>,
    skipped: &mut Vec<Skipped>,
    line: usize,
    ts: &Option<String>,
    id: Option<String>,
    parent: Option<String>,
) {
    let semantic = native_semantic(data);
    match semantic_kind(&semantic) {
        PartKind::ToolCall => project_part(
            data,
            role,
            model,
            records,
            record_lines,
            skipped,
            line,
            ts,
            id,
            parent,
        ),
        PartKind::ToolResult => project_part(
            data,
            role,
            model,
            records,
            record_lines,
            skipped,
            line,
            ts,
            id,
            parent,
        ),
        _ => {
            let Some(text) = extract_text(data) else {
                return;
            };
            let body = match role {
                "user" | "human" => Body::User {
                    text,
                    attachments: Vec::new(),
                },
                "assistant" | "agent" => Body::Assistant {
                    text,
                    model: model.map(str::to_string),
                },
                other => Body::System {
                    subtype: other.to_string(),
                    text,
                },
            };
            push_full(records, record_lines, line, ts.clone(), body, id, parent);
        }
    }
}

/// File-decoded values carry no outer role; the envelope's own semantic
/// drives the record.
fn project_json_envelope(
    value: &Value,
    records: &mut Vec<Record>,
    record_lines: &mut Vec<usize>,
    skipped: &mut Vec<Skipped>,
    line: usize,
) {
    let role = extract_role(value);
    project_envelope(
        value,
        &role,
        None,
        records,
        record_lines,
        skipped,
        line,
        &extract_timestamp(value),
        find_string(value, &["id", "messageId", "message_id"]),
        None,
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PartKind {
    Text,
    Reasoning,
    ToolCall,
    ToolResult,
    Metadata,
    Error,
    Event,
}

/// LicoUp `history_message_kind_from_semantic`, folded onto canonical
/// kinds: text-ish semantics stay text, `reasoning|analysis|thinking`
/// → reasoning, error semantics → error, attachment/metadata → meta,
/// `tool|function` + result markers → tool result, `tool|function` →
/// tool call, everything else → event (skipped).
fn semantic_kind(semantic: &str) -> PartKind {
    let semantic = normalize_semantic(semantic);
    if semantic.is_empty()
        || matches!(
            semantic.as_str(),
            "text"
                | "input-text"
                | "output-text"
                | "markdown"
                | "message"
                | "summary-text"
                | "user"
                | "human"
                | "assistant"
                | "agent"
                | "model"
                | "ai"
                | "planner-response"
                | "generic"
        )
        || semantic.ends_with("-user-message")
        || semantic.ends_with("-assistant-message")
        || matches!(semantic.as_str(), "user-message" | "assistant-message")
    {
        return PartKind::Text;
    }
    if semantic.contains("reasoning")
        || semantic.contains("analysis")
        || semantic.contains("thinking")
    {
        return PartKind::Reasoning;
    }
    if semantic.contains("error")
        || semantic.contains("failure")
        || semantic.contains("failed")
        || semantic.contains("exception")
    {
        return PartKind::Error;
    }
    if semantic == "metadata"
        || matches!(
            semantic.as_str(),
            "image" | "image-url" | "document" | "attachment" | "input-json-delta" | "file"
        )
    {
        return PartKind::Metadata;
    }
    if matches!(
        semantic.as_str(),
        "tool-result"
            | "tool-output"
            | "function-result"
            | "function-output"
            | "function-call-output"
    ) || ((semantic.contains("tool") || semantic.contains("function"))
        && [
            "result",
            "output",
            "complete",
            "completed",
            "response",
            "end",
        ]
        .iter()
        .any(|marker| semantic.contains(marker)))
    {
        return PartKind::ToolResult;
    }
    if matches!(
        semantic.as_str(),
        "tool"
            | "tool-call"
            | "tool-use"
            | "function"
            | "function-call"
            | "run-command"
            | "view-file"
            | "list-directory"
            | "grep-search"
            | "read-url-content"
            | "generate-image"
            | "code-action"
    ) || semantic.contains("tool")
        || semantic.contains("function")
    {
        return PartKind::ToolCall;
    }
    PartKind::Event
}

/// LicoUp `normalize_history_message_semantic` — lowercase alnum runs
/// joined by `-`.
fn normalize_semantic(value: &str) -> String {
    let mut normalized = String::new();
    let mut separator = false;
    for character in value.trim().chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !normalized.is_empty() {
                normalized.push('-');
            }
            separator = false;
            normalized.push(character.to_ascii_lowercase());
        } else {
            separator = true;
        }
    }
    normalized
}

/// A block's self-declared type — `type`/`kind`/`role`/`eventType`/
/// `event_type`, first present.
fn native_semantic(value: &Value) -> String {
    value
        .as_object()
        .and_then(|object| {
            ["type", "kind", "role", "eventType", "event_type"]
                .iter()
                .find_map(|key| object.get(*key).and_then(Value::as_str))
        })
        .unwrap_or_default()
        .to_string()
}

fn part_call_id(part: &Value) -> Option<String> {
    find_string(
        part,
        &["callID", "callId", "call_id", "toolCallId", "tool_call_id"],
    )
}

/// LicoUp `extract_role`: user/human → `user`, assistant/agent/model/ai
/// → `agent`, tool → `tool`, else the raw role (default `system`).
fn extract_role(value: &Value) -> String {
    let role = find_string(value, &["role", "author", "speaker", "type", "source"])
        .unwrap_or_else(|| "system".to_string())
        .to_ascii_lowercase();
    if role.contains("user") || role.contains("human") || role == "1" {
        "user".to_string()
    } else if role.contains("assistant")
        || role.contains("agent")
        || role.contains("model")
        || role.contains("ai")
        || role == "0"
        || role == "2"
    {
        "agent".to_string()
    } else if role.contains("tool") {
        "tool".to_string()
    } else {
        role
    }
}

/// LicoUp `find_string`: key lookup descending through `message` nests;
/// strings and integers both resolve.
fn find_string(value: &Value, keys: &[&str]) -> Option<String> {
    let mut candidate = value;
    loop {
        let object = candidate.as_object()?;
        for key in keys {
            if let Some(text) = object.get(*key).and_then(Value::as_str) {
                return Some(text.to_string());
            }
            if let Some(number) = object.get(*key).and_then(Value::as_i64) {
                return Some(number.to_string());
            }
        }
        candidate = object.get("message")?;
    }
}

fn extract_timestamp(value: &Value) -> Option<String> {
    find_string(
        value,
        &[
            "createdAt",
            "updatedAt",
            "timestamp",
            "date",
            "created_at",
            "updated_at",
        ],
    )
    .or_else(|| json_time(value, "created"))
    .or_else(|| json_time(value, "completed"))
}

/// `time.created`/`time.completed` epoch in a message blob.
fn json_time(value: &Value, key: &str) -> Option<String> {
    value
        .get("time")
        .and_then(|time| time.get(key))
        .and_then(epoch_value_to_rfc3339)
}

fn message_model(value: &Value) -> Option<String> {
    find_string(value, &["modelID", "modelId", "model_id", "modelName"])
        .or_else(|| {
            value.get("model").and_then(|model| match model {
                Value::String(text) => Some(text.clone()),
                Value::Object(_) => find_string(model, &["modelID", "modelId", "id", "name"]),
                _ => None,
            })
        })
        .filter(|model| !model.trim().is_empty())
}

/// A `model` session column may be a bare id or a JSON
/// `{providerID, modelID}` object.
fn session_model_label(raw: Option<&str>) -> Option<String> {
    let raw = raw?.trim().trim_matches('"');
    if raw.starts_with('{') {
        return serde_json::from_str::<Value>(raw)
            .ok()
            .and_then(|value| find_string(&value, &["modelID", "modelId", "id", "name"]));
    }
    (!raw.is_empty()).then(|| raw.to_string())
}

/// The first usage container a blob carries — the keys the source's
/// usage reader probes on message `data`.
fn usage_container(value: &Value) -> Option<&Value> {
    for key in [
        "usage",
        "tokens",
        "tokenUsage",
        "token_usage",
        "usageMetadata",
        "usage_metadata",
        "responseUsage",
        "response_usage",
        "tokenCount",
        "gen_ai.usage",
    ] {
        if let Some(usage) = value.get(key).filter(|usage| usage.is_object()) {
            return Some(usage);
        }
    }
    None
}

/// LicoUp `openagent_usage_from_columns`: session token columns → the
/// normalized counter object (`source: openagent-sqlite`).
fn session_usage(
    input: Option<i64>,
    output: Option<i64>,
    reasoning: Option<i64>,
    cache_read: Option<i64>,
    cache_write: Option<i64>,
) -> Option<Value> {
    let cached_input_tokens = cache_read.unwrap_or(0).max(0);
    let prompt_tokens = [input, cache_read, cache_write]
        .into_iter()
        .flatten()
        .filter(|value| *value > 0)
        .sum::<i64>();
    let completion_tokens = [output, reasoning]
        .into_iter()
        .flatten()
        .filter(|value| *value > 0)
        .sum::<i64>();
    let total_tokens = prompt_tokens + completion_tokens;
    if total_tokens <= 0 {
        return None;
    }
    Some(serde_json::json!({
        "scope": "session",
        "promptTokens": prompt_tokens,
        "cachedInputTokens": cached_input_tokens,
        "completionTokens": completion_tokens,
        "totalTokens": total_tokens,
        "source": "openagent-sqlite"
    }))
}

/// LicoUp `extract_text`: depth-first walk over `text|content|message|
/// messages|prompt|response|answer|summary|value|parts|items|turns`,
/// skipping tool/metadata-typed objects and joining survivors on `\n`.
fn extract_text(value: &Value) -> Option<String> {
    let mut pending = vec![value];
    let mut parts = Vec::new();
    while let Some(candidate) = pending.pop() {
        match candidate {
            Value::String(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(decoded) = parse_embedded_json(trimmed) {
                    if let Some(text) = extract_text_shallow(&decoded) {
                        if !text.trim().is_empty() {
                            parts.push(text);
                            continue;
                        }
                    }
                }
                parts.push(trimmed.to_string());
            }
            Value::Array(items) => pending.extend(items.iter().rev()),
            Value::Object(object) => {
                if object_is_tool_or_metadata(object) {
                    continue;
                }
                let child = if object_is_text(object) {
                    object
                        .get("text")
                        .filter(|value| value_may_have_text(value))
                        .or_else(|| object.get("content"))
                } else {
                    [
                        "text", "content", "message", "messages", "prompt", "response", "answer",
                        "summary", "value", "parts", "items", "turns",
                    ]
                    .iter()
                    .find_map(|key| object.get(*key).filter(|value| value_may_have_text(value)))
                };
                if let Some(child) = child {
                    pending.push(child);
                }
            }
            _ => {}
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// Same walk without embedded-JSON re-decode — the recursive step of
/// [`extract_text`].
fn extract_text_shallow(value: &Value) -> Option<String> {
    let mut pending = vec![value];
    let mut parts = Vec::new();
    while let Some(candidate) = pending.pop() {
        match candidate {
            Value::String(text) => {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
            Value::Array(items) => pending.extend(items.iter().rev()),
            Value::Object(object) => {
                if object_is_tool_or_metadata(object) {
                    continue;
                }
                let child = [
                    "text", "content", "message", "messages", "prompt", "response", "answer",
                    "summary", "value", "parts", "items", "turns",
                ]
                .iter()
                .find_map(|key| object.get(*key).filter(|value| value_may_have_text(value)));
                if let Some(child) = child {
                    pending.push(child);
                }
            }
            _ => {}
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn value_may_have_text(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(object) => !object.is_empty(),
        _ => false,
    }
}

fn object_is_tool_or_metadata(object: &serde_json::Map<String, Value>) -> bool {
    let kind = object
        .get("type")
        .or_else(|| object.get("kind"))
        .or_else(|| object.get("role"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        kind.as_str(),
        "tool_result"
            | "tool-use"
            | "tool_use"
            | "tool"
            | "function_call"
            | "function_call_output"
            | "input_json_delta"
            | "thinking"
            | "redacted_thinking"
            | "image"
            | "image_url"
            | "document"
            | "attachment"
            | "metadata"
            | "system"
    )
}

fn object_is_text(object: &serde_json::Map<String, Value>) -> bool {
    let kind = object
        .get("type")
        .or_else(|| object.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        kind.as_str(),
        "text" | "input_text" | "output_text" | "markdown"
    )
}

/// A text field may itself be serialized JSON carrying deeper text.
fn parse_embedded_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    let looks_like_json = (trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']'));
    if !looks_like_json {
        return None;
    }
    let value = serde_json::from_str::<Value>(trimmed).ok()?;
    embedded_may_hold_text(&value).then_some(value)
}

fn embedded_may_hold_text(value: &Value) -> bool {
    let mut pending = vec![value];
    while let Some(candidate) = pending.pop() {
        match candidate {
            Value::Array(items) => pending.extend(items),
            Value::Object(object) => {
                if object.keys().any(|key| {
                    matches!(
                        key.as_str(),
                        "text"
                            | "content"
                            | "message"
                            | "messages"
                            | "prompt"
                            | "response"
                            | "answer"
                            | "summary"
                            | "value"
                            | "parts"
                            | "items"
                            | "turns"
                    )
                }) {
                    return true;
                }
                pending.extend(object.values());
            }
            _ => {}
        }
    }
    false
}

/// LicoUp `extract_json_from_text`: the first `{`/`[` slice that parses.
fn extract_json_from_text(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    for start in ['{', '['] {
        if let Some(offset) = trimmed.find(start) {
            if let Ok(value) = serde_json::from_str::<Value>(&trimmed[offset..]) {
                return Some(value);
            }
        }
    }
    None
}

/// LicoUp `epoch_number_to_rfc3339`: magnitude selects s/ms/µs → RFC 3339.
fn epoch_number_to_rfc3339(value: i64) -> Option<String> {
    if value <= 0 {
        return None;
    }
    let absolute = (value as i128).abs();
    let millis = if absolute >= 1_000_000_000_000_000 {
        value / 1_000
    } else if absolute >= 10_000_000_000 {
        value
    } else {
        value.checked_mul(1_000)?
    };
    let millis = u64::try_from(millis).ok()?;
    Some(aco::util::epoch_ms_rfc3339(millis))
}

fn epoch_value_to_rfc3339(value: &Value) -> Option<String> {
    value
        .as_i64()
        .or_else(|| {
            value
                .as_str()
                .and_then(|text| text.trim().parse::<i64>().ok())
        })
        .and_then(epoch_number_to_rfc3339)
}

/// LicoUp `bounded_project_workspace`, minus the platform unbounded-root
/// policy: a recorded directory must trim to an absolute path.
fn bounded_workspace(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || !Path::new(trimmed).is_absolute() {
        return None;
    }
    Some(trimmed.to_string())
}

fn push_record(
    records: &mut Vec<Record>,
    lines: &mut Vec<usize>,
    line: usize,
    ts: Option<String>,
    body: Body,
) {
    push_full(records, lines, line, ts, body, None, None);
}

fn push_full(
    records: &mut Vec<Record>,
    lines: &mut Vec<usize>,
    line: usize,
    ts: Option<String>,
    body: Body,
    id: Option<String>,
    parent: Option<String>,
) {
    records.push(Record {
        id,
        parent,
        ts,
        ordinal: 0,
        body,
    });
    lines.push(line);
}

fn finish(conv: &mut Conversation, mut records: Vec<Record>) {
    for (ordinal, record) in records.iter_mut().enumerate() {
        record.ordinal = ordinal as u64;
    }
    conv.records = records;
}

// ---------------------------------------------------------------------------
// File decoders (accepted non-sqlite artifacts).
// ---------------------------------------------------------------------------

/// `jsonl`/`ndjson`: group lines by native session id the way the source
/// does; the first group becomes the conversation, lines belonging to
/// another session are reported skipped rather than mixed in.
fn decode_jsonl(path: &Path) -> Result<Decoded> {
    let file = File::open(path).map_err(Error::io(path))?;
    let file_id = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "kilocode-session".to_string());
    let mut groups: Vec<(String, Vec<(usize, Value)>)> = Vec::new();
    let mut skipped = Vec::new();
    let mut line_count = 0;
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
        let Some(session) = native_session_id(&value).or_else(|| uuid_component(path)) else {
            skipped.push(Skipped {
                line: index,
                native_type: value
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                reason: "projection".to_string(),
            });
            continue;
        };
        if let Some((_, lines)) = groups.iter_mut().find(|(id, _)| *id == session) {
            lines.push((index, value));
        } else {
            groups.push((session, vec![(index, value)]));
        }
    }
    let mut groups = groups.into_iter();
    let Some((session_id, lines)) = groups.next() else {
        let mut conv = Conversation::new(file_id);
        conv.agent = Some("kilocode".to_string());
        return Ok(Decoded {
            conversation: conv,
            record_lines: Vec::new(),
            line_count,
            skipped,
        });
    };
    // Lines belonging to a different session are reported, never mixed
    // into the decoded conversation.
    for (other_session, other_lines) in groups {
        for (index, _) in other_lines {
            skipped.push(Skipped {
                line: index,
                native_type: Some(format!("session:{other_session}")),
                reason: "projection".to_string(),
            });
        }
    }
    let mut conv = Conversation::new(session_id);
    conv.agent = Some("kilocode".to_string());
    let mut records = Vec::new();
    let mut record_lines = Vec::new();
    for (index, value) in &lines {
        collect_records_from_value(value, &mut records, &mut record_lines, &mut skipped, *index);
    }
    conv.updated_at = records
        .iter()
        .rev()
        .find_map(|record| record.ts.clone())
        .or(conv.created_at.clone());
    finish(&mut conv, records);
    Ok(Decoded {
        conversation: conv,
        record_lines,
        line_count,
        skipped,
    })
}

/// `json`: explicit `sessions|conversations|chats|chatSessions` arrays
/// decode their first session item; a bare document decodes itself.
fn decode_json_document(path: &Path) -> Result<Decoded> {
    let file = File::open(path).map_err(Error::io(path))?;
    let value: Value =
        serde_json::from_reader(BufReader::new(file)).map_err(|e| Error::Decode {
            path: path.to_path_buf(),
            line: 0,
            message: e.to_string(),
        })?;
    let file_id = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "kilocode-session".to_string());
    let mut skipped = Vec::new();
    let mut session_value: Option<&Value> = None;
    let mut session_id = None;
    if let Some(object) = value.as_object() {
        for key in ["sessions", "conversations", "chats", "chatSessions"] {
            if let Some(Value::Array(items)) = object.get(key) {
                session_value = items.first();
                session_id = session_value.and_then(native_session_id);
                if items.len() > 1 {
                    skipped.push(Skipped {
                        line: 0,
                        native_type: Some(key.to_string()),
                        reason: "projection".to_string(),
                    });
                }
                break;
            }
        }
    }
    let target = session_value.unwrap_or(&value);
    let session_id = session_id
        .or_else(|| native_session_id(target))
        .unwrap_or(file_id);
    let mut conv = Conversation::new(session_id);
    conv.agent = Some("kilocode".to_string());
    let mut records = Vec::new();
    let mut record_lines = Vec::new();
    collect_records_from_value(target, &mut records, &mut record_lines, &mut skipped, 0);
    conv.updated_at = records
        .iter()
        .rev()
        .find_map(|record| record.ts.clone())
        .or(conv.created_at.clone());
    finish(&mut conv, records);
    Ok(Decoded {
        conversation: conv,
        record_lines,
        line_count: 1,
        skipped,
    })
}

/// `md`/`txt`/`log`: a file that reads as a conversation transcript
/// (user + response markers) becomes one `transcript` meta record.
fn decode_text(path: &Path) -> Result<Decoded> {
    let raw = std::fs::read_to_string(path).map_err(Error::io(path))?;
    let file_id = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "kilocode-session".to_string());
    let mut conv = Conversation::new(file_id);
    conv.agent = Some("kilocode".to_string());
    let mut records = Vec::new();
    let mut record_lines = Vec::new();
    let mut skipped = Vec::new();
    let line_count = raw.lines().count();
    if !raw.trim().is_empty() && looks_like_transcript(&raw) {
        push_record(
            &mut records,
            &mut record_lines,
            0,
            None,
            Body::Meta {
                subtype: "transcript".to_string(),
                data: serde_json::json!({"text": raw}),
            },
        );
    } else {
        skipped.push(Skipped {
            line: 0,
            native_type: None,
            reason: "projection".to_string(),
        });
    }
    finish(&mut conv, records);
    Ok(Decoded {
        conversation: conv,
        record_lines,
        line_count,
        skipped,
    })
}

/// LicoUp `looks_like_text_conversation`: user and response markers in
/// structured or labeled transcript text.
fn looks_like_transcript(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    let structured = raw.trim_start().starts_with('{') || raw.trim_start().starts_with('[');
    let has_user = (structured
        && (lower.contains("\"role\":\"user\"") || lower.contains("\"role\": \"user\"")))
        || lower.contains("role: user")
        || lower.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with("user:")
                || line.starts_with("human:")
                || line.starts_with("prompt:")
                || line.starts_with("question:")
        });
    let has_response = (structured
        && (lower.contains("\"role\":\"assistant\"")
            || lower.contains("\"role\": \"assistant\"")
            || lower.contains("\"role\":\"agent\"")
            || lower.contains("\"role\": \"agent\"")))
        || lower.contains("role: assistant")
        || lower.contains("role: agent")
        || lower.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with("assistant:")
                || line.starts_with("agent:")
                || line.starts_with("response:")
                || line.starts_with("answer:")
        });
    (has_user && has_response)
        || (structured && lower.contains("\"messages\"") && (has_user || has_response))
}

/// LicoUp `extract_native_session_id` + the snapshot identity's bare
/// `id`, descending through `message` nests.
fn native_session_id(value: &Value) -> Option<String> {
    find_string(
        value,
        &[
            "sessionId",
            "session_id",
            "conversationId",
            "conversation_id",
            "chatId",
            "chat_id",
            "threadId",
            "thread_id",
            "sessionKey",
            "session_key",
            "sessionID",
            "id",
        ],
    )
    .filter(|value| !value.trim().is_empty())
}

/// A directory-layout id: a 36-char UUID path component, when one names
/// the session's parent directory.
fn uuid_component(path: &Path) -> Option<String> {
    path.components().rev().skip(1).find_map(|component| {
        let value = component.as_os_str().to_str()?;
        let bytes = value.as_bytes();
        (bytes.len() == 36
            && bytes.iter().enumerate().all(|(index, byte)| {
                if matches!(index, 8 | 13 | 18 | 23) {
                    *byte == b'-'
                } else {
                    byte.is_ascii_hexdigit()
                }
            }))
        .then(|| value.to_string())
    })
}
