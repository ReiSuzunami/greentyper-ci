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
Runtime authority or durable state. Separate normalizers reduce each supported
terminal stream to provider-neutral text deltas, one canonical function call,
and one optional Usage Record. The Runtime Kernel can drive that neutral
interface through Tool Runtime approval and one Tool continuation. The product
has configured OpenAI/openai-compatible Responses and Chat Completions HTTP
adapters, exact DeepSeek Responses, Chat Completions, and Messages pairs, and an
OpenCode Go Chat adapter for release-catalog-verified Chat models plus a
Responses adapter for the exact GPT-5.6 Luna pair and a Messages adapter for
release-catalog-verified Messages models. Each uses a
no-proxy, no-redirect blocking client, streams the response through its matching
decoder, and drives the single-Agent Runtime. Config Runtime resolves the
selected Provider Profile and freezes its normalized origin, declared routes,
explicit dialect, pricing source, and opaque credential reference in the
Provider Epoch.
The release-bundled templates supply those defaults; the adapters admit them and
explicit compatible gateways only after the frozen Profile declares the selected
dialect and its endpoint. Adapter selection normally never infers one dialect
from another. The bounded DeepSeek exception resolves a preferred `responses`
dialect before admission: V4 Flash remains Responses, while V4 Pro selects Chat
Completions because Pro does not support Responses. The effective dialect is
then frozen in the Provider Epoch. This is model-capability resolution, not a
retry or a switch after network I/O. Every OpenCode Go record remains a catalog
fact until its template and selected dialect have an explicit product adapter;
verified Chat Completions records, the exact GPT-5.6 Luna Responses record, and
verified Messages records are admitted in the current slice.
Before each request, the selected adapter resolves secret material from an
origin-bound product vault; remote origins require HTTPS. The headless CLI selects the
configured OpenAI adapter with `--dialect responses` or
`--dialect chat_completions`, or the configured DeepSeek adapter with
`--dialect responses`, `--dialect chat_completions`, or `--dialect messages`,
or a configured OpenCode Go Chat model with `--dialect chat_completions`,
GPT-5.6 Luna with `--dialect responses`, or a release-verified OpenCode Go
Messages model with `--dialect messages`, and
retains the
deterministic simulator only when no custom profile is selected. Windows
Credential Manager is the current platform
backend; non-Windows product credential access fails closed. Automatic retry,
resumable reconnect, live inference conformance, and broader Tool presentation
remain separate work.
All three HTTP dialect paths reject serialized request bodies above 128 KiB
before network I/O. A Tool continuation also rejects any second Tool call at
the adapter boundary; the Runtime repeats that invariant before projection.
The frozen `canonical` Context Mode validates and materializes a bounded
conversation from a durable checkpoint's recent raw tail plus later completed
canonical Items, or from bounded completed canonical history when no checkpoint
exists. The first Turn still retains the existing scalar Responses input and
one-user-message shapes. A leading Assistant Item from a split retained Turn is
omitted up to the next User boundary. Archived artifact bodies are not fetched.
Responses uses an ordered message-input array, while Chat Completions and
Messages use their native message arrays. The supported Tool continuation
keeps the same conversation: stateful Responses binds the response ID, while
stateless DeepSeek Responses, Chat Completions, and Messages append the Tool
call/result to the stored in-memory request messages. This adds no credential,
Tool, Agent, or Config authority and persists no extra raw Provider payload.
The Kernel durably brackets each request and continuation as a separate Usage
Attempt before invoking this adapter, so transport failure, interruption,
successful usage, and replay remain distinguishable without persisting raw
Provider events. The typed `provider_native` Context Mode is rejected before
credential lookup, adapter construction, network I/O, or Runtime admission.
When Config supplies a matching Price Schedule, the Runtime freezes the resolved
schedule book in the Config Epoch and appends a separate pay-as-you-go cost
evaluation after normalized Usage. This is provider-neutral accounting: the HTTP
adapter neither calculates cost nor turns a catalog price or subscription quota
into a provider-reported charge.
For custom origins, a reviewed release-bundled rate card defaults to the
distinct `template_mirror` source. An explicit `unknown`, `manual`, or
`provider_reported` source overrides that default. Templates without a bundled
rate card still require an explicit pricing decision. Mirroring a rate card
never mirrors credentials, origin authority, or Provider identity.

## Transport Interruption and Explicit Recovery

`ProviderUnavailableStage` gives every adapter the same redacted transport
boundary: `BeforeResponse`, `BeforeFirstEvent`, or `AfterFirstEvent`. Responses,
Chat Completions, and Messages classify response-body read failures and an early
EOF from incomplete SSE at the exact decoder-progress boundary. Invalid event
shape, identity, ordering, bounds, or terminal semantics remains
`InvalidResponse`; interruption is not used to excuse malformed Provider data.
Errors expose only the stage and bounded diagnostic byte count, never the
upstream body, Provider text, Tool arguments, endpoint, or credential.

The adapters make one network attempt. They do not automatically retry a
status failure, reconnect a partial stream, or join bytes from two responses.
Inference requests currently carry no idempotency key, so even a
`BeforeResponse` classification does not prove that the remote service did no
work or incurred no usage. Each attempted request is already closed as one
durable failed Usage Attempt with its cost evaluation before the Turn becomes
blocked.

An explicitly configured Model Preset fallback is a separate Runtime policy,
not transport retry. Config resolves at most 16 candidates; the product
preflights every candidate Context Mode before credential lookup, preflights
every adapter before opening Runtime state, and schema 14 freezes a
distinct Config/Provider Epoch for each candidate. Runtime advances only after
`BeforeResponse` or `BeforeFirstEvent`, after closing the failed candidate's
Usage Attempt and cost evaluation. `AfterFirstEvent`, malformed output,
Tool-derived blocks, and every post-Tool continuation failure never switch
candidates.

If a process exits while an eligible early failure is blocked,
`greentyper retry --ledger PATH --turn ID` first appends a validated
`ProviderFallbackRequested` for the next frozen candidate. When no fallback
remains, it appends `TurnRetryRequested` for the active candidate. Both preserve
the exact Turn, input, immutable Epoch history, and prior Usage/cost evidence,
then expose `resume-required` for one new durable Usage Attempt. The request may
repeat remote work, usage, or billing. If adapter construction fails after the
recovery transaction, the durable state remains `resume-required`; another
early failure blocks again. Schema-10-or-earlier blocks without the stage reject
recovery without mutation.

`greentyper cancel --ledger PATH --turn ID` remains the explicit terminal
recovery for a typed Provider-origin block. Schema 10 introduced the block origin
and validated `TurnCancelled`; current schema 14 preserves that contract. The
event
clears only the pending Turn; it retains its user item, immutable Usage/cost
history, and frozen Config and Provider Epochs. Repeating the exact cancellation
is a no-op. Product retry and cancellation strictly open existing Runtime, Team,
and Tool Ledgers, authenticate the recovered Active Agent Session that owns the
Turn, and leave Team and Tool bytes unchanged. Missing or incomplete state is
never created or repaired. Prepared delivery, admission awaiting resume,
incomplete streaming state, Tool approval or reconciliation, and Tool-derived
blocks remain fail-closed on their existing recovery paths. Cancellation itself
never calls a Provider or Tool and creates no Usage Attempt.

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
- `response.output_item.added` and `response.output_item.done` for message,
  reasoning, and function-call items;
- `response.content_part.added` and `response.content_part.done` for
  `output_text` and `reasoning_text` content;
- `response.output_text.delta` and `response.output_text.done`;
- `response.reasoning_text.delta` and `response.reasoning_text.done`;
- `response.function_call_arguments.delta` and
  `response.function_call_arguments.done`;
- `response.completed`, `response.failed`, and `response.incomplete`; and
- the top-level `error` event.

One decoded stream is limited to 4 MiB, each framed line to 1 MiB, and the
semantic stream to 4096 events. Output-item and content-part indices must be
below 1024. Accumulated function arguments are limited to 64 KiB and 64 nested
JSON levels. Visible and reasoning text share the lower of the decoder's
caller-supplied limit and the core 512 KiB maximum.

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
Reasoning text is validated and bounded as dialect-local protocol state but is
not projected into visible assistant output, canonical Provider events, or the
Runtime Ledger.

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
Cached prompt usage accepts both OpenAI's nested
`prompt_tokens_details.cached_tokens` and DeepSeek's top-level
`prompt_cache_hit_tokens`. If both are present they must agree; DeepSeek's
reported cache hit plus cache miss must equal reported prompt tokens. Conflicts
fail closed instead of selecting one source.

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

The concrete DeepSeek adapters are intentionally narrower than their wire
formats. Responses admits only V4 Flash on the official `deepseek` template,
uses the frozen `/responses` route and Bearer authorization, maps the selected
output limit to `max_output_tokens`, accepts only the documented low/high/max
reasoning efforts, and rejects service tier before network I/O. Its one Tool
continuation is stateless: the adapter reconstructs the bounded input and
function result instead of using unsupported `previous_response_id` state.
Chat Completions admits only the official `deepseek` template with an
explicit frozen `chat_completions` dialect and route, uses Bearer authorization,
maps the selected output limit to `max_tokens`, and rejects limits above the
documented 384K maximum. It explicitly disables thinking because reasoning
deltas are not yet canonicalized, rejects preset reasoning effort or service
tier before network I/O, and omits the Beta-only Tool `strict` flag on the
ordinary endpoint. Messages admits the same template only with an explicit
frozen `messages` dialect and route, uses `x-api-key` plus
`anthropic-version: 2023-06-01`, and also disables thinking. Messages uses the
selected output limit as `max_tokens`, with a conservative 4096 fallback when
the Turn has no selected limit.

OpenCode Go Chat Completions admits only the `opencode-go` template with an
exact release-catalog model/dialect match. That check runs before credential
lookup. The adapter uses Bearer authorization, the frozen Chat route, and
`max_completion_tokens`; it rejects preset reasoning effort or service tier
before network I/O because those gateway semantics are not yet canonicalized.

OpenCode Go Responses admits only the exact release-catalog
`opencode-go/gpt-5.6-luna` pair. The model/dialect check runs before credential
lookup. The adapter uses Bearer authorization, the frozen Responses route, and
`max_output_tokens`; it rejects preset reasoning effort or service tier before
network I/O because those gateway semantics are not yet canonicalized. One
approved Tool continuation uses the standard Responses correlation body and
the same frozen request policy. The continuation identity remains process-local.

OpenCode Go Messages admits only exact release-catalog `opencode-go` models
whose supported dialects include Messages. That check runs before credential
lookup. The adapter uses `x-api-key`, pins `anthropic-version: 2023-06-01`, and
uses the frozen Messages route plus `max_tokens`. It omits the DeepSeek-only
`thinking` field, rejects preset reasoning effort or service tier before
network I/O, and supports one `tool_use`/`tool_result` continuation through the
same durable approval boundary. Prepared output can be acknowledged after
restart without repeating the Tool effect; Provider continuation itself remains
process-local.

OpenAI Responses maps the frozen output limit to `max_output_tokens`, while
OpenAI Chat Completions maps it to `max_completion_tokens`; those adapters omit
the field when no limit is selected. The same Config Epoch freezes typed
reasoning effort and service tier. OpenAI Responses emits
`reasoning: { effort }`, OpenAI Chat emits `reasoning_effort`, and both emit
`service_tier` on initial requests and Tool continuations. DeepSeek Chat and
both Messages adapters reject either policy field until their reasoning blocks
and tier semantics are canonicalized; they do not silently discard the request.
DeepSeek Responses maps its supported reasoning effort but still
rejects service tier. Requested values
enter Usage Attempts separately from observed tier metadata. Initial requests
and one in-process Tool continuation use the same Config Epoch values. Replay
reconstructs them after restart, but does not make Tool continuation resumable.
A route string alone never admits an OpenCode Go Messages model; the exact
release-catalog model/dialect pair is also required.

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

Product integration tests exercise every installed template/dialect pair
against actual loopback TCP servers. Config Runtime resolves the fixture
profile and each adapter uses its
exact frozen dialect endpoint; the server validates route, model, input or
messages, streaming flags, and synthetic credential header, then fragments a
bounded SSE response across network writes. Chat tests cover canonical text and
usage, one approved function call and exact continuation body, missing explicit
dialect or credential before network access, HTTP 503, wrong content type, and
malformed SSE with fixed redacted errors. Responses tests additionally prove
canonical replay, request timeout, and exclusion of an upstream error body from
stderr and the Runtime Ledger. DeepSeek Responses tests prove the exact Flash
model gate and route, bounded reasoning validation without visible
chain-of-thought projection, Pro-to-Chat pre-admission resolution,
unsupported-policy rejection before network I/O, and one stateless Tool
continuation. Module tests drive
a locally generated HTTPS certificate through an explicitly trusted test root,
reject the same certificate under default trust, verify endpoint and status
classification, require an origin-bound credential before network access, and
redact Authorization. Messages tests additionally prove the exact DeepSeek and
release-verified OpenCode Go adapter gates, `x-api-key` without `Authorization`,
the pinned version header, adapter-specific omission or inclusion of thinking
policy, fixed output-token cap, canonical text/cache usage, HTTP 503, wrong
content type, redacted upstream error event, and exact `tool_use`/`tool_result`
continuation through durable Tool approval. The OpenCode Go product test also
proves prepared-output restart/acknowledgement without a second Tool effect.
The fixture remains synthetic and does not constitute a live-provider test.

The product also has an explicit Provider connection-test port for configured
OpenAI-compatible Profiles. It sends one bounded GET to the
frozen Profile's configured `models` route, with proxy discovery and redirects
disabled and the same origin-bound credential policy as inference. A successful
response must use JSON, fit within 256 KiB, and contain at most 1,024 unique
model IDs of at most 256 bytes each. The port ignores every remote field except
`id`, sorts accepted IDs, and adds an optional release-catalog key only after an
exact template/model match. Unknown IDs carry no dialect, capability, adapter,
endpoint, pricing, or credential authority. The terminal-neutral Provider
Profile wizard and `config test-provider` keep this result ephemeral. The
explicit CLI `config discovery refresh PROFILE` path may instead persist only
the sorted IDs, locally derived release keys, observation time, template, and
opaque Profile fingerprint in an independent schema-1 state file after success.
Failure preserves the prior snapshot. Read-only `catalog PROFILE` labels
fingerprint drift stale; explicit acceptance requires a current exact ID plus a
Profile-supported dialect and creates an ordinary Config Preset. None of these
paths updates a Provider Epoch, retries, exposes response bodies, endpoints, or
credentials, or grants remote metadata execution authority.

The Direct VT `/model` browser consumes the same shared projection. It starts
from the saved observation, merges release/discovery provenance, derives Recent
from durable Usage, and runs one bounded foreground probe when an eligible
selected Profile is opened; F5 explicitly retries it. Ineligible Profiles and
ordinary browsing perform no network I/O, and there is no background polling.
The action creates one bounded worker lazily and joins it after the result; no
worker, timer, or automatic retry survives into idle terminal time.
A current unknown model still needs an explicit Profile-supported
dialect. The exact Profile fingerprint, observation timestamp, and model are
revalidated immediately before an ordinary Config Draft is created; drift
returns to the browser without a Config or Ledger write.

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
[DeepSeek Responses guide](https://api-docs.deepseek.com/guides/responses_api/),
[DeepSeek Chat Completions reference](https://api-docs.deepseek.com/api/create-chat-completion),
[Anthropic Messages streaming reference](https://docs.anthropic.com/en/api/messages-streaming)
and [DeepSeek Anthropic compatibility guide](https://api-docs.deepseek.com/guides/anthropic_api/),
plus the [OpenCode Go endpoint matrix](https://opencode.ai/docs/go/).

## Still Pending

- Live inference conformance, automatic Provider Profile starter offers and
  updates, configurable proxy policy,
  broader TLS platform evidence, automatic retry policy, and partial-stream
  reconnect. Release Provider
  Template defaults and seed catalog facts are bundled, but the current adapters
  do not reconnect or retry partial streams.
- Background/periodic catalog discovery is excluded by the current Performance
  Contract unless measured evidence supports an approved exception.
- Broader normalization into the eventual provider-neutral canonical Item
  model, including reasoning, refusal, annotations, and hosted Tools.
- Reasoning, refusal, annotation, hosted-tool, and other Responses event kinds
  not listed above.
- DeepSeek Chat/Messages thinking/signature/server-tool blocks, provider-native
  Context Mode execution, Chat
  Completions refusal/reasoning and other delta kinds, and non-streaming
  Provider responses.
- Multiple Tool calls, parallel calls, persisted resumable Provider
  continuation data, and durable storage of a redacted Tool result reference.
- Rich TUI/App Server Tool presentation, non-Windows credential backends, raw
  diagnostic artifact policy, fuzzing, reconnect fixtures, live
  credential-gated tests, and cross-process crash coverage around Provider and
  presentation boundaries. The current CLI interaction is intentionally one
  fixed `local.echo` approval prompt.
