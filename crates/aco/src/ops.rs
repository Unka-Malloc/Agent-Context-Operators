//! The document-moving operators: export, transform, import.
//!
//! - `export` decodes a session into the canonical `aco-conversation/1`
//!   document.
//! - `transform` decodes with the source codec and encodes with the
//!   target codec — the projection report says what survived.
//! - `import` writes a canonical document (or a native file via
//!   `--from`) into a target agent's store under a fresh session id.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{codec::Codec, store, util, Conversation, Error, Projection, Result};

/// One row of `aco list`.
#[derive(Debug, Serialize)]
pub struct SessionRow {
    pub agent: String,
    pub session_id: String,
    pub path: PathBuf,
    pub kind: String,
    /// First record timestamp, when the file decodes.
    pub started: Option<String>,
    pub records: Option<usize>,
    pub decode_error: Option<String>,
}

pub fn list(codecs: &[&dyn Codec], home: &Path) -> Vec<SessionRow> {
    let mut rows = Vec::new();
    for codec in codecs {
        let Ok(scan) = store::scan(*codec, home) else {
            continue;
        };
        for file in scan.sessions {
            let session_id = codec.session_id(&file.path);
            let row = match codec.decode(&file.path) {
                Ok(d) => SessionRow {
                    agent: codec.id().into(),
                    session_id,
                    path: file.path.clone(),
                    kind: file.kind.into(),
                    started: d
                        .conversation
                        .records
                        .iter()
                        .find_map(|r| r.ts.clone())
                        .or(d.conversation.created_at.clone()),
                    records: Some(d.conversation.records.len()),
                    decode_error: None,
                },
                Err(e) => SessionRow {
                    agent: codec.id().into(),
                    session_id,
                    path: file.path.clone(),
                    kind: file.kind.into(),
                    started: None,
                    records: None,
                    decode_error: Some(e.to_string()),
                },
            };
            rows.push(row);
        }
    }
    rows.sort_by(|a, b| b.started.cmp(&a.started));
    rows
}

/// `aco export`: resolve + decode → canonical document.
pub fn export(codec: &dyn Codec, home: &Path, session: &str) -> Result<(Conversation, PathBuf)> {
    let file = store::resolve_session(codec, home, session)?;
    let decoded = codec.decode(&file.path)?;
    Ok((decoded.conversation, file.path))
}

/// `aco transform`: decode source, encode target. `session_or_file` is a
/// store id/stem for the source agent, or a path to a native file.
pub fn transform(
    from: &dyn Codec,
    to: &dyn Codec,
    home: &Path,
    session_or_file: &str,
) -> Result<(crate::Encoded, Conversation)> {
    let (conversation, _) = export(from, home, session_or_file)?;
    let mut conversation = conversation;
    conversation.agent = Some(to.id().to_string());
    let encoded = to.encode(&conversation)?;
    Ok((encoded, conversation))
}

#[derive(Debug)]
pub struct ImportReport {
    pub written: Option<PathBuf>,
    pub target: PathBuf,
    pub session_id: String,
    pub projection: Projection,
    pub dry_run: bool,
}

/// `aco import`: canonical document or `--from` native file → target
/// store. The session id is always regenerated — import never collides
/// with or overwrites an existing session.
pub fn import(
    target: &dyn Codec,
    home: &Path,
    file: &Path,
    from: Option<&dyn Codec>,
    dry_run: bool,
) -> Result<ImportReport> {
    let text = std::fs::read_to_string(file).map_err(Error::io(file))?;
    let mut conversation = match from {
        // Explicit native source: decode with that codec.
        Some(codec) => codec.decode(file)?.conversation,
        // Canonical document by default.
        None => Conversation::from_json(&text)?,
    };
    conversation.session_id = uuid::Uuid::new_v4().to_string();
    conversation.agent = Some(target.id().to_string());
    if conversation.created_at.is_none() {
        conversation.created_at = Some(util::now_rfc3339());
    }

    let encoded = target.encode(&conversation)?;
    let target_path = target.store_path(home, &conversation)?;
    let report = ImportReport {
        written: None,
        target: target_path.clone(),
        session_id: conversation.session_id.clone(),
        projection: encoded.projection,
        dry_run,
    };
    if dry_run {
        return Ok(report);
    }
    if target_path.exists() {
        return Err(Error::TargetExists(target_path));
    }
    util::atomic_write(&target_path, &(encoded.lines.join("\n") + "\n"))?;
    Ok(ImportReport {
        written: Some(target_path),
        ..report
    })
}

/// Store-inventory rows for `aco stores`.
#[derive(Debug, Serialize)]
pub struct StoreRow {
    pub agent: String,
    pub kind: &'static str,
    pub path: PathBuf,
    pub store: String,
    pub sessions: bool,
}

pub fn stores(codecs: &[&dyn Codec], home: &Path) -> Vec<StoreRow> {
    let mut rows = Vec::new();
    for codec in codecs {
        for root in codec.history() {
            rows.push(StoreRow {
                agent: codec.id().into(),
                kind: root.kind,
                path: util::expand_home(root.path, home),
                store: format!("{:?}", root.store),
                sessions: codec.session_kinds().contains(&root.kind),
            });
        }
    }
    rows
}
