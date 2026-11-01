//! Session-store discovery over `a4aa` `AgentCard::history`.
//!
//! Each codec names the `HistoryRoot::kind`s that hold sessions; every
//! other declared root is enumerated as `StoreFileKind::Other` so
//! operators can report coverage honestly instead of silently scoping to
//! whatever happens to be readable.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{codec::Codec, Error, Result};

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

/// One session the codec enumerated inside an agent's declared history
/// surface — a file for file stores, a `(file, row)` pair for container
/// stores.
#[derive(Clone, Debug)]
pub struct SessionEntry {
    pub session: crate::codec::SessionRef,
    /// The `HistoryRoot::kind` it was found under.
    pub kind: &'static str,
}

/// Everything discovery learned about one agent's store.
#[derive(Debug)]
pub struct StoreScan {
    /// Sessions under session-holding roots.
    pub sessions: Vec<SessionEntry>,
    /// `HistoryRoot::kind`s the codec could not enumerate (no files
    /// selected, container enumeration unsupported, …).
    pub unreadable_roots: Vec<&'static str>,
}

/// Enumerate a codec's sessions plus the roots it cannot read.
pub fn scan(codec: &dyn Codec, home: &Path) -> Result<StoreScan> {
    let mut scan = StoreScan {
        sessions: Vec::new(),
        unreadable_roots: Vec::new(),
    };
    for root in codec.history() {
        if !codec.session_kinds().contains(&root.kind) {
            continue;
        }
        let path = codec.expand_root(&root, home);
        match codec.enumerate(&root, &path) {
            Ok(sessions) => {
                scan.sessions
                    .extend(sessions.into_iter().map(|session| SessionEntry {
                        session,
                        kind: root.kind,
                    }))
            }
            Err(_) => scan.unreadable_roots.push(root.kind),
        }
    }
    scan.sessions.sort_by(|a, b| {
        a.session
            .path
            .cmp(&b.session.path)
            .then(a.session.id.cmp(&b.session.id))
    });
    Ok(scan)
}

/// Recursive file walk shared by codecs' default enumeration.
pub fn collect_files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).map_err(Error::io(&d))? {
            let entry = entry.map_err(Error::io(&d))?;
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    Ok(out)
}

/// Resolve a `--session` argument (a path or a store-relative id/stem
/// fragment) to one concrete session under the agent's store.
pub fn resolve_session(codec: &dyn Codec, home: &Path, session: &str) -> Result<SessionEntry> {
    let as_path = Path::new(session);
    if as_path.is_file() {
        return Ok(SessionEntry {
            session: crate::codec::SessionRef {
                id: codec.session_id(as_path),
                path: as_path.to_path_buf(),
                key: None,
                parent: None,
            },
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
    let stem_of = |e: &SessionEntry| {
        e.session
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let exact: Vec<_> = scan
        .sessions
        .iter()
        .filter(|e| e.session.id == session || stem_of(e) == session)
        .collect();
    let matches = if exact.is_empty() {
        scan.sessions
            .iter()
            .filter(|e| e.session.id.contains(session) || stem_of(e).contains(session))
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
                .map(|e| e.session.id.clone())
                .collect::<Vec<_>>()
                .join(", "),
        )),
    }
}

/// The card used only for non-session store inventory (`aco stores`).
pub fn roots_of(codec: &dyn Codec) -> Vec<HistoryRoot> {
    codec.history()
}
