# Provider Runtime

## Decision

Provider protocol handling is split into two layers inside `greentyper-core`:

1. `provider::sse` frames bounded Server-Sent Events without knowing a Provider
   dialect.
2. `provider::responses` validates and assembles the supported OpenAI Responses
   streaming event subset into typed, dialect-scoped facts.

The Responses facts retain OpenAI response, item, output, and content indices.
They are not provider-neutral Runtime Events, canonical Items, Tool authority,
or durable state. The Runtime Kernel still uses the deterministic
`ProviderRuntime` simulator. No HTTP client, credential lookup, Provider
Profile routing, retry policy, product delivery, or Tool execution is wired to
this decoder yet.

## Interface

```rust
let mut decoder = ResponsesSseDecoder::new(max_output_bytes)?;
for chunk in transport_chunks {
    decoder.push(chunk)?;
}
let dialect_events = decoder.finish()?;
```

`SseParser` is separately reusable by transports that need only framing. Both
parsers become poisoned after an error so callers cannot continue from a state
whose byte or protocol position is uncertain.

## SSE Framing Contract

The generic framer:

- accepts LF, CRLF, and lone CR line endings, including boundaries split across
  chunks;
- waits for a complete line before validating UTF-8, so multi-byte characters
  may be split across transport chunks;
- ignores comments and unknown SSE fields;
- joins multiple `data` fields with a newline and defaults a missing or empty
  `event` field to `message`;
- enforces explicit total-stream and per-line byte limits plus at most 1024
  `data` lines per event; and
- rejects an unterminated final event rather than guessing at completion.

The acceptance transport benchmark uses this same core framer with its own
smaller benchmark limits. It no longer carries a second SSE implementation.

## Supported Responses Events

The first dialect slice recognizes:

- `response.created` and `response.in_progress`;
- `response.output_item.added` and `response.output_item.done` for message and
  function-call items;
- `response.content_part.added` and `response.content_part.done` for
  `output_text` content;
- `response.output_text.delta` and `response.output_text.done`;
- `response.function_call_arguments.delta` and
  `response.function_call_arguments.done`;
- `response.completed`, `response.failed`, and `response.incomplete`; and
- the top-level `error` event.

One decoded stream is limited to 4 MiB, each framed line to 1 MiB, and the
semantic stream to 4096 events. Output-item and content-part indices must be
below 1024. Accumulated function arguments are limited to 64 KiB and 64 nested
JSON levels. Text uses the lower of the decoder's caller-supplied limit and the
core 512 KiB maximum.

The decoder enforces these invariants:

1. Sequence numbers are strictly increasing; gaps are allowed.
2. A response or item must be introduced before its deltas or completion.
3. Item, output, and content identities must remain consistent.
4. Full text and function arguments supplied by `done` events must exactly
   match the accumulated deltas.
5. Function arguments are parsed only after completion, must be a JSON object,
   may be nested at most 64 levels, and are serialized into deterministic JSON
   key order.
6. `response.completed` requires every introduced output item to be complete.
   Failed, incomplete, and error terminals may end a partial stream.
7. Exactly one terminal event is allowed and no semantic event may follow it.
8. Unknown JSON fields are tolerated, while unknown event, item, or content
   kinds fail closed.

Usage preserves optional input, cached-input, cache-write, output, reasoning,
and total token counts plus optional service tier. Missing values remain
unknown instead of becoming zero. Debug output reports lengths and indices but
does not print Provider text, function arguments, identifiers, or error text.

## Tool Boundary

A decoded function call is only Provider data. A later adapter must convert it
into a Tool request through the Tool Runtime, where the current `AgentSession`,
canonical argument hash, resource binding, Capability Snapshot, Approval Grant,
durable `EffectPrepared` record, and reconciliation rules are enforced. The
Provider call ID cannot authorize or directly execute a Tool.

## Evidence

Redacted fixtures under `tests/fixtures/provider/responses/v1/` cover text and
function-call assembly, complete usage details, failed and incomplete
responses, top-level errors, unknown fields, chunk-split UTF-8, and service
tier. Module tests additionally cover line endings, sequence errors and gaps,
event-count, output, item, argument-byte, argument-depth, and SSE data-line
bounds, non-object arguments, terminal ordering, missing terminal events,
poisoning, optional usage, and redacted Debug output.

The event shapes are checked against the official
[OpenAI Responses streaming event reference](https://developers.openai.com/api/reference/resources/responses/streaming-events/).

## Still Pending

- A concrete HTTP/SSE transport, credential and origin binding, Provider
  Profiles, reconnect classification, and retry policy.
- Normalization from these dialect facts into provider-neutral Runtime Items,
  Runtime Events, and complete Usage Records.
- Reasoning, refusal, annotation, hosted-tool, and other Responses event kinds
  not listed above.
- Wiring function calls through Tool Runtime approval and effect execution, then
  continuing the Provider Turn canonically.
- Product presentation and acknowledgement, raw diagnostic artifact policy,
  fuzzing, reconnect fixtures, live credential-gated tests, and cross-process
  crash coverage around Provider delivery boundaries.
