# Operators

All operators resolve stores through the codec's `history()` — the
`a4aa-<agent>` card's declared `HistoryRoot`s, projected into ACOP's
vocabulary. Only roots the codec lists in `session_kinds` are treated as
conversations; other declared roots (prompt history, memory, indexes)
appear in `acop stores` but are never searched as sessions. Roots whose
store kind ACOP cannot read (SQLite, opaque directories) are reported as
unreadable, not silently skipped.

`--session` accepts a path, an exact session id, or an unambiguous
id/stem fragment; ambiguity fails with the candidate list.

## search

```
acop search <pattern> [--agent a,b|--all] [-E] [-C n] [--since d] [--until d] [--kind k] [--json]
```

Decodes every session file of the selected agents and matches against
each record's flat text (literal substring, or regex with `-E`). Hits
print with `-C` records of context, the file path + line, record
ordinal, kind and timestamp. `--since/--until` are lexicographic RFC
3339 bounds (a date like `2026-08-01` prefix-matches). Files that fail
to decode are reported as notes, never fatal.

## list

```
acop list [--agent a,b|--all] [--json]
```

Session inventory: id, first timestamp, decoded record count, path.

## export

```
acop export --agent <a> --session <id|path> [-o <file>]
```

Writes the canonical `acop-conversation/1` document (stdout without
`-o`).

## transform

```
acop transform --from <a> --to <b> --session <id|path> [-o <file>]
```

Decodes with the source codec, re-targets the document, encodes with
the target codec and writes the *native* lines. The projection report
on stderr names every adapted/dropped record. Transform output is a
native file — feed it to `acop import --from <b>` to install it.

## import

```
acop import --agent <a> --file <path> [--from <a>] [--dry-run]
```

Reads a canonical document by default, or a native file with `--from`.
The session id is **always regenerated** — import never collides with
or overwrites an existing session. The codec decides the store layout:
codex lands at `~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-<ts>-<uuid>.jsonl`,
claude at `~/.claude/projects/<cwd-slug>/<uuid>.jsonl` (`-acop-imported`
when no cwd is known). `--dry-run` prints the target path and projection
without writing.

## fork

```
acop fork --agent <a> --session <id|path> [--to <b>] [--at <n>] [-o <file>] [--dry-run] [--json]
```

Derives a new session from an existing one — the artifact-level
counterpart of an agent's own session-fork. The child gets a **fresh
session id** (never collides with the parent), lands in `--to`'s store
(default: same agent) through the codec's `store_path`, and `--at <n>`
cuts the history after canonical record ordinal `n` (inclusive);
without it the whole history is inherited.

Three paths, in order — "native where offered, honest projection
otherwise":

1. **native** — the format carries a derivation shape. `dsh` writes a
   real fork header: `id` rewritten, `inheritedEventCount` set to
   `max kept seq + 1`, event rows byte-verbatim — the same shape
   `subagent-fork-in-process` produces at runtime. Compressed
   (`.zstd`) generations decompress transparently; the child is
   written plain.
2. **verbatim** — file stores with no native fork shape. The artifact
   is copied (truncated to the fork point when `--at` is given) and
   every occurrence of the source session id is rewritten. claude,
   codex, pi, kimi-code, cursor file chats, copilot and the generic
   document stores take this path. The agent sees a history
   byte-identical to the parent's own.
3. **canonical** — the only cross-agent path, and the fallback where
   bytes cannot be copied: container rows (cursor `store.db`,
   kilo-code's sqlite store) project through `encode`, and the
   projection report lists records the target format cannot carry.
   Read-only stores (kilo-code) are refused cleanly.

Provenance: the canonical document marks derivation in `forked_from`
(`agent`, `session_id`, `at`); native shapes carry it in their own
fields where the format has them. Whether a resumed fork re-reads its
history as an identical rendered prefix — and therefore hits provider
prompt caches — is the agent's serialization property, not ACOP's; fork
guarantees the artifact is correct, not that a cache warms.

## archive

```
acop archive --session <id|path> [--agent <a>] [-o <file>] [--dry-run] [--json]
```

Retains a finished session: the canonical transcript is exported to
`-o` (default `~/.local/state/acop/archive/<agent>/<sha256>.json`, the
SHA-256 hex of the session id — the id comes from artifact content and
never becomes a path; the report carries it), then the agent store's **own** archive marker is flipped where one
exists — codex rollouts relocate under `~/.codex/archived_sessions/`
(`thread/unarchive` is the return path), opencode `session_v2` rows get
`time_archived`. Stores with no marker (claude, dsh, pi, …) report
`marked: false` honestly — the transcript is the whole archive and the
session stays listed rather than pretending to hide.

`--agent` is optional: with only a bare id every codec's store is
tried in turn — callers holding an unlabeled session id get the same
behavior without naming the lane.

## edit

```
acop edit --agent <a> --session <id|path> (--delete <sel> | --redact <sel> [--marker t]) [--dry-run]
```

Fine-grained mutation of one produced file, line-granular over the
native JSONL store:

- `--delete` removes every line *all* of whose decoded records the
  selector matches. A line that produced several records (e.g. one
  claude message fanning out into text+thinking+tool_use) conflicts on
  a partial selection — it is kept and reported, never partially
  rewritten.
- `--redact` asks the codec to blank the line's textual payload in
  place — message text, thinking/reasoning, tool arguments and outputs
  (codex `encrypted_content` is cleared too). The record stays; the
  content is gone. Default marker is `<redacted>`.

Every write is atomic and preceded by a backup under
`~/.local/state/acop/backups/<ts>-<rand>/` mirroring the absolute path.
Unparseable lines pass through untouched.

### Selectors

Comma-union of `kind:<tag>`, `index:<a>` or `index:<a>-<b>`,
`id:<record-id>`, `matching:<regex>` over record ordinals/text.
Kinds: `user assistant reasoning tool-call tool-result system meta`.

## snapshot

```
acop snapshot create [--agent a,b|--all]
acop snapshot list
acop snapshot restore <name> [--force] [--dry-run]
```

`create` copies every readable session file of the selected agents into
`~/.local/state/acop/snapshots/<ts>-<rand>/` plus a `manifest.json`
recording each file's source. `restore` copies them back; an existing
target refuses unless `--force` — the target is itself user data.
`<name>` must be one snapshot directory directly under `snapshots/`, and
every stored file must resolve inside it; the recorded sources are the
write targets the operator approves by naming the snapshot.
Snapshots are local copies only; nothing is uploaded or deduplicated.

## stores / capabilities

`acop stores` inventories every declared history root per agent (kind,
store kind, session flag, expanded path). `acop capabilities` prints each
codec's per-kind matrix — the scoreboard for what a format can carry.
