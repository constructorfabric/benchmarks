## Context

`gears/mini-chat/mini-chat` is a stub (two empty plugin modules imported by `apps/cf-gears-example-server/src/registered_gears.rs`). The contract lives in `gears/mini-chat/docs` (PRD, DESIGN, ADR-0001…0010, acceptance-criteria) and in the generated OpenAPI `docs/api/api.json` (paths `/mini-chat/v1/...`, schemas `ChatDetailDto`, `MiniChatMessageDto`, `MiniChatSseEvent`, `Problem`, …). A hidden black-box suite starts the debug server with its own config (keys/defaults from DESIGN Appendix B), drives the HTTP/SSE API, inspects the gear's SQLite DB, and inspects what the gear sends to an OpenAI-compatible mock provider (via OAGW, `kind: openai_responses`, storage `openai`/`azure`). It seeds `quota_usage` rows per test user, so the schema must match DESIGN §3.7 exactly.

Platform building blocks used (all already in the workspace / lockfile):
- `#[toolkit::gear(name, deps, capabilities, lifecycle)]`, `GearCtx` (`config_or_default`, `db_required`, `client_hub`), `DatabaseCapability::migrations`, `RestApiCapability::register_rest`, `OperationBuilder` (+ OData extension) with `require_license_features` on the platform base license feature (as types-registry does).
- `toolkit-db`: `DBProvider`/`Db`, secure ORM (`Scopable`, `AccessScope`), OData pagination, and the transactional outbox (`Outbox::builder`, `Record::to(queue, partition)`, `TransactionalMessageHandler`/leased handlers, default tables `toolkit_outbox_*`).
- `toolkit-canonical-errors` (`CanonicalError` → RFC 9457 Problem), `authz-resolver-sdk` (`PolicyEnforcer`), `authn-resolver-sdk` (S2S client credentials), `oagw-sdk` (`ServiceGatewayClientV1`: upstream/route CRUD and `proxy_request` with streaming bodies), `types-registry-sdk` (GTS plugin discovery).

## Goals / Non-Goals

**Goals:**
- Serve every route of DESIGN §3.3 with the exact DTOs of `docs/api/api.json`, the canonical error mapping (ADR-0004), and the SSE contract (incl. ADR-0010 deviations: ping only before first content, `stream_interrupted`, replay without `downgrade_reason`).
- Persist exactly the schema of DESIGN §3.7 (SQLite UUIDs as 16-byte blobs, `period_start` as DATE text) plus the outbox tables.
- Implement preflight → reserve → stream → CAS finalization with quota settlement and outbox emission (usage + audit) in one transaction; orphan watchdog; upload reaper; cleanup and thread-summary handlers.
- Provide `mini_chat_sdk` with plugin contracts and the two bundled plugin gears.
- Unit and in-process integration tests for every acceptance-criteria item.

**Non-Goals (per ADRs):** MCP (ADR-0006), document summaries (ADR-0007), policy snapshot cache / notify endpoint, image quota (ADR-0008), hard purge and chat-deletion audit (ADR-0009), temporary chats (P2), Kubernetes Lease elector (only the no-op elector; the `k8s` feature compiles but behaves as single process). The Chat Completions / vLLM / Anthropic adapters are implemented minimally (request mapping + stream parsing) since the E2E rig only uses `openai_responses`.

## Decisions

1. **Crate layout (DDD-light)** — `mini_chat::{config, gear, api::rest::{dto, handlers, routes, sse, error}, domain::{model, error, service::{chat, message, stream, turn, attachment, quota, finalize, context, replay}, ports}, infra::{db::{entity, migrations, repo}, llm::{provider, adapters::{openai_responses, chat_completions, vllm, anthropic}, sse_parser, sanitize}, oagw::{provision, storage}, outbox::{enqueuer, handlers}, workers::{orphan_watchdog, upload_reaper}, plugins::{static_audit, static_model_policy}, policy_gateway, audit_gateway, thumbnail}}`. Rationale: mirrors DESIGN §1.3 layering and the module names the example server imports. Alternative (flat module) rejected: too large to navigate.

2. **One gear crate hosts three gears** — `mini-chat`, `static-mini-chat-model-policy-plugin`, `static-mini-chat-audit-plugin` each registered with `#[toolkit::gear]` in their module; the plugin gears register their GTS instance in types-registry and their client in ClientHub under a scoped key, following `static-authz-plugin` / `static-credstore-plugin`. The main gear resolves them lazily by vendor + priority (no caching of "not found").

3. **Persistence via SeaORM entities + raw SQL where the ORM is awkward** — partial unique indexes, CAS updates (`UPDATE ... WHERE state='running'`), and bucket increments are issued as SeaORM `update_many` with filters (returning `rows_affected`) so the code runs on SQLite and Postgres. Migrations create both dialect variants. Tenant/owner scoping: the chat is loaded through an owner+tenant-scoped query (AccessScope compiled from the PDP decision with an added owner predicate); child tables are filtered by that chat id plus `tenant_id`.

4. **Streaming pipeline** — the handler spawns the setup in a task (so a disconnect does not abort setup) and returns `Sse` built from a bounded `mpsc` (capacity `streaming.sse_channel_capacity`). A provider task reads OAGW's streaming body, parses SSE events incrementally (`event:`/`data:` lines; event name falls back to `data.type`), forwards translated events immediately, tracks tool counts/limits, and on terminal data runs CAS finalization before emitting `done`/`error`. Client disconnect = receiver dropped → `CancellationToken` cancelled → provider body dropped → finalize `cancelled`. Ping timer runs only until the first delta/tool. If the provider task ends without sending a terminal event, the relay emits `error{stream_interrupted}`.

5. **Quota service** — pure functions for `credits_micro` (checked), text estimation, candidate reserve, cascade, warnings; a repository for bucket rows (`total`, `tier:premium` × daily/monthly with persisted `period_start`). Preflight reads rows in a transaction; the reserve transaction increments and re-checks. Settlement helper shared by stream finalization and watchdog; billing-outcome derivation is one function keyed on (state, error_code, usage).

6. **Policy plugin access** — no cache: every preflight calls `get_current_policy_version`, `get_policy_snapshot`, `get_user_limits`; settlement reads the snapshot for `policy_version_applied`. The static plugin returns version 1 from its config (`model_catalog`, `kill_switches`, `default_standard_limits`, `default_premium_limits`).

7. **Provider access through OAGW** — at `start` the gear exchanges client credentials via authn-resolver and creates (or reuses) one upstream per provider entry/tenant override with alias = `upstream_alias || host`, plus chat and RAG routes; failures due to unreadable secrets are retried in the background with backoff 2 s→60 s. All provider calls use `proxy_request` to `/{alias}{path}` with the S2S (or request) security context. File/vector-store operations use the OpenAI (`/v1/...`) or Azure (`/openai/...?api-version=`) layout by `storage_kind`.

8. **Errors** — a `DomainError` enum maps to `CanonicalError` in one place (`api::rest::error`), producing the reasons/violations/resource types of ADR-0004; SSE errors use `{code, message}` with `sanitize_provider_message` (regexes from DESIGN §3.3).

9. **Outbox** — five queues registered in `start`, partition by tenant (usage, audit) or chat (cleanup, summary); the enqueuer is a port used inside domain transactions; wakes fire after commit. Handlers implement DESIGN §3.6/§5.6 semantics (cleanup attempts, summary CAS, audit drop/retry/reject).

10. **Testing strategy** — unit tests next to modules (`*_tests.rs` per repo convention) for pure logic (credits, estimation, cascade, sanitization, SSE parsing, context truncation, billing derivation, config validation); integration tests in `gears/mini-chat/mini-chat/tests/` that build the services over an in-memory SQLite DB with a fake provider transport and fake policy/authz, exercising each acceptance-criteria area end to end (send/replay/conflict/mutations/attachments/quota/cleanup/watchdog/summary/authz). A traceability table in `tests/README.md` maps each acceptance item to its tests.

## Risks / Trade-offs

- [Hidden suite expects exact wire details not spelled out in docs] → follow `docs/api/api.json` schemas verbatim and DESIGN examples; keep optional fields omitted when null where DESIGN says so (`title`, `model`, `input_tokens`, `error_code`, `assistant_message_id`, `doc_summary`).
- [OAGW streaming/multipart API mismatch with assumptions] → wrap OAGW behind a `ProviderTransport` port so adapters/tests are independent; verify against oagw-sdk before coding.
- [SQLite write contention under concurrent streams] → short transactions; WAL + busy_timeout come from server config; CAS updates are single statements. Every write transaction starts with a no-op write (`lock_for_write`) so read-then-write transactions wait under `busy_timeout` instead of failing with `SQLITE_BUSY_SNAPSHOT` (deferred-transaction upgrade); contention that outlives the timeout maps to 503 with `retry_after_seconds` (found during apply; covered by `persistence::concurrent_writers_do_not_surface_lock_errors`).
- [Scope is very large] → implement the OpenAI Responses path and all REST/SSE semantics first, then background workers, then secondary adapters.

## Migration Plan

New gear; no data migration. Rollback = build without the `mini-chat` feature.

## Open Questions

- Exact OAGW route matching for `{model}` in `api_path` and query allowlists — resolved during implementation by reading `oagw-sdk`/`oagw`.
