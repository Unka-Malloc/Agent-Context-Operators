//! `aco edit` — fine-grained mutation of one produced session file.
//!
//! Two operators, both line-granular over the native JSONL store:
//!
//! - `--delete <sel>` removes every source line *all* of whose decoded
//!   records are selected. A line that produced several records (e.g. a
//!   Claude message with mixed content parts) conflicts when only part
//!   of its records are selected — it is kept and reported, never
//!   partially rewritten.
//! - `--redact <sel> [--marker <text>]` asks the codec to rewrite the
//!   line's textual payload in place (thinking, arguments, outputs) —
//!   the record stays, the content is gone.
//!
//! Every write is atomic and preceded by a file backup under the
//! snapshot area (`~/.local/state/aco/backups/`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::{codec::Codec, snapshot, util, Error, Result, Selectors};

pub enum EditOp {
    Delete(Selectors),
    Redact {
        selectors: Selectors,
        marker: String,
    },
}

#[derive(Debug, Default)]
pub struct EditReport {
    pub path: PathBuf,
    /// Pre-write backup location, when a write happened.
    pub backup: Option<PathBuf>,
    /// Source line indices (0-based) removed.
    pub removed: Vec<usize>,
    /// Source line indices rewritten in place.
    pub rewritten: Vec<usize>,
    /// Human-readable conflicts (partial line selections, etc.).
    pub conflicts: Vec<String>,
    /// Selected ordinals that landed on no writable line.
    pub unmatched: Vec<u64>,
    pub dry_run: bool,
}

pub fn apply(
    codec: &dyn Codec,
    path: &Path,
    op: &EditOp,
    home: &Path,
    dry_run: bool,
) -> Result<EditReport> {
    let decoded = codec.decode(path)?;
    let raw = std::fs::read_to_string(path).map_err(Error::io(path))?;
    let mut lines: Vec<serde_json::Value> = raw
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or(serde_json::Value::Null))
        .collect();
    lines.truncate(decoded.line_count.max(lines.len()));
    while lines.len() < decoded.line_count {
        lines.push(serde_json::Value::Null);
    }

    let selectors = match op {
        EditOp::Delete(s) | EditOp::Redact { selectors: s, .. } => s,
    };
    let selected: BTreeSet<u64> = selectors
        .apply(&decoded.conversation.records)
        .into_iter()
        .collect();

    // line index -> ordinals of the records it produced
    let mut line_records: BTreeMap<usize, Vec<u64>> = BTreeMap::new();
    for (i, rec) in decoded.conversation.records.iter().enumerate() {
        if let Some(line) = decoded.record_lines.get(i) {
            line_records.entry(*line).or_default().push(rec.ordinal);
        }
    }
    let selected_lines: BTreeSet<usize> = line_records
        .iter()
        .filter(|(_, ords)| ords.iter().any(|o| selected.contains(o)))
        .map(|(l, _)| *l)
        .collect();

    let mut report = EditReport {
        path: path.to_path_buf(),
        dry_run,
        ..Default::default()
    };
    // Ordinals the selection named but no line owns (Meta-only decoders
    // may legitimately produce records without a line — report them).
    let lined: BTreeSet<u64> = line_records.values().flatten().copied().collect();
    report.unmatched = selected.difference(&lined).copied().collect();

    let mut drop_lines: BTreeSet<usize> = BTreeSet::new();
    match op {
        EditOp::Delete(_) => {
            for line in &selected_lines {
                let ords = &line_records[line];
                if ords.iter().all(|o| selected.contains(o)) {
                    drop_lines.insert(*line);
                } else {
                    report.conflicts.push(format!(
                        "line {} produced records {} but only {} selected — kept",
                        line + 1,
                        ords.iter()
                            .map(u64::to_string)
                            .collect::<Vec<_>>()
                            .join(","),
                        ords.iter()
                            .filter(|o| selected.contains(o))
                            .map(u64::to_string)
                            .collect::<Vec<_>>()
                            .join(","),
                    ));
                }
            }
        }
        EditOp::Redact { marker, .. } => {
            for line in &selected_lines {
                match codec.redact_line(&lines[*line], marker) {
                    Some(new) => {
                        lines[*line] = new;
                        report.rewritten.push(*line);
                    }
                    None => report.conflicts.push(format!(
                        "line {} holds no redactable text — skipped",
                        line + 1
                    )),
                }
            }
        }
    }
    report.removed = drop_lines.iter().copied().collect();

    if dry_run || (report.removed.is_empty() && report.rewritten.is_empty()) {
        return Ok(report);
    }

    let backup = snapshot::backup_file(home, path)?;
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if drop_lines.contains(&i) {
            continue;
        }
        if let serde_json::Value::Null = line {
            // Preserve unparseable lines verbatim.
            if let Some(raw_line) = raw.lines().nth(i) {
                out.push_str(raw_line);
                out.push('\n');
            }
            continue;
        }
        out.push_str(&serde_json::to_string(line).expect("line serializes"));
        out.push('\n');
    }
    util::atomic_write(path, &out)?;
    report.backup = Some(backup);
    Ok(report)
}
