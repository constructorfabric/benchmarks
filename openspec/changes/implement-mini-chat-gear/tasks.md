## 1. SDK crate and workspace wiring

- [x] 1.1 Create `gears/mini-chat/mini-chat-sdk` (package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`), add to workspace members and `[workspace.dependencies]`, make the gear depend on it
- [x] 1.2 SDK types: `PolicySnapshot`, `ModelCatalogEntry` (+ `EstimationBudgets`, `ModelGeneralConfig`, `ModelApiParams`, `ModelToolSupport`, `ModelPreference`, `ModelTier`), `KillSwitches` (all fields required), `UserLimits`/`TierLimits`, `UsageEvent`/`UsageTokens`, audit event types
- [x] 1.3 SDK plugin contracts: `MiniChatModelPolicyPluginClientV1`, `MiniChatAuditPluginClientV1`, error types, GTS plugin specs `MiniChatModelPolicyPluginSpecV1` / `MiniChatAuditPluginSpecV1`
- [x] 1.4 Tests: SDK serde round-trips, `enabled` default, `KillSwitches` missing-field rejection, system-task usage event omits `user_id`/`turn_id`

## 2. Gear skeleton, config and plugins

- [x] 2.1 Gear config structs with defaults/validation from DESIGN Appendix B (deny unknown keys where specified, provider entries, `${VAR}` expansion, deprecated-field warnings)
- [x] 2.2 `#[toolkit::gear(name = "mini-chat")]` with db/rest/stateful capabilities, init/start/stop lifecycle
- [x] 2.3 Bundled plugin gears `static-mini-chat-model-policy-plugin` (catalog, kill switches, default limits, multiplier validation) and `static-mini-chat-audit-plugin`; GTS registration and ClientHub registration
- [x] 2.4 Policy gateway and audit gateway resolving plugins via types-registry by vendor/priority
- [x] 2.5 Tests: config defaults/validation (ranges, removed keys rejected), static policy plugin validation

## 3. Persistence

- [x] 3.1 Migrations (SQLite + Postgres) for chats, messages, chat_turns, attachments, message_attachments, thread_summaries, chat_vector_stores, quota_usage, message_reactions with constraints/indexes of DESIGN §3.7, plus outbox migrations
- [x] 3.2 SeaORM entities and repositories (chat, message, turn, attachment, reaction, summary, vector store, quota) with tenant/owner scoping and CAS updates
- [x] 3.3 Tests: migrations apply on SQLite; partial unique index enforces one running turn per chat; CAS finalization affects one row only once

## 4. Authorization and errors

- [x] 4.1 PEP: PolicyEnforcer evaluation per action/resource type (chat, model, user_quota), AccessScope with owner predicate, 403/503 fail-closed mapping
- [x] 4.2 DomainError → CanonicalError mapping (ADR-0004 reasons, violations, resource types, Retry-After)
- [x] 4.3 Provider message sanitization (`[provider_id]`, `[url]`, `[credential]`)
- [x] 4.4 Tests: error mapping table (status, category, reason per variant), sanitization cases, foreign-user isolation (404 on every chat-scoped endpoint)

## 5. Chat, message, model, reaction and quota-status APIs

- [x] 5.1 Chat CRUD handlers/services: title/model validation, default model, Location header, soft delete + cleanup enqueue, message_count, updated_at bumps
- [x] 5.2 List chats/messages with OData filter/orderby/cursor/limit (clamp 100, 0 → INVALID_LIMIT) and odata resource type errors
- [x] 5.3 Message DTO enrichment (attachments batch query, my_reaction batch lookup, optional fields)
- [x] 5.4 Models API (enabled only, no internal fields) and reactions (assistant only, upsert, idempotent delete)
- [x] 5.5 Quota status endpoint (tiers × periods, warnings, next_reset)
- [x] 5.6 Route registration with OperationBuilder (paths, operation ids, license feature, rate-limit zones) and OpenAPI schemas
- [x] 5.7 Tests: chat CRUD lifecycle, title/model validation, list filter/order/pagination/malformed input, ordering by activity, message list contract, models visibility, reactions, quota status consistency

## 6. LLM provider layer and OAGW

- [x] 6.1 Provider resolver (provider_id + tenant overrides → adapter kind + alias) and OAGW provisioning with background retry
- [x] 6.2 OpenAI Responses adapter: request builder (instructions, input, tools, user, metadata, max_output_tokens, api_params) and SSE event translation (delta, tools, citations, completed/incomplete/failed/error, usage)
- [x] 6.3 Chat Completions, vLLM Responses and Anthropic Messages adapters (request mapping + stream parsing + usage normalization)
- [x] 6.4 File storage / vector store clients (OpenAI and Azure layouts) and non-streaming summary call
- [x] 6.5 Tests: request shape snapshot, SSE parser (event line vs `type`), event translation incl. citations/tool events, provider error → code mapping (429 → rate_limited, timeout → provider_timeout)

## 7. Streaming send pipeline

- [x] 7.1 Preflight: idempotency (replay / request_id_conflict) before parallel-turn guard, kill switches, quota cascade, INPUT_TOO_LONG, image guards, context assembly with truncation, provider resolution
- [x] 7.2 Reserve transaction (reserve + re-check, user message, attachment validation + links, running turn, chats.updated_at)
- [x] 7.3 SSE relay (bounded channel, ping before first content, no buffering, disconnect → cancel), provider task with tool-call limits and last_progress_at heartbeat
- [x] 7.4 CAS finalization: completed/incomplete/failed/cancelled, assistant message persistence, settlement (actual/estimated/overshoot cap), usage + audit outbox, thread-summary trigger, done/error gating, stream_interrupted
- [x] 7.5 Replay path (side-effect free) and turn status endpoint
- [x] 7.6 Tests: stream event order, request id correlation, preflight rejections create no turn/no provider call, persisted assistant message + usage, replay without side effects, conflicting reuse, replay before parallel guard, one running turn per chat, new turn after terminal, cancellation with partial / null content, failure paths, web search tool/citations/limits/daily quota, context assembly and budget errors

## 8. Turn mutations

- [x] 8.1 Retry/edit: read-only preview (latest, terminal, owner), mutation preflight, commit (soft delete, replaced_by_request_id, new user message, copied attachment links, new running turn, summary invalidation, audit), then context/reserve/stream; post-commit setup failures mark the turn failed
- [x] 8.2 Delete last turn (soft delete turn + messages, summary invalidation, audit)
- [x] 8.3 Tests: latest/terminal checks (400 STATE, 409 NOT_LATEST_TURN, 403 owner), new request id, concurrent mutations (one wins, other 409), attachments carried forward, quota/context checks on mutation

## 9. Attachments

- [x] 9.1 Upload handler: pre-body chat/model resolution, multipart streaming with byte counter, MIME inference/validation, kind/purpose derivation, kill switches, per-chat limits, concurrency limit
- [x] 9.2 Upload service: provider file upload, vector store get-or-create protocol, indexing poll with 25 s deadline and background continuation, thumbnail generation (WebP, bounds), failure states
- [x] 9.3 Get/delete attachment (locked check, idempotent delete, cleanup enqueue)
- [x] 9.4 Upload reaper worker
- [x] 9.5 Tests: upload document/image, size/type/limit validation, indexing failure and timeout handling, availability to provider tools (file_search / code_interpreter / input_image), citation mapping, delete lifecycle and lock, reaper recovery

## 10. Background processing

- [x] 10.1 Outbox pipeline start with five queues; enqueuer port
- [x] 10.2 Usage handler (publish_usage) and audit handler (drop/retry/reject, 120-attempt cap)
- [x] 10.3 Attachment cleanup and chat cleanup handlers (attempt accounting, vector store deletion ordering)
- [x] 10.4 Thread summary handler (range selection, prompt, parsing, CAS commit, system usage event, retries) and mutation-driven invalidation
- [x] 10.5 Orphan watchdog (stale-progress CAS, estimated settlement, usage/audit enqueue)
- [x] 10.6 Tests: chat deletion cleanup, attachment cleanup retries/terminal failure, summary success/failure/CAS conflict/invalidation, watchdog finalization and skip of refreshed turns, usage published exactly once per turn

## 11. Integration and verification

- [x] 11.1 Build the example server with `mini-chat,static-authn,static-authz,single-tenant,static-credstore`
- [x] 11.2 Run `cargo test -p cf-gears-mini-chat -p cf-gears-mini-chat-sdk` and fix failures
- [x] 11.3 Smoke-run the server with a mock OpenAI-compatible provider: create chat, stream, list messages, upload, delete
- [x] 11.4 Write the acceptance-criteria → test traceability table
