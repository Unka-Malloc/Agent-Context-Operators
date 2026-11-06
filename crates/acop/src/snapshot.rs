//! `acop snapshot` — point-in-time copies of agent session stores under
//! `~/.local/state/acop/snapshots/<name>/`, plus the per-file backup the
//! edit operator takes before every write.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{codec::Codec, store, util, Error, Result};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    /// Snapshot directory name: `<yyyymmddTHHMMSS>-<short-uuid>`.
    pub name: String,
    pub created_at: String,
    pub entries: Vec<Entry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub agent: String,
    /// `HistoryRoot::kind` the file was found under.
    pub kind: String,
    /// Original absolute location.
    pub source: PathBuf,
    /// Path inside the snapshot directory.
    pub stored: PathBuf,
}

pub fn snapshots_dir(home: &Path) -> PathBuf {
    util::state_dir(home).join("snapshots")
}

fn backups_dir(home: &Path) -> PathBuf {
    util::state_dir(home).join("backups")
}

/// Copy every readable session file of `codecs` into a new snapshot.
/// Returns the manifest; the snapshot directory is removed if nothing
/// was copied.
pub fn create(home: &Path, codecs: &[&dyn Codec]) -> Result<Manifest> {
    let name = format!(
        "{}-{}",
        util::now_rfc3339()
            .replace([':', 'T'], "-")
            .trim_end_matches('Z'),
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    let dir = snapshots_dir(home).join(&name);
    let mut manifest = Manifest {
        name,
        created_at: util::now_rfc3339(),
        entries: Vec::new(),
    };
    for codec in codecs {
        let scan = store::scan(*codec, home)?;
        // Container sessions share one backing file — copy it once.
        let mut seen = std::collections::HashSet::new();
        for (i, entry) in scan.sessions.iter().enumerate() {
            if !seen.insert(entry.session.path.clone()) {
                continue;
            }
            let stored = PathBuf::from(codec.id()).join(format!(
                "{i:05}-{}",
                entry
                    .session
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            ));
            let dest = dir.join(&stored);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(Error::io(parent))?;
            }
            fs::copy(&entry.session.path, &dest).map_err(Error::io(&entry.session.path))?;
            manifest.entries.push(Entry {
                agent: codec.id().to_string(),
                kind: entry.kind.to_string(),
                source: entry.session.path.clone(),
                stored,
            });
        }
    }
    if manifest.entries.is_empty() {
        let _ = fs::remove_dir_all(&dir);
        return Ok(manifest);
    }
    util::atomic_write(
        &dir.join("manifest.json"),
        &serde_json::to_string_pretty(&manifest).expect("manifest serializes"),
    )?;
    Ok(manifest)
}

/// All snapshots with valid manifests, newest first.
pub fn list(home: &Path) -> Result<Vec<Manifest>> {
    let dir = snapshots_dir(home);
    let mut out = Vec::new();
    if dir.is_dir() {
        for entry in fs::read_dir(&dir).map_err(Error::io(&dir))? {
            let entry = entry.map_err(Error::io(&dir))?;
            let manifest_path = entry.path().join("manifest.json");
            if let Ok(text) = fs::read_to_string(&manifest_path) {
                if let Ok(m) = serde_json::from_str::<Manifest>(&text) {
                    out.push(m);
                }
            }
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
}

/// Copy a snapshot's files back to their recorded sources. An existing
/// target refuses the copy unless `force` — the target is itself user
/// data.
pub fn restore(home: &Path, name: &str, force: bool, dry_run: bool) -> Result<Manifest> {
    let dir = snapshots_dir(home).join(name);
    let manifest_path = dir.join("manifest.json");
    let text = fs::read_to_string(&manifest_path)
        .map_err(|_| Error::SnapshotNotFound(name.to_string()))?;
    let manifest: Manifest = serde_json::from_str(&text)
        .map_err(|e| Error::Document(format!("corrupt manifest: {e}")))?;
    if !dry_run {
        for entry in &manifest.entries {
            if entry.source.exists() && !force {
                return Err(Error::TargetExists(entry.source.clone()));
            }
        }
        for entry in &manifest.entries {
            let src = dir.join(&entry.stored);
            if let Some(parent) = entry.source.parent() {
                fs::create_dir_all(parent).map_err(Error::io(parent))?;
            }
            fs::copy(&src, &entry.source).map_err(Error::io(&src))?;
        }
    }
    Ok(manifest)
}

/// The edit operator's pre-write backup: `<state>/backups/<ts>-<rand>/`
/// mirroring the absolute path, so concurrent edits never collide.
pub fn backup_file(home: &Path, source: &Path) -> Result<PathBuf> {
    let stamp = format!(
        "{}-{}",
        util::now_rfc3339()
            .replace([':', 'T'], "-")
            .trim_end_matches('Z'),
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    // Mirror the absolute source path below the backup root: drop the
    // platform root components (a unix `/` or a windows drive prefix) so
    // `join` can never collapse back onto the source itself.
    let rel: PathBuf = source
        .components()
        .skip_while(|c| {
            matches!(
                c,
                std::path::Component::Prefix(_) | std::path::Component::RootDir
            )
        })
        .collect();
    let dest = backups_dir(home).join(stamp).join(rel);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(Error::io(parent))?;
    }
    fs::copy(source, &dest).map_err(Error::io(source))?;
    Ok(dest)
}
