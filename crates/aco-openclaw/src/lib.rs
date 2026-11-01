//! aco codec: the `openclaw` (OpenClaw) conversation store.
//!
//! The a4aa card declares one session root — `~/.openclaw`
//! (`openclaw-store`, a plain directory). OpenClaw has no bespoke wire
//! format LicoUp knows: session artifacts land as JSONL/NDJSON event
//! logs, single JSON documents, markdown or plaintext transcripts, and
//! SQLite databases (`*.sqlite`/`*.sqlite3`/`*.db`), so every artifact
//! decodes through the shared [`aco::generic`] lane. This codec
//! contributes the store facts only: the accepted extension set with
//! LicoUp's exclusions, container enumeration as one [`SessionRef`] per
//! stored session key, and the `<session-id>.jsonl` import path under
//! the store root.

use std::path::{Path, PathBuf};

use aco::codec::{Capability, CapabilityMatrix, Codec, Decoded, Encoded, SessionRef};
use aco::model::Conversation;
use aco::store::HistoryRoot;
use aco::{Error, Result};
use serde_json::Value;

/// Extensions LicoUp's `openclaw` history adapter accepts as
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

/// The codec instance aco's registry hands out.
pub struct OpenClawCodec;

/// One codec instance for the registry.
pub fn codec() -> &'static dyn Codec {
    &OpenClawCodec
}

impl Codec for OpenClawCodec {
    fn id(&self) -> &'static str {
        "openclaw"
    }

    fn history(&self) -> Vec<HistoryRoot> {
        a4aa_open_claw::card().history.to_vec()
    }

    /// The card declares no transcript-usage surface (`transcript:
    /// None`); usage counters a persisted line carries still decode as
    /// `meta{usage}` records.
    fn transcript_usage(&self) -> Option<aco::TranscriptUsage> {
        a4aa_open_claw::card().usage.transcript
    }

    fn session_kinds(&self) -> &'static [&'static str] {
        &["openclaw-store"]
    }

    fn capabilities(&self) -> &'static CapabilityMatrix {
        &CAPABILITIES
    }

    /// A UUID-bearing directory component names the session where the
    /// layout carries one; otherwise the file stem — the generic
    /// identity rule.
    fn session_id(&self, path: &Path) -> String {
        aco::generic::session_id(path)
    }

    /// LicoUp's `accepts_file` extension set for the `openclaw`
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

    /// A database file enumerates one [`SessionRef`] per stored session
    /// key; a `.json` document with an explicit `sessions[]` array
    /// enumerates one ref per session item; every other accepted
    /// artifact stays one session per file.
    fn sessions_in_file(&self, path: &Path) -> Result<Vec<SessionRef>> {
        let ext = extension_of(path);
        if SQLITE_EXTENSIONS.contains(&ext.as_str()) && !rejected_file_name(path) {
            return aco::generic::sqlite_session_refs(path);
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

    /// `key = Some(..)` reads that container session — a sqlite row
    /// group or a `sessions[]` item; `key = None` falls through to the
    /// file decoder.
    fn decode_session(&self, session: &SessionRef) -> Result<Decoded> {
        match &session.key {
            Some(key) => match extension_of(&session.path).as_str() {
                ext if SQLITE_EXTENSIONS.contains(&ext) => {
                    aco::generic::decode_sqlite(&session.path, Some(key), self.id())
                }
                "jsonl" | "ndjson" => {
                    aco::generic::decode_jsonl_session(&session.path, key, self.id())
                }
                _ => aco::generic::decode_json_document_session(&session.path, key, self.id()),
            },
            None => self.decode(&session.path),
        }
    }

    /// Database artifacts decode through the shared container probe
    /// (`key = None`); every file format dispatches by extension.
    fn decode(&self, path: &Path) -> Result<Decoded> {
        if SQLITE_EXTENSIONS.contains(&extension_of(path).as_str()) {
            return aco::generic::decode_sqlite(path, None, self.id());
        }
        aco::generic::dispatch(path, self.id())
    }

    fn encode(&self, conversation: &Conversation) -> Result<Encoded> {
        aco::generic::encode_jsonl(conversation)
    }

    /// Imported sessions land as `<session-id>.jsonl` directly under
    /// the store root.
    fn store_path(&self, home: &Path, conversation: &Conversation) -> Result<PathBuf> {
        let root = self
            .history()
            .into_iter()
            .find(|root| root.kind == "openclaw-store")
            .map(|root| self.expand_root(&root, home))
            .ok_or_else(|| Error::Usage("openclaw session root undeclared".to_string()))?;
        Ok(root.join(format!("{}.jsonl", conversation.session_id)))
    }

    fn redact_line(&self, line: &Value, marker: &str) -> Option<Value> {
        aco::generic::redact_line(line, marker)
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
