# Operators

All operators resolve stores through the codec's `history()` — the
`a4aa-<agent>` card's declared `HistoryRoot`s, projected into ACO's
vocabulary. Only roots the codec lists in `session_kinds` are treated as
conversations; other declared roots (prompt history, memory, indexes)
appear in `aco stores` but are never searched as sessions. Roots whose
store kind ACO cannot read (SQLite, opaque directories) are reported as
unreadable, not silently skipped.

`--session` accepts a path, an exact session id, or an unambiguous
id/stem fragment; ambiguity fails with the candidate list.

## search

```
aco search <pattern> [--agent a,b|--all] [-E] [-C n] [--since d] [--until d] [--kind k] [--json]
```

Decodes every session file of the selected agents and matches against
each record's flat text (literal substring, or regex with `-E`). Hits
print with `-C` records of context, the file path + line, record
ordinal, kind and timestamp. `--since/--until` are lexicographic RFC
3339 bounds (a date like `2026-08-01` prefix-matches). Files that fail
to decode are reported as notes, never fatal.

## list

```
aco list [--agent a,b|--all] [--json]
```

Session inventory: id, first timestamp, decoded record count, path.

## export

```
aco export --agent <a> --session <id|path> [-o <file>]
```

Writes the canonical `aco-conversation/1` document (stdout without
`-o`).

## transform

```
aco transform --from <a> --to <b> --session <id|path> [-o <file>]
```

Decodes with the source codec, re-targets the document, encodes with
the target codec and writes the *native* lines. The projection report
on stderr names every adapted/dropped record. Transform output is a
native file — feed it to `aco import --from <b>` to install it.

## import

```
aco import --agent <a> --file <path> [--from <a>] [--dry-run]
```

Reads a canonical document by default, or a native file with `--from`.
The session id is **always regenerated** — import never collides with
or overwrites an existing session. The codec decides the store layout:
codex lands at `~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-<ts>-<uuid>.jsonl`,
claude at `~/.claude/projects/<cwd-slug>/<uuid>.jsonl` (`-aco-imported`
when no cwd is known). `--dry-run` prints the target path and projection
without writing.

## edit

```
aco edit --agent <a> --session <id|path> (--delete <sel> | --redact <sel> [--marker t]) [--dry-run]
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
`~/.local/state/aco/backups/<ts>-<rand>/` mirroring the absolute path.
Unparseable lines pass through untouched.

### Selectors

Comma-union of `kind:<tag>`, `index:<a>` or `index:<a>-<b>`,
`id:<record-id>`, `matching:<regex>` over record ordinals/text.
Kinds: `user assistant reasoning tool-call tool-result system meta`.

## snapshot

```
aco snapshot create [--agent a,b|--all]
aco snapshot list
aco snapshot restore <name> [--force] [--dry-run]
```

`create` copies every readable session file of the selected agents into
`~/.local/state/aco/snapshots/<ts>-<rand>/` plus a `manifest.json`
recording each file's source. `restore` copies them back; an existing
target refuses unless `--force` — the target is itself user data.
Snapshots are local copies only; nothing is uploaded or deduplicated.

## stores / capabilities

`aco stores` inventories every declared history root per agent (kind,
store kind, session flag, expanded path). `aco capabilities` prints each
codec's per-kind matrix — the scoreboard for what a format can carry.
