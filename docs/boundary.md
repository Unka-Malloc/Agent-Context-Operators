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
`ahe::install` — `a4aa::context` re-exports the whole `aco` API, and the
per-agent `aco-<agent>` codec crates pull store locations from the same
`a4aa-<agent>` cards the runtime adapter already uses. One card, one
store-location authority, two consumers: the driver (a4aa) and the
artifact operators (aco).
