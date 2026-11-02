//! End-to-end CLI coverage over a synthetic home directory: search,
//! export, transform, import, edit, snapshot. All fixture content is
//! invented — no real conversation data.

use std::path::{Path, PathBuf};
use std::process::Command;

fn acop(home: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_acop"))
        .args(args)
        .env("HOME", home)
        .output()
        .expect("acop runs");
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
    let (ok, out, _) = acop(home.path(), &["list", "--json"]);
    assert!(ok);
    assert!(
        out.contains("abcdefab-1234-5678-9abc-def012345678"),
        "{out}"
    );

    // search hits with context.
    let (ok, out, _) = acop(
        home.path(),
        &["search", "release notes", "--agent", "codex"],
    );
    assert!(ok, "{out}");
    assert!(out.contains("2 hits"), "{out}");
    assert!(out.contains("release notes summary"), "{out}");

    // export produces the canonical document.
    let exported = home.path().join("conv.json");
    let (ok, _, err) = acop(
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
    assert!(doc.contains("acop-conversation/1"), "{doc}");

    // transform codex → claude writes native claude lines.
    let transformed = home.path().join("claude.jsonl");
    let (ok, _, err) = acop(
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
    let (ok, out, err) = acop(
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
    let (ok, out, err) = acop(
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
    let (ok, out, _) = acop(home.path(), &["snapshot", "create", "--all"]);
    assert!(ok, "{out}");
    let (ok, out, _) = acop(home.path(), &["snapshot", "list"]);
    assert!(ok, "{out}");
    assert!(out.contains("file"), "{out}");

    // dry-run edit leaves the file untouched.
    let before = std::fs::read_to_string(&rollout).unwrap();
    let (ok, _, _) = acop(
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
    let (ok, out, err) = acop(
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

#[test]
fn fork_paths() {
    let home = tempfile::tempdir().unwrap();
    seed_codex(home.path());

    // Same-agent fork of a file store → verbatim copy under a fresh id.
    let (ok, out, err) = acop(
        home.path(),
        &[
            "fork",
            "--agent",
            "codex",
            "--session",
            "abcdefab",
            "--json",
        ],
    );
    assert!(ok, "{err} {out}");
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["mode"], "verbatim");
    assert_eq!(report["records"], 4);
    let new_id = report["session_id"].as_str().unwrap();
    assert_ne!(new_id, "abcdefab-1234-5678-9abc-def012345678");
    let written = PathBuf::from(report["written"].as_str().unwrap());
    let text = std::fs::read_to_string(&written).unwrap();
    assert!(text.contains(new_id), "{text}");
    assert!(
        !text.contains("abcdefab-1234-5678-9abc-def012345678"),
        "{text}"
    );
    // The forked session is enumerable and decodable.
    let (ok, out, _) = acop(home.path(), &["list", "--json"]);
    assert!(ok);
    assert!(out.contains(new_id), "{out}");

    // --at truncates a verbatim fork to the leading lines.
    let (ok, out, err) = acop(
        home.path(),
        &[
            "fork",
            "--agent",
            "codex",
            "--session",
            "abcdefab",
            "--at",
            "1",
        ],
    );
    assert!(ok, "{err} {out}");
    assert!(out.contains("2 records"), "{out}");

    // Cross-agent fork → canonical projection into the claude store.
    let (ok, out, err) = acop(
        home.path(),
        &[
            "fork",
            "--agent",
            "codex",
            "--session",
            "abcdefab",
            "--to",
            "claude",
            "--json",
        ],
    );
    assert!(ok, "{err} {out}");
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["mode"], "canonical");
    let written = PathBuf::from(report["written"].as_str().unwrap());
    let text = std::fs::read_to_string(&written).unwrap();
    assert!(text.contains("\"type\":\"user\""), "{text}");

    // dry-run reports but writes nothing.
    let (ok, out, err) = acop(
        home.path(),
        &[
            "fork",
            "--agent",
            "codex",
            "--session",
            "abcdefab",
            "--dry-run",
            "--json",
        ],
    );
    assert!(ok, "{err}");
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["dry_run"], true);
    assert!(report["written"].is_null());
    assert!(!PathBuf::from(report["target"].as_str().unwrap()).exists());

    // Out-of-range fork point fails as a usage error.
    let (ok, _, err) = acop(
        home.path(),
        &[
            "fork",
            "--agent",
            "codex",
            "--session",
            "abcdefab",
            "--at",
            "99",
        ],
    );
    assert!(!ok);
    assert!(err.contains("out of range"), "{err}");
}

#[test]
fn archive_retains_transcript_and_marks_store() {
    let home = tempfile::tempdir().unwrap();
    let rollout = seed_codex(home.path());
    let transcript = home.path().join("audit/conversations/sess.json");

    // No --agent: the bare id resolves across codec stores.
    let (ok, out, err) = acop(
        home.path(),
        &[
            "archive",
            "--session",
            "abcdefab",
            "-o",
            transcript.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(ok, "{err} {out}");
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["agent"], "codex");
    assert_eq!(report["marked"], true, "codex archived_sessions applies");
    // Transcript landed as the canonical document.
    let doc = std::fs::read_to_string(&transcript).unwrap();
    assert!(doc.contains("acop-conversation/1"), "{doc}");
    // The store marker moved the rollout into archived_sessions — still
    // enumerable, gone from the live list dir.
    assert!(!rollout.exists());
    let archived = home
        .path()
        .join(".codex/archived_sessions")
        .join(rollout.file_name().unwrap());
    assert!(archived.exists(), "rollout relocated under archived root");
    let (ok, out, _) = acop(home.path(), &["list", "--json"]);
    assert!(ok);
    assert!(
        out.contains("abcdefab-1234-5678-9abc-def012345678"),
        "{out}"
    );

    // An id no store holds is a clean miss.
    let (ok, _, err) = acop(home.path(), &["archive", "--session", "no-such"]);
    assert!(!ok);
    assert!(err.contains("no-such"), "{err}");
}
