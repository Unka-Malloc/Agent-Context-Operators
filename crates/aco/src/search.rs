//! `aco search` — grep-style scan across one or more agents' session
//! stores, with record-level context.

use std::path::{Path, PathBuf};

use regex::Regex;

use crate::{codec::Codec, store, RecordKind};

#[derive(Debug)]
pub struct SearchOptions {
    /// Literal needle, or a regex when `regex` is set.
    pub pattern: String,
    pub regex: bool,
    /// Records of context on each side of a hit.
    pub context: usize,
    /// Inclusive lower/upper bounds on record timestamps (lexicographic
    /// RFC 3339 compare; dates like `2026-08-01` prefix-match).
    pub since: Option<String>,
    pub until: Option<String>,
    /// Restrict hits to one record kind.
    pub kind: Option<RecordKind>,
}

#[derive(Debug)]
pub struct Hit {
    pub agent: String,
    pub session_id: String,
    pub path: PathBuf,
    pub line: usize,
    pub ordinal: u64,
    pub kind: RecordKind,
    pub ts: Option<String>,
    pub before: Vec<String>,
    pub text: String,
    pub after: Vec<String>,
}

#[derive(Debug, Default)]
pub struct SearchOutcome {
    pub hits: Vec<Hit>,
    pub files_scanned: usize,
    /// `(agent, path, error)` for session files that failed to decode.
    pub decode_errors: Vec<(String, PathBuf, String)>,
    /// `(agent, kinds)` history roots ACO cannot read at all.
    pub unreadable_roots: Vec<(String, Vec<&'static str>)>,
}

pub fn run(codecs: &[&dyn Codec], home: &Path, opts: &SearchOptions) -> SearchOutcome {
    let matcher = match opts.regex {
        true => Regex::new(&opts.pattern).ok(),
        false => Some(Regex::new(&regex::escape(&opts.pattern)).expect("escaped literal compiles")),
    };
    let mut out = SearchOutcome::default();
    let Some(matcher) = matcher else {
        out.decode_errors.push((
            String::new(),
            PathBuf::new(),
            format!("invalid regex `{}`", opts.pattern),
        ));
        return out;
    };

    for codec in codecs {
        let Ok(scan) = store::scan(*codec, home) else {
            continue;
        };
        if !scan.unreadable_roots.is_empty() {
            out.unreadable_roots
                .push((codec.id().to_string(), scan.unreadable_roots.clone()));
        }
        for file in scan.sessions {
            out.files_scanned += 1;
            match codec.decode(&file.path) {
                Ok(decoded) => collect_hits(*codec, &file, &decoded, &matcher, opts, &mut out.hits),
                Err(e) => out.decode_errors.push((
                    codec.id().to_string(),
                    file.path.clone(),
                    e.to_string(),
                )),
            }
        }
    }
    out
}

fn collect_hits(
    codec: &dyn Codec,
    file: &store::StoreFile,
    decoded: &crate::Decoded,
    matcher: &Regex,
    opts: &SearchOptions,
    hits: &mut Vec<Hit>,
) {
    let session_id = codec.session_id(&file.path);
    let texts: Vec<String> = decoded
        .conversation
        .records
        .iter()
        .map(|r| r.text())
        .collect();
    for (i, rec) in decoded.conversation.records.iter().enumerate() {
        if let Some(kind) = opts.kind {
            if rec.kind() != kind {
                continue;
            }
        }
        if let Some(ts) = &rec.ts {
            if let Some(since) = &opts.since {
                if ts.as_str() < since.as_str() {
                    continue;
                }
            }
            if let Some(until) = &opts.until {
                if ts.as_str() > until.as_str() {
                    continue;
                }
            }
        }
        if !matcher.is_match(&texts[i]) {
            continue;
        }
        let lo = i.saturating_sub(opts.context);
        let hi = (i + opts.context + 1).min(texts.len());
        hits.push(Hit {
            agent: codec.id().to_string(),
            session_id: session_id.clone(),
            path: file.path.clone(),
            line: decoded.record_lines.get(i).copied().unwrap_or(0) + 1,
            ordinal: rec.ordinal,
            kind: rec.kind(),
            ts: rec.ts.clone(),
            before: texts[lo..i].to_vec(),
            text: texts[i].clone(),
            after: texts[i + 1..hi].to_vec(),
        });
    }
}
