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
/// data. The `source` targets are the locations the snapshot recorded;
/// naming the snapshot is the operator's approval to write them back.
///
/// `name` must be one normal path component naming a directory directly
/// under `snapshots_dir(home)`, its `manifest.json` must live in that
/// directory, and every `stored` path must resolve inside it — symlinks
/// included — before anything is copied.
pub fn restore(home: &Path, name: &str, force: bool, dry_run: bool) -> Result<Manifest> {
    let not_found = || Error::SnapshotNotFound(name.to_string());
    if !is_one_normal_component(name) {
        return Err(not_found());
    }
    let root = snapshots_dir(home)
        .canonicalize()
        .map_err(|_| not_found())?;
    let dir = root.join(name).canonicalize().map_err(|_| not_found())?;
    if dir.parent() != Some(root.as_path()) {
        return Err(not_found());
    }
    let manifest_path = dir
        .join("manifest.json")
        .canonicalize()
        .map_err(|_| not_found())?;
    if manifest_path.parent() != Some(dir.as_path()) {
        return Err(not_found());
    }
    let text = fs::read_to_string(&manifest_path).map_err(|_| not_found())?;
    let manifest: Manifest = serde_json::from_str(&text)
        .map_err(|e| Error::Document(format!("corrupt manifest: {e}")))?;
    let mut sources = Vec::with_capacity(manifest.entries.len());
    for entry in &manifest.entries {
        sources.push(stored_inside(&dir, &entry.stored)?);
    }
    if !dry_run {
        for entry in &manifest.entries {
            if entry.source.exists() && !force {
                return Err(Error::TargetExists(entry.source.clone()));
            }
        }
        for (entry, src) in manifest.entries.iter().zip(&sources) {
            if let Some(parent) = entry.source.parent() {
                fs::create_dir_all(parent).map_err(Error::io(parent))?;
            }
            fs::copy(src, &entry.source).map_err(Error::io(src))?;
        }
    }
    Ok(manifest)
}

/// Exactly one `Normal` component with no separator of either platform.
fn is_one_normal_component(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
        && !name.contains(['/', '\\'])
}

/// A manifest `stored` path: relative, only normal components, and —
/// once symlinks resolve — still inside the snapshot directory.
fn stored_inside(dir: &Path, stored: &Path) -> Result<PathBuf> {
    let corrupt = || {
        Error::Document(format!(
            "manifest entry escapes the snapshot: {}",
            stored.display()
        ))
    };
    if stored.as_os_str().is_empty()
        || stored.to_string_lossy().contains('\\')
        || !stored
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return Err(corrupt());
    }
    let resolved = dir
        .join(stored)
        .canonicalize()
        .map_err(Error::io(dir.join(stored)))?;
    if !resolved.starts_with(dir) || !resolved.is_file() {
        return Err(corrupt());
    }
    Ok(resolved)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_snapshot(home: &Path, name: &str, stored: &str, source: &Path) -> PathBuf {
        let dir = snapshots_dir(home).join(name);
        fs::create_dir_all(dir.join("synthetic")).unwrap();
        fs::write(dir.join("synthetic/00000-a.jsonl"), "{}\n").unwrap();
        let manifest = Manifest {
            name: name.to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            entries: vec![Entry {
                agent: "synthetic".to_string(),
                kind: "synthetic-store".to_string(),
                source: source.to_path_buf(),
                stored: PathBuf::from(stored),
            }],
        };
        fs::write(
            dir.join("manifest.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        dir
    }

    #[test]
    fn restore_copies_a_contained_snapshot_back() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("store/a.jsonl");
        write_snapshot(tmp.path(), "snap-1", "synthetic/00000-a.jsonl", &target);
        restore(tmp.path(), "snap-1", false, false).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "{}\n");
    }

    #[test]
    fn restore_refuses_names_that_are_not_one_component() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("store/a.jsonl");
        // A manifest planted outside the snapshots directory.
        let outside = tmp.path().join("planted");
        fs::create_dir_all(&outside).unwrap();
        let planted = write_snapshot(tmp.path(), "snap-1", "synthetic/00000-a.jsonl", &target);
        fs::rename(&planted, outside.join("snap")).unwrap();
        fs::create_dir_all(snapshots_dir(tmp.path())).unwrap();
        let absolute = outside.join("snap");
        for name in [
            "../../../planted/snap",
            "..",
            ".",
            "",
            "a/b",
            r"a\b",
            r"..\planted",
            absolute.to_str().unwrap(),
        ] {
            let err = restore(tmp.path(), name, true, false).unwrap_err();
            assert!(matches!(err, Error::SnapshotNotFound(_)), "{name:?}: {err}");
        }
        assert!(!target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn restore_refuses_symlinks_out_of_the_snapshots_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("store/a.jsonl");
        let outside = write_snapshot(tmp.path(), "real", "synthetic/00000-a.jsonl", &target);
        let moved = tmp.path().join("elsewhere");
        fs::rename(&outside, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, snapshots_dir(tmp.path()).join("linked")).unwrap();
        let err = restore(tmp.path(), "linked", true, false).unwrap_err();
        assert!(matches!(err, Error::SnapshotNotFound(_)), "{err}");

        // A manifest symlinked in from outside the snapshot is refused too.
        let dir = snapshots_dir(tmp.path()).join("manifest-link");
        fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(moved.join("manifest.json"), dir.join("manifest.json")).unwrap();
        let err = restore(tmp.path(), "manifest-link", true, false).unwrap_err();
        assert!(matches!(err, Error::SnapshotNotFound(_)), "{err}");
        assert!(!target.exists());
    }

    #[test]
    fn restore_refuses_stored_paths_outside_the_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("store/a.jsonl");
        let secret = tmp.path().join("outside.txt");
        fs::write(&secret, "outside").unwrap();
        for (i, stored) in [
            "../../../../outside.txt".to_string(),
            secret.to_string_lossy().into_owned(),
            r"synthetic\..\..\outside.txt".to_string(),
            String::new(),
        ]
        .iter()
        .enumerate()
        {
            let name = format!("snap-{i}");
            write_snapshot(tmp.path(), &name, stored, &target);
            assert!(
                restore(tmp.path(), &name, true, false).is_err(),
                "{stored:?}"
            );
            assert!(
                restore(tmp.path(), &name, true, true).is_err(),
                "{stored:?}"
            );
        }
        assert!(!target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn restore_refuses_a_stored_symlink_out_of_the_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("store/a.jsonl");
        let secret = tmp.path().join("outside.txt");
        fs::write(&secret, "outside").unwrap();
        let dir = write_snapshot(tmp.path(), "snap", "synthetic/link", &target);
        std::os::unix::fs::symlink(&secret, dir.join("synthetic/link")).unwrap();
        assert!(restore(tmp.path(), "snap", true, false).is_err());
        assert!(!target.exists());
    }
}
