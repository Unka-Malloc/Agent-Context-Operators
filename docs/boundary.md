# Boundary: Context Operators vs Hook Engine

Two different questions get asked about agent conversations, and they
belong to two different systems.

| Question | Owner |
| --- | --- |
| "While the agent is *running* — intercept a turn, inject a message, gate a tool call" | **Agent Hook Engine** (`ahe`) — lifecycle hooks over the live session |
| "About a conversation that *already exists* — search it, move it between agents, edit out process state, snapshot it" | **Agent Context Operators** (`aco`) — operators over produced artifacts |

ACO reads and writes files inside declared session stores. It never
attaches to a running session, never evaluates hooks, and never injects
content a live agent will consume mid-turn. An `import` writes a *new*
session file into a store; whether and when an agent surfaces that
history to a future session is the agent's own behavior.

Conversely, AHE never rewrites history: its job ends where the artifact
begins. Editing a produced transcript and inserting into a running
conversation are different operations — the second is out of scope here.

## Aggregation

Hosts do not need to know this crate exists: `a4aa` aggregates it next to
`ahe::install` — `a4aa::context` re-exports the whole `aco` API. The
artifact vocabulary — `HistoryRoot`, `StoreKind`, `TranscriptUsage` — is
owned by `aco::store` and re-exported through `a4aa::card`, so a card and
a codec speak one type language. Each `a4aa-<agent>` card remains the
single data authority for where stores live and where persisted token
counters sit (`UsageSurface.transcript`); an `aco-<agent>` codec projects
the card's declaration directly — the types *are* ACO's, nothing is
translated — and exposes the usage spec through
`Codec::transcript_usage()` so a consumer decodes sessions through ACO
instead of keeping a parallel store adapter. One card, one data
authority, two consumers: the driver (a4aa) and the artifact operators
(aco).
