//! acop codec: the `opencode` (OpenCode) conversation store.
//!
//! The a4aa card declares the session root as `opencode-data-store` —
//! `~/.local/share/opencode`, plus the same store under the
//! `{xdg_data}`/`{appdata}` homes OpenCode's own tooling honors (the
//! `opencode-config` roots are inventory, not sessions). OpenCode's data
//! store holds no bespoke wire format LicoUp knows: session artifacts
//! land as JSONL/NDJSON event logs, single JSON documents, markdown or
//! plaintext transcripts, and SQLite databases
//! (`*.sqlite`/`*.sqlite3`/`*.db`), so every artifact decodes through the
//! shared [`acop::generic`] lane. This codec contributes the store facts
//! only: the accepted extension set with LicoUp's exclusions, container
//! enumeration as one [`SessionRef`] per stored session key, and the
//! `<session-id>.jsonl` import path under the home store root.

use std::path::{Path, PathBuf};

use acop::codec::{Capability, CapabilityMatrix, Codec, Decoded, Encoded, SessionRef};
use acop::model::Conversation;
use acop::store::HistoryRoot;
use acop::{Error, Result};
use serde_json::Value;

/// Extensions LicoUp's `opencode` history adapter accepts as
/// session-shaped artifacts under the store root.
const ACCEPTED_EXTENSIONS: &[&str] = &[
    "jsonl", "ndjson", "json", "md", "txt", "log", "sqlite", "sqlite3", "db",
];

/// Extensions that open as SQLite containers — one database can hold
/// many sessions keyed by row.
const SQLITE_EXTENSIONS: &[&str] = &["sqlite", "sqlite3", "db"];

/// The generic wire shape's capability table: user, assistant,
/// reasoning, tool-call and tool-result are native; `system` rides an
/// adapted `{"type":"system"}` event line and `meta{usage}` rides an
/// assistant message's `usage` field — other meta subtypes are reported
/// dropped by the encoder's projection.
const CAPABILITIES: CapabilityMatrix = CapabilityMatrix::new(
    true,
    [
        Capability::Native,  // user
        Capability::Native,  // assistant
        Capability::Native,  // reasoning
        Capability::Native,  // tool-call
        Capability::Native,  // tool-result
        Capability::Adapted, // system — {"type":"system"} event line
        Capability::Adapted, // meta — usage rides message.usage; other subtypes drop
    ],
);

/// The codec instance acop's registry hands out.
pub struct OpenCodeCodec;

/// One codec instance for the registry.
pub fn codec() -> &'static dyn Codec {
    &OpenCodeCodec
}

impl Codec for OpenCodeCodec {
    fn id(&self) -> &'static str {
        "opencode"
    }

    fn history(&self) -> Vec<HistoryRoot> {
        a4aa_opencode::card().history.to_vec()
    }

    /// The card declares no transcript-usage surface (`transcript:
    /// None`); usage counters a persisted line carries still decode as
    /// `meta{usage}` records.
    fn transcript_usage(&self) -> Option<acop::TranscriptUsage> {
        a4aa_opencode::card().usage.transcript
    }

    fn session_kinds(&self) -> &'static [&'static str] {
        &["opencode-data-store"]
    }

    fn capabilities(&self) -> &'static CapabilityMatrix {
        &CAPABILITIES
    }

    /// The card's `{xdg_data}`/`{xdg_config}`/`{appdata}` templates
    /// resolve through the host environment the way OpenCode's own
    /// tooling resolves them; `{home}` roots keep the default
    /// expansion.
    fn expand_root(&self, root: &HistoryRoot, home: &Path) -> PathBuf {
        if let Some(rest) = root.path.strip_prefix("{xdg_data}/") {
            return env_dir("XDG_DATA_HOME", home.join(".local/share")).join(rest);
        }
        if let Some(rest) = root.path.strip_prefix("{xdg_config}/") {
            return env_dir("XDG_CONFIG_HOME", home.join(".config")).join(rest);
        }
        if let Some(rest) = root.path.strip_prefix("{appdata}/") {
            return env_dir("APPDATA", home.join("AppData/Roaming")).join(rest);
        }
        acop::util::expand_home(root.path, home)
    }

    /// A UUID-bearing directory component names the session where the
    /// layout carries one; otherwise the file stem — the generic
    /// identity rule.
    fn session_id(&self, path: &Path) -> String {
        acop::generic::session_id(path)
    }

    /// LicoUp's `accepts_file` extension set for the `opencode`
    /// adapter, minus its global exclusions (`*.backup`,
    /// `codebase-external.sqlite`).
    fn select_session_files(&self, files: Vec<PathBuf>) -> Vec<PathBuf> {
        files
            .into_iter()
            .filter(|p| {
                !rejected_file_name(p) && ACCEPTED_EXTENSIONS.contains(&extension_of(p).as_str())
            })
            .collect()
    }

    /// The store's own archive flag is `session_v2.time_archived` — the
    /// same column opencode's list hides on and Probator's conceal sweep
    /// has always flipped. Only database-row sessions carry it; plain
    /// transcript files report no marker.
    fn archive_session(&self, session: &SessionRef, _home: &Path, dry_run: bool) -> Result<bool> {
        let Some(key) = session.key.as_deref() else {
            return Ok(false);
        };
        let table = key.split('/').next().unwrap_or_default();
        if !matches!(table, "session_v2" | "session") {
            return Ok(false);
        }
        if dry_run {
            return Ok(true);
        }
        let conn = rusqlite::Connection::open(&session.path)
            .map_err(|e| Error::io(&session.path)(std::io::Error::other(e.to_string())))?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        conn.execute(
            &format!(
                "UPDATE {table} SET time_archived = ?1 WHERE id = ?2 AND time_archived IS NULL"
            ),
            rusqlite::params![now_ms, session.id],
        )
        .map_err(|e| Error::io(&session.path)(std::io::Error::other(e.to_string())))?;
        Ok(true)
    }

    /// A database file enumerates one [`SessionRef`] per stored
    /// session: `session` rows under the OpenAgent schema when present,
    /// else the shared probe's `table/key` row groups. A `.json`
    /// document yields `sessions[]` items and a `.jsonl` transcript its
    /// declared session groups. Every other accepted artifact stays one
    /// session per file.
    fn sessions_in_file(&self, path: &Path) -> Result<Vec<SessionRef>> {
        let ext = extension_of(path);
        if SQLITE_EXTENSIONS.contains(&ext.as_str()) && !rejected_file_name(path) {
            return Ok(database_session_refs(path));
        }
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

    /// `key = Some(..)` reads that container session — an OpenAgent
    /// `session.id` first, then a probe selector, or a `sessions[]`
    /// item / session group on JSON artifacts; `key = None` falls
    /// through to the file decoder.
    fn decode_session(&self, session: &SessionRef) -> Result<Decoded> {
        match &session.key {
            Some(key) => match extension_of(&session.path).as_str() {
                "jsonl" | "ndjson" => {
                    acop::generic::decode_jsonl_session(&session.path, key, self.id())
                }
                "json" => {
                    acop::generic::decode_json_document_session(&session.path, key, self.id())
                }
                _ => {
                    if let Some(connection) =
                        acop::generic::openagent::open_read_only(&session.path)
                    {
                        if let Some(decoded) = acop::generic::openagent::decode(
                            &connection,
                            &session.path,
                            key,
                            self.id(),
                        )? {
                            return Ok(decoded);
                        }
                    }
                    acop::generic::decode_sqlite(&session.path, Some(key), self.id())
                }
            },
            None => self.decode(&session.path),
        }
    }

    /// Database artifacts decode through the shared container probe
    /// (`key = None`); every file format dispatches by extension.
    fn decode(&self, path: &Path) -> Result<Decoded> {
        if SQLITE_EXTENSIONS.contains(&extension_of(path).as_str()) {
            return acop::generic::decode_sqlite(path, None, self.id());
        }
        acop::generic::dispatch(path, self.id())
    }

    fn encode(&self, conversation: &Conversation) -> Result<Encoded> {
        acop::generic::encode_jsonl(conversation)
    }

    /// Imported sessions land as `<session-id>.jsonl` under the
    /// home-relative `opencode-data-store` root.
    fn store_path(&self, home: &Path, conversation: &Conversation) -> Result<PathBuf> {
        let root = self
            .history()
            .into_iter()
            .find(|root| root.kind == "opencode-data-store" && root.path.starts_with("{home}"))
            .map(|root| self.expand_root(&root, home))
            .ok_or_else(|| Error::Usage("opencode session root undeclared".to_string()))?;
        Ok(root.join(format!("{}.jsonl", conversation.session_id)))
    }

    fn redact_line(&self, line: &Value, marker: &str) -> Option<Value> {
        acop::generic::redact_line(line, marker)
    }
}

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

/// Session refs in one database: `session` rows keyed by `id` under
/// the OpenAgent schema, else the shared probe's `table/key` groups.
fn database_session_refs(db: &Path) -> Vec<SessionRef> {
    if let Some(connection) = acop::generic::openagent::open_read_only(db) {
        if let Some(refs) = acop::generic::openagent::session_refs(&connection, db) {
            return refs;
        }
    }
    acop::generic::sqlite_session_refs(db).unwrap_or_default()
}

/// An environment-declared base directory with a home-relative fallback.
fn env_dir(var: &str, fallback: PathBuf) -> PathBuf {
    std::env::var(var)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or(fallback)
}
