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
//! That schema family is shared with OpenCode and Cursor's bundled
//! agent stores; the enumeration and decoding live in
//! [`aco::generic::openagent`]. A database that lacks the
//! `session`/`message`/`part` triple falls back to the shared schema
//! probe in [`aco::generic`], which groups history-bearing rows into
//! sessions by their key column.
//!
//! The store is read-only for ACO: Kilo Code has no file session format
//! to write, so [`Codec::encode`] and [`Codec::store_path`] report
//! [`Error::Usage`] and the capability matrix stays
//! [`CapabilityMatrix::READ_ONLY`].

use std::path::{Path, PathBuf};

use aco::codec::{Codec, Decoded, Encoded, SessionRef};
use aco::generic::openagent;
use aco::model::Conversation;
use aco::store::{HistoryRoot, StoreKind};
use aco::{CapabilityMatrix, Error, Result};
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

/// Enumerate session-bearing rows in one database: the OpenAgent
/// `session` table when the full schema is present, else the shared
/// schema probe's `table/key` row groups.
fn enumerate_database(db: &Path) -> Vec<SessionRef> {
    let Some(connection) = openagent::open_read_only(db) else {
        return Vec::new();
    };
    if let Some(refs) = openagent::session_refs(&connection, db) {
        return refs;
    }
    aco::generic::sqlite_session_refs(db).unwrap_or_default()
}

/// `key` is a `session.id` under the OpenAgent schema or a `table/key`
/// selector from the generic probe — try the schema decode first.
fn decode_container_session(db: &Path, key: &str) -> Result<Decoded> {
    let connection = openagent::open_read_only(db).ok_or_else(|| Error::Decode {
        path: db.to_path_buf(),
        line: 0,
        message: "cannot open sqlite database".to_string(),
    })?;
    if let Some(decoded) = openagent::decode(&connection, db, key, "kilocode")? {
        return Ok(decoded);
    }
    aco::generic::decode_sqlite(db, Some(key), "kilocode")
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
    /// session rows, a `.json` document yields `sessions[]` items, a
    /// `.jsonl` transcript yields its declared session groups; a plain
    /// accepted file stays one session per file.
    fn sessions_in_file(&self, path: &Path) -> Result<Vec<SessionRef>> {
        let ext = extension_of(path);
        if SQLITE_EXTENSIONS.contains(&ext.as_str()) && !rejected_file_name(path) {
            return Ok(enumerate_database(path));
        }
        if ext == "json" {
            let refs = aco::generic::json_document_session_refs(path);
            if !refs.is_empty() {
                return Ok(refs);
            }
        }
        if matches!(ext.as_str(), "jsonl" | "ndjson") {
            let refs = aco::generic::jsonl_session_refs(path);
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

    /// `SqliteDb` roots enumerate one [`SessionRef`] per session-bearing
    /// row: OpenAgent `session` rows keyed by `id`, or generic-probe row
    /// groups keyed `table/key`. Other store kinds keep the file
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
                    parent: None,
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
    /// Some("table/key")` decodes a generic-probe row group; `key`
    /// selectors on `.json`/`.jsonl` files read that `sessions[]` item
    /// or session group; `key = None` falls through to
    /// [`Codec::decode`] on the path.
    fn decode_session(&self, session: &SessionRef) -> Result<Decoded> {
        match &session.key {
            Some(key) => match extension_of(&session.path).as_str() {
                "jsonl" | "ndjson" => {
                    aco::generic::decode_jsonl_session(&session.path, key, "kilocode")
                }
                "json" => {
                    aco::generic::decode_json_document_session(&session.path, key, "kilocode")
                }
                _ => decode_container_session(&session.path, key),
            },
            None => self.decode(&session.path),
        }
    }

    /// Decode one accepted artifact through the shared generic decoders:
    /// `jsonl`/`ndjson` line sessions, `json` documents, `md`/`txt`/`log`
    /// text transcripts. A database holding several sessions cannot
    /// become one canonical conversation — the caller must name a
    /// session.
    fn decode(&self, path: &Path) -> Result<Decoded> {
        match extension_of(path).as_str() {
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
            ext if ACCEPTED_EXTENSIONS.contains(&ext) => aco::generic::dispatch(path, "kilocode"),
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
