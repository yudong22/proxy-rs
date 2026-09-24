# Architecture: Functional Layer Hierarchy

## Domain

Bidirectional API protocol translation proxy: Anthropic Claude API ↔ OpenAI Chat Completion API.

Primary operation: translate a request in format A to format B, forward it upstream, translate the response back from B to A. Two temporal modes: batch (non-streaming) and incremental (streaming SSE).

## Layer Hierarchy

```
LAYER 0: Protocol Types
    │
LAYER 1: Translation Core (pure atomic mappings)
    │
    ├──────────────────┐
    │                  │
LAYER 2a: Pipeline   LAYER 2b: Stream Translator
  (batch translation    (incremental response
   with policy)          state machine)
    │                  │
    └──────────────────┘
           │
LAYER 3: I/O Shell
  (HTTP, SSE framing, config loading, wiring)
```

## LAYER 0: Protocol Types

**Module:** `src/models/anthropic.rs`, `src/models/openai.rs`

**Concepts:** Wire format data shapes for both APIs. Pure algebraic types, no logic.

**Types:**
- `anthropic::{Request, Message, ContentBlock, SystemPrompt, Response, StreamEvent, ...}`
- `openai::{Request, Message, ContentPart, Response, StreamChunk, ...}`

**Inexpressible:**
- Cross-protocol references
- Business logic
- I/O

**Depends on:** Nothing.

## LAYER 1: Translation Core

**Module:** `src/translate/core.rs`

**Concepts:** Atomic pure mappings between protocol concepts. Each function translates exactly one concept. No configuration, no routing, no state.

**Functions:**
```
translate_message(anthropic::Message) -> Result<Vec<openai::Message>>
translate_tool(anthropic::Tool) -> openai::Tool
normalize_schema(Value) -> Value
remove_term(text, term) -> String
map_stop_reason(Option<&str>) -> Option<String>
is_batch_tool(&anthropic::Tool) -> bool
```

**Inexpressible:**
- Configuration-dependent behavior
- Temporal state
- I/O

**Depends on:** Layer 0

## LAYER 2a: Translation Pipeline

**Module:** `src/translate/pipeline.rs`

**Concepts:** Config-aware composition of Layer 1 atoms into full request/response translations. Policy decisions: model routing, prompt sanitization, tool filtering.

**Types:**
```
TranslationPolicy { reasoning_model, completion_model, model_map, ignore_terms }
```

**Functions:**
```
translate_request(anthropic::Request, &TranslationPolicy) -> Result<openai::Request>
translate_response(openai::Response, fallback_model) -> Result<anthropic::Response>
translate_models_list(openai::ModelsListResponse) -> anthropic::ModelsListResponse
```

**Inexpressible:**
- Streaming/temporal concerns
- I/O
- Raw byte manipulation

**Depends on:** Layer 1, Layer 0

## LAYER 2b: Stream Translator

**Module:** `src/translate/stream.rs`

**Concepts:** Pure state machine translating OpenAI streaming chunks into Anthropic SSE events. Valid state transitions enforced by types.

**Types:**
```
BlockState { Idle, Thinking { index }, Text { index }, ToolUse { index, id } }
StreamState { message_id, model, fallback_model, block, next_index, message_started }
```

**Functions:**
```
initial_state(fallback_model: String) -> StreamState
translate_chunk(&mut StreamState, &openai::StreamChunk) -> Vec<anthropic::StreamEvent>
translate_done(&mut StreamState) -> Vec<anthropic::StreamEvent>
translate_error(message: String) -> Vec<anthropic::StreamEvent>
```

**Key invariant:** Every `ContentBlockStart` is followed by exactly one `ContentBlockStop` before the next `ContentBlockStart`. The `BlockState` enum enforces this.

**Inexpressible:**
- I/O, async, bytes
- SSE framing
- Configuration (model routing happens in Layer 2a before streaming starts)

**Depends on:** Layer 1 (reuses `map_stop_reason`), Layer 0

## LAYER 3: I/O Shell

**Module:** `src/proxy.rs`, `src/config.rs`, `src/service.rs`, `src/router.rs`, `src-tauri/src/main.rs`

**Concepts:** Side effects. HTTP server/client, SSE byte framing, configuration loading, logging, app/daemon lifecycle.

**Functions:**
```
proxy_handler(config, client, request) -> Response
list_models_handler(config, client) -> Response
forward_request(...) -> Response
create_flavor_sse_stream(...) -> impl Stream
serialize_sse_event(&str, &T) -> String
```

**Inexpressible:** Business logic. Handlers call pure functions and pipe data.

**Depends on:** Layer 2a, Layer 2b, Layer 0

## Compilation Chain

Streaming request trace:

```
proxy_handler(req)                                        [Layer 3: I/O]
  → peeked = peek_json_body(req, max_body_bytes())        [Layer 3: bounded read]
  → Json(req) = extract_or_reject(peeked, ...)            [Layer 3: typed extraction]
  → policy = translation_policy(&config)                  [Layer 2a]
  → openai_req = translate_request(req, &policy)          [Layer 2a]
      → model = select_model(req, &policy)                [Layer 2a: routing]
      → messages = translate_message(msg) for each        [Layer 1: atom]
      → tools = translate_tool(t) for each                [Layer 1: atom]
      → system = sanitize(system, &policy.ignore_terms)   [Layer 1: atom + Layer 2a]
  → forward_request(config, client, openai_req, ...)      [Layer 3: I/O]
      → upstream = client.post(url).send()                [Layer 3: I/O]
      → create_flavor_sse_stream(upstream, flavor)        [Layer 3: wire protocol]
          → frames = split on "\n\n"                      [Layer 3: wire protocol]
          → chunk = deserialize(frame)                    [Layer 3: serde]
          → events = translate_chunk(&mut state, &chunk)  [Layer 2b: pure]
          → bytes = serialize_sse_event(event_type, event) [Layer 3: wire protocol]
          → yield bytes                                   [Layer 3: I/O]
```

### Request-body contract

Every JSON handler buffers the body twice — once in `peek_json_body` (the raw
bytes are the only source for `metadata.user_id`, which the translation types
deliberately drop) and once in the `Json` extractor. Both passes are bounded by
the same figure, `util::max_body_bytes()`, which `router.rs` installs as axum's
`DefaultBodyLimit` and the handlers pass to the peek. They must not diverge: a
router limit below the peek's would reject in the extractor with a rejection the
handler has already passed.

A body that fails either pass is answered by `reject_request`, which records the
outcome before returning. Rejections happen before the handler's own logging, so
without it a rejected body leaves no trace in `proxy.log`, the GUI console or the
stats DB. Statuses are preserved rather than flattened to 400 — 413 over the cap,
400 unparseable, 415 wrong content type, 422 wrong shape — and `ProxyError::status`
is the single table both the response and the recorded row read.

## Module Layout

```
src/                    ← the `proxy_rs` library (proxy core)
  models/
    mod.rs
    anthropic.rs        ← Layer 0
    openai.rs           ← Layer 0
    responses.rs        ← Layer 0
  translate/
    mod.rs              ← re-exports
    core.rs             ← Layer 1
    pipeline.rs         ← Layer 2a
    stream.rs           ← Layer 2b
    responses.rs        ← Layer 2b (Responses API)
  proxy.rs              ← Layer 3: HTTP handlers
  router.rs             ← Layer 3: route table (single source of truth)
  service.rs            ← Layer 3: service lifecycle handle
  config.rs             ← Layer 3: config loading (+ env overlay)
  settings.rs           ← Layer 3: persisted settings, logs
  stats.rs              ← Layer 3: SQLite daily stats
  credits.rs            ← Layer 3: gateway wallet balance
  providers.rs          ← Layer 3: provider presets / models discovery
  util.rs               ← cross-cutting helpers (truncate, dates, headers)
  error.rs              ← cross-cutting

src-tauri/src/main.rs   ← desktop app: Tauri commands + tray (uses the library)
```

## Invariants

1. Layers 1, 2a, 2b contain NO I/O, NO async, NO logging
2. Layer 3 contains NO business logic — only wiring
3. translate/ modules never import from proxy.rs, config.rs, router.rs, or settings.rs
4. proxy.rs never constructs Anthropic StreamEvents directly — only via translate/stream.rs
5. All state in translate/ is passed explicitly — no globals, no Arc, no hidden dependencies
6. Every route is registered exactly once, in `router.rs`
7. The proxy core is a library; the desktop app is its only front-end and must
   build its `Config` via `Config::from_settings`
