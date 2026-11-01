//! End-to-end CLI coverage over a synthetic home directory: search,
//! export, transform, import, edit, snapshot. All fixture content is
//! invented — no real conversation data.

use std::path::{Path, PathBuf};
use std::process::Command;

fn aco(home: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_aco"))
        .args(args)
        .env("HOME", home)
        .output()
        .expect("aco runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// One synthetic codex rollout under the store layout.
fn seed_codex(home: &Path) -> PathBuf {
    let dir = home.join(".codex/sessions/2026/01/01");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rollout-2026-01-01T00-00-00-abcdefab-1234-5678-9abc-def012345678.jsonl");
    let lines = [
        serde_json::json!({"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"abcdefab-1234-5678-9abc-def012345678","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/demo","originator":"test"}}),
        serde_json::json!({"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"message","id":"m1","role":"user","content":[{"type":"input_text","text":"please summarize the release notes"}]}}),
        serde_json::json!({"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"reasoning","id":"r1","summary":[{"type":"summary_text","text":"user wants a summary"}],"encrypted_content":"enc"}}),
        serde_json::json!({"timestamp":"2026-01-01T00:00:03Z","type":"response_item","payload":{"type":"message","id":"m2","role":"assistant","content":[{"type":"output_text","text":"release notes summary here"}]}}),
    ];
    std::fs::write(
        &path,
        lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
    path
}

#[test]
fn end_to_end() {
    let home = tempfile::tempdir().unwrap();
    let rollout = seed_codex(home.path());

    // list discovers the seeded session.
    let (ok, out, _) = aco(home.path(), &["list", "--json"]);
    assert!(ok);
    assert!(
        out.contains("abcdefab-1234-5678-9abc-def012345678"),
        "{out}"
    );

    // search hits with context.
    let (ok, out, _) = aco(
        home.path(),
        &["search", "release notes", "--agent", "codex"],
    );
    assert!(ok, "{out}");
    assert!(out.contains("2 hits"), "{out}");
    assert!(out.contains("release notes summary"), "{out}");

    // export produces the canonical document.
    let exported = home.path().join("conv.json");
    let (ok, _, err) = aco(
        home.path(),
        &[
            "export",
            "--agent",
            "codex",
            "--session",
            "abcdefab-1234-5678-9abc-def012345678",
            "-o",
            exported.to_str().unwrap(),
        ],
    );
    assert!(ok, "{err}");
    let doc = std::fs::read_to_string(&exported).unwrap();
    assert!(doc.contains("aco-conversation/1"), "{doc}");

    // transform codex → claude writes native claude lines.
    let transformed = home.path().join("claude.jsonl");
    let (ok, _, err) = aco(
        home.path(),
        &[
            "transform",
            "--from",
            "codex",
            "--to",
            "claude",
            "--session",
            "abcdefab-1234-5678-9abc-def012345678",
            "-o",
            transformed.to_str().unwrap(),
        ],
    );
    assert!(ok, "{err}");
    let t = std::fs::read_to_string(&transformed).unwrap();
    assert!(t.contains("\"type\":\"user\""), "{t}");
    assert!(t.contains("\"type\":\"thinking\""), "{t}");

    // import lands it in the claude store under a fresh id.
    let (ok, out, err) = aco(
        home.path(),
        &[
            "import",
            "--agent",
            "claude",
            "--file",
            exported.to_str().unwrap(),
        ],
    );
    assert!(ok, "{err}");
    assert!(out.contains("imported as session"), "{out}");
    let imported_dir = home.path().join(".claude/projects/-tmp-demo");
    assert!(imported_dir.is_dir(), "{out}");
    let imported: Vec<_> = std::fs::read_dir(&imported_dir).unwrap().collect();
    assert_eq!(imported.len(), 1);

    // edit --redact scrubs the reasoning record in the codex file.
    let (ok, out, err) = aco(
        home.path(),
        &[
            "edit",
            "--agent",
            "codex",
            "--session",
            "abcdefab",
            "--redact",
            "kind:reasoning",
        ],
    );
    assert!(ok, "{err}");
    assert!(out.contains("backup:"), "{out}");
    let edited = std::fs::read_to_string(&rollout).unwrap();
    assert!(!edited.contains("user wants a summary"), "{edited}");
    assert!(edited.contains("<redacted>"), "{edited}");
    assert!(edited.contains("\"encrypted_content\":null"), "{edited}");

    // snapshot create + list.
    let (ok, out, _) = aco(home.path(), &["snapshot", "create", "--all"]);
    assert!(ok, "{out}");
    let (ok, out, _) = aco(home.path(), &["snapshot", "list"]);
    assert!(ok, "{out}");
    assert!(out.contains("file"), "{out}");

    // dry-run edit leaves the file untouched.
    let before = std::fs::read_to_string(&rollout).unwrap();
    let (ok, _, _) = aco(
        home.path(),
        &[
            "edit",
            "--agent",
            "codex",
            "--session",
            "abcdefab",
            "--delete",
            "kind:assistant",
            "--dry-run",
        ],
    );
    assert!(ok);
    assert_eq!(before, std::fs::read_to_string(&rollout).unwrap());

    // delete for real removes the assistant line.
    let (ok, out, err) = aco(
        home.path(),
        &[
            "edit",
            "--agent",
            "codex",
            "--session",
            "abcdefab",
            "--delete",
            "kind:assistant",
        ],
    );
    assert!(ok, "{err}");
    assert!(out.contains("removed line"), "{out}");
    let after = std::fs::read_to_string(&rollout).unwrap();
    assert!(!after.contains("release notes summary"), "{after}");
}
