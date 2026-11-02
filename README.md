# ACOP — Agent Context Operators

Operators over agent conversation artifacts: search, export, import,
transform, edit and snapshot, applied to the sessions agents have already
written to disk. ACOP never touches a running conversation — runtime hook
and injection concerns belong to the Agent Hook Engine.

The canonical conversation model is a superset of every agent's native
record vocabulary. Per-agent codecs (`crates/acop-<agent>`) project that
superset onto the records each native format can carry; what a format
cannot express is declared `unsupported` in the codec's capability matrix
and reported — never silently dropped. The matrix is the scoreboard for
how faithfully each agent's store can carry a conversation.

## Commands

```
acop search <pattern> [--agent codex,claude|--all] [-E] [-C n] [--since <date>] [--kind user|assistant|reasoning|tool-call|tool-result|system|meta] [--json]
acop list [--agent <name>|--all]
acop export --agent <name> --session <id|path> [-o <file>] [--format acop-json]
acop transform --from <name> --to <name> --session <id|path> [-o <file>]
acop import --agent <name> --file <path> [--from <name>] [--dry-run]
acop edit --agent <name> --session <id|path> --delete <sel> | --redact <sel> [--marker <text>] [--dry-run]
acop snapshot create --agent <name>|--all | list | restore <name> [--dry-run]
```

Selectors (`<sel>`): `kind:<tag>`, `index:<a>-<b>`, `id:<record-id>`,
`matching:<regex>` — comma-combinable. Edit snapshots the affected file
before writing.

Hosts embedding `a4aa` get these operators for free — `a4aa::context`
re-exports the whole `acop` API next to `a4aa::hooks`. ACOP owns the
artifact vocabulary (`HistoryRoot`, `StoreKind`, `TranscriptUsage`,
re-exported through `a4aa::card`); each `acop-<agent>` codec draws store
locations and transcript-usage specs from the same `a4aa-<agent>` card
the runtime adapter uses — one data authority, two consumers.

## Crate layout

- `crates/acop` — canonical model, store discovery over the codec's
  declared `HistoryRoot`s, the `Codec` trait and capability matrix,
  selectors, search/edit/snapshot engines, the `acop-conversation/1`
  document format.
- `crates/acop-cli` — the `acop` binary and the codec registry.
- `crates/acop-<agent>` — one codec per agent (`acop-antigravity`,
  `acop-claude`, `acop-codex`, `acop-copilot`, `acop-cursor`, `acop-dsh`,
  `acop-hermes`, `acop-kilo-code`, `acop-kimi-code`, `acop-openclaw`,
  `acop-opencode`, `acop-pi`); depends on `acop` and the matching
  `a4aa-<agent>` store descriptor only.

## Install

```
cargo install --path crates/acop-cli
```

## Run and verify

```
scripts/verify.sh    # fmt + clippy + tests
```

## Documentation

Entry point: [docs/README.md](docs/README.md).
