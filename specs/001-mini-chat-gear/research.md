# Research: mini-chat gear

Platform API notes collected by research agents are kept verbatim under
`/tmp/research/*.md` during implementation; the decisions are summarised here.

## R1. Provider calls use the gear's S2S security context

- **Decision**: every OAGW `proxy_request` (chat, summary, files, vector stores) runs with the S2S
  `SecurityContext` obtained at gear start via `authn_resolver` client credentials (the same
  context that provisions the upstreams). User identity reaches the provider through the `user`
  and `metadata` request fields.
- **Rationale**: OAGW resolves `/{alias}` along the caller's tenant chain
  (`resolution.rs::resolve_alias`); upstreams created by the gear live in the S2S tenant and are
  invisible to user tenants that are not its descendants (static auth, e2e tenants A/B).
- **Alternatives**: proxy with the user context and create upstreams with inherited sharing — fails
  for tenants outside the S2S tenant's subtree.

## R2. Test strategy

- **Decision**: (a) unit tests in `*_tests.rs` companion files (repo lint DE1101 convention) for
  pure logic; (b) integration tests in `gears/mini-chat/mini-chat/tests/` that build the full
  service graph on in-memory SQLite with the OAGW test harness (`oagw` feature `test-utils`,
  `AppHarness::facade()` as `ServiceGatewayClientV1`) and an in-process axum mock
  OpenAI-compatible provider, a configurable PDP stub (allow with tenant/owner constraints, deny,
  failure), and the bundled static policy/audit plugins; requests are driven through the gear's
  axum router with `tower::ServiceExt::oneshot`; (c) a Python smoke suite against the real
  server binary and `tests/e2e_smoke/mock_provider.py`.
- **Rationale**: exercises the real wire path (OAGW → provider) and real SQL without containers.

## R3. SSE response

- **Decision**: handlers return `axum::response::sse::Sse` over a stream fed by a bounded
  `tokio::mpsc` channel (`streaming.sse_channel_capacity`), with `KeepAlive` comments every 30 s;
  `ping` events are produced by the relay only until the first content event. The provider task
  runs in a spawned task holding a `CancellationToken`; dropping the response stream cancels it.
- **Rationale**: matches DESIGN "Streaming Cancellation" and ADR-0010 ping semantics.

## R4. Thumbnails

- **Decision**: `image` 0.25 (`ImageReader::with_guessed_format`, `Limits` with
  `max_alloc = thumbnail.max_decode_bytes`), `resize` to fit preserving aspect ratio,
  `WebPEncoder::new_lossless`; skip when encoded size > `thumbnail.max_bytes`.

## R5. Credits, estimation, sanitization

Implemented as pure functions with exhaustive unit tests: `credits_micro_checked` (DESIGN §5.3),
text estimate (§5.5.4), reserve (§5.4.1), cascade (§3 Downgrade Decision Flow), provider error
sanitizer (§3.3 non-exposure invariant: `resp_`, `chatcmpl-`, `cmpl-`, `msg_` ids, `file-`,
`file_`, `assistant-`, `vs_` ids with ≥12 alnum chars → `[provider_id]`, URLs → `[url]`, `sk-` keys
and `Bearer` tokens → `[credential]`).

## R6. Gear declaration and lifecycle

- **Decision**: `#[toolkit::gear(name = "mini-chat", deps = [types_registry, authn_resolver,
  authz_resolver, oagw], capabilities = [db, rest, stateful])]` with `RunnableCapability`
  (`start`: S2S exchange → OAGW provisioning (+ background reconcile) → outbox pipeline (5 queues) →
  orphan watchdog + upload reaper; `stop`: cancel workers, stop outbox). Config via
  `ctx.config_expanded_or_default()` with `toolkit_macros::ExpandVars` for provider `host`,
  `auth_config`, tenant overrides and `client_credentials`. Bundled plugins are separate gears in
  their own modules (`infra::plugins::static_audit`, `infra::plugins::static_model_policy`), each
  `deps = [types_registry]`, registering a `PluginV1<Spec>` instance and a scoped client. Host-side
  selection is lazy (`GtsPluginSelector` + `choose_plugin_instance`), audit "client missing" resets
  the cached id.
- Notes: `research-notes/toolkit-gear-rest.md`, `research-notes/plugins.md`.

## R7. Persistence

- **Decision**: hand-written per-backend SQL migrations (PG + SQLite) in one migration module plus
  `toolkit_db::outbox::outbox_migrations()` (default prefix `toolkit_outbox`, tables
  `toolkit_outbox_*` — each gear has its own SQLite file; the hidden suite inspects these rows).
  Entities derive `Scopable` (`chats`: tenant+owner+resource; children: tenant+resource; quota and
  reactions: tenant+owner). Queries only through the secure API; CAS via
  `update_many().secure()…exec()` and `rows_affected`. Quota counters: insert-if-missing
  (`ON CONFLICT DO NOTHING`) then `col_expr(col + n)`. No raw SQL outside migrations.
  Transactions via `DBProvider::transaction`; outbox `Wake` fired after commit.
- Tests: shared-cache in-memory SQLite (`sqlite:file:<uuid>?mode=memory&cache=shared`, 4 conns).
- Notes: `research-notes/toolkit-db.md`.

## R8. REST, OData, errors

- **Decision**: `OperationBuilder` per route with `.authenticated().require_license_features([&License])`
  (base license feature), canonical extractors `toolkit::api::rest::extract::{Json, Path}`, the
  `OData` extractor + `toolkit_db::odata::paginate_odata` with hand-written `FilterField` /
  `ODataFieldMapping` for chats (`updated_at`, `id`, `title`) and messages (`created_at`, `id`,
  `role`); limit default 20, max 100 (clamped). Errors via `#[resource_error]` markers for
  `cf.core.mini_chat.{chat,message,turn,attachment,model}.v1~`. `Location` built from the request
  path (gateway prefix excluded). No throttling zones are attached (an undefined zone name fails
  gateway startup; the hidden suite's config is unknown).
- Upload: raw `axum::body::Body` + `multer` streaming with byte counting; MIME / limits checked
  before and while reading.

## R9. OAGW and identity

- **Decision**: provisioning per provider entry and tenant override: `create_upstream`
  (scheme http when `use_http`, explicit alias when configured or the host is an IP, otherwise the
  OAGW-derived alias `host` / `host:port`), `AlreadyExists` → reuse via `list_upstreams`;
  `FailedPrecondition` (secret not readable) → background reconcile with 2 s→60 s backoff;
  routes: chat `POST {api_path prefix}` (query allowlist from `api_path`), RAG routes
  `{prefix}/files`, `{prefix}/vector_stores` (methods POST/GET/DELETE, allowlist `api-version`).
  Proxy calls use the S2S context (R1). Responses: upstream non-2xx → provider error mapping,
  `Err(DeadlineExceeded)` / gateway 504 → `provider_timeout`, 429 → `rate_limited`.
- AuthZ: `PolicyEnforcer::access_scope_with` + `.ensure_owner(subject)`; `Denied`/`CompileFailed`
  → 403 `AUTHZ_DENIED`, `EvaluationFailed` → 503 `Retry-After: 5`.
- Notes: `research-notes/oagw-authn-authz.md`.

## R10. Outbox pipeline starts in `init` (deviation from DESIGN "Gear lifecycle")

- **Decision**: the outbox pipeline (five leased queues) is started at the end of `init`, after the
  domain services are built, with a bounded retry on SQLite lock errors; `start` keeps the S2S
  exchange, OAGW provisioning and the leader-only workers; `stop` stops the pipeline.
- **Rationale**: the runtime serves REST routes before `start` of every gear has finished. With
  the pipeline started in `start`, (a) a request transaction committed during queue registration
  made the outbox start fail with `SQLITE_BUSY_SNAPSHOT` and the server exited, and (b) a request
  that enqueued an outbox message (turn finalization, chat/attachment deletion) before the
  enqueuer was bound failed. Both were reproduced by the black-box smoke suite
  (`tests/e2e_smoke/test_zz_startup.py`). Handlers that need the S2S context or a plugin simply
  return `Retry` until `start` has provisioned them.
- **Alternatives**: waiting for the binding inside the enqueuer (holds the SQLite write lock while
  the outbox needs it), failing requests until `start` completes (visible 5xx at startup).

## R11. Write transactions take the write lock first

- **Decision**: every gear write transaction goes through `WriteTransaction::write_transaction`
  (`infra/db/mod.rs`), whose first statement is a no-op `UPDATE chats … WHERE id IS NULL`.
- **Rationale**: a SQLite deferred transaction that reads first and writes later fails with
  `SQLITE_BUSY_SNAPSHOT` (code 517, not retried by `busy_timeout`) when another connection — the
  outbox workers — committed in between; this made retry/edit/delete and uploads right after a
  turn return 500. Starting with a write acquires the lock up front and waits via `busy_timeout`
  (the `BEGIN IMMEDIATE` behaviour, which `toolkit-db` does not expose). On Postgres the statement
  matches no rows and takes no row locks. Covered by the integration suites (`messages_turns`,
  `acceptance_gaps::concurrent_mutations_resolve_to_a_single_winner`).
- **Alternatives**: `Db::transaction_with_retry` (requires re-invocable closures at every call site
  and a `DbErr`-carrying domain error).

## R12. Timestamp `$filter` on SQLite

- **Decision**: on SQLite, `updated_at` / `created_at` comparisons in `$filter` are rewritten into
  equivalent half-open ranges (`ge t`, `lt t + 1 ns`) before `paginate_odata`
  (`infra/db/odata.rs::sqlite_safe_query`).
- **Rationale**: `toolkit-db` binds filter literals as chrono values (`…+00:00`) while the entities
  store `time::OffsetDateTime` text (`…Z`); text comparison of the same instant made `eq` match
  nothing and `gt` match the equal row. Postgres compares typed values and is left unchanged.
