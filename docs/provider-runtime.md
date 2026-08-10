# Provider Runtime

## Decision

Provider protocol handling is split into a transport layer and dialect-scoped
decoders inside `greentyper-core`:

1. `provider::sse` frames bounded Server-Sent Events without knowing a Provider
   dialect.
2. `provider::responses` validates and assembles the supported OpenAI Responses
   streaming event subset into typed, dialect-scoped facts.
3. `provider::chat_completions` validates and assembles the supported OpenAI
   Chat Completions streaming subset into separate typed facts.
4. `provider::messages` validates and assembles the supported Anthropic
   Messages streaming subset into its own typed facts.

The dialect facts retain their wire identities and ordering data. They are not
Runtime authority or durable state. Separate normalizers reduce either supported
terminal stream to provider-neutral text deltas, one canonical function call,
and one optional Usage Record. The Runtime Kernel can drive that neutral
interface through Tool Runtime approval and one Tool continuation. The product
has configured OpenAI/openai-compatible Responses and Chat Completions HTTP
adapters plus one DeepSeek Messages adapter. Each uses a
no-proxy, no-redirect blocking client, streams the response through its matching
decoder, and drives the single-Agent Runtime. Config Runtime resolves the
selected Provider Profile and freezes its normalized origin, declared routes,
explicit dialect, pricing source, and opaque credential reference in the
Provider Epoch.
The release-bundled OpenAI template now supplies those defaults; the adapters
admit it and explicit compatible gateways only after the frozen Profile declares
the selected dialect and its endpoint. Adapter selection never infers one
dialect from another. DeepSeek Responses/Chat Completions and every OpenCode Go
record remain catalog facts until their template and selected dialect have an
explicit product adapter.
Before each request, the selected adapter resolves secret material from an
origin-bound product vault; remote origins require HTTPS. The headless CLI selects the
configured OpenAI adapter with `--dialect responses` or
`--dialect chat_completions`, or the configured DeepSeek adapter with
`--dialect messages`, and retains the deterministic simulator only when no
custom profile is selected. Windows
Credential Manager is the current platform
backend; non-Windows product credential access fails closed. Retry, reconnect,
live-provider validation, and broader Tool presentation remain separate work.
The Kernel durably brackets each request and continuation as a separate Usage
Attempt before invoking this adapter, so transport failure, interruption,
successful usage, and replay remain distinguishable without persisting raw
Provider events.
When Config supplies a matching Price Schedule, the Runtime freezes the resolved
schedule book in the Config Epoch and appends a separate pay-as-you-go cost
evaluation after normalized Usage. This is provider-neutral accounting: the HTTP
adapter neither calculates cost nor turns a catalog price or subscription quota
into a provider-reported charge.

## Interface

```rust
let mut decoder = ResponsesSseDecoder::new(max_output_bytes)?;
for chunk in transport_chunks {
    decoder.push(chunk)?;
}
let dialect_events = decoder.finish()?;
let provider_events = normalize_responses_events(&dialect_events)?;
```

Chat Completions follows the same shape with
`ChatCompletionsSseDecoder` and `normalize_chat_completions_events`; the two
dialect event types are intentionally not interchangeable. Messages uses
`MessagesSseDecoder` and `normalize_messages_events`; none of the three wire
event types are interchangeable.

`SseParser` is separately reusable by transports that need only framing. The
framer and all three dialect decoders become poisoned after an error so callers
cannot continue from a state whose byte or protocol position is uncertain.

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

## Supported Chat Completions Events

The Chat Completions slice accepts `data` messages containing one streamed
completion with a stable completion ID, model, and optional service tier. It
supports exactly one choice at index 0, assistant-role content deltas, one
fragmented function `tool_calls` entry at index 0, a usage-only chunk after the
choice finishes, and the terminal `[DONE]` sentinel. `stop` and `tool_calls` are
complete finish reasons; `length` and `content_filter` produce a fixed
incomplete classification rather than successful canonical output.

One decoded stream has the same 4 MiB total, 1 MiB line, 4096-event, 64 KiB
argument, 64-level argument-depth, and caller-bounded text limits as the
Responses decoder. The decoder rejects multiple choices or Tool calls, changed
wire identity, changed service tier, usage before choice completion, duplicate
usage, content after completion, missing or duplicate terminal state, refusal
deltas, and the deprecated `function_call` shape. Function arguments must form
one canonical JSON object. Missing usage fields remain unknown. Supported usage
preserves prompt, cached-prompt, completion, reasoning-completion, and total
tokens plus optional service tier without inventing cache-write counts.

`normalize_chat_completions_events` removes Chat chunk identities, preserves the
Tool call ID only as correlation data, and emits the same provider-neutral
`TextDelta`, `FunctionCall`, and `Completed` facts used by the Kernel. Debug and
error paths report only bounded categories or byte counts, never Provider text,
arguments, identifiers, or upstream error bodies.

## Supported Messages Events

The Messages slice accepts the ordered Anthropic stream:

1. one `message_start` with a stable message ID, model, assistant role, empty
   initial content, and optional input/cache usage;
2. contiguous `content_block_start`, `content_block_delta`, and
   `content_block_stop` sequences for text or one `tool_use` block;
3. one `message_delta` with a supported stop reason and cumulative output
   usage; and
4. one terminal `message_stop`.

Bounded `ping` events are ignored. A bounded top-level `error` becomes a fixed
unavailable classification without exposing its body. Text deltas and partial
Tool JSON may cross transport chunks. Tool input is parsed only when its block
stops, must be one JSON object, and is canonicalized before entering the neutral
Provider seam. Anthropic's uncached `input_tokens`, cache-read tokens, and
cache-write tokens are checked and summed into the provider-neutral total input;
output is then checked into total tokens. Cache classes remain separate for
pricing evidence. An omitted optional cache field stays unknown in that class,
but contributes no reported cache tokens, so known uncached input is not lost.

The decoder uses the same 4 MiB stream, 1 MiB line, 4096-event, 64 KiB Tool
input, 64-level Tool-input depth, and caller-bounded text limits. It rejects
changed identity, non-contiguous or overlapping blocks, multiple Tool calls,
delta/block type mismatches, decreasing cumulative output usage, duplicate or
post-terminal events, unsupported content blocks or stop reasons, and missing
terminal state. `max_tokens` and context-window stops become incomplete rather
than successful output. Unknown JSON fields are tolerated; unknown semantic
event and block kinds fail closed.

The concrete adapter is intentionally narrower than the wire format. It admits
only the official `deepseek` template with an explicit frozen `messages`
dialect and route, uses `x-api-key` plus `anthropic-version: 2023-06-01`, and
disables proxy discovery and redirects. DeepSeek thinking is explicitly disabled
because reasoning blocks are not yet canonicalized. Requests currently use the
conservative fixed `max_tokens = 4096`; selected Model Preset output-limit
wiring remains pending. OpenCode Go is not admitted from a route string alone.

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

When `local.echo` is enabled, all three HTTP adapters advertise the wire-safe function
name `local_echo`, map it back to the stable product Tool identity `local.echo`,
and reject every unconfigured returned Tool. Responses continuation sends one
`function_call_output` item correlated by the Provider call ID and previous
response ID. Chat Completions continuation reconstructs the bounded user,
assistant Tool-call, and Tool-result message sequence with the same call ID.
Messages continuation reconstructs Anthropic `tool_use` and `tool_result`
content blocks and selects `tool_choice: none`. Those correlation details remain
process-local; they are not authority and are not written to the Runtime or
Tool Ledger. Consequently none of the adapters can
resume a Provider continuation after process loss; the durable Runtime blocks
rather than repeating a successful or ambiguous Tool effect.

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

Redacted fixtures under `tests/fixtures/provider/chat_completions/v1/` cover
fragmented text, one fragmented function call, usage-only completion, Tool
continuation, and incomplete termination. Module tests additionally cover
split transport chunks, identity and service-tier changes, multiple choices,
duplicate usage, missing and post-terminal data, output and argument limits,
argument depth, poisoning, canonical normalization, and redacted Debug output.

Redacted fixtures under `tests/fixtures/provider/messages/v1/` cover fragmented
text, ping, one fragmented `tool_use`, split input/cache/output usage,
incomplete termination, exact HTTP request and header shape, one Tool
continuation, protocol ordering, bounds, poisoning, and redacted Debug output.

The Kernel tracer-bullet test decodes a first fixture containing text and one
function call, durably approves and executes one injected Tool, decodes a
continuation fixture, prepares and acknowledges the combined output, and then
replays all three Ledgers. Companion tests cover stale Sessions, ambiguous Tool
effects, non-UTF-8 Tool output, and process death after durable Tool success
without effect repetition.

Product integration tests exercise all three adapters against actual loopback TCP
servers. Config Runtime resolves the fixture profile and each adapter uses its
exact frozen dialect endpoint; the server validates route, model, input or
messages, streaming flags, and synthetic credential header, then fragments a
bounded SSE response across network writes. Chat tests cover canonical text and
usage, one approved function call and exact continuation body, missing explicit
dialect or credential before network access, HTTP 503, wrong content type, and
malformed SSE with fixed redacted errors. Responses tests additionally prove
canonical replay, request timeout, and exclusion of an upstream error body from
stderr and the Runtime Ledger. Module tests drive
a locally generated HTTPS certificate through an explicitly trusted test root,
reject the same certificate under default trust, verify endpoint and status
classification, require an origin-bound credential before network access, and
redact Authorization. Messages tests additionally prove the exact DeepSeek-only
adapter gate, `x-api-key` without `Authorization`, pinned version header,
explicit non-thinking request, fixed output-token cap, canonical text/cache
usage, HTTP 503, wrong content type, redacted upstream error event, and exact
`tool_use`/`tool_result` continuation through durable Tool approval. The fixture
remains synthetic and does not constitute a live-provider test.

The product also has an explicit Provider connection-test port for configured
OpenAI-compatible Profiles. It sends one bounded GET to the
frozen Profile's configured `models` route, with proxy discovery and redirects
disabled and the same origin-bound credential policy as inference. It does not
read the response body, discover models, mutate Config, retry, or expose the
endpoint or credential in its status. The terminal-neutral Provider Profile
wizard can test an uncommitted validated candidate; `config test-provider` tests
the currently selected committed Profile.

A two-request test for each dialect verifies its exact function definition,
initial Tool-call stream, canonical `local.echo` mapping, approved output
correlation, dialect-specific continuation request, final text, and both Usage
Records.
Runtime tests additionally verify that those two records become two immutable
Usage Attempts and that a process interruption closes the prior attempt before
an explicit resume starts a new one.
Product-driver tests separately prove denial invokes no executor and an
interrupted approval can be re-presented after reopen before exactly one
effect. A binary integration test verifies the opt-in driver creates and
replays its dedicated Team and Tool Ledgers while preserving stdout delivery
and final `ready` status.

The event shapes are checked against the official
[OpenAI Responses streaming event reference](https://developers.openai.com/api/reference/resources/responses/streaming-events/)
and
[Chat Completions streaming event reference](https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events), plus the
[Anthropic Messages streaming reference](https://docs.anthropic.com/en/api/messages-streaming)
and [DeepSeek Anthropic compatibility guide](https://api-docs.deepseek.com/guides/anthropic_api/).

## Still Pending

- Live credential-gated provider validation, live catalog discovery/refresh,
  starter-preset acceptance, configurable proxy policy, broader TLS platform
  evidence, reconnect classification, and retry policy. Release Provider
  Template defaults and seed catalog facts are bundled, but the current adapters
  do not reconnect or retry partial streams.
- Broader normalization into the eventual provider-neutral canonical Item
  model, including reasoning, refusal, annotations, and hosted Tools.
- Reasoning, refusal, annotation, hosted-tool, and other Responses event kinds
  not listed above.
- DeepSeek Responses/Chat Completions, all OpenCode Go execution, Messages
  thinking/signature/server-tool blocks and preset-driven output limits, Chat
  Completions refusal/reasoning and other delta kinds, and non-streaming
  Provider responses.
- Multiple Tool calls, parallel calls, persisted resumable Provider
  continuation data, and durable storage of a redacted Tool result reference.
- Rich TUI/App Server Tool presentation, non-Windows credential backends, raw
  diagnostic artifact policy, fuzzing, reconnect fixtures, live
  credential-gated tests, and cross-process crash coverage around Provider and
  presentation boundaries. The current CLI interaction is intentionally one
  fixed `local.echo` approval prompt.
