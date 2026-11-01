//! The codec contract between `aco` and per-agent `aco-<agent>` crates.
//!
//! A codec knows one agent's conversation store: where sessions live
//! (via the `a4aa-<agent>` card's `HistoryRoot`s), how to decode a native
//! session file into canonical records, how to encode canonical records
//! back into native lines for transform/import, how to redact one raw
//! line for edit, and where an imported session file must land.
//!
//! The capability matrix declares, per [`RecordKind`], what the native
//! format can carry. Projection never drops silently: every unsupported
//! record lands in the [`Projection`] report the operator surfaces.

use std::path::{Path, PathBuf};

use crate::model::{Conversation, Record, RecordKind};
use crate::store::HistoryRoot;
use crate::Result;

/// How one record kind survives the trip into a native format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Capability {
    /// The format expresses this kind directly.
    Native,
    /// The format expresses it through a documented adaptation.
    Adapted,
    /// The format cannot carry it; projection reports the drop.
    Unsupported,
}

/// Per-kind support table for one codec.
#[derive(Debug)]
pub struct CapabilityMatrix {
    /// Records the format can persist at all (encode is implemented).
    pub writable: bool,
    kinds: [Capability; 7],
}

impl CapabilityMatrix {
    pub const fn new(writable: bool, kinds: [Capability; 7]) -> Self {
        Self { writable, kinds }
    }

    /// Read-only store: every kind declared unsupported for writing.
    pub const READ_ONLY: CapabilityMatrix =
        CapabilityMatrix::new(false, [Capability::Unsupported; 7]);

    pub fn of(&self, kind: RecordKind) -> Capability {
        self.kinds[kind as usize]
    }
}

/// What projection did to a conversation on the way into a format.
#[derive(Clone, Debug, Default)]
pub struct Projection {
    /// `(ordinal, kind)` records emitted through an adaptation.
    pub adapted: Vec<(u64, RecordKind)>,
    /// `(ordinal, kind)` records the format cannot carry.
    pub dropped: Vec<(u64, RecordKind)>,
}

impl Projection {
    pub fn is_clean(&self) -> bool {
        self.adapted.is_empty() && self.dropped.is_empty()
    }
}

/// A native line the decoder deliberately did not turn into a record.
#[derive(Clone, Debug)]
pub struct Skipped {
    pub line: usize,
    /// The native line type tag, when one was recognized.
    pub native_type: Option<String>,
    /// Why it was skipped (`projection`, `opaque`, `corrupt`).
    pub reason: String,
}

/// Output of decoding one session file.
#[derive(Debug)]
pub struct Decoded {
    pub conversation: Conversation,
    /// `record_lines[i]` is the source line index (0-based) that produced
    /// `conversation.records[i]`. A line may produce several records.
    pub record_lines: Vec<usize>,
    /// Total lines in the source file.
    pub line_count: usize,
    pub skipped: Vec<Skipped>,
}

/// Output of encoding a conversation into one format.
#[derive(Debug)]
pub struct Encoded {
    /// Native file lines (JSONL).
    pub lines: Vec<String>,
    pub projection: Projection,
}

/// One agent's conversation-store knowledge.
pub trait Codec {
    /// Agent id, identical to the a4aa card/dialect id (`codex`, …).
    fn id(&self) -> &'static str;

    /// The agent's declared history surface — the codec owns it. a4aa's
    /// card projects these same constants back for runtime consumers, so
    /// `aco` core carries no agent knowledge and stays dependency-free
    /// for re-export through `a4aa`.
    fn history(&self) -> Vec<HistoryRoot>;

    /// Where token counters appear in this store's persisted records, if
    /// the agent reports any. A usage consumer decodes through ACO and
    /// applies this spec — it never needs a parallel store adapter.
    fn transcript_usage(&self) -> Option<crate::store::TranscriptUsage> {
        None
    }

    /// History kinds from `card().history` that hold *sessions* this
    /// codec reads (e.g. `codex-session-store`). Other declared roots —
    /// prompt history, memory files, indexes — are stores ACO reports
    /// but does not treat as conversations.
    fn session_kinds(&self) -> &'static [&'static str];

    fn capabilities(&self) -> &'static CapabilityMatrix;

    /// The session's store identifier for `--session` lookup — the file
    /// stem by default (codex `rollout-…-<uuid>`, claude `<uuid>`).
    fn session_id(&self, path: &Path) -> String {
        path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Decode one native session file.
    fn decode(&self, path: &Path) -> Result<Decoded>;

    /// Encode canonical records into native lines. Implementations must
    /// consult `capabilities()` and fill `Encoded::projection` honestly.
    fn encode(&self, conversation: &Conversation) -> Result<Encoded>;

    /// Where an imported conversation lands inside the agent's store.
    /// `conversation.session_id` has already been regenerated by the
    /// import operator; the codec derives directory layout and filename.
    fn store_path(&self, home: &Path, conversation: &Conversation) -> Result<PathBuf>;

    /// Rewrite one raw line with its textual content replaced by
    /// `marker`. `None` means the line holds no redactable text; the
    /// edit operator then skips it for `--redact`.
    fn redact_line(&self, line: &serde_json::Value, marker: &str) -> Option<serde_json::Value>;

    /// Kinds projected through this codec — helper for projection fills.
    fn project<'a>(&self, conversation: &'a Conversation) -> (Vec<&'a Record>, Projection) {
        let mut kept = Vec::new();
        let mut proj = Projection::default();
        for rec in &conversation.records {
            match self.capabilities().of(rec.kind()) {
                Capability::Native => kept.push(rec),
                Capability::Adapted => {
                    kept.push(rec);
                    proj.adapted.push((rec.ordinal, rec.kind()));
                }
                Capability::Unsupported => proj.dropped.push((rec.ordinal, rec.kind())),
            }
        }
        (kept, proj)
    }
}
