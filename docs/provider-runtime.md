# Provider Runtime

## Decision

Provider protocol handling is split into two layers inside `greentyper-core`:

1. `provider::sse` frames bounded Server-Sent Events without knowing a Provider
   dialect.
2. `provider::responses` validates and assembles the supported OpenAI Responses
   streaming event subset into typed, dialect-scoped facts.

The Responses facts retain OpenAI response, item, output, and content indices.
They are not Runtime authority or durable state. A separate normalizer reduces
the supported terminal stream to provider-neutral text deltas, one canonical
function call, and one optional Usage Record. The Runtime Kernel can drive that
neutral interface through Tool Runtime approval and one Tool continuation. The
product has a configured Responses HTTP adapter that sends one request through
a no-proxy, no-redirect blocking client, streams the response through this
decoder, and drives the single-Agent Runtime. Config Runtime resolves the
selected Provider Profile and freezes its normalized origin, Responses route,
dialect, pricing source, and opaque credential reference in the Provider Epoch.
The release-bundled OpenAI template now supplies those defaults; the adapter
admits it and explicit compatible gateways only after the frozen Profile declares
Responses support and a Responses endpoint. Release DeepSeek and OpenCode Go
records remain catalog facts until their selected dialect has a product adapter.
Before each request, the adapter resolves secret material from an origin-bound
product vault; remote origins require HTTPS. The headless CLI uses this adapter
for configured profiles and retains the deterministic simulator only when no
custom profile is selected. Windows Credential Manager is the current platform
backend; non-Windows product credential access fails closed. Retry, reconnect,
live-provider validation, and broader Tool presentation remain separate work.
The Kernel durably brackets each request and continuation as a separate Usage
Attempt before invoking this adapter, so transport failure, interruption,
successful usage, and replay remain distinguishable without persisting raw
Provider events.

## Interface

```rust
let mut decoder = ResponsesSseDecoder::new(max_output_bytes)?;
for chunk in transport_chunks {
    decoder.push(chunk)?;
}
let dialect_events = decoder.finish()?;
let provider_events = normalize_responses_events(&dialect_events)?;
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

`normalize_responses_events` removes Responses wire identities that the Kernel
does not need, preserves the Provider call ID only as stable correlation data,
maps supported usage fields without fabricating missing values, and classifies
failed, incomplete, or error terminals without persisting upstream free text.

## Tool Boundary

A decoded function call remains only Provider data. The Kernel's explicit
Provider Turn driver requires a current `AgentSession`, maps the call to a
stable Tool identity, and asks the caller to resolve raw resource descriptors.
Tool Runtime then enforces canonical argument hashing, resource binding, the
Capability Snapshot, an Approval Grant, durable `EffectPrepared`, and
reconciliation. The Provider call ID cannot authorize or directly execute a
Tool.

The current tracer bullet supports at most one function call in a Turn. After a
successful UTF-8 Tool result, `ProviderRuntime::continue_after_tool` supplies a
second neutral Provider step and the Kernel prepares one canonical assistant
output containing both text phases and both Usage Records. A denied, failed,
ambiguous, oversized, or non-UTF-8 result never reaches Provider continuation.
If the process dies after a durable Tool success but before continuation, the
raw result is intentionally unavailable after restart: recovery blocks the
Turn and never invokes the successful effect again.

When `local.echo` is enabled, the HTTP adapter advertises the Responses-safe
function name `local_echo`, maps it back to the stable product Tool identity
`local.echo`, and rejects every unconfigured returned Tool. Continuation sends
one `function_call_output` item correlated by the Provider call ID and the
previous response ID. Those response identifiers remain process-local; they
are not authority and are not written to the Runtime or Tool Ledger.

The product `ProductDriver` owns the narrow user-visible path. It restores the
Kernel-derived Agent Session, presents and flushes any durable Team operation
receipt before acknowledgement, presents the exact arguments and resources,
accepts only explicit approval or denial, then delegates effect ordering back
to the Kernel. Final canonical output is acknowledged only after stdout flush.

## Evidence

Redacted fixtures under `tests/fixtures/provider/responses/v1/` cover text and
function-call assembly, complete usage details, failed and incomplete
responses, top-level errors, unknown fields, chunk-split UTF-8, and service
tier. Module tests additionally cover line endings, sequence errors and gaps,
event-count, output, item, argument-byte, argument-depth, and SSE data-line
bounds, non-object arguments, terminal ordering, missing terminal events,
poisoning, optional usage, and redacted Debug output.

The Kernel tracer-bullet test decodes a first fixture containing text and one
function call, durably approves and executes one injected Tool, decodes a
continuation fixture, prepares and acknowledges the combined output, and then
replays all three Ledgers. Companion tests cover stale Sessions, ambiguous Tool
effects, non-UTF-8 Tool output, and process death after durable Tool success
without effect repetition.

Product integration tests exercise the adapter against an actual loopback TCP
server. Config Runtime resolves the fixture profile and the adapter uses its
frozen Responses endpoint; the server validates that route, model, input,
streaming flag, and synthetic Authorization header, then fragments a bounded
SSE response across network writes. Tests prove canonical Runtime output and
replay, fixed classification for HTTP 503 and request timeout, and exclusion of
the upstream error body from stderr and the Runtime Ledger. Module tests drive
a locally generated HTTPS certificate through an explicitly trusted test root,
reject the same certificate under default trust, verify endpoint and status
classification, require an origin-bound credential before network access, and
redact Authorization. The fixture remains synthetic and does not constitute a
live-provider test.

The product also has an explicit Provider connection-test port and one current
HTTP adapter for OpenAI-compatible Profiles. It sends one bounded GET to the
frozen Profile's configured `models` route, with proxy discovery and redirects
disabled and the same origin-bound credential policy as inference. It does not
read the response body, discover models, mutate Config, retry, or expose the
endpoint or credential in its status. The terminal-neutral Provider Profile
wizard can test an uncommitted validated candidate; `config test-provider` tests
the currently selected committed Profile.

A two-request HTTP test verifies the exact function definition, initial Tool
call stream, canonical `local.echo` mapping, approved output correlation,
previous-response continuation request, final text, and both Usage Records.
Runtime tests additionally verify that those two records become two immutable
Usage Attempts and that a process interruption closes the prior attempt before
an explicit resume starts a new one.
Product-driver tests separately prove denial invokes no executor and an
interrupted approval can be re-presented after reopen before exactly one
effect. A binary integration test verifies the opt-in driver creates and
replays its dedicated Team and Tool Ledgers while preserving stdout delivery
and final `ready` status.

The event shapes are checked against the official
[OpenAI Responses streaming event reference](https://developers.openai.com/api/reference/resources/responses/streaming-events/).

## Still Pending

- Live credential-gated provider validation, live catalog discovery/refresh,
  starter-preset acceptance, configurable proxy policy, broader TLS platform
  evidence, reconnect classification, and retry policy. Release Provider
  Template defaults and seed catalog facts are bundled, but the current adapter
  does not reconnect or retry partial streams.
- Broader normalization into the eventual provider-neutral canonical Item
  model, including reasoning, refusal, annotations, and hosted Tools.
- Reasoning, refusal, annotation, hosted-tool, and other Responses event kinds
  not listed above.
- Multiple Tool calls, parallel calls, persisted resumable Provider
  continuation data, and durable storage of a redacted Tool result reference.
- Rich TUI/App Server Tool presentation, non-Windows credential backends, raw
  diagnostic artifact policy, fuzzing, reconnect fixtures, live
  credential-gated tests, and cross-process crash coverage around Provider and
  presentation boundaries. The current CLI interaction is intentionally one
  fixed `local.echo` approval prompt.
