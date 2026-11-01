# Agent rules for Agent Context Operators

## Boundary (hard rule)

ACO operates on conversation artifacts an agent has already produced:
files and stores on disk. Runtime concerns — hook evaluation, injecting
messages into a live session, intercepting a running turn — belong to the
Agent Hook Engine, never to this repository. Editing a produced artifact
and inserting into a running conversation are different operations; the
second one is out of scope here.

## Codec independence (hard rule)

Every agent's conversation format gets its own codec crate under
`crates/aco-<agent>`. A codec crate depends on `aco` core and on the
matching `a4aa-<agent>` descriptor (store locations and transcript-usage
specs — the card stays the single data authority; the `HistoryRoot`/
`StoreKind`/`TranscriptUsage` types are `aco::store`'s own, re-exported
through `a4aa::card`, so a projection is a field read, not a rewrite),
and on nothing else.
Working on one codec must not require changes to a sibling codec.

The canonical conversation model is a **superset** of every agent's
native record vocabulary. Per-agent codecs project that superset onto
what the native format can carry; kinds a format cannot express are
declared `unsupported` in the capability matrix and reported on export,
transform and import — never silently dropped.

## Privacy (hard rule)

Conversation artifacts are user data. Tests, fixtures and examples use
synthetic content only; real session files are never copied into the
repository or into reports. Search and export operate locally; nothing
in this crate transmits conversation content anywhere.

## Verify

`scripts/verify.sh` — fmt + clippy + tests across the workspace.

The workspace builds against one sibling checkout — Cargo path
dependencies resolve it at the version `Cargo.toml` declares:

| Path | Repository | Provides |
| --- | --- | --- |
| `../Adapter-For-All-Agents` | `Unka-Malloc/Adapter-For-All-Agents` | `a4aa`, `a4aa-<agent>` |

Clone it beside this repository before building; a fresh clone without
it cannot compile.
