//! `acop` — Agent Context Operators CLI.
//!
//! Registry of the per-agent codecs plus the command-line surface:
//! `search`, `list`, `export`, `transform`, `import`, `fork`, `archive`,
//! `edit`, `snapshot`, `stores`, `capabilities`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use acop::edit::{self, EditOp};
use acop::search::{self, SearchOptions};
use acop::{ops, snapshot, Codec, Error, Selectors};

fn registry() -> Vec<&'static dyn Codec> {
    vec![
        acop_antigravity::codec(),
        acop_claude::codec(),
        acop_codex::codec(),
        acop_copilot::codec(),
        acop_cursor::codec(),
        acop_dsh::codec(),
        acop_hermes::codec(),
        acop_kilo_code::codec(),
        acop_kimi_code::codec(),
        acop_openclaw::codec(),
        acop_opencode::codec(),
        acop_pi::codec(),
    ]
}

fn codec_named(name: &str) -> Result<&'static dyn Codec, Error> {
    registry()
        .into_iter()
        .find(|c| c.id() == name)
        .ok_or_else(|| {
            Error::UnknownAgent(
                name.to_string(),
                registry()
                    .iter()
                    .map(|c| c.id())
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        })
}

/// `--agent a,b` or `--all` → codec subset.
fn codecs_for(agent: Option<&str>, all: bool) -> Result<Vec<&'static dyn Codec>, Error> {
    if all || agent.is_none() {
        return Ok(registry());
    }
    agent
        .unwrap()
        .split(',')
        .map(|n| codec_named(n.trim()))
        .collect()
}

struct Args {
    positional: Vec<String>,
    flags: std::collections::BTreeMap<String, Option<String>>,
}

impl Args {
    fn parse(argv: &[String]) -> Result<Self, Error> {
        let mut out = Args {
            positional: Vec::new(),
            flags: Default::default(),
        };
        // Flags that take no value.
        const BOOL: &[&str] = &["all", "json", "dry-run", "force", "E", "help"];
        let mut i = 0;
        while i < argv.len() {
            let arg = &argv[i];
            if let Some(name) = arg.strip_prefix("--") {
                let (name, inline) = match name.split_once('=') {
                    Some((n, v)) => (n.to_string(), Some(v.to_string())),
                    None => (name.to_string(), None),
                };
                let value = if inline.is_none() && !BOOL.contains(&name.as_str()) {
                    i += 1;
                    argv.get(i)
                        .cloned()
                        .ok_or_else(|| Error::Usage(format!("--{name} needs a value")))?
                } else {
                    inline.unwrap_or_default()
                };
                out.flags.insert(name, Some(value));
            } else if let Some(name) = arg.strip_prefix('-') {
                if name == "E" {
                    out.flags.insert("E".into(), Some(String::new()));
                } else if name.len() == 1 {
                    i += 1;
                    out.flags.insert(
                        name.to_string(),
                        Some(
                            argv.get(i)
                                .cloned()
                                .ok_or_else(|| Error::Usage(format!("-{name} needs a value")))?,
                        ),
                    );
                } else {
                    return Err(Error::Usage(format!("unknown flag {arg}")));
                }
            } else {
                out.positional.push(arg.clone());
            }
            i += 1;
        }
        Ok(out)
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.flags.get(name).and_then(|v| v.as_deref()).or_else(|| {
            if self.flags.contains_key(name) {
                Some("")
            } else {
                None
            }
        })
    }

    fn has(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }

    fn take(&mut self, name: &str) -> Option<String> {
        self.flags.remove(name).and_then(|v| v)
    }

    fn require(&self, name: &str) -> Result<&str, Error> {
        self.get(name)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Error::Usage(format!("missing --{name}")))
    }
}

fn print_projection(p: &acop::Projection) {
    for (ord, kind) in &p.dropped {
        eprintln!(
            "  dropped  record #{ord} ({}) — unsupported by target",
            kind.name()
        );
    }
    for (ord, kind) in &p.adapted {
        eprintln!("  adapted  record #{ord} ({})", kind.name());
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Usage(msg)) => {
            eprintln!("error: {msg}\n");
            eprint!("{}", usage());
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Error> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match argv.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => {
            eprint!("{}", usage());
            return Err(Error::Usage("missing command".into()));
        }
    };
    let mut args = Args::parse(rest)?;
    let home = acop::util::home()?;
    let agent = args.get("agent").map(String::from);
    let all = args.has("all");
    let json_out = args.has("json");

    match cmd {
        "search" => cmd_search(&args, agent.as_deref(), all, json_out, &home),
        "list" => cmd_list(&args, agent.as_deref(), all, json_out, &home),
        "export" => cmd_export(&args, &home),
        "transform" => cmd_transform(&args, &home),
        "import" => cmd_import(&args, &home),
        "fork" => cmd_fork(&args, &home, json_out),
        "archive" => cmd_archive(&args, &home, json_out),
        "edit" => cmd_edit(&args, &home),
        "snapshot" => cmd_snapshot(&mut args, &home),
        "stores" => cmd_stores(&args, agent.as_deref(), all, &home),
        "capabilities" => cmd_capabilities(agent.as_deref(), all),
        "help" | "--help" | "-h" => {
            print!("{}", usage());
            Ok(())
        }
        other => Err(Error::Usage(format!("unknown command `{other}`"))),
    }
}

fn cmd_search(
    args: &Args,
    agent: Option<&str>,
    all: bool,
    json_out: bool,
    home: &Path,
) -> Result<(), Error> {
    let pattern = args
        .positional
        .first()
        .ok_or_else(|| Error::Usage("search needs a pattern".into()))?;
    let opts = SearchOptions {
        pattern: pattern.clone(),
        regex: args.has("E"),
        context: args.get("C").and_then(|v| v.parse().ok()).unwrap_or(2),
        since: args.get("since").map(String::from),
        until: args.get("until").map(String::from),
        kind: args
            .get("kind")
            .map(|k| {
                acop::RecordKind::parse(k)
                    .ok_or_else(|| Error::Usage(format!("unknown kind `{k}`")))
            })
            .transpose()?,
    };
    let codecs = codecs_for(agent, all)?;
    let out = search::run(&codecs, home, &opts);

    if json_out {
        let hits: Vec<_> = out
            .hits
            .iter()
            .map(|h| {
                serde_json::json!({
                    "agent": h.agent, "session_id": h.session_id, "path": h.path,
                    "line": h.line, "ordinal": h.ordinal, "kind": h.kind.name(),
                    "ts": h.ts, "before": h.before, "text": h.text, "after": h.after,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "hits": hits,
                "files_scanned": out.files_scanned,
                "decode_errors": out.decode_errors,
                "unreadable_roots": out.unreadable_roots,
            }))
            .expect("json")
        );
        return Ok(());
    }

    println!(
        "🔍 {} hit{} across {} session file{}:",
        out.hits.len(),
        if out.hits.len() == 1 { "" } else { "s" },
        out.files_scanned,
        if out.files_scanned == 1 { "" } else { "s" },
    );
    for (i, h) in out.hits.iter().enumerate() {
        println!("{}", "=".repeat(65));
        println!(
            "### [{}] {} | agent: {} | {}#L{} (record #{}, {})",
            i + 1,
            h.ts.clone().unwrap_or_else(|| "no-ts".into()),
            h.agent,
            h.path.display(),
            h.line,
            h.ordinal,
            h.kind.name(),
        );
        for t in &h.before {
            println!("    {t}");
        }
        println!(">>> {}", h.text);
        for t in &h.after {
            println!("    {t}");
        }
    }
    for (agent, kinds) in &out.unreadable_roots {
        eprintln!(
            "note: {agent}: unreadable store kinds: {}",
            kinds.join(", ")
        );
    }
    for (agent, path, err) in &out.decode_errors {
        eprintln!("note: {agent}: {}: {err}", path.display());
    }
    Ok(())
}

fn cmd_list(
    _args: &Args,
    agent: Option<&str>,
    all: bool,
    json_out: bool,
    home: &Path,
) -> Result<(), Error> {
    let rows = ops::list(&codecs_for(agent, all)?, home);
    if json_out {
        println!("{}", serde_json::to_string_pretty(&rows).expect("json"));
        return Ok(());
    }
    for r in &rows {
        println!(
            "{:<9} {:<38} {:<20} {:>6}  {}",
            r.agent,
            r.session_id,
            r.started.clone().unwrap_or_else(|| "-".into()),
            r.records
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".into()),
            r.path.display(),
        );
    }
    Ok(())
}

fn write_doc(doc: &str, out: Option<&str>) -> Result<(), Error> {
    match out {
        Some("-") | None => println!("{doc}"),
        Some(path) => acop::util::atomic_write(Path::new(path), doc)?,
    }
    Ok(())
}

fn cmd_export(args: &Args, home: &Path) -> Result<(), Error> {
    let codec = codec_named(args.require("agent")?)?;
    let session = args.require("session")?;
    let (conv, _path) = ops::export(codec, home, session)?;
    write_doc(&conv.to_json(), args.get("o"))
}

fn cmd_transform(args: &Args, home: &Path) -> Result<(), Error> {
    let from = codec_named(args.require("from")?)?;
    let to = codec_named(args.require("to")?)?;
    let session = args.require("session")?;
    let (encoded, conv) = ops::transform(from, to, home, session)?;
    if !encoded.projection.is_clean() {
        eprintln!("projection {} → {}:", from.id(), to.id());
        print_projection(&encoded.projection);
    }
    match args.get("o") {
        Some("-") | None => {
            for line in &encoded.lines {
                println!("{line}");
            }
        }
        Some(path) => {
            acop::util::atomic_write(Path::new(path), &(encoded.lines.join("\n") + "\n"))?
        }
    }
    let _ = conv;
    Ok(())
}

fn cmd_import(args: &Args, home: &Path) -> Result<(), Error> {
    let target = codec_named(args.require("agent")?)?;
    let file = PathBuf::from(args.require("file")?);
    let from = args.get("from").map(codec_named).transpose()?;
    let report = ops::import(target, home, &file, from, args.has("dry-run"))?;
    if !report.projection.is_clean() {
        eprintln!("projection → {}:", target.id());
        print_projection(&report.projection);
    }
    if report.dry_run {
        println!(
            "dry-run: would import as session {} → {}",
            report.session_id,
            report.target.display()
        );
    } else {
        println!(
            "imported as session {} → {}",
            report.session_id,
            report.written.expect("written on non-dry-run").display()
        );
    }
    Ok(())
}

fn cmd_fork(args: &Args, home: &Path, json_out: bool) -> Result<(), Error> {
    let from = codec_named(args.require("agent")?)?;
    let to = args.get("to").map(codec_named).transpose()?.unwrap_or(from);
    let session = args.require("session")?;
    let at = args
        .get("at")
        .map(|v| {
            v.parse::<u64>()
                .map_err(|_| Error::Usage(format!("--at needs a record ordinal, got `{v}`")))
        })
        .transpose()?;
    let report = ops::fork(
        from,
        to,
        home,
        session,
        at,
        args.get("o").map(Path::new),
        args.has("dry-run"),
    )?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "mode": report.mode,
                "from_agent": report.from_agent,
                "to_agent": report.to_agent,
                "source_session": report.source_session,
                "source_path": report.source_path,
                "session_id": report.session_id,
                "target": report.target,
                "written": report.written,
                "records": report.records,
                "at": report.at,
                "dropped": report.projection.as_ref().map(|p| p.dropped.len()).unwrap_or(0),
                "adapted": report.projection.as_ref().map(|p| p.adapted.len()).unwrap_or(0),
                "dry_run": report.dry_run,
            }))
            .expect("json")
        );
    } else {
        if let Some(p) = &report.projection {
            if !p.is_clean() {
                eprintln!("projection {} → {}:", report.from_agent, report.to_agent);
                print_projection(p);
            }
        }
        let mode = serde_json::to_value(report.mode)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{:?}", report.mode));
        if report.dry_run {
            println!(
                "dry-run: would fork {}:{} → {} session {} ({}, {} records) at {}",
                report.from_agent,
                report.source_session,
                report.to_agent,
                report.session_id,
                mode,
                report.records,
                report.target.display(),
            );
        } else {
            println!(
                "forked {}:{} → {} session {} ({}, {} records) → {}",
                report.from_agent,
                report.source_session,
                report.to_agent,
                report.session_id,
                mode,
                report.records,
                report.written.expect("written on non-dry-run").display(),
            );
        }
    }
    Ok(())
}

fn cmd_archive(args: &Args, home: &Path, json_out: bool) -> Result<(), Error> {
    let session = args.require("session")?;
    // `--agent` scopes the lookup; without it every codec's store is
    // tried — Probator records ids without lane labels, so callers
    // holding just an id get the same archive behavior.
    let codec = match args.get("agent") {
        Some(name) => codec_named(name)?,
        None => registry()
            .into_iter()
            .find(|c| acop::store::resolve_session(*c, home, session).is_ok())
            .ok_or_else(|| Error::SessionNotFound(session.to_string(), "any".into()))?,
    };
    let report = ops::archive(
        codec,
        home,
        session,
        args.get("o").map(Path::new),
        args.has("dry-run"),
    )?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "agent": report.agent,
                "session": report.session,
                "source_path": report.source_path,
                "transcript": report.transcript,
                "transcript_written": report.transcript_written,
                "marked": report.marked,
                "dry_run": report.dry_run,
            }))
            .expect("json")
        );
        return Ok(());
    }
    let verb = if report.dry_run {
        "would archive"
    } else {
        "archived"
    };
    println!(
        "{verb} {}:{} → {}",
        report.agent,
        report.session,
        report.transcript.display(),
    );
    if !report.marked {
        eprintln!(
            "note: {}'s store has no archive marker — session stays listed",
            report.agent
        );
    }
    Ok(())
}

fn cmd_edit(args: &Args, home: &Path) -> Result<(), Error> {
    let codec = codec_named(args.require("agent")?)?;
    let session = args.require("session")?;
    let file = acop::store::resolve_session(codec, home, session)?
        .session
        .path;
    let op = match (args.get("delete"), args.get("redact")) {
        (Some(sel), None) => EditOp::Delete(Selectors::parse(sel)?),
        (None, Some(sel)) => EditOp::Redact {
            selectors: Selectors::parse(sel)?,
            marker: args
                .get("marker")
                .map(String::from)
                .unwrap_or_else(|| "<redacted>".into()),
        },
        _ => {
            return Err(Error::Usage(
                "edit needs exactly one of --delete <sel> or --redact <sel>".into(),
            ))
        }
    };
    let report = edit::apply(codec, &file, &op, home, args.has("dry-run"))?;
    let verb = if report.dry_run { "would " } else { "" };
    for l in &report.removed {
        println!("{verb}removed line {}", l + 1);
    }
    for l in &report.rewritten {
        println!("{verb}rewrote line {}", l + 1);
    }
    for c in &report.conflicts {
        println!("conflict: {c}");
    }
    if !report.unmatched.is_empty() {
        println!(
            "note: selected ordinals with no writable line: {:?}",
            report.unmatched
        );
    }
    if let Some(b) = &report.backup {
        println!("backup: {}", b.display());
    }
    Ok(())
}

fn cmd_snapshot(args: &mut Args, home: &Path) -> Result<(), Error> {
    match args.positional.first().map(String::as_str) {
        Some("create") => {
            let agent = args.take("agent");
            let codecs = codecs_for(agent.as_deref(), args.has("all"))?;
            let m = snapshot::create(home, &codecs)?;
            println!(
                "snapshot {} — {} file{}",
                m.name,
                m.entries.len(),
                if m.entries.len() == 1 { "" } else { "s" }
            );
            Ok(())
        }
        Some("list") | None => {
            for m in snapshot::list(home)? {
                let agents: std::collections::BTreeSet<_> =
                    m.entries.iter().map(|e| e.agent.as_str()).collect();
                println!(
                    "{}  {}  {} file{}  [{}]",
                    m.name,
                    m.created_at,
                    m.entries.len(),
                    if m.entries.len() == 1 { "" } else { "s" },
                    agents.into_iter().collect::<Vec<_>>().join(",")
                );
            }
            Ok(())
        }
        Some("restore") => {
            let name = args
                .positional
                .get(1)
                .ok_or_else(|| Error::Usage("snapshot restore needs a name".into()))?;
            let m = snapshot::restore(home, name, args.has("force"), args.has("dry-run"))?;
            let verb = if args.has("dry-run") {
                "would restore"
            } else {
                "restored"
            };
            println!(
                "{verb} {} file{} from {name}",
                m.entries.len(),
                if m.entries.len() == 1 { "" } else { "s" }
            );
            Ok(())
        }
        Some(other) => Err(Error::Usage(format!("unknown snapshot verb `{other}`"))),
    }
}

fn cmd_stores(_args: &Args, agent: Option<&str>, all: bool, home: &Path) -> Result<(), Error> {
    for row in ops::stores(&codecs_for(agent, all)?, home) {
        println!(
            "{:<9} {:<30} {:<10} {} {}",
            row.agent,
            row.kind,
            row.store,
            if row.sessions { "sessions" } else { "" },
            row.path.display(),
        );
    }
    Ok(())
}

fn cmd_capabilities(agent: Option<&str>, all: bool) -> Result<(), Error> {
    for codec in codecs_for(agent, all)? {
        println!("{}:", codec.id());
        for kind in acop::RecordKind::ALL {
            let cap = codec.capabilities().of(kind);
            println!(
                "  {:<12} {}",
                kind.name(),
                match cap {
                    acop::Capability::Native => "native",
                    acop::Capability::Adapted => "adapted",
                    acop::Capability::Unsupported => "unsupported",
                }
            );
        }
        if !codec.capabilities().writable {
            println!("  (read-only store)");
        }
    }
    Ok(())
}

fn usage() -> &'static str {
    "acop — Agent Context Operators\n\
     \n\
     USAGE\n\
     \x20 acop search <pattern> [--agent a,b|--all] [-E] [-C n] [--since d] [--until d] [--kind k] [--json]\n\
     \x20 acop list [--agent a,b|--all] [--json]\n\
     \x20 acop export --agent <a> --session <id|path> [-o <file>]\n\
     \x20 acop transform --from <a> --to <b> --session <id|path> [-o <file>]\n\
     \x20 acop import --agent <a> --file <path> [--from <a>] [--dry-run]\n\
     \x20 acop fork --agent <a> --session <id|path> [--to <b>] [--at <n>] [-o <file>] [--dry-run] [--json]\n\
     \x20 acop archive --session <id|path> [--agent <a>] [-o <file>] [--dry-run] [--json]\n\
     \x20 acop edit --agent <a> --session <id|path> --delete <sel>|--redact <sel> [--marker t] [--dry-run]\n\
     \x20 acop snapshot create [--agent a,b|--all] | list | restore <name> [--force] [--dry-run]\n\
     \x20 acop stores [--agent a,b|--all]\n\
     \x20 acop capabilities [--agent a,b|--all]\n\
     \n\
     SELECTORS (comma-union): kind:<tag> index:<a>[-<b>] id:<id> matching:<regex>\n\
     KINDS: user assistant reasoning tool-call tool-result system meta\n"
}
