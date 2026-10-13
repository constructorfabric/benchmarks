# Implementation Plan: Mini-Chat Gear (multi-tenant AI chat) and its SDK

**Branch**: `001-mini-chat-gear` | **Date**: 2026-10-03 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/001-mini-chat-gear/spec.md`

## Summary

Implement the `mini-chat` gear (`gears/mini-chat/mini-chat`, lib `mini_chat`) and a new SDK crate
(`gears/mini-chat/mini-chat-sdk`, package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`) following the
gear docs (PRD, DESIGN, ADRs). The gear registers with the toolkit runtime, owns its SQLite/Postgres schema
(SeaORM migrations + toolkit-db outbox), serves the REST/SSE API under `/mini-chat/v1`, provisions OAGW
upstreams/routes for configured providers at start, proxies LLM requests through OAGW
(`openai_responses` adapter fully; other kinds best effort), enforces reserve→execute→settle credit quotas,
and runs background work (outbox handlers for usage/audit/attachment cleanup/chat cleanup/thread summary,
orphan watchdog, upload reaper). Two bundled plugin gears (`static-mini-chat-model-policy-plugin`,
`static-mini-chat-audit-plugin`) live in `mini_chat::infra::plugins::{static_model_policy, static_audit}`
and are discovered through types-registry.

Detailed behavioral facts are condensed in [research-design-notes.md](./research-design-notes.md);
platform conventions in [research-platform-rest-db.md](./research-platform-rest-db.md) and
[research-platform-integrations.md](./research-platform-integrations.md); decisions in
[research.md](./research.md).

## Technical Context

**Language/Version**: Rust (toolchain pinned by `rust-toolchain.toml`, edition 2024)

**Primary Dependencies**: toolkit (gear runtime, REST operation builder, ClientHub), toolkit-db (SeaORM,
secure scoping, outbox), toolkit-canonical-errors, toolkit-odata, toolkit-security, authz-resolver-sdk,
authn-resolver-sdk, oagw-sdk, types-registry-sdk, axum, serde, utoipa, image (thumbnails), multer.

**Storage**: SQLite (dev/test) and PostgreSQL via SeaORM migrations; shared toolkit-db outbox tables.

**Testing**: `cargo test -p cf-gears-mini-chat -p cf-gears-mini-chat-sdk` — unit tests for pure domain logic
(credits, estimation, cascade, context truncation, SSE translation, sanitization, error mapping) and
integration tests running the domain services + REST router against in-memory SQLite with a fake
provider (`LlmProvider` trait implementation) and the static policy plugin.

**Target Platform**: Linux server (`cf-gears-example-server`).

**Project Type**: Rust library crates (gear + SDK) linked into the example server.

**Performance Goals**: SSE relay without buffering; bounded channel (32).

**Constraints**: offline build, lockfile-bounded deps; must build with
`--no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore`.

**Scale/Scope**: ~20 routes, 9 tables, 5 outbox queues, 2 background workers, 2 bundled plugins.

## Constitution Check

| Principle | Gate | Status |
|---|---|---|
| I. Component docs are the contract | Routes/DTOs from `contracts/openapi-mini-chat.json`; config from `contracts/config.md`; schema from `data-model.md` | PASS |
| II. Test-first | Every acceptance item mapped to tests in tasks.md; test tasks precede implementation | PASS |
| III. Platform conventions | gear macro, SDK crate, ClientHub, secure ORM, canonical errors, OData, OAGW | PASS |
| IV. Offline/lockfile deps | Only workspace deps already in Cargo.lock | PASS |
| V. Security & isolation | owner+tenant scoping on every query; provider id sanitization | PASS |
| VI. Streaming integrity | bounded channel relay; CAS finalization; replay side-effect-free | PASS |

Post-design re-check: PASS (no violations; see Complexity Tracking).

## Architecture

### SDK crate `mini_chat_sdk`
- `models.rs`: `ModelCatalogEntry`, `ModelTier`, `EstimationBudgets`, `ModelGeneralConfig` (`ApiParams`,
  `ModelFeatures`, `ModelToolSupport`, `SupportedEndpoints`), `ModelPreference`, `KillSwitches`,
  `PolicySnapshot`, `PolicyVersionInfo`, `UserLimits`, `TierLimits`, `UsageEvent`, `UsageTokens`,
  `TurnAuditEvent` (+ mutation audit events), `ToolCalls`.
- `plugin_api.rs`: `MiniChatModelPolicyPluginClientV1` (get_current_policy_version, get_policy_snapshot,
  get_user_limits, publish_usage, check_user_license default) and `MiniChatAuditPluginClientV1`
  (emit_turn_audit / emit_mutation_audit) with `PublishError`/`AuditPluginError` (Transient/Permanent/Timeout).
- `gts.rs`: plugin spec schema types `MiniChatModelPolicyPluginSpecV1`, `MiniChatAuditPluginSpecV1`.

### Gear crate `mini_chat`
```text
src/
├── lib.rs                     # pub mod api, config, domain, infra, gear
├── gear.rs                    # #[toolkit::gear(name="mini-chat", deps=[types_registry, authn_resolver, authz_resolver, oagw], capabilities=[db, rest, stateful])]
├── config.rs                  # MiniChatConfig (deny_unknown_fields) + validation
├── api/rest/                  # routes.rs, handlers/*.rs, dto.rs, sse.rs, error mapping
├── domain/
│   ├── error.rs               # DomainError -> CanonicalError mapping
│   ├── credits.rs, estimate.rs, periods.rs   # pure arithmetic
│   ├── policy.rs              # model resolution, default model, cascade
│   ├── context.rs             # ContextPlan assembly & truncation
│   ├── sanitize.rs            # provider id / url / credential scrubbing
│   ├── authz.rs               # PEP helper (actions, resource types)
│   └── service/               # chat, message, stream, turn, attachment, reaction, model, quota, finalize, replay, summary
└── infra/
    ├── db/                    # entities/*.rs, migrations/*.rs, repo/*.rs
    ├── llm/                   # provider config/resolver, openai_responses adapter, SSE parser, files & vector stores
    ├── oagw_provisioning.rs
    ├── outbox/                # usage, audit, attachment_cleanup, chat_cleanup, thread_summary handlers
    ├── workers/               # orphan_watchdog.rs, upload_reaper.rs
    ├── policy_gateway.rs, audit_gateway.rs
    └── plugins/{static_model_policy, static_audit}/
```

Key flows follow DESIGN §3.6 (send, retry/edit, upload, cancellation, thread summary, cleanup) exactly; see
research-design-notes.md for the normative ordering of checks.

### Testing strategy
- Provider abstraction: domain depends on an `LlmClient`/`ProviderTransport` trait; production
  implementation uses OAGW `proxy_request`; tests use a scripted fake producing Responses-API SSE events and
  recording requests (to assert provider request shape and "no provider call" invariants).
- Policy plugin: tests use the static plugin implementation directly (configurable catalog, limits, kill
  switches).
- AuthZ: tests use a fake PolicyEnforcer/AuthZ client returning owner constraints, deny, or failure.
- DB: in-memory SQLite with gear migrations + outbox migrations.

## Project Structure

### Documentation (this feature)

```text
specs/001-mini-chat-gear/
├── plan.md, research.md, research-design-notes.md, research-platform-*.md
├── data-model.md, quickstart.md
├── contracts/ (openapi-mini-chat.json, config.md, sse.md, errors.md)
└── tasks.md
```

### Source Code (repository root)

```text
gears/mini-chat/mini-chat-sdk/   # new SDK crate (workspace member)
gears/mini-chat/mini-chat/       # gear crate (existing member)
  src/ (see Architecture)
  tests/                         # integration tests (not in testing/e2e/suites/mini_chat)
Cargo.toml                       # add SDK member + workspace dep
```

**Structure Decision**: two crates (SDK + gear) mirroring other gears (e.g. credstore-sdk/credstore); bundled
plugins live inside the gear crate because the example server imports them from there.

## Complexity Tracking

| Violation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|-------------------------------------|
| (none) | | |

## Platform decisions (from research-platform-*.md)

- Gear: `#[toolkit::gear(name="mini-chat", deps=[types_registry, authz_resolver, authn_resolver, oagw],
  capabilities=[db, rest, stateful], lifecycle(entry="serve", stop_timeout="30s", await_ready))]`; services in
  `OnceLock`; `migrations()` = gear migrations + `outbox_migrations_with_prefix("mini_chat_outbox")`.
  `use oagw as _;` etc. to force-link OAGW (no `oagw` server feature in the required build).
- Config via `ctx.config_expanded_or_default::<MiniChatConfig>()` (`ExpandVars` on provider host/auth_config and
  client_credentials), `deny_unknown_fields` where DESIGN requires.
- REST: `OperationBuilder` with literal `/mini-chat/v1/...` paths, `.authenticated()
  .require_license_features([License])` (base feature), canonical extractors `toolkit::api::rest::extract::{Json, Path}`,
  `OData` extractor + `toolkit_db::odata::paginate_odata` (`LimitCfg{default:20,max:100}`), per-route `error_*`
  sets matching the contract, `.sse_json::<MiniChatSseEvent>()` for stream endpoints, `.multipart_file_request("file", ..)`
  for upload, `.with_throttling(ThrottlingSpec{rl_mini_chat_chat, ifl_mini_chat_chat, require_security_context:false,
  dry_run:true})` on create_chat. Services injected with `axum::Extension`.
- Errors: `#[resource_error(gts_id!("cf.core.mini_chat.{chat,message,turn,attachment,model}.v1~"))]` markers;
  `From<DomainError> for CanonicalError`; PEP `Denied|CompileFailed` → 403 `AUTHZ_DENIED`, `EvaluationFailed` →
  503 `Retry-After: 5`.
- AuthZ: `PolicyEnforcer::access_scope(_with)` per DESIGN action matrix, then explicit `user_id = subject_id`
  owner filter (static PDP only constrains tenant).
- DB: SeaORM entities with `#[derive(Scopable)]`, all statements through secure wrappers (`.secure().scope_with`,
  `secure_insert`, `update_many().secure()`), background jobs with `AccessScope::allow_all()`; transactions via
  `DBProvider::transaction`; raw per-backend SQL migrations (UUID BLOB in SQLite).
- Outbox: `Outbox::builder(db).table_prefix(..).processor_tuning(batch_size(1)).queue(name, Partitions::of(n))
  .leased(handler)[.lease(LeaseConfig)]...start()`; enqueue with `Record::to(queue, partition).payload(json,
  "application/json").build()` inside the business transaction, `wake.fire()` after commit; max attempts checked
  in handlers via `msg.attempts`.
- OAGW: at start, S2S context via `AuthNResolverClient::exchange_client_credentials`; `create_upstream`
  (alias = upstream_alias || host; `SharingMode::Inherit`; header passthrough allowlist) + routes (chat POST
  prefix from api_path; RAG prefix `/v1` or `/openai` with GET/POST/DELETE and `api-version` allowlist);
  `AlreadyExists` → reuse; `FailedPrecondition(auth.config.secret_ref)` → background reconcile (2s→60s).
  Proxy with the user's SecurityContext to `/{alias}{path}`; handle `Err(CanonicalError)`, gateway problem
  responses (`ErrorSource::Gateway`) and provider statuses; parse SSE with `oagw_sdk::sse::ServerEventsStream`;
  stream end without terminal event ⇒ provider_error.
- Plugins: SDK spec types `MiniChatModelPolicyPluginSpecV1` / `MiniChatAuditPluginSpecV1`
  (`gts_type_schema(base = PluginV1)`), static plugin gears register instance + scoped client; main gear resolves
  lazily with `GtsPluginSelector` + `choose_plugin_instance` (no caching of "not found").
- Tests: in-memory SQLite (`connect_db` + `run_migrations_for_testing`), `tower::ServiceExt::oneshot` on the
  router with `SecurityContext` in extensions, mock `AuthZResolverApi`, fake provider transport trait.
