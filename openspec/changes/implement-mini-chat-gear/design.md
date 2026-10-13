## Context

`gears/mini-chat/docs/` (PRD, DESIGN, ADR-0001..0010, acceptance-criteria) is the contract; the generated OpenAPI in `docs/api/api.json` pins every DTO. The gear crate exists with all dependencies but no code; the SDK crate is missing. The example server already imports `mini_chat`, `mini_chat::infra::plugins::static_audit` and `mini_chat::infra::plugins::static_model_policy`, and `config/mini-chat.yaml` shows the config shape. A hidden black-box suite starts the debug server with its own config (keys/defaults from DESIGN Appendix B), talks HTTP/SSE, inspects the SQLite database and records what reaches an OpenAI-compatible mock provider (through OAGW, `use_http` upstream on 127.0.0.1).

## Goals / Non-Goals

**Goals:**
- Wire-exact REST/SSE API, error contract, DB schema and provider requests as specified.
- Correct turn lifecycle with CAS finalization, quota reserve/settle, outbox usage/audit, cleanup, thread summary, orphan watchdog, upload reaper.
- Automated tests for every acceptance-criteria item, runnable with `cargo test -p cf-gears-mini-chat` (offline, SQLite, scripted fake provider).

**Non-Goals:**
- MCP (ADR-0006), document summaries (ADR-0007), policy snapshot cache, image quotas (ADR-0008), hard purge and chat-deletion audit (ADR-0009), metrics beyond what is cheap to emit, Kubernetes leader election internals beyond a no-op elector (the `k8s` feature compiles to the same no-op path).

## Decisions

1. **Layering (DDD-light).** `src/` = `gear.rs` (registration, init/start/stop, REST), `config.rs` (serde with `deny_unknown_fields` where Appendix B requires, validation), `api/rest/` (DTOs, handlers, routes, SSE writer, error→Problem mapping), `domain/` (services: chat, message, reaction, stream, turn, attachment, quota, model, context assembly, finalization; errors; ports), `infra/` (`db/` entities + migrations + repos, `llm/` provider adapters + OAGW transport + storage clients, `outbox/` handlers, `workers/` watchdog + reaper, `plugins/` static_model_policy + static_audit, `oagw_provisioning.rs`). Alternative (one flat module) rejected: the stream pipeline alone is large and must be unit-testable.

2. **Provider access behind a small transport port.** The domain talks to `LlmTransport` (send JSON → streaming SSE byte stream / JSON response; multipart upload; DELETE/GET) implemented by an OAGW `ServiceGatewayClientV1` proxy wrapper in production and by a scripted fake in tests. Adapters (`openai_responses`, `openai_chat_completions`, `vllm_responses`, `anthropic_messages`) build request bodies and parse SSE into internal `ProviderEvent`s, independent of transport. This keeps tests offline and lets the acceptance suite observe real requests through OAGW.

3. **Persistence: SeaORM over the gear's DB handle, scoped by Secure ORM.** Tenant/owner scoping is compiled from the PDP decision into `AccessScope`; chat-scoped child queries first load the chat through the scoped query (404 otherwise) and then filter by `chat_id` + `tenant_id`. Migrations are written once per dialect where needed (SQLite stores UUIDs as 16-byte blobs). CAS updates are single `UPDATE ... WHERE state='running'` statements checked via `rows_affected`. Unique partial index `UNIQUE(chat_id) WHERE state='running' AND deleted_at IS NULL` backs the parallel-turn guard; its violation maps to `turn_already_running` / `GENERATION_IN_PROGRESS`.

4. **Outbox via toolkit-db.** Five queues (usage, attachment cleanup, chat cleanup, thread summary, audit), enqueued in the same transaction as the domain change; handlers return Ok/Retry/Reject. Payloads are JSON (SDK `UsageEvent`, audit events, cleanup/summary payloads).

5. **Streaming.** Handler spawns setup in a task (disconnect during setup cannot leave a running turn), then returns an axum SSE response fed by a bounded `mpsc` (capacity `streaming.sse_channel_capacity`). A provider task reads provider SSE, forwards events immediately, tracks text/usage/tool counts, refreshes `last_progress_at` at most every 30 s, enforces per-turn tool limits, and finalizes via the finalization service before sending `done`/`error`. Dropping the SSE body cancels a `CancellationToken` → provider request dropped → `cancelled` finalization. The relay emits `stream_interrupted` if the task ends without a terminal event. Ping events before the first content event at `sse_ping_interval_seconds`.

6. **Quota service.** Pure functions for estimation, credits (checked arithmetic), cascade, warnings and status; DB functions for preflight read, reserve increment + re-check and settlement on `(tenant, user, period_type, period_start, bucket)` rows (upsert then increment). Period starts are computed at preflight and carried to settlement; the watchdog derives them from `started_at`.

7. **Policy and audit plugins via types-registry + ClientHub**, resolved lazily by `vendor` (lowest priority value wins). Bundled static plugins are separate gears in this crate (`static-mini-chat-model-policy-plugin`, `static-mini-chat-audit-plugin`).

8. **Errors.** One domain error enum mapped to `toolkit-canonical-errors` Problems with the exact categories/reasons/resource types of ADR-0004; SSE errors are `{code, message}` with message sanitization (regex-based).

9. **Tests.** In-crate unit tests for pure logic (credits, estimation, cascade, truncation, sanitization, SSE parsing, request building, billing outcome derivation) and integration tests (`tests/`) that build the REST router over an in-memory/temporary SQLite DB with a permissive mock PDP and a scripted fake transport, driving HTTP/SSE end-to-end and asserting DB rows and captured provider requests — one test module per acceptance-criteria area.

## Risks / Trade-offs

- [Platform API details (outbox, OAGW, secure ORM, OData) differ from assumptions] → research the existing gears first and copy their idioms; build early and often.
- [Hidden suite checks exact wire details not covered by our tests] → follow DESIGN/OpenAPI literally; keep field names/omission rules in DTO serde attributes.
- [SQLite concurrency (single writer) under parallel streams] → short transactions, busy timeout from config, unique indexes as arbiters.
- [Large scope] → prioritise the HTTP/SSE/DB/provider contract; non-observable internals (metrics) kept minimal.

## Migration Plan

New gear; no data migration. Rollback = build without the `mini-chat` feature.

## Open Questions

- None blocking; deviations discovered during implementation are recorded in the synced specs.
