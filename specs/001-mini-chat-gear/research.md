# Research: Mini Chat Gear

All unknowns from the Technical Context are resolved below. Sources: the gear docs
(`gears/mini-chat/docs`), the toolkit libraries under `libs/`, and existing gears (credstore,
types-registry, oagw, authn-resolver).

## R1. Gear declaration and lifecycle

- **Decision**: `#[toolkit::gear(name = "mini-chat", deps = [types_registry, authn_resolver,
  authz_resolver, oagw], capabilities = [db, rest, stateful])]` with a hand-written
  `RunnableCapability` (`start` / `stop`) instead of `lifecycle(entry = ...)`.
- **Rationale**: DESIGN §3.2 "Gear lifecycle" needs distinct init/start/stop phases: start
  provisions OAGW, starts the outbox pipeline and spawns the watchdog and reaper; stop cancels and
  joins them. `RunnableCapability` maps onto this directly.
- **Alternatives**: `lifecycle(entry="serve")` (one serve loop) was rejected because the
  provisioning and outbox start must finish before workers spawn and must fail startup on a
  misconfiguration.

## R2. Bundled plugins in the same crate

- **Decision**: `infra::plugins::static_model_policy` declares the gear
  `static-mini-chat-model-policy-plugin` and `infra::plugins::static_audit` declares
  `static-mini-chat-audit-plugin`. Both have `deps = [types_registry]`. They register a GTS instance
  with `PluginV1::<Spec>::build_registration` and a scoped client
  (`ClientScope::gts_id(instance_id)`). The host resolves them lazily with `GtsPluginSelector` and
  `choose_plugin_instance` (filtered by `vendor`, lowest `priority`), copying credstore's
  `plugin_select.rs`.
- **Rationale**: this is the platform pattern. Lazy resolution tolerates init order.
- **Spec types** (in the SDK): `MiniChatModelPolicyPluginSpecV1`, GTS
  `cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~`, and
  `MiniChatAuditPluginSpecV1`, GTS `cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~`.

## R3. REST wiring, errors, OData

- **Decision**: register routes with `OperationBuilder` at the paths `{url_prefix}/v1/...`. Use the
  operation ids and tags from `docs/api/api.json`. Every route uses `.authenticated()` and
  `.require_license_features([&BaseLicense])`, so the platform base feature is required (ADR-0008).
  `create_chat` binds `ThrottlingSpec { rate_limit_zone: "rl_mini_chat_chat", in_flight_limit_zone:
  "ifl_mini_chat_chat", require_security_context: false, dry_run: true }`; both zones are defined in
  every repo config, and the e2e config comments say an undefined zone fails startup.
- **Errors**: return `CanonicalError` from handlers. Each mini-chat resource has a `#[resource_error]`
  marker: `gts.cf.core.mini_chat.{chat,message,turn,attachment,model}.v1~`. Bodies and paths use the
  platform extractors `toolkit::api::rest::extract::{Json, Path}`; they produce
  `json_syntax_error` / `invalid_json_body` (422) / `missing_json_content_type` (415) /
  `invalid_path_params`. PDP failure maps to `service_unavailable().with_retry_after_seconds(5)`,
  which emits the `Retry-After` header.
- **OData**: the `OData` extractor plus `toolkit_db::odata::paginate_odata` with
  `LimitCfg { default: 20, max: 100 }`. Filter-field enums are written by hand for chats
  (`updated_at`, `id`, `title`) and messages (`created_at`, `id`, `role`). Errors convert through
  `CanonicalError::from(ODataError)`, which keeps `resource_type = gts.cf.core.odata.query.v1~`.
- **SSE**: `axum::response::sse::Sse` over a bounded `mpsc` receiver stream, with
  `KeepAlive::new().interval(30s)` (an SSE comment). The `ping` events before content are emitted
  by the stream task itself. The route declares `.sse_json::<MiniChatSseEvent>()`.

## R4. Persistence

- **Decision**: SeaORM entities with `#[derive(Scopable)]`.
  - `chats`, `quota_usage`, `message_reactions`: `tenant_col = "tenant_id"`, `owner_col = "user_id"`.
  - Child tables: `tenant_col` only, filtered by a `chat_id` taken from an owner-scoped chat query.
  - Raw-SQL migrations per backend (Postgres / SQLite), like credstore.
  - UUIDs are BLOB(16) on SQLite, timestamps are TEXT RFC 3339, `period_start` is DATE (`time::Date`).
  - The toolkit outbox tables use the default prefix (`toolkit_outbox_*`, via `outbox_migrations()`).
- **Owner scoping**: static-authz emits only `owner_tenant_id`. The PEP scope is therefore narrowed
  with `AccessScope::ensure_owner(subject_id)` (DESIGN §3.8: "every chat-scoped operation adds an
  owner predicate").
- **Transactions**: `DBProvider::transaction`. Outbox enqueue happens inside the same transaction;
  the `Wake` is fired after commit.

## R5. Outbox queues and handlers

- **Decision**: one `Outbox::builder(db)` pipeline with five leased queues:
  - `mini-chat.usage_snapshot` and `mini-chat.audit` (lease 60 s), partitioned by tenant
  - `mini-chat.attachment_cleanup`, partitioned by tenant
  - `mini-chat.chat_cleanup` and `mini-chat.thread_summary` (lease `claim_timeout_secs`),
    partitioned by chat
- `num_partitions` defaults to 4. Processor tuning uses `batch_size(1)`, so `attempts` is
  per-message.
- Payloads are JSON. Partition index = `hash(uuid) % partitions`.

## R6. OAGW provisioning and proxying

- **Provisioning (at start)**:
  - Exchange `client_credentials` through `AuthNResolverClient::exchange_client_credentials` for an
    S2S `SecurityContext`.
  - For every provider and tenant override, `create_upstream`:
    - endpoint `{host, port, scheme http|https}`, protocol `HTTP_PROTOCOL_ID`, alias = `upstream_alias`
    - auth `{plugin_type, sharing: Inherit, config}`
    - header passthrough allowlist: `accept`, `api-version`, `anthropic-version`
  - An existing alias (AlreadyExists) is reused.
  - Then `create_route` for the chat prefix and the RAG prefix:
    - methods POST/GET/DELETE, `PathSuffixMode::Append`
    - query allowlist `["api-version"]` for azure
    - an AlreadyExists route conflict is tolerated
  - A deterministic validation failure (config) fails start. A `FailedPrecondition` caused by an
    unreadable secret is retried in the background with 2 s→60 s backoff.
- **Proxy identity**: requests are proxied with the gear's S2S security context, not the user's.
  - **Rationale**: OAGW resolves upstreams within the caller's tenant chain and reads the API-key
    secret with the caller's context. Only the S2S context is guaranteed to see the upstream it
    created and its secret, for users of every tenant, including tenants outside the root's subtree.
  - Tenant and user attribution still go to the provider in `user` and `metadata` (DESIGN §4
    "Provider Request Metadata").
- **Streaming**: `proxy_request` returns `Body::Stream`. It is parsed incrementally by an internal
  SSE line decoder, so nothing is buffered. Non-2xx statuses are mapped:
  - 429 → `rate_limited`
  - 504, or a gateway `DeadlineExceeded` → `provider_timeout`
  - anything else → `provider_error`

## R7. Provider adapter scope

- **Decision**: implement `openai_responses` fully: request building, SSE translation, files and
  vector stores (openai and azure storage kinds).
  - `openai_chat_completions` and `vllm_responses`: compact adapters with text streaming only.
  - `anthropic_messages`: accepted in config, with text streaming.
- **Rationale**: ADR-0005 confirms the E2E rig uses `openai_responses` only. The acceptance checklist
  needs every behaviour exercised through the Responses wire format.

## R8. Testing strategy (explicitly required)

- **Unit tests** (in-crate `#[cfg(test)]`): credit arithmetic, cascade, estimation, truncation,
  sanitization, error mapping, billing derivation, the SSE parser, MIME and filename rules, title
  validation.
- **Integration tests** (`gears/mini-chat/mini-chat/tests/`), in-process:
  - a per-test SQLite database with the gear's migrations (see R9)
  - the real domain services and the axum router built by `register_routes`
  - a mock `AuthZResolverApi`
  - the in-crate static policy plugin client
  - a fake `ServiceGatewayClientV1` that emulates an OpenAI-compatible provider: Responses SSE,
    Files, Vector Stores. It records requests and supports scripted failures.
- Requests are driven through `tower::ServiceExt::oneshot`. Assertions cover HTTP/SSE bodies, DB
  rows and recorded provider requests. Outbox handlers are invoked directly.
- A separate smoke run builds the example server and starts it with a test config against a Python
  mock provider (quickstart.md).
- **Alternatives**: driving the full server binary from Rust tests was rejected as too slow and
  brittle for every acceptance item. It is covered once by the quickstart smoke run instead.

## R9. Test database: temporary file-backed SQLite (WAL) instead of shared-cache in-memory

- **Decision**: each integration harness opens its own temporary file database
  (`sqlite://<tmp>/mini_chat_test_<uuid>.db?mode=rwc&journal_mode=WAL&busy_timeout=10000`), applies
  the gear and outbox migrations, and deletes the files when the harness is dropped. The suites run on
  a multi-threaded Tokio runtime so the outbox workers make progress next to the request handlers.
- **Rationale**: with `file:...?mode=memory&cache=shared` the outbox workers (one processor per
  partition) and the request path compete for SQLite shared-cache table locks. These fail at once
  with `SQLITE_LOCKED` ("database is deadlocked"), there is no busy wait, and outbox messages stall.
  A WAL file database behaves like the production SQLite deployment: lock waits honour
  `busy_timeout`, and read-then-write transactions that lose the race (`SQLITE_BUSY` /
  `SQLITE_BUSY_SNAPSHOT`) are retried by `MiniChatService::tx` (`transaction_with_retry_max`). The
  database is still per-test, hermetic and disposable, so FR-017's intent (an isolated database with
  no external server) is kept.
- **Alternatives**: a single-connection in-memory pool. Rejected: it serializes the stream task, the
  outbox workers and the handlers on one connection, which hides the contention production sees.
