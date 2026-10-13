# Phase 0 Research: Mini Chat Gear

Detailed API notes collected during research are summarized here as decisions. Normative behavior is in `gears/mini-chat/docs/`.

## R1. Gear shape and lifecycle
- **Decision**: `#[toolkit::gear(name = "mini-chat", deps = [types_registry, authn_resolver, authz_resolver, oagw], capabilities = [db, rest, stateful])]` with a direct `RunnableCapability` (`start`/`stop`). `init` loads/validates config, resolves ClientHub clients (`AuthZResolverApi`, `ServiceGatewayClientV1`, `AuthNResolverClient`, `TypesRegistryClient`), builds services. `start` exchanges client credentials (S2S), provisions OAGW upstreams/routes (deferred retry loop for not-yet-readable secrets), starts the outbox pipeline (5 queues) and spawns orphan watchdog + upload reaper. `stop` cancels workers, joins with a bound, stops the outbox.
- **Rationale**: matches DESIGN "Gear lifecycle"; `lifecycle(...)` single-task form cannot express start/stop ordering cleanly.
- **Alternatives**: `lifecycle(entry="serve")` (credstore/types-registry) — rejected, the design separates start work from stop.

## R2. Persistence
- **Decision**: SeaORM entities with `#[derive(Scopable)]`, raw dual-dialect DDL migrations (PG `UUID/TIMESTAMPTZ`, SQLite `BLOB/TEXT`), exact table/column names of DESIGN §3.7. Shared outbox via `toolkit_db::outbox::outbox_migrations()` (default table prefix `toolkit_outbox`) appended to the gear migrations. Timestamps `time::OffsetDateTime`, set from Rust.
- **Rationale**: SQLite UUID columns must be BLOB (sqlx encodes bytes; DESIGN says so). Partial unique indexes work on both engines.
- **Scope**: chats/quota/reactions scoped with tenant + owner; child tables tenant-scoped with chat ids obtained from an owner-scoped chat lookup. Background workers use `AccessScope::allow_all()`.

## R3. Authorization (PEP)
- **Decision**: `PolicyEnforcer` with resource types `gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~` (props owner_tenant_id, owner_id, id), model (permission-only, `require_constraints=false`), user quota. Compiled scope is narrowed with `ensure_owner(subject_id)` (static-authz returns only a tenant predicate). `Denied`/`CompileFailed` → 403 `AUTHZ_DENIED`; `EvaluationFailed` → 503 + `Retry-After: 5`.
- **License**: `.authenticated().require_license_features([&BaseLicense])` — the gateway's license middleware accepts the base feature without a plugin.

## R4. Errors
- **Decision**: `toolkit-canonical-errors` with `#[resource_error(gts_id!("cf.core.mini_chat.<x>.v1~"))]` for chat/message/turn/attachment/model and the OData type from toolkit-odata. Platform extractors `toolkit::api::rest::extract::{Json, Path}` give `json_syntax_error`/`invalid_json_body`/`missing_json_content_type`/`invalid_path_params`. OData extractor gives the OData reasons.

## R5. Provider integration (OAGW)
- **Decision**: all provider traffic via `ServiceGatewayClientV1::proxy_request` to `/{alias}{path}` using the **S2S security context** obtained at start (fallback: request context). Upstreams created with the S2S context: endpoint `{scheme(use_http), host, port}`, alias `upstream_alias` or host (explicit alias for IP hosts; if OAGW rejects an explicit alias for a hostname the derived alias is used), header passthrough allowlist (`content-type`, `accept`), auth plugin from `auth_plugin_type`/`auth_config` (omitted when not configured). Routes: chat `POST` prefix from `api_path` (query allowlist from its query), RAG routes `/v1/files`, `/v1/vector_stores` (openai) or `/openai/files`, `/openai/vector_stores` with `api-version` allowlisted (azure). AlreadyExists → reuse (look up by alias via `list_upstreams`).
- **Rationale**: upstreams are owned by the creating tenant; proxying with the S2S context makes them reachable for every tenant/user and lets the API-key plugin read the secret owned by the gear identity.
- **Adapter**: `openai_responses` implemented fully (request builder, SSE translation, usage/annotations). `vllm_responses` reuses it without tools; `openai_chat_completions` and `anthropic_messages` implemented with basic text streaming (ADR-0005 notes they have no E2E coverage).
- **SSE parsing**: `oagw_sdk::ServerEventsStream` over the streamed body; event name from `event:` or the JSON `type`.

## R6. Streaming to the client
- **Decision**: per-request bounded `mpsc` (capacity `streaming.sse_channel_capacity`), provider task spawned with a `CancellationToken`; the SSE body stream holds a drop guard that cancels the token on client disconnect; axum `Sse` with 30 s comment keep-alive; explicit `ping` events before the first content event. Terminal event only after finalization commit.

## R7. Outbox
- **Decision**: one `Outbox` pipeline with 5 leased queues (`usage`, `attachment_cleanup`, `chat_cleanup`, `thread_summary` lease = claim_timeout, `audit` lease 60 s), `Partitions::of(outbox.num_partitions)`, handler `batch_size(1)` so `attempts` behaves per message; enqueue inside the domain transaction, `Wake::fire()` after commit.

## R8. Plugins
- **Decision**: SDK declares `MiniChatModelPolicyPluginSpecV1` / `MiniChatAuditPluginSpecV1` (`#[gts_type_schema(base = PluginV1)]`) and the plugin client traits. Static plugins are separate gears in the mini-chat crate (`infra::plugins::static_model_policy` = gear `static-mini-chat-model-policy-plugin`, `infra::plugins::static_audit` = gear `static-mini-chat-audit-plugin`) registering a GTS instance + scoped client. The main gear selects by `vendor` with `GtsPluginSelector`; an absent policy plugin yields an empty catalog (gear still boots).

## R9. Thumbnails
- **Decision**: `image` crate (png/jpeg/gif/webp decode, lossless WebP encode), resize to fit `thumbnail.width x height`, skip on size/pixel limits, decoder allocation limited via `image::Limits`.

## R10. Testing
- **Decision**: unit tests in companion `*_tests.rs` files; integration tests in `gears/mini-chat/mini-chat/tests/` that assemble the gear's REST router (`register_routes`) with real SQLite (shared-cache memory DB, migrations), a capturing/allowing mock `AuthZResolverApi`, a scripted fake `ServiceGatewayClientV1` (records requests; returns Responses-API SSE, Files and Vector Stores JSON), the static policy plugin service as a direct client, and the outbox pipeline; assertions on HTTP status/Problem bodies, SSE events, DB rows and recorded provider requests. A final manual smoke run of the real server (`cf-gears-example-server`) against a Python mock provider validates wiring.
- **Alternatives**: OAGW `test-utils` harness (pulls pingora; heavier) — rejected for unit/integration tests.

## R11. On-demand provisioning of deferred upstreams (added during convergence)
- **Decision**: in addition to the DESIGN reconcile schedule (first retry after 2 s, doubling to 60 s, warning after 2 min), a proxied provider or storage call whose upstream alias is still pending triggers one immediate, bounded (3 s) provisioning attempt for that alias (`ProviderResolver::ensure_ready` → `Provisioner` `ProvisionHook`).
- **Rationale**: the provider credential (`cred://…`) often becomes readable only after start (e.g. the secret is created through credstore after boot). With only the backoff schedule the first requests could fail for up to 60 s after the secret appears. The extra attempt does not change the reconcile schedule, the provisioned resources, or the error contract; when the credential is still unreadable the call proceeds and fails as before (`provider_error` / storage 503).
- **Alternatives**: shorter fixed reconcile interval (contradicts DESIGN), blocking requests until provisioned (unbounded latency) — rejected.

## R12. SQLite contention and cursor encoding (added during implementation)
- **Write lock up front**: on SQLite every write transaction starts with a no-op `UPDATE` so the write lock is taken before any read; concurrent writers (outbox workers, parallel requests) then wait on the busy handler instead of failing a read→write upgrade with `SQLITE_BUSY` / `SQLITE_BUSY_SNAPSHOT`. No effect on PostgreSQL.
- **Datetime cursor keys**: the OData cursor decoder binds datetime keys as `time::OffsetDateTime` (`…Z`) while chrono values are stored as `…+00:00`; on SQLite the mappers therefore encode `updated_at` / `created_at` cursor keys as the exact storage text (`cursor_kind = String`), keeping keyset boundaries exact.
