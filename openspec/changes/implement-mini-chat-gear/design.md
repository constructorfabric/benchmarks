## Context

The `mini-chat` gear's implementation was removed; only its documents remain (`gears/mini-chat/docs`). The example server already links the gear (`use mini_chat as _;`, `mini_chat::infra::plugins::static_audit`, `mini_chat::infra::plugins::static_model_policy`) behind the `mini-chat` feature, and the gear crate's `Cargo.toml` already lists all dependencies that are allowed (offline build: only crates in `Cargo.lock`). The acceptance suite is black-box: it starts `cf-gears-example-server` with its own config that uses the DESIGN keys/defaults, talks HTTP/SSE to `/mini-chat/v1/...`, inspects the gear's SQLite database and the requests that reach an OpenAI-compatible mock provider through OAGW.

Platform pieces used: ToolKit gear macro and lifecycle (`db`, `rest`, `stateful`), `OperationBuilder` REST registration with OpenAPI, `toolkit-canonical-errors` (`CanonicalError` → `Problem`), `toolkit-db` Secure ORM + migrations + shared outbox, `toolkit-odata` cursor pagination, `authz-resolver-sdk` PolicyEnforcer, `authn-resolver-sdk` client-credentials exchange, `oagw-sdk` `ServiceGatewayClientV1` (upstream/route CRUD and in-process `proxy_request`), types-registry plugin discovery.

## Goals / Non-Goals

**Goals:**
- Serve every endpoint, body, SSE event, status code and reason documented in DESIGN §3.3 / ADR-0004 and `docs/api/api.json`.
- Persist the documented schema (table and column names, value encodings: UUIDs as 16-byte blobs on SQLite, `period_start` as `YYYY-MM-DD`) so that DB-level checks and seeding work.
- Implement quota reserve/settle, downgrade cascade, CAS finalization, outbox usage/audit/cleanup/summary, watchdog, reaper and OAGW provisioning.
- Ship the SDK crate (`cf-gears-mini-chat-sdk`) with plugin contracts and the two bundled plugins.
- Tests for every acceptance-criteria item.

**Non-Goals:**
- MCP (ADR-0006), document summaries (ADR-0007), PolicySnapshot cache / image quotas (ADR-0008), hard purge and chat-deletion audit (ADR-0009), immutable replay `done` (ADR-0010).
- Full parity of the Chat Completions / vLLM / Anthropic adapters beyond request building and stream parsing; the acceptance rig uses `openai_responses`.
- Metric exporter correctness (instruments are created but not asserted).

## Decisions

1. **Layout (DDD-light).** `mini-chat-sdk` holds transport-agnostic plugin contracts and models. The gear crate has `config.rs`, `gear.rs`, `api/rest/{dto,handlers,routes,sse,error}.rs`, `domain/{chat,message,stream,turn,attachment,quota,context,model_resolver,finalize,summary,sanitize,errors}.rs`, `infra/{db/{entities,migrations,repo},llm/{adapters,oagw},outbox,workers,plugins/{static_model_policy,static_audit}}`. Alternative (single big module) rejected for reviewability.
2. **Persistence through Secure ORM with scoped queries; CAS via conditional updates.** Chat queries use the `AccessScope` returned by the PolicyEnforcer plus an explicit owner predicate (defence in depth); child tables filter by the chat id obtained from the scoped chat query. CAS (`UPDATE chat_turns ... WHERE id=? AND state='running'`) and quota bucket increments use conditional `update_many` / upserts within the transaction and check `rows_affected`. Migrations are written so the same statements run on SQLite and PostgreSQL, with dialect branches where needed.
3. **Shared toolkit outbox for all asynchronous work** (five queues: usage, attachment cleanup, chat cleanup, thread summary, audit), enqueued in the same transaction as the domain change; handlers return `Ok/Retry/Reject`. No gear-local polling worker for these.
4. **Streaming pipeline.** The handler runs setup in a spawned task (disconnect before the stream opens does not abort setup). After the reserve commit a provider task reads the provider SSE through OAGW, translates events via the adapter and sends them over a bounded `mpsc` channel (`sse_channel_capacity`) to the SSE response; terminal events are emitted only after the finalization transaction commits. Dropping the response cancels a `CancellationToken`, aborting the provider stream and finalizing `cancelled`. Pings are emitted only before the first content event.
5. **Provider adapters.** One adapter trait (build request, parse event, map error) with `openai_responses` as the primary implementation; Chat Completions, vLLM and Anthropic adapters share the internal event model. Requests go to `/{alias}{api_path}` through `ServiceGatewayClientV1::proxy_request`. File and vector-store calls use `storage_kind` (`openai`: `/v1/...`, `azure`: `/openai/...?api-version=`).
6. **Policy and audit via plugins resolved through types-registry** (vendor + priority), queried on every request (no snapshot cache, ADR-0008). Bundled static plugins are separate gears inside the gear crate.
7. **Errors.** One domain error enum mapped to `CanonicalError` in the API layer; SSE errors use `{code,message}` with the sanitizer. Gear-specific resource types `gts.cf.core.mini_chat.{chat,message,turn,attachment,model}.v1~`.
8. **Tests.** Rust unit tests for pure logic (credits, estimation, cascade, context truncation, sanitizer, billing derivation, SSE translation, config validation) and a Python black-box suite under `testing/e2e/mini_chat_impl/` (not `suites/mini_chat`) with an in-process OpenAI-compatible mock provider, covering every acceptance-criteria item end to end.

## Risks / Trade-offs

- [Hidden suite seeds DB rows with an assumed encoding] → follow SeaORM/SQLite defaults (UUID blob, RFC3339 text timestamps, DATE text) and the documented column names exactly.
- [Large surface implemented at once] → shared domain helpers (finalization, settlement, billing derivation) used by every path; e2e suite exercises each path.
- [OAGW/credstore secret resolution scope] → provisioning uses the S2S context; secrets configured in static credstore; provisioning failures for unreadable secrets are retried in background.
- [Concurrency on SQLite] → single writer; conflicts surface as unique-index violations mapped to 409 reasons.

## Migration Plan

New gear; no data migration. Enabling the `mini-chat` feature and a `gears.mini-chat` config section activates it. Rollback: disable the feature.

## Open Questions

- Exact multipart body limits are owned by api-gateway `defaults.body_limit_bytes`; mini-chat adds none.
