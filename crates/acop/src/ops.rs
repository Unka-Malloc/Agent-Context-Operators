//! The document-moving operators: export, transform, import.
//!
//! - `export` decodes a session into the canonical `acop-conversation/1`
//!   document.
//! - `transform` decodes with the source codec and encodes with the
//!   target codec — the projection report says what survived.
//! - `import` writes a canonical document (or a native file via
//!   `--from`) into a target agent's store under a fresh session id.
//! - `fork` derives a new session from an existing one — natively where
//!   the format offers a derivation shape, verbatim on plain file
//!   stores, through the canonical projection otherwise.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{
    codec::Codec, codec::ForkMode, model::ForkOrigin, store, util, Conversation, Error, Projection,
    Result,
};

/// One row of `acop list`.
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
        for entry in scan.sessions {
            let row = match codec.decode_session(&entry.session) {
                Ok(d) => SessionRow {
                    agent: codec.id().into(),
                    session_id: entry.session.id.clone(),
                    path: entry.session.path.clone(),
                    kind: entry.kind.into(),
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
                    session_id: entry.session.id.clone(),
                    path: entry.session.path.clone(),
                    kind: entry.kind.into(),
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

/// `acop export`: resolve + decode → canonical document.
pub fn export(codec: &dyn Codec, home: &Path, session: &str) -> Result<(Conversation, PathBuf)> {
    let entry = store::resolve_session(codec, home, session)?;
    let decoded = codec.decode_session(&entry.session)?;
    Ok((decoded.conversation, entry.session.path))
}

/// `acop transform`: decode source, encode target. `session_or_file` is a
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

/// `acop import`: canonical document or `--from` native file → target
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

/// How a derived session was produced; re-export of
/// [`crate::ForkMode`] kept beside the report for readability.
#[derive(Debug)]
pub struct ForkReport {
    /// `native` (codec-shaped), `verbatim` (byte copy + id rewrite) or
    /// `canonical` (projected re-encode).
    pub mode: ForkMode,
    pub from_agent: String,
    pub to_agent: String,
    /// Source store session id.
    pub source_session: String,
    pub source_path: PathBuf,
    /// Fresh session id the fork was written under — never collides
    /// with the parent.
    pub session_id: String,
    pub target: PathBuf,
    pub written: Option<PathBuf>,
    /// Canonical records the child inherited.
    pub records: usize,
    /// The `--at` fork point (inclusive source ordinal), if truncated.
    pub at: Option<u64>,
    /// Record-level projection report — `Some` on the canonical path
    /// only; native/verbatim carry no projection loss.
    pub projection: Option<Projection>,
    pub dry_run: bool,
}

/// `acop fork`: derive a new session in `to`'s store from an existing
/// `from` session, optionally cutting it after ordinal `at`.
///
/// Order of paths, honoring "native where offered, honest projection
/// otherwise":
///
/// 1. Same-agent + codec [`Codec::fork_session`] hook — a native
///    derivation shape (e.g. DSH `inheritedEventCount`).
/// 2. Same-agent verbatim — byte-copy the artifact, rewrite only the
///    session id. Eligible on file stores (`key.is_none()`) whose file
///    is not a database container; truncation keeps the first
///    `record_lines[at]` lines.
/// 3. Canonical — decode, truncate, re-encode through `to.encode`.
///    Mandatory for cross-agent forks and the fallback everywhere else.
///    `Projection` reports what the target could not carry.
///
/// `--at` is an inclusive canonical ordinal: records `0..=at` survive.
pub fn fork(
    from: &dyn Codec,
    to: &dyn Codec,
    home: &Path,
    session: &str,
    at: Option<u64>,
    out: Option<&Path>,
    dry_run: bool,
) -> Result<ForkReport> {
    let entry = store::resolve_session(from, home, session)?;
    let decoded = from.decode_session(&entry.session)?;
    let conversation = decoded.conversation;
    if let Some(a) = at {
        if a as usize >= conversation.records.len() {
            return Err(Error::Usage(format!(
                "fork point {a} out of range — {} session {} has {} records",
                from.id(),
                entry.session.id,
                conversation.records.len()
            )));
        }
    }
    let new_id = uuid::Uuid::new_v4().to_string();
    let same_agent = from.id() == to.id();
    // Line index the verbatim/native paths keep through, inclusive:
    // the source line that produced record `at`. Whole-history forks
    // keep everything (`None`).
    let keep_lines = at.map(|a| decoded.record_lines[a as usize] + 1);

    let mut skeleton = conversation.clone();
    skeleton.session_id = new_id.clone();
    skeleton.agent = Some(to.id().to_string());
    skeleton.created_at = Some(util::now_rfc3339());
    let target = match out {
        Some(path) => path.to_path_buf(),
        None => to.store_path(home, &skeleton)?,
    };
    let records_kept = at
        .map(|a| a as usize + 1)
        .unwrap_or(conversation.records.len());
    let report = |mode, projection, written| ForkReport {
        mode,
        from_agent: from.id().to_string(),
        to_agent: to.id().to_string(),
        source_session: entry.session.id.clone(),
        source_path: entry.session.path.clone(),
        session_id: new_id.clone(),
        target: target.clone(),
        written,
        records: records_kept,
        at,
        projection,
        dry_run,
    };

    if same_agent {
        // 1. Native derivation shape.
        if let Some(mode) =
            from.fork_session(&entry.session, &target, &new_id, keep_lines, dry_run)?
        {
            return Ok(report(mode, None, (!dry_run).then(|| target.clone())));
        }
        // 2. Verbatim byte-copy — file stores only. A container row
        // (`key`) or a database file would copy sibling sessions.
        if verbatim_eligible(&entry.session) {
            if !dry_run {
                verbatim_write(&entry.session, &target, &new_id, keep_lines)?;
            }
            return Ok(report(
                ForkMode::Verbatim,
                None,
                (!dry_run).then(|| target.clone()),
            ));
        }
    }

    // 3. Canonical projection — also the only cross-agent path.
    if !to.capabilities().writable {
        return Err(Error::Usage(format!(
            "{} has a read-only store — a forked session cannot be written",
            to.id()
        )));
    }
    let mut conv = conversation;
    conv.session_id = new_id.clone();
    conv.agent = Some(to.id().to_string());
    conv.created_at = Some(util::now_rfc3339());
    conv.forked_from = Some(ForkOrigin {
        agent: from.id().to_string(),
        session_id: entry.session.id.clone(),
        at,
    });
    if let Some(a) = at {
        conv.records.truncate(a as usize + 1);
    }
    let encoded = to.encode(&conv)?;
    if target.exists() {
        return Err(Error::TargetExists(target));
    }
    if !dry_run {
        util::atomic_write(&target, &(encoded.lines.join("\n") + "\n"))?;
    }
    Ok(report(
        ForkMode::Canonical,
        Some(encoded.projection),
        (!dry_run).then(|| target.clone()),
    ))
}

/// A session a byte-copy can fork: file-backed, not a container row,
/// not a database file (copying one would drag sibling sessions along).
fn verbatim_eligible(source: &crate::codec::SessionRef) -> bool {
    const DB: &[&str] = &["db", "db3", "sqlite", "sqlite3"];
    source.key.is_none()
        && !source
            .path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| DB.contains(&e))
}

/// Verbatim fork: copy the artifact (optionally truncated to
/// `keep_lines`) and rewrite every occurrence of the source session id
/// to `new_id`.
fn verbatim_write(
    source: &crate::codec::SessionRef,
    dest: &Path,
    new_id: &str,
    keep_lines: Option<usize>,
) -> Result<()> {
    let text = std::fs::read_to_string(&source.path).map_err(Error::io(&source.path))?;
    let body: String = match keep_lines {
        Some(keep) => text.lines().take(keep).collect::<Vec<_>>().join("\n") + "\n",
        None => text,
    };
    if dest.exists() {
        return Err(Error::TargetExists(dest.to_path_buf()));
    }
    util::atomic_write(dest, &body.replace(&source.id, new_id))
}

/// One archived session: where the canonical transcript landed and
/// whether the store's own archive marker was applied.
#[derive(Debug)]
pub struct ArchiveReport {
    pub agent: String,
    /// The store's session id.
    pub session: String,
    pub source_path: PathBuf,
    /// The canonical document destination — `out` when given, else
    /// `~/.local/state/acop/archive/<agent>/<sha256(session)>.json`.
    pub transcript: PathBuf,
    pub transcript_written: bool,
    /// The store's archive marker was applied. `false` means the store
    /// has no such flag — the transcript is the whole archive, and the
    /// session stays listed rather than pretending to be hidden.
    pub marked: bool,
    pub dry_run: bool,
}

/// `acop archive`: retain a finished session — export the canonical
/// transcript, then flip the store's own archive marker where it has
/// one. The transcript is the durable record; the marker is best-effort
/// housekeeping and reports `false` honestly where a store offers none.
pub fn archive(
    codec: &dyn Codec,
    home: &Path,
    session: &str,
    out: Option<&Path>,
    dry_run: bool,
) -> Result<ArchiveReport> {
    let entry = store::resolve_session(codec, home, session)?;
    let decoded = codec.decode_session(&entry.session)?;
    let transcript = match out {
        Some(path) => path.to_path_buf(),
        None => archive_dir(home, codec).join(archive_file_name(&entry.session.id)),
    };
    if !dry_run {
        util::atomic_write(&transcript, &decoded.conversation.to_json())?;
    }
    let marked = codec.archive_session(&entry.session, home, dry_run)?;
    Ok(ArchiveReport {
        agent: codec.id().to_string(),
        session: entry.session.id,
        source_path: entry.session.path,
        transcript,
        transcript_written: !dry_run,
        marked,
        dry_run,
    })
}

fn archive_dir(home: &Path, codec: &dyn Codec) -> PathBuf {
    util::state_dir(home).join("archive").join(codec.id())
}

/// The default archive file name. The session id comes from artifact
/// content, so it never becomes a path component: the SHA-256 hex of the
/// id is one fixed-length component on every platform, and the report
/// (and the document itself) carries the original id.
fn archive_file_name(session_id: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}.json", Sha256::digest(session_id.as_bytes()))
}

/// Store-inventory rows for `acop stores`.
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
                path: codec.expand_root(&root, home),
                store: format!("{:?}", root.store),
                sessions: codec.session_kinds().contains(&root.kind),
            });
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{Capability, CapabilityMatrix, Decoded, Encoded, SessionRef};
    use crate::model::{Body, Record};
    use crate::store::{HistoryRoot, StoreKind};

    static CAPS: CapabilityMatrix = CapabilityMatrix::new(true, [Capability::Native; 7]);

    /// A minimal jsonl codec: one `{"session","n"}` row per user
    /// record — just enough to exercise the fork paths without any
    /// real agent data.
    struct MockCodec(&'static str);

    impl Codec for MockCodec {
        fn id(&self) -> &'static str {
            self.0
        }
        fn history(&self) -> Vec<HistoryRoot> {
            vec![HistoryRoot {
                path: "{home}/.mock/sessions",
                kind: "mock-sessions",
                store: StoreKind::JsonlDir,
            }]
        }
        fn session_kinds(&self) -> &'static [&'static str] {
            &["mock-sessions"]
        }
        fn capabilities(&self) -> &'static CapabilityMatrix {
            &CAPS
        }
        fn decode(&self, path: &Path) -> Result<Decoded> {
            let text = std::fs::read_to_string(path).map_err(Error::io(path))?;
            let mut records = Vec::new();
            let mut record_lines = Vec::new();
            let mut session_id = "mock-session".to_string();
            for (i, line) in text.lines().enumerate() {
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
                session_id = v["session"].as_str().unwrap().to_string();
                records.push(Record {
                    id: None,
                    parent: None,
                    ts: None,
                    ordinal: i as u64,
                    body: Body::User {
                        text: format!("row {}", v["n"].as_u64().unwrap()),
                        model: None,
                        attachments: vec![],
                    },
                });
                record_lines.push(i);
            }
            let mut conversation = Conversation::new(session_id);
            conversation.agent = Some(self.0.to_string());
            conversation.records = records;
            Ok(Decoded {
                conversation,
                record_lines,
                line_count: text.lines().count(),
                skipped: vec![],
            })
        }
        fn encode(&self, conversation: &Conversation) -> Result<Encoded> {
            Ok(Encoded {
                lines: conversation
                    .records
                    .iter()
                    .map(|r| {
                        serde_json::json!({"session": conversation.session_id, "n": r.ordinal})
                            .to_string()
                    })
                    .collect(),
                projection: self.project(conversation).1,
            })
        }
        fn store_path(&self, home: &Path, conversation: &Conversation) -> Result<PathBuf> {
            Ok(home
                .join(format!(".mock-{}/sessions", self.0))
                .join(format!("{}.jsonl", conversation.session_id)))
        }
        fn redact_line(
            &self,
            _line: &serde_json::Value,
            _marker: &str,
        ) -> Option<serde_json::Value> {
            None
        }
    }

    struct ReadOnly(MockCodec);
    impl Codec for ReadOnly {
        fn id(&self) -> &'static str {
            "ro"
        }
        fn history(&self) -> Vec<HistoryRoot> {
            self.0.history()
        }
        fn session_kinds(&self) -> &'static [&'static str] {
            self.0.session_kinds()
        }
        fn capabilities(&self) -> &'static CapabilityMatrix {
            const RO: CapabilityMatrix = CapabilityMatrix::new(false, [Capability::Unsupported; 7]);
            &RO
        }
        fn decode(&self, path: &Path) -> Result<Decoded> {
            self.0.decode(path)
        }
        fn encode(&self, _conversation: &Conversation) -> Result<Encoded> {
            Err(Error::Usage("read-only".into()))
        }
        fn store_path(&self, _home: &Path, _c: &Conversation) -> Result<PathBuf> {
            Err(Error::Usage("read-only".into()))
        }
        fn redact_line(&self, _l: &serde_json::Value, _m: &str) -> Option<serde_json::Value> {
            None
        }
    }

    fn session_file(dir: &Path, id: &str, rows: u64) -> PathBuf {
        let path = dir.join(format!("{id}.jsonl"));
        let lines: Vec<String> = (0..rows)
            .map(|n| serde_json::json!({"session": id, "n": n}).to_string())
            .collect();
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    #[test]
    fn verbatim_fork_rewrites_id_and_keeps_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let path = session_file(tmp.path(), "sess-1", 3);
        let codec = MockCodec("mock");
        let report = fork(
            &codec,
            &codec,
            tmp.path(),
            path.to_str().unwrap(),
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(report.mode, ForkMode::Verbatim);
        assert_eq!(report.records, 3);
        let written = report.written.unwrap();
        let text = std::fs::read_to_string(&written).unwrap();
        assert!(!text.contains("sess-1"), "{text}");
        assert!(text.contains(&report.session_id), "{text}");
        assert_eq!(text.lines().count(), 3);
        assert!(path.exists(), "source untouched");
    }

    #[test]
    fn verbatim_fork_truncates_at_fork_point() {
        let tmp = tempfile::tempdir().unwrap();
        let path = session_file(tmp.path(), "sess-1", 4);
        let codec = MockCodec("mock");
        let report = fork(
            &codec,
            &codec,
            tmp.path(),
            path.to_str().unwrap(),
            Some(1),
            None,
            false,
        )
        .unwrap();
        assert_eq!(report.records, 2);
        let text = std::fs::read_to_string(report.written.unwrap()).unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");
    }

    #[test]
    fn cross_agent_fork_uses_canonical_projection() {
        let tmp = tempfile::tempdir().unwrap();
        let path = session_file(tmp.path(), "sess-1", 3);
        let (mock, other) = (MockCodec("mock"), MockCodec("other"));
        let report = fork(
            &mock,
            &other,
            tmp.path(),
            path.to_str().unwrap(),
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(report.mode, ForkMode::Canonical);
        assert!(report.target.to_string_lossy().contains(".mock-other"));
        let text = std::fs::read_to_string(report.written.unwrap()).unwrap();
        assert!(text.contains(&report.session_id), "{text}");
    }

    #[test]
    fn read_only_target_reports_unsupported() {
        let tmp = tempfile::tempdir().unwrap();
        let path = session_file(tmp.path(), "sess-1", 2);
        let (mock, ro) = (MockCodec("mock"), ReadOnly(MockCodec("ro")));
        let err = fork(
            &mock,
            &ro,
            tmp.path(),
            path.to_str().unwrap(),
            None,
            None,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("read-only"), "{err}");
    }

    #[test]
    fn fork_point_out_of_range_is_a_usage_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = session_file(tmp.path(), "sess-1", 2);
        let codec = MockCodec("mock");
        let err = fork(
            &codec,
            &codec,
            tmp.path(),
            path.to_str().unwrap(),
            Some(9),
            None,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("out of range"), "{err}");
    }

    #[test]
    fn dry_run_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = session_file(tmp.path(), "sess-1", 2);
        let codec = MockCodec("mock");
        let report = fork(
            &codec,
            &codec,
            tmp.path(),
            path.to_str().unwrap(),
            None,
            None,
            true,
        )
        .unwrap();
        assert!(report.dry_run);
        assert!(!report.target.exists());
        assert!(report.written.is_none());
    }

    struct Markable(MockCodec);
    impl Codec for Markable {
        fn id(&self) -> &'static str {
            "markable"
        }
        fn history(&self) -> Vec<HistoryRoot> {
            self.0.history()
        }
        fn session_kinds(&self) -> &'static [&'static str] {
            self.0.session_kinds()
        }
        fn capabilities(&self) -> &'static CapabilityMatrix {
            self.0.capabilities()
        }
        fn decode(&self, path: &Path) -> Result<Decoded> {
            self.0.decode(path)
        }
        fn encode(&self, c: &Conversation) -> Result<Encoded> {
            self.0.encode(c)
        }
        fn store_path(&self, home: &Path, c: &Conversation) -> Result<PathBuf> {
            self.0.store_path(home, c)
        }
        fn archive_session(&self, _s: &SessionRef, _h: &Path, _d: bool) -> Result<bool> {
            Ok(true)
        }
        fn redact_line(&self, _l: &serde_json::Value, _m: &str) -> Option<serde_json::Value> {
            None
        }
    }

    #[test]
    fn archive_exports_transcript_and_reports_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let path = session_file(tmp.path(), "sess-1", 2);

        // A codec with no archive marker: transcript still lands.
        let codec = MockCodec("mock");
        let dest = tmp.path().join("out/sess-1.json");
        let report = archive(
            &codec,
            tmp.path(),
            path.to_str().unwrap(),
            Some(&dest),
            false,
        )
        .unwrap();
        assert!(!report.marked);
        let doc = std::fs::read_to_string(&dest).unwrap();
        assert!(doc.contains("acop-conversation/1"), "{doc}");

        // A codec with a marker reports it applied.
        let marked = Markable(MockCodec("markable"));
        let dest2 = tmp.path().join("out2/sess-1.json");
        let report = archive(
            &marked,
            tmp.path(),
            path.to_str().unwrap(),
            Some(&dest2),
            false,
        )
        .unwrap();
        assert!(report.marked);

        // dry-run writes no transcript and reports what would happen.
        let dest3 = tmp.path().join("out3/sess-1.json");
        let report = archive(
            &marked,
            tmp.path(),
            path.to_str().unwrap(),
            Some(&dest3),
            true,
        )
        .unwrap();
        assert!(report.dry_run);
        assert!(!report.transcript_written);
        assert!(!dest3.exists());
        assert!(report.marked, "dry-run still reports the marker applies");
    }

    /// A codec whose store reports an artifact-chosen session id.
    struct CraftedId(MockCodec, &'static str);
    impl Codec for CraftedId {
        fn id(&self) -> &'static str {
            "crafted"
        }
        fn history(&self) -> Vec<HistoryRoot> {
            self.0.history()
        }
        fn session_kinds(&self) -> &'static [&'static str] {
            self.0.session_kinds()
        }
        fn capabilities(&self) -> &'static CapabilityMatrix {
            self.0.capabilities()
        }
        fn session_id(&self, _path: &Path) -> String {
            self.1.to_string()
        }
        fn decode(&self, path: &Path) -> Result<Decoded> {
            self.0.decode(path)
        }
        fn encode(&self, c: &Conversation) -> Result<Encoded> {
            self.0.encode(c)
        }
        fn store_path(&self, home: &Path, c: &Conversation) -> Result<PathBuf> {
            self.0.store_path(home, c)
        }
        fn redact_line(&self, _l: &serde_json::Value, _m: &str) -> Option<serde_json::Value> {
            None
        }
    }

    #[test]
    fn archive_never_uses_the_session_id_as_a_path() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let path = session_file(tmp.path(), "sess-1", 1);
        for id in [
            "../../../../escape",
            "../escape",
            "/absolute/escape",
            r"..\..\escape",
            r"C:\escape",
            "C:escape",
            "nested/escape",
            ".",
            "",
        ] {
            let codec = CraftedId(MockCodec("mock"), id);
            let report = archive(&codec, &home, path.to_str().unwrap(), None, false).unwrap();
            let dir = archive_dir(&home, &codec);
            assert_eq!(report.transcript.parent().unwrap(), dir, "{id:?}");
            let name = report.transcript.file_name().unwrap().to_str().unwrap();
            let stem = name.strip_suffix(".json").unwrap();
            assert_eq!(stem.len(), 64, "{id:?}");
            assert!(stem.chars().all(|c| c.is_ascii_hexdigit()), "{id:?}");
            assert!(report.transcript.is_file(), "{id:?}");
            assert_eq!(report.session, id, "the report keeps the original id");
        }
        assert!(!tmp.path().join("escape.json").exists());
        assert!(!home.join("escape.json").exists());
        let written = std::fs::read_dir(archive_dir(&home, &CraftedId(MockCodec("mock"), "")))
            .unwrap()
            .count();
        assert_eq!(written, 9, "one distinct file per id");
    }

    #[test]
    fn container_session_skips_verbatim_into_canonical() {
        let tmp = tempfile::tempdir().unwrap();
        // A (path, key) session pair — like a SQLite row — must not be
        // byte-copied; it falls through to canonical encode.
        let src = SessionRef {
            id: "sess-1".into(),
            path: session_file(tmp.path(), "sess-1", 2),
            key: Some("row:7".into()),
            parent: None,
        };
        assert!(!verbatim_eligible(&src));
    }
}
