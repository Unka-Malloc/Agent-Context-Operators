# The canonical conversation model — `acop-conversation/1`

Every operator speaks one normalized document. A codec decodes a native
session file into this model; export serializes it directly; transform
projects it through the target codec's capability matrix; import writes
it into a store under a fresh session id.

```json
{
  "format": "acop-conversation/1",
  "agent": "codex",
  "session_id": "…",
  "title": "…",
  "cwd": "…",
  "created_at": "…", "updated_at": "…",
  "records": [ … ]
}
```

`format` is the contract tag; readers reject anything else. `agent`
records the codec that produced the document (absent for hand-authored
documents).

## Records — the superset vocabulary

`Record` carries the store's own node identity when it has one
(`id`/`parent`, e.g. Claude `uuid`/`parentUuid`), an `ts` RFC 3339
timestamp when recorded, a sequential `ordinal`, and a tagged `body`.

| `body.kind` | Meaning |
| --- | --- |
| `user` | A human-authored turn (`text`, optional `attachments`) |
| `assistant` | A model-authored turn (`text`, optional `model`) |
| `reasoning` | Model thinking/CoT — a *process quantity* |
| `tool-call` | A tool/function invocation (`call_id`, `name`, `args`) |
| `tool-result` | The result returned to the agent (`call_id`, `content`, `is_error`) |
| `system` | Harness-level records — injected instructions, mode changes, lifecycle events (`subtype`, `text`) |
| `meta` | Store state the model never reads as conversation — `session_meta`, indexes, usage counters, titles, snapshots (`subtype`, `data`) |

Reasoning, tool calls/results and system records are the "process
quantities" fine-grained editing targets: they are conversation *process*
state, not user/assistant content.

## Capability matrix

Each codec declares per-kind support, the vocabulary AHE adaptation
already uses:

- `native` — the format expresses the kind directly
- `adapted` — carried through a documented adaptation (e.g. canonical
  `system` → codex `role:developer` message)
- `unsupported` — the format cannot carry it

Projection never drops silently: `transform`/`import` report every
adapted and dropped `(ordinal, kind)` pair. The matrix is the scoreboard
for how faithfully a store can carry a conversation; `acop capabilities`
prints it.

Current matrices:

| kind | codex | claude |
| --- | --- | --- |
| user | native | native |
| assistant | native | native |
| reasoning | adapted (summary only — encrypted blobs are not reproducible) | native |
| tool-call | adapted (`custom_tool_call`) | native |
| tool-result | adapted (`custom_tool_call_output`) | native |
| system | adapted (`role:developer`) | native |
| meta | unsupported | unsupported |
