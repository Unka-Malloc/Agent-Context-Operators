# ACO — Agent Context Operators

Operators over agent conversation artifacts: search, export, import,
transform, edit and snapshot, applied to the sessions agents have already
written to disk. ACO never touches a running conversation — runtime hook
and injection concerns belong to the Agent Hook Engine.

The canonical conversation model is a superset of every agent's native
record vocabulary. Per-agent codecs (`crates/aco-<agent>`) project that
superset onto the records each native format can carry; what a format
cannot express is declared `unsupported` in the codec's capability matrix
and reported — never silently dropped. The matrix is the scoreboard for
how faithfully each agent's store can carry a conversation.

## Commands

```
aco search <pattern> [--agent codex,claude|--all] [-E] [-C n] [--since <date>] [--kind user|assistant|reasoning|tool-call|tool-result|system|meta] [--json]
aco list [--agent <name>|--all]
aco export --agent <name> --session <id|path> [-o <file>] [--format aco-json]
aco transform --from <name> --to <name> --session <id|path> [-o <file>]
aco import --agent <name> --file <path> [--from <name>] [--dry-run]
aco edit --agent <name> --session <id|path> --delete <sel> | --redact <sel> [--marker <text>] [--dry-run]
aco snapshot create --agent <name>|--all | list | restore <name> [--dry-run]
```

Selectors (`<sel>`): `kind:<tag>`, `index:<a>-<b>`, `id:<record-id>`,
`matching:<regex>` — comma-combinable. Edit snapshots the affected file
before writing.

Hosts embedding `a4aa` get these operators for free — `a4aa::context`
re-exports the whole `aco` API next to `a4aa::hooks`. ACO owns the
artifact vocabulary (`HistoryRoot`, `StoreKind`, `TranscriptUsage`,
re-exported through `a4aa::card`); each `aco-<agent>` codec draws store
locations and transcript-usage specs from the same `a4aa-<agent>` card
the runtime adapter uses — one data authority, two consumers.

## Crate layout

- `crates/aco` — canonical model, store discovery over the codec's
  declared `HistoryRoot`s, the `Codec` trait and capability matrix,
  selectors, search/edit/snapshot engines, the `aco-conversation/1`
  document format.
- `crates/aco-cli` — the `aco` binary and the codec registry.
- `crates/aco-<agent>` — one codec per agent (`aco-codex`, `aco-claude`,
  `aco-dsh` today); depends on `aco` and the matching `a4aa-<agent>` store
  descriptor only.

## Install

```
cargo install --path crates/aco-cli
```

## Run and verify

```
scripts/verify.sh    # fmt + clippy + tests
```

## Documentation

Entry point: [docs/README.md](docs/README.md).
