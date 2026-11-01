//! Session-store discovery over `a4aa` `AgentCard::history`.
//!
//! Each codec names the `HistoryRoot::kind`s that hold sessions; every
//! other declared root is enumerated as `StoreFileKind::Other` so
//! operators can report coverage honestly instead of silently scoping to
//! whatever happens to be readable.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{codec::Codec, util, Error, Result};

/// A declared location in an agent's history surface — ACO's own
/// vocabulary. `aco-<agent>` codecs fill it from the owning descriptor
/// (today: the `a4aa-<agent>` card's `history`); ACO never hardcodes
/// store locations itself.
#[derive(Clone, Copy, Debug)]
pub struct HistoryRoot {
    /// `{home}`-relative path template.
    pub path: &'static str,
    /// Stable kind label the descriptor reports
    /// (`"claude-project-transcripts"`, `"codex-session-store"`, …).
    pub kind: &'static str,
    pub store: StoreKind,
}

/// How a history root is physically stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StoreKind {
    JsonlFile,
    JsonlDir,
    SqliteDb,
    MarkdownFile,
    Directory,
    Other,
}

/// Token-accounting locations inside persisted records — the artifact
/// half of a4aa's `UsageSurface` (live-wire and hosted counters stay
/// there). Declared facts only: a counter the agent does not report
/// stays absent. A usage consumer reads sessions through `decode` plus
/// this spec — it never needs a parallel store adapter.
#[derive(Clone, Copy, Debug)]
pub struct TranscriptUsage {
    /// `HistoryRoot::kind` holding token records.
    pub root: &'static str,
    /// Record discriminator `(dotted path, equals value)` — only matching
    /// records carry counters. `None`: any record bearing `container` is a
    /// usage record (for stores that mark usage by presence, not type).
    pub record: Option<(&'static str, &'static str)>,
    /// Dotted path to the counter object inside a matching record.
    pub container: &'static str,
    /// Field names read as input/prompt tokens; first present wins.
    pub input: &'static [&'static str],
    /// Field names read as cached input tokens.
    pub cached: &'static [&'static str],
    /// Field names read as output/completion tokens.
    pub output: &'static [&'static str],
    /// Field names read as a reported total; empty where the store has
    /// none — the consumer sums, the declaration never invents a total.
    pub total: &'static [&'static str],
}

/// One concrete file belonging to an agent's declared history surface.
#[derive(Clone, Debug)]
pub struct StoreFile {
    pub path: PathBuf,
    /// The `HistoryRoot::kind` it was found under.
    pub kind: &'static str,
}

/// Everything discovery learned about one agent's store.
#[derive(Debug)]
pub struct StoreScan {
    /// Files under session-holding roots, recursively for directories.
    pub sessions: Vec<StoreFile>,
    /// `HistoryRoot::kind`s whose [`StoreKind`] ACO cannot read
    /// (`SqliteDb`, `MarkdownFile`, `Directory`, `Other`).
    pub unreadable_roots: Vec<&'static str>,
}

/// Enumerate a codec's session files plus the roots it cannot read.
pub fn scan(codec: &dyn Codec, home: &Path) -> Result<StoreScan> {
    let mut scan = StoreScan {
        sessions: Vec::new(),
        unreadable_roots: Vec::new(),
    };
    for root in codec.history() {
        let is_session = codec.session_kinds().contains(&root.kind);
        let path = util::expand_home(root.path, home);
        match root.store {
            StoreKind::JsonlFile if is_session => {
                if path.is_file() {
                    scan.sessions.push(StoreFile {
                        path,
                        kind: root.kind,
                    });
                }
            }
            StoreKind::JsonlDir | StoreKind::Directory if is_session => {
                collect_jsonl(&path, root.kind, &mut scan.sessions)?;
            }
            StoreKind::JsonlDir | StoreKind::Directory => {
                // Declared but non-session root: skipped by design, still
                // readable — not reported as unreadable.
            }
            _ => scan.unreadable_roots.push(root.kind),
        }
    }
    scan.sessions.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(scan)
}

fn collect_jsonl(dir: &Path, kind: &'static str, out: &mut Vec<StoreFile>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).map_err(Error::io(&d))? {
            let entry = entry.map_err(Error::io(&d))?;
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                out.push(StoreFile { path: p, kind });
            }
        }
    }
    Ok(())
}

/// Resolve a `--session` argument (a path or a store-relative id/stem
/// fragment) to one concrete file under the agent's store.
pub fn resolve_session(codec: &dyn Codec, home: &Path, session: &str) -> Result<StoreFile> {
    let as_path = Path::new(session);
    if as_path.is_file() {
        return Ok(StoreFile {
            path: as_path.to_path_buf(),
            kind: "explicit-path",
        });
    }
    let scan = scan(codec, home)?;
    if scan.sessions.is_empty() {
        return Err(Error::NoSessionStore(
            codec.id().to_string(),
            scan.unreadable_roots.join(", "),
        ));
    }
    let stem_of = |f: &StoreFile| {
        f.path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let exact: Vec<_> = scan
        .sessions
        .iter()
        .filter(|f| codec.session_id(&f.path) == session || stem_of(f) == session)
        .collect();
    let matches = if exact.is_empty() {
        scan.sessions
            .iter()
            .filter(|f| codec.session_id(&f.path).contains(session) || stem_of(f).contains(session))
            .collect()
    } else {
        exact
    };
    match matches.len() {
        0 => Err(Error::SessionNotFound(
            session.to_string(),
            codec.id().to_string(),
        )),
        1 => Ok(matches[0].clone()),
        _ => Err(Error::AmbiguousSession(
            session.to_string(),
            codec.id().to_string(),
            matches
                .iter()
                .map(|f| codec.session_id(&f.path))
                .collect::<Vec<_>>()
                .join(", "),
        )),
    }
}

/// The card used only for non-session store inventory (`aco stores`).
pub fn roots_of(codec: &dyn Codec) -> Vec<HistoryRoot> {
    codec.history()
}
