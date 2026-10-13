# Research: Mini Chat Gear (Phase 0)

All unknowns of the Technical Context are resolved below. Each entry: Decision / Rationale /
Alternatives.

## R1. Gear registration and lifecycle

- **Decision**: `#[toolkit::gear(name = "mini-chat", deps = [types_registry, authn_resolver,
  authz_resolver, oagw], capabilities = [db, rest, stateful], lifecycle(entry = "serve",
  stop_timeout = "30s", await_ready))]` in `gear.rs`; `init` builds config, DB provider, gateways,
  services; `register_rest` registers routes (paths built as `format!("{url_prefix}/v1/...")`);
  `serve` (start phase) performs S2S exchange + OAGW provisioning, starts the outbox pipeline, spawns
  the orphan watchdog and upload reaper, waits for cancellation, then stops workers.
  Plugin gears: `static-mini-chat-model-policy-plugin` and `static-mini-chat-audit-plugin`, each in
  its own Rust module (`infra::plugins::static_model_policy`, `infra::plugins::static_audit`), deps
  `[types_registry]`, registering a `PluginV1<Spec>` instance and a scoped client.
- **Rationale**: types-registry exposes plugin instances only after its `post_init`; authn S2S exchange
  needs plugin discovery, so provisioning cannot run in `init`. Two gears with the same dep must not
  share a module (macro emits `pub use ::types_registry as _gear_dep_types_registry`).
- **Alternatives**: lazy provisioning on first request (rejected: first user turn would pay the cost
  and race).

## R2. REST, extractors, SSE, multipart

- **Decision**: `OperationBuilder` with `.authenticated().require_license_features([BASE])`
  (`CORE_GLOBAL_BASE_LICENSE_FEATURE`); handlers use `toolkit::api::rest::extract::{Json, Path}` so
  rejections become `json_syntax_error` / `invalid_json_body` (422) / `missing_json_content_type`
  (415) / `invalid_path_params`. Errors returned as `CanonicalError` (`ApiResult`), converted to
  `Problem` by the canonical error middleware. SSE responses built with
  `axum::response::sse::Sse::new(stream).keep_alive(KeepAlive::new().interval(30s))`, events
  `Event::default().event(name).data(json)`. Upload takes the raw request body and parses multipart
  with `multer` (dependency already in Cargo.toml) so errors map to `BOUNDARY_REQUIRED` /
  `MULTIPART_ERROR` / `MISSING_FILE` / `MISSING_CONTENT_TYPE` and the size is counted while
  streaming. `create_chat` binds `ThrottlingSpec { rl_mini_chat_chat, ifl_mini_chat_chat,
  require_security_context: false, dry_run: true }`.
- **Rationale**: matches ADR-0004 reasons exactly; zones defined in every shipped config.

## R3. Identity and authorization

- **Decision**: `SecurityContext::subject_tenant_id()` / `subject_id()` from `Extension`. PEP via
  `PolicyEnforcer::access_scope_with(ctx, ResourceType(CHAT, [owner_tenant_id, owner_id, id]),
  action, Some(chat_id), AccessRequest::new().resource_property(owner_tenant_id, tenant)
  .resource_property(owner_id, subject))`. The static authz plugin only emits
  `owner_tenant_id IN (tenant)`, so the gear always adds the owner predicate (`user_id =
  subject_id`) itself (DESIGN §3.8 "defence in depth"). Errors: `Denied`/`CompileFailed` → 403
  `AUTHZ_DENIED`; `EvaluationFailed` → 503 + `Retry-After: 5`. Models: permission-only
  (`require_constraints(false)`); quota status: UserQuota resource.
- **Rationale**: fail-closed, owner-only content.

## R4. OAGW provider calls and provisioning

- **Decision**: `ServiceGatewayClientV1::proxy_request(s2s_ctx, Request(uri = "/{alias}{path}"))`
  with `oagw_sdk::Body` (bytes for JSON, multipart stream for file uploads). Response body is always a
  stream; SSE parsed incrementally by our own parser (event/data lines). Errors: `Err(CanonicalError
  ::DeadlineExceeded)` or an `Ok` 504 Problem with `ErrorSource::Gateway` → `provider_timeout`;
  upstream 429 → `rate_limited` (message includes numeric `Retry-After`); other upstream errors /
  gateway errors → `provider_error` with sanitized message. Provisioning in `serve`: exchange
  `client_credentials` via `AuthNResolverClient::exchange_client_credentials`, then per provider entry
  (and tenant override) `create_upstream(Server{Endpoint{scheme: Http|Https, host, port}},
  HTTP_PROTOCOL_ID).alias(alias).auth(AuthConfig{plugin_type, config})` (auth omitted when no
  `auth_plugin_type`), reuse on `AlreadyExists` (look up via `list_upstreams`), then routes:
  `POST {chat route prefix}` (query allowlist from `api_path` query keys) and, for storage-capable
  entries, RAG routes (`POST {prefix}/files`, `DELETE {prefix}/files`, `POST/GET/DELETE
  {prefix}/vector_stores`, allowlist `api-version` for azure) with `PathSuffixMode::Append`. Existing
  routes (`AlreadyExists`) are accepted. A secret-not-readable failure (`FailedPrecondition`) defers
  the entry to a background reconcile loop (2 s, doubling to 60 s, one warning after 2 min).
  Deterministic misconfiguration fails startup.
- **Alias**: configured `upstream_alias` or host; when OAGW returns a different alias (hostname with
  non-standard port), the provisioning records an alias override consulted by the resolver, so the
  proxy path always names an existing upstream.
- **Proxy identity**: provider requests use the S2S security context (upstreams are owned by the S2S
  tenant and private), and the apikey auth plugin reads credstore with that context.
- **Rationale**: OAGW storage is in-memory (provision on every start); aliases are lower-cased and
  derived from host for hostnames, explicit for IPs.

## R5. Plugins

- **Decision**: SDK specs `MiniChatModelPolicyPluginSpecV1`, `MiniChatAuditPluginSpecV1`
  (`#[gts_type_schema(base = PluginV1, …)]`). Host selection with `list_instances(pattern)` +
  `choose_plugin_instance::<Spec>(vendor)` + `GtsPluginSelector` + `try_get_scoped`. Policy: resolved
  lazily on each use (cached instance id). Audit: lookup each delivery when absent (not cached), cached
  when found, reset when the client is missing (DESIGN §3.2).

## R6. Persistence, transactions, outbox, OData

- **Decision**: SeaORM entities derive `toolkit_db::secure::Scopable`
  (`chats`, `quota_usage`, `message_reactions`: tenant + owner `user_id`; other tables: tenant only,
  `no_owner`). All runtime statements go through the secure builders (raw SQL is impossible outside
  migrations): selects `Entity::find().filter(..).secure().scope_with(&scope)`, CAS / counter updates
  `update_many().col_expr(col, Expr::col(col).add(v)).filter(..).secure().scope_with(&scope).exec()`
  (`rows_affected`), inserts `Entity::insert(am).secure().scope_unchecked(&scope)?.exec()`, upserts of
  quota bucket rows via `on_conflict(SecureOnConflict::columns(..).do_nothing)` + increment update.
  Request paths use the PDP-compiled scope (tenant) plus an explicit owner predicate on `chats`;
  child tables are filtered by `tenant_id` + `chat_id` after an owner-scoped chat lookup. Background
  workers use `AccessScope::allow_all()` with explicit `tenant_id` filters.
  Transactions: `DBProvider::transaction(|tx| Box::pin(async move {..}))`; outbox enqueues inside the
  same transaction return `Wake`s that are fired after commit.
  Migrations: one gear migration with Postgres and SQLite variants (raw DDL via `execute_unprepared`),
  UUID = `UUID`/`BLOB`, partial and composite unique indexes as in DESIGN §3.7, followed by
  `toolkit_db::outbox::outbox_migrations()` (default `toolkit_outbox_*` tables).
  Outbox: `Outbox::builder(db).queue(name, Partitions::of(n)).leased(handler)[.lease(..)]` × 5
  queues started in `serve`; enqueue via a shared `Arc<Outbox>`-holder that becomes available after
  start (`OutboxEnqueuer` holds an `OnceLock<Arc<Outbox>>`; requests before start fail with internal
  error). Processor tuning `batch_size(1)` so `msg.attempts` is per message. Payload type
  `application/json`; partition = hash(uuid) % partitions; payload limit 64 KiB
  (`OutboxError::PayloadTooLarge` → 400 on chat delete, 500 elsewhere).
  OData: `toolkit::api::odata::OData` extractor; pagination with `toolkit_db::odata::paginate_with_odata`
  over a `FieldMap` (chats: `updated_at`, `id`, `title`; messages: `created_at`, `id`, `role`),
  tiebreaker `id`, `LimitCfg { default: 20, max: 100 }`; errors convert to the canonical
  `gts.cf.core.odata.query.v1~` problems automatically.
- **Rationale**: the toolkit forbids unscoped DB access; secure builders cover CAS and atomic counters.
- **Alternatives**: raw SQL repositories (not possible: `DBRunner` exposes no executor).

## R7. Streaming relay

- **Decision**: provider task (spawned) → bounded `tokio::mpsc` (capacity
  `streaming.sse_channel_capacity`) → SSE response stream. The SSE stream owns a drop guard that
  cancels a `CancellationToken` when the client disconnects; the provider task selects on
  cancellation and on channel-send failure and finalizes the turn as `cancelled`. Ping: the SSE
  stream emits `ping` on idle timer only until the first delta/tool. Terminal events are sent only
  after the finalization transaction commits; if the provider task ends without sending a terminal
  event the relay synthesizes `error{stream_interrupted}`.
- **Setup isolation**: send/retry/edit handlers run the setup in a spawned task and await it, so a
  client disconnect during setup does not abort a half-committed turn.

## R8. Thumbnails

- **Decision**: `image` 0.25 decode with `image::io::Limits` (max alloc = `thumbnail.max_decode_bytes`),
  `thumbnail()` fit inside W×H, encode lossless WebP (`WebPEncoder::new_lossless`); skip when over
  `max_bytes`, `max_pixels` or decode limit.

## R9. Testing strategy

- **Decision**: (1) unit tests for pure modules; (2) in-process router tests: real handlers + SQLite
  in-memory DB + fake `ServiceGatewayClientV1` (scripted SSE responses, records requests) + fake policy
  plugin + allow-all/deny/fail authz fakes; (3) Python black-box suite (`tests/blackbox`) against the
  built binary with an aiohttp mock provider (Responses API SSE, Files, Vector Stores), inspecting the
  SQLite file and the recorded provider requests.
- **Rationale**: (2) gives fast deterministic coverage of the full pipeline; (3) proves wiring
  (OAGW provisioning, plugins, config, migrations) end to end like the acceptance suite.
