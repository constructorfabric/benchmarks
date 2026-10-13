# Mini-Chat — Platform Integration Notes (research digest, 2026-10-04)

Reference for implementers. Collected by read-only research of the workspace. Three reports: (1) gear/REST/plugin conventions, (2) DB / authz / OData / outbox / canonical-error libraries, (3) OAGW / AuthN S2S / api-gateway / e2e harness. Verify exact signatures in the cited source files before relying on them.

I mapped the full gear pattern: declaration, config, DB, ClientHub, REST, plugins, linking and tests. The main things to know before you start:

- **No mini-chat SDK crate exists.** The workspace has no `mini-chat-sdk`. The design refers to the "Mini Chat SDK" and to `MiniChatAuditPluginClientV1`, `MiniChatModelPolicyPluginClientV1` and their `…PluginSpecV1` types. You either create `gears/mini-chat/mini-chat-sdk` (the convention, see §3) or put those types inside `mini_chat`.
- **There is no git history to recover.** The repo has one commit (`spec-bench seed`). `gears/mini-chat/mini-chat/src/lib.rs` only has empty `pub mod infra { pub mod plugins { pub mod static_audit {} pub mod static_model_policy {} } }`.
- **The target REST surface is already documented.** `/app/docs/api/api.json` has all 19 `/mini-chat/v1/...` operations with operation ids `mini_chat.*` (§2).

## 1. Gear declaration, lifecycle, config, DB, ClientHub

**Macro** (`/app/libs/toolkit-macros/src/lib.rs`, re-exported as `toolkit::gear`). Real example from `/app/gears/credstore/credstore/src/gear.rs`:
```rust
#[toolkit::gear(
    name = "credstore",                       // kebab-case, enforced at compile time
    deps = [authz_resolver, tenant_resolver, types_registry], // crate idents; runtime name = ident with '_'→'-'
    capabilities = [system, db, rest, stateful],  // valid: db, rest, rest_host, stateful, system, grpc_hub, grpc
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct CredStoreGear { service: OnceLock<Arc<Service>> }   // + impl Default (or ctor = Expr)
```
- Other attributes: `ctor = MyGear::new()` and `client = path::Trait`. `client` only adds a compile-time object-safety check.
- The macro generates `pub const MODULE_NAME: &str` and an `inventory::submit!` registrator.
- It also emits `pub use ::<dep> as _gear_dep_<dep>` for each dep. So every `deps` crate must be a direct Cargo dependency and must appear in `[workspace.metadata.cargo-shear] ignored`. `authn-resolver`, `authz-resolver`, `oagw` and `types-registry` are already in that list.
- The gear crate needs `inventory`, `async-trait`, `anyhow` and `tokio-util` as dependencies: the lifecycle expansion references `::async_trait`, `::tokio_util::sync::CancellationToken` and `::anyhow`.
- Each capability is checked at compile time against a trait impl: `db` → `DatabaseCapability`, `rest` → `RestApiCapability`. `stateful` without `lifecycle(...)` → `RunnableCapability`. `system` needs no trait, but every existing gear writes `impl SystemCapability for X {}`.

**Traits** (`/app/libs/toolkit/src/contracts.rs`):
```rust
#[async_trait] pub trait Gear: Send + Sync + 'static {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()>; }
pub trait DatabaseCapability: Send + Sync {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>>; }
pub trait RestApiCapability: Send + Sync {
    fn register_rest(&self, ctx: &GearCtx, router: axum::Router, openapi: &dyn OpenApiRegistry) -> anyhow::Result<axum::Router>;
    fn healthcheck(&self, _ctx: &GearCtx) -> Option<Arc<dyn Healthcheck>> { None } }
#[async_trait] pub trait RunnableCapability: Send + Sync {
    async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()>;
    async fn stop(&self, deadline_token: CancellationToken) -> anyhow::Result<()>; }
#[async_trait] pub trait SystemCapability: Send + Sync {
    fn pre_init(&self, _sys: &SystemContext) -> anyhow::Result<()> { Ok(()) }
    async fn post_init(&self, _sys: &SystemContext) -> anyhow::Result<()> { Ok(()) } }
```
- With `lifecycle(...)`, write an inherent method instead of `RunnableCapability`:
  - `pub(crate) async fn serve(self: Arc<Self>, cancel: CancellationToken, ready: toolkit::lifecycle::ReadySignal) -> anyhow::Result<()>`
  - Without `await_ready`, drop the `ready` argument.
  - Call `ready.notify()` once ready, then loop on `tokio::select!{ () = cancel.cancelled() => break, _ = interval.tick() => … }`. See `CredStoreGear::serve`.
- `docs/toolkit_unified_system/08_lifecycle_stateful_tasks.md` shows an outdated "custom RunnableCapability" API (`run` / `RunnableHandle`). The real trait is `start`/`stop` as above.

**Phase order** (`/app/libs/toolkit/src/runtime/host_runtime.rs`):
1. `pre_init` (system gears only)
2. DB migrations
3. `init` in dependency order; system gears go first
4. proxy-wiring
5. `post_init` (system gears)
6. REST
7. gRPC
8. start/stop

Migrations are tracked per gear name (`migration_runner::run_migrations_for_gear`). Background work belongs in `serve`/start, because types-registry only switches to ready mode in its `post_init`.

**Config** (`/app/libs/toolkit/src/context.rs`, `config.rs`). `GearCtx` reads `gears.<gear-name>.config`:
- `ctx.config::<T>()` errors if the gear or its `config` key is missing.
- `ctx.config_or_default::<T>()` falls back to `T::default()` when absent; a parse error is still an error.
- `ctx.config_expanded[_or_default]::<T>()` also expands `${VAR}` in fields marked `#[expand_vars]`. It needs `#[derive(toolkit_macros::ExpandVars)]`; see `static-credstore-plugin/src/config.rs`.
- `ctx.raw_config()` returns the raw JSON.

The convention is:
```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CredStoreConfig { pub vendor: String, … }   // + impl Default, + fn validate(&self) -> Result<(), String>
```
`init` then does `let cfg: Cfg = ctx.config_or_default()?; cfg.validate().map_err(|e| anyhow!(...))?;`.

**DB.** `ctx.db_required()?` returns `toolkit_db::DBProvider<toolkit_db::DbError>`; `ctx.db()` returns an `Option`. Credstore re-wraps it with its own error type:
```rust
let db_raw = ctx.db_required()?;
let db: Arc<DBProvider<DomainError>> = Arc::new(DBProvider::new(db_raw.db()));   // DomainError: From<DbError>
```
- `DBProvider` methods (`/app/libs/toolkit-db/src/db_provider.rs`): `conn() -> Result<DbConn, E>`, `transaction(|tx: &DbTx| Box::pin(async move {…}))`, `transaction_with_config(TxConfig, f)`, `db() -> Db` (the `Db` is what the outbox builder takes).
- Secure ORM (`toolkit_db::secure`):
  - Entities derive `Scopable` with `#[secure(tenant_col="tenant_id", resource_col="id", no_owner, no_type)]`.
  - Queries go through `Entity::find().secure().scope_with(&AccessScope)?…`.
  - Writes use `SecureInsertExt`, `SecureUpdateExt` and `SecureDeleteExt`.
  - `AccessScope::for_tenant(id)` and `allow_all()` come from `toolkit_security`.
- Migrations follow credstore's `infra/storage/migrations.rs`: `pub struct Migrator; impl MigratorTrait for Migrator { fn migrations() -> Vec<Box<dyn MigrationTrait>> {…} }`, with `#[derive(DeriveMigrationName)] pub struct Migration;` and backend-specific raw SQL for Postgres and SQLite.
  - `DatabaseCapability::migrations` returns `Migrator::migrations()`.
  - If the gear uses the outbox, append `toolkit_db::outbox::outbox_migrations_with_prefix(PREFIX)` and use the same prefix in `Outbox::builder(db).table_prefix(PREFIX)?.queue(name, Partitions::of(n)).leased(handler).start().await?`. Example: `types-registry/src/gear.rs` and `infra/outbox.rs`. The outbox README is `/app/libs/toolkit-db/src/outbox/README.md`.

**ClientHub** (`/app/libs/toolkit/src/client_hub.rs`):
```rust
let authz = ctx.client_hub().get::<dyn AuthZResolverApi>()?;            // Result<Arc<T>, ClientHubError>
let enforcer = authz_resolver_sdk::PolicyEnforcer::new(authz);           // or PolicyEnforcer::from_hub(hub)
let oagw = ctx.client_hub().get::<dyn oagw_sdk::api::ServiceGatewayClientV1>()?;
let authn = ctx.client_hub().get::<dyn authn_resolver_sdk::AuthNResolverClient>()?; // exchange_client_credentials(&ClientCredentialsRequest{client_id, client_secret: SecretString, ..})
ctx.client_hub().register::<dyn MyClientV1>(Arc::new(LocalClient::new(svc)) as Arc<dyn MyClientV1>);
```
- Other methods: `try_get`, `register_scoped` / `get_scoped` / `try_get_scoped(&ClientScope)`, `ClientScope::gts_id(&str)`.
- Service state is stored in `OnceLock<Arc<Service>>` fields and set in `init`, failing with `"{} already initialized", Self::MODULE_NAME`. `register_rest` reads it back with `.get().cloned().ok_or_else(...)`.

## 2. REST routes

Builder: `toolkit::api::OperationBuilder` (`/app/libs/toolkit/src/api/operation_builder.rs`). Typestate forces you to choose an auth mode and a license mode before `.register()`. Pattern from `credstore/src/api/rest/routes.rs`:
```rust
let router = OperationBuilder::post("/credstore/v1/secrets")
    .operation_id("credstore.create_secret").summary("…").description("…").tag(TAG)
    .authenticated()                    // or .anonymous() (sets license too); .exposed() = visibility axis
    .no_license_required()              // or .require_license_features([&feature]) / ::<F>([])
    .path_param("ref", "desc")          // .query_param(..), .param(ParamSpec::header("If-Match").required(true))
    .json_request::<CreateSecretRequestDto>(openapi, "desc")
    .handler(handlers::create_secret)
    .no_content_response(StatusCode::CREATED, "…")   // json_response_with_schema::<T>(openapi, StatusCode::OK, "…")
    .error_400(openapi).error_401(openapi).error_403(openapi).error_409(openapi).error_500(openapi).error_503(openapi)
    .register(router, openapi);
router.layer(axum::Extension(svc))
```
- **Other request/response helpers:**
  - `.json_array_response_with_schema::<T>`
  - `.sse_json::<T>(openapi, "desc")`, which emits `text/event-stream` 200
  - `.multipart_file_request("file", Some("desc"))`, which also restricts the content type to `multipart/form-data`
  - `.octet_stream_request`, `.allow_content_types(&[...])`
  - `.response_header(ResponseHeaderSpec::new(...))`, e.g. the `Location` header on 201
  - `.error_404/413/415/422/429/502/504`, `.standard_errors(openapi)`
- **OData helpers:** `OperationBuilderODataExt::{with_odata_filter::<FilterField>(), with_odata_select(), with_odata_orderby::<…>()}`. DTOs derive `ODataFilterable` from `toolkit_odata_macros`, with `#[odata(filter(kind="Uuid"))]`. `toolkit-odata-macros` is not yet in mini-chat's Cargo.toml.
- **Licenses:** `toolkit::api::operation_builder::{LicenseFeature, CORE_GLOBAL_BASE_LICENSE_FEATURE}`. The constant is `gts.cf.core.lic.feat.v1~cf.core.global.base.v1`, which DESIGN requires for mini-chat. Write `enum License { Base }` with `impl AsRef<str>` and `impl LicenseFeature for License {}`.
- **Rate limiting:**
  ```rust
  .with_throttling(ThrottlingSpec { rate_limit_zone: Some("rl_mini_chat_chat".into()),
      in_flight_limit_zone: Some("ifl_mini_chat_chat".into()), require_security_context: false, dry_run: … })
  ```
  The zones are defined in `gears.api-gateway.config.rate_limit_zones` / `in_flight_limit_zones`, and gateway startup fails if a referenced zone is missing.
- **URL prefix:** `url_prefix` is a mini-chat config field (DESIGN §6819, default `/mini-chat`), not a toolkit feature. Build paths like `format!("{prefix}/v1/chats")`. The gateway's own `prefix_path` (e.g. `/cf`) is stripped before matching.
- **Expected operations (from `docs/api/api.json`):**
  - `list_chats` / `list_messages`: OData filter and orderby
  - `create_chat`: 201 with `Location` header, throttling zones
  - `upload_attachment`: `multipart/form-data`
  - `stream_message`: `POST …/messages:stream` → `text/event-stream` with schema `MiniChatSseEvent`
  - All operations use `bearerAuth`.
- **Handlers** get `SecurityContext` and the service as extensions:
  ```rust
  pub async fn create_secret(uri: axum::http::Uri, Extension(ctx): Extension<SecurityContext>,
      Extension(svc): Extension<Arc<Service>>, Json(body): Json<Dto>) -> ApiResult<impl IntoResponse>
  ```
  - `use toolkit::api::canonical_prelude::*;` brings in `CanonicalError`, `Problem`, `resource_error`, `ApiResult<T> = Result<T, CanonicalError>`, `OData`, `JsonBody`, `JsonPage`, `created_json`, `no_content`, `ok_json`, `extract`, and axum's `Json`, `StatusCode`, `IntoResponse`.
  - The gateway auth middleware inserts `SecurityContext`. With `auth_disabled: true` it injects a default subject/tenant context.
- **Canonical-error extractors:** use `toolkit::api::rest::extract::{Json, Path, Query}` (also `canonical_prelude::extract`) and `toolkit::api::odata::OData`. They map axum rejections to `CanonicalError` and render `application/problem+json` (`invalid_argument`; route bugs become `internal`). Credstore uses plain `axum::Json`, which does not do this.
- **Error mapping** (`credstore/src/infra/sdk_error_mapping.rs`):
  ```rust
  #[resource_error(gts_id!("cf.core.credstore.secret.v1~"))] pub(crate) struct SecretResource;
  impl From<DomainError> for CanonicalError {
      … SecretResource::not_found("…").with_resource("secret").create(),
        SecretResource::invalid_argument().with_field_violation(field, detail, "REASON").create(), … }
  ```
- **DTOs** use `#[toolkit_macros::api_dto(request, response)]`, which adds serde, utoipa and the `RequestApiDto`/`ResponseApiDto` marker traits, often with `#[serde(deny_unknown_fields)]`. `sse_json` requires `T: ResponseApiDto`.
- **PEP (authorization)** follows `credstore/src/domain/authz.rs`: `ResourceType::new(gts_id, &[pep_properties::OWNER_TENANT_ID])` and `enforcer.access_scope_with(ctx, &res, action, None, &AccessRequest::new().resource_property(OWNER_TENANT_ID, tenant).require_constraints(true))`. Map `Denied`/`CompileFailed` to 403 and `EvaluationFailed` to 503.
- **Stale doc:** `docs/TOOLKIT_PLUGINS.md` shows `.require_auth(...)`, which does not exist; use `.authenticated()`.

## 3. Plugins and the SDK

**SDK crate layout** (credstore-sdk, tenant-resolver-sdk, license-resolver-sdk):
- `api.rs`: public client trait
- `plugin_api.rs`: plugin trait (`#[async_trait] pub trait XPluginClientV1: Send + Sync { async fn …(&self, ctx: &SecurityContext, …) -> Result<_, XError>; }`)
- `models.rs`
- `error.rs`: a `thiserror` enum with variants like `NotFound`, `ServiceUnavailable{…}`, `NoPluginAvailable`, `Internal(String)`
- `gts.rs`
- `lib.rs` re-exports everything
- optional `test-util` feature with a `test_util`/`testing` module

SDK dependencies: `async-trait`, `thiserror`, `serde`, `serde_json`, `schemars`, `uuid`, `gts`, `toolkit-gts`, `toolkit`, `toolkit-security`, plus `utoipa` if needed. mini-chat currently lacks `schemars`.

**Plugin spec type** (`tenant-resolver-sdk/src/gts.rs`):
```rust
use toolkit::gts::PluginV1; use toolkit_gts::gts_type_schema;
#[derive(Default)]
#[gts_type_schema(dir_path = "schemas", base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.tenant_resolver.plugin.v1~"),
    description = "Tenant Resolver plugin specification", properties = "")]
pub struct TenantResolverPluginSpecV1;
```
`#[gts_type_schema]` and `gts_instance!` submit to a link-time inventory. Types-registry seeds that inventory during its own `init` (`all_inventory_type_schemas()`), so there is no runtime registration for schemas.

**Plugin gear** (`single-tenant-tr-plugin/src/gear.rs`, `static-credstore-plugin/src/gear.rs`):
```rust
#[toolkit::gear(name = "static-credstore-plugin", deps = [types_registry])]   // no capabilities
pub struct StaticCredStorePlugin { service: OnceLock<Arc<Service>> }
async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
    let cfg: Cfg = ctx.config_expanded_or_default()?;          // {vendor:"constructorfabric", priority:100,…}, deny_unknown_fields
    let service = Arc::new(Service::from_config(&cfg)?);       // validate before registering
    let (instance_id, instance_json) = PluginV1::<CredStorePluginSpecV1>::build_registration(
        "cf.core._.static_credstore.v1", cfg.vendor.clone(), cfg.priority)?;   // (GtsInstanceId, Value)
    let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
    RegisterResult::ensure_all_ok(&registry.register(vec![instance_json]).await?)?;
    self.service.set(service.clone()).map_err(|_| anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
    ctx.client_hub().register_scoped::<dyn CredStorePluginClientV1>(ClientScope::gts_id(&instance_id), service);
    Ok(()) }
```
- `PluginV1<P>` has fields `{ id: GtsInstanceId, vendor: String, priority: i16 /* lower wins */, properties: P }` (`/app/libs/toolkit-gts/src/plugin.rs`).
- For mini-chat, the two plugin gears live in `pub mod mini_chat::infra::plugins::{static_audit, static_model_policy}`. Each module holds its own `#[toolkit::gear(name = "static-mini-chat-audit-plugin" / "static-mini-chat-model-policy-plugin", deps = [types_registry])]` struct, configured under the matching key in `config/mini-chat.yaml`.

**Host-side resolution** (lazy, on first call): `credstore/src/infra/plugin_select.rs` and `tenant-resolver/src/domain/service.rs`, using `toolkit::plugins::{GtsPluginSelector, choose_plugin_instance, ChoosePluginError}`.
```rust
let id = self.selector.get_or_init(|| async {
    let reg = hub.get::<dyn TypesRegistryClient>()?;
    let type_id = XPluginSpecV1::gts_type_id();
    let instances = reg.list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*"))).await?;
    choose_plugin_instance::<XPluginSpecV1>(&vendor, instances.iter().map(|e| (e.id.as_ref(), &e.object)))
}).await?;                                     // ChoosePluginError::{PluginNotFound, InvalidPluginInstance}
hub.try_get_scoped::<dyn XPluginClientV1>(&ClientScope::gts_id(&id))  // None ⇒ "not registered yet" (503)
```
- `choose_plugin_instance` filters by vendor, picks the lowest priority, and checks `content.id == gts_id`.
- `selector.reset().await` clears the cache. DESIGN requires this for the audit gateway when a resolved instance has no client.
- `toolkit::telemetry::ThrottledLog` throttles the "plugin unavailable" warnings.

## 4. Example server linking and config

`/app/apps/cf-gears-example-server/src/registered_gears.rs` already has the lines:
```rust
#[cfg(feature = "mini-chat")] use mini_chat as _;
#[cfg(feature = "mini-chat")] use mini_chat::infra::plugins::static_audit as _;
#[cfg(feature = "mini-chat")] use mini_chat::infra::plugins::static_model_policy as _;
```
- Cargo features already exist: `mini-chat = ["dep:mini-chat"]`, `k8s = ["mini-chat/k8s"]`, with dependency `mini-chat = { package = "cf-gears-mini-chat", path = "../../gears/mini-chat/mini-chat", optional = true }`.
- Run command: `cargo run --bin cf-gears-server --features mini-chat,static-authn,static-authz,single-tenant,static-credstore -- --config config/mini-chat.yaml run`.
- Config keys under `gears:` are exactly the `name = "…"` values. Each entry is `{ database: { server: "sqlite_mini_chat", file: "mini_chat.db" }, config: {…} }`, and the DB server is declared under `database.servers`.
- `config/mini-chat.yaml` already holds the `mini-chat` config (vendor, orphan_watchdog, client_credentials, thread_summary_worker, providers…) and both plugin sections.
- The DESIGN config tables (around line 6800+) list all fields. They also note that `ModelCatalogEntry` and the other nested SDK types do not reject unknown keys, while the top-level plugin configs do.
- `config/quickstart.yaml` and `config/e2e-local.yaml` also contain `mini-chat` sections.

## 5. Test patterns

- **Gear `init` test** (`/app/gears/system/license-resolver/license-resolver/src/gear_tests.rs`):
  - Build a `ConfigProvider` impl whose `get_gear_config(name)` returns `{"config": {...}}`.
  - Create the context with `GearCtx::new(Gear::MODULE_NAME, Uuid, Arc::new(provider), Arc::new(ClientHub::new()), CancellationToken::new())`; `.with_db(..)` is available with the `db` feature.
  - Call `gear.init(&ctx)`, then assert `hub.get::<dyn Trait>()`.
  - That example omits the `config` wrapper, so it actually exercises the defaults.
- **SQLite repo tests** (`credstore/src/infra/storage/repo_impl/repo_tests.rs`):
  ```rust
  let db = toolkit_db::connect_db(&format!("sqlite:file:x_{}?mode=memory&cache=shared", Uuid::new_v4()),
      ConnectOpts { max_conns: Some(1), min_conns: Some(1), ..Default::default() }).await?;
  toolkit_db::migration_runner::run_migrations_for_testing(&db, Migrator::migrations()).await?;
  let provider = DBProvider::<DomainError>::new(db);
  ```
  This needs `toolkit-db` with the `sqlite` feature in dev-dependencies. mini-chat enables `sqlite` and `pg` in normal dependencies.
- **Route tests** (`credstore/src/api/rest/routes_tests.rs`): build the service with fakes, call `register_routes(Router::new(), &OpenApiRegistryImpl::new(), svc)`, put `SecurityContext` into the request extensions, and send with `tower::ServiceExt::oneshot`. mini-chat has no `tower` dev-dependency yet.
- **Test helpers:**
  - `credstore/src/domain/secret/test_support.rs`: `make_ctx(subject, tenant)` via `SecurityContext::builder().subject_id(..).subject_tenant_id(..).build()`, plus `MockAuthZResolver` (implements `AuthZResolverApi::evaluate(PlatformSecurityContext, EvaluationRequest)` and returns an `Eq` predicate on `OWNER_TENANT_ID`), `deny_enforcer()`, `TypeAwareAuthZResolver`.
  - `oagw/src/domain/test_support.rs`: `allow_all_enforcer()`, `DenyingAuthZResolverClient`, `CapturingAuthZResolverClient`, `MockTenantResolverClient`.
  - `types_registry_sdk::testing::MockTypesRegistryClient` (feature `test-util`): `with_instances(...)`, `make_test_instance`, useful for plugin-selection tests.
  - `toolkit_db::test_support::QueryRecorder` (feature `test-support`).
- **Conventions:**
  - Tests go in separate `*_tests.rs` files via `#[cfg(test)] #[path = "x_tests.rs"] mod x_tests;`. `gears/mini-chat` is currently on the dylint exclusion list for the DE1101 rule, which requires that.
  - Test files use `#![allow(clippy::unwrap_used, clippy::expect_used)]`.
  - The guide is `/app/docs/toolkit_unified_system/12_unit_testing.md`: SQLite in-memory plus mocked AuthZ, with E2E reserved for Postgres and HTTP.

## Gaps in `mini-chat/Cargo.toml` against these patterns
- Missing for the patterns above:
  - `schemars`, needed for `gts_type_schema` if the SDK types live in this crate
  - `toolkit-odata-macros`
  - `tower` as a dev-dependency
  - `types-registry-sdk` with the `test-util` feature as a dev-dependency
  - an SDK crate, if you create one; add it to workspace `members` and `[workspace.dependencies]`
- Already present: `inventory`, `async-trait`, `tokio-util`, `anyhow`, `toolkit-macros`, `toolkit-gts`, `gts`.
- Likely `deps`: `[types_registry, authz_resolver, authn_resolver, oagw]`. All four are already direct dependencies and already in the cargo-shear ignore list.
- The base GTS resource types such as `gts.cf.core.ai_chat.chat.v1~` are not defined anywhere. The mini-chat code would have to declare them (or just use the id strings in `ResourceType::new`).

---

I've covered all four areas. The two gears that use these libraries in real code are `gears/credstore/credstore` and `gears/system/types-registry/types-registry`. `gears/mini-chat/mini-chat/src/lib.rs` is still a stub, but its `Cargo.toml` already depends on toolkit-db (`sqlite`, `pg`), toolkit-odata (`with-utoipa`), toolkit-canonical-errors (`axum`), toolkit-security and authz-resolver-sdk.

---

# 1. Persistence: toolkit-db and toolkit-db-macros

## 1.1 Entities: `#[derive(Scopable)]`
Sources: `/app/libs/toolkit-db-macros/src/lib.rs`, `scopable.rs`. Import with `toolkit_db::secure::Scopable`.

**You must declare all four dimensions** or it won't compile:
- tenant: `tenant_col = "col"` or `no_tenant`
- resource: `resource_col = "col"` or `no_resource`
- owner: `owner_col = "col"` or `no_owner`
- type: `type_col = "col"` or `no_type`
- `unrestricted`, for global tables only; no other attribute may be combined with it.
- `pep_prop(name = "col")`, which can repeat. It cannot reuse the reserved names `owner_tenant_id`, `id` or `owner_id`.

The derive generates `resolve_property()`, which maps `tenant_col` to `"owner_tenant_id"`, `resource_col` to `"id"`, `owner_col` to `"owner_id"`, and each `pep_prop` to its own name.

Real usage, `/app/gears/credstore/credstore/src/infra/storage/entity/secrets.rs`:
```rust
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "credstore_secrets")]
#[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
pub struct Model { #[sea_orm(primary_key, auto_increment = false)] pub id: Uuid, pub tenant_id: Uuid, ... }
```
For per-user rows, use `owner_col = "user_id"` so that `owner_id` scope filters bind to that column. Every types-registry entity uses `#[secure(unrestricted)]`.

## 1.2 `AccessScope` (defined in toolkit-security, re-exported as `toolkit_db::secure::AccessScope`)
Source: `/app/libs/toolkit-security/src/access_scope.rs`.

**Semantics:** a scope is an OR of `ScopeConstraint`s, and each constraint is an AND of `ScopeFilter`s.

Constructors:
- `AccessScope::allow_all()`
- `deny_all()`; `default()` is also deny-all
- `for_tenant(id)` / `for_tenants(vec)`
- `for_resource(id)` / `for_resources(vec)`
- `single(ScopeConstraint::new(vec![...]))`
- `from_constraints(vec)`

Filters: `ScopeFilter::eq(prop, v)`, `ScopeFilter::r#in(prop, vals)`, `ScopeFilter::in_uuids(prop, vec)`, plus `in_tenant_subtree` and the group variants.

Property names are constants: `pep_properties::{OWNER_TENANT_ID="owner_tenant_id", RESOURCE_ID="id", OWNER_ID="owner_id"}`.

Helpers:
- `is_unconstrained()`, `is_deny_all()`, `has_property()`, `contains_uuid()`
- `tenant_only()` and `tenant_and_owner()`. Both **widen** the scope by dropping other filters.
- **`ensure_owner(owner_id)`**. It injects or narrows an `owner_id` eq filter in every constraint; on an unconstrained scope the result is a single `owner_id` filter. Use it to make sure user-owned rows are only seen by their owner.

**Policy:** an empty scope compiles to `WHERE 1=0`. A tenant scope on an entity with no `tenant_col` is also deny-all.

## 1.3 Getting the DB from `GearCtx`
`/app/libs/toolkit/src/context.rs`:
- `ctx.db() -> Option<DBProvider<DbError>>`
- `ctx.db_required() -> anyhow::Result<DBProvider<DbError>>`

The gear must declare `capabilities = [..., db, ...]` and `impl DatabaseCapability`.

Credstore (`/app/gears/credstore/credstore/src/gear.rs`):
```rust
#[toolkit::gear(name = "credstore", deps = [authz_resolver, tenant_resolver, types_registry],
    capabilities = [system, db, rest, stateful], lifecycle(entry = "serve", stop_timeout = "30s", await_ready))]
...
let db_raw = ctx.db_required()?;
let db: Arc<DBProvider<DomainError>> = Arc::new(DBProvider::new(db_raw.db()));
...
impl DatabaseCapability for CredStoreGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        use sea_orm_migration::MigratorTrait;
        crate::infra::storage::migrations::Migrator::migrations()
    }
}
```

`DBProvider<E>` (`/app/libs/toolkit-db/src/db_provider.rs`) requires `E: From<DbError>`:
- `new(db: Db)`
- `db() -> Db`
- `conn() -> Result<DbConn<'_>, E>`
- `transaction(|tx: &DbTx| Box::pin(async move {..})) -> Result<T,E>`
- `transaction_with_config(TxConfig, f)`

`Db` (`secure/db.rs`) adds:
- `transaction_with_retry(TxConfig, extract_db_err: Fn(&E)->Option<&DbErr>, body)`, which retries on contention up to `DEFAULT_TX_RETRY_ATTEMPTS` = 3
- `transaction_with_retry_max`
- `lock()` / `try_lock()` (advisory locks)
- `backend()`

**Guard:** calling `conn()` inside a transaction closure returns `DbError::ConnRequestedInsideTx`.

`TxConfig` options: `default()`, `serializable()`, `read_only()`, `with_isolation(TxIsolationLevel)`.

Runners: `DbConn`, `DbTx`, `SecureConn` and `SecureTx` all implement the sealed trait `DBRunner` (exported `#[doc(hidden)]`). Repository functions take `&impl DBRunner`.

## 1.4 Secure query APIs (`toolkit_db::secure`)
Extension traits: `SecureEntityExt`, `SecureUpdateExt`, `SecureDeleteExt`, `SecureInsertExt`, `SecureInsertManyExt`.

**Select.** `E::find().secure()` returns `SecureSelect<E, Unscoped>`. `.scope_with(&scope)` returns `SecureSelect<E, Scoped>` and is infallible in the current code. Only the `Scoped` state has the execution methods (enforced by the type system):
- `.filter(Condition)`, `.order_by(col, Order)`, `.limit()`, `.offset()`
- `.and_id(uuid)?`
- `.all(&runner)`, `.one(&runner)`, `.count(&runner)`

You can also chain SeaORM `.filter()`, `.order_by()` and `.limit()` before `.secure()`, as credstore does.

**Update** (credstore `writes.rs`):
```rust
let rows = entity::secrets::Entity::update_many()
    .col_expr(Column::Status, Expr::value(SecretStatus::Active.as_smallint()))
    .filter(Condition::all().add(Column::Id.eq(id)).add(Column::Version.eq(v)))
    .secure().scope_with(scope).exec(&conn).await.map_err(map_scope_err)?.rows_affected;
```
Credstore uses the version filter plus `rows_affected == 0` as its optimistic-concurrency check.

**Delete:** `Entity::delete_many().filter(..).secure().scope_with(scope).exec(&conn)`.

**Insert:**
- `Entity::insert(am).secure().scope_with_model(&scope, &am)?` validates the model against the scope. `.scope_unchecked(&scope)?` skips that check; credstore uses it.
- Then `.on_conflict(SecureOnConflict::columns([...]).update_columns([...])?)` and `.exec(&runner)` or `.exec_with_returning(&runner)`.
- Helpers: `secure_insert::<E>(am, &scope, &runner) -> Result<E::Model, ScopeError>`. It fails with `Invalid("tenant_id is required")` if the tenant column is NotSet. Also `secure_insert_many::<E>(models, &scope, &runner)`.

**Update one row:** `secure_update_with_scope::<E>(am, &scope, id, &runner)`. It re-reads the row inside the scope first; if the row isn't found it returns `Denied`, and changing `tenant_id` returns `Denied("tenant_id is immutable")`. `validate_tenant_in_scope(tid, &scope)` is also available.

**Errors:** `ScopeError` is `#[non_exhaustive]`, with variants `Db(DbErr)`, `Invalid(&'static str)`, `TenantNotInScope{tenant_id}`, `Denied(&'static str)` and graph-related ones. It also has `is_unique_violation()` and `is_foreign_key_violation()`. Credstore's `map_scope_err` maps `TenantNotInScope` to AccessDenied and everything else to Internal, except `Db`, which goes through a classifier: unique violation becomes Conflict, serialization failure becomes ServiceUnavailable.

Transactions in practice (types-registry `acceptance.rs`): clone `scope` and `Arc`s into the closure, because the closure must be `'static`:
```rust
let tx_scope = scope.clone();
db.transaction(|tx| { let tx_scope = tx_scope.clone(); Box::pin(async move {
    stores.insert_operation(tx, &tx_scope, ...).await?; ... Ok(value)
})}).await
```

## 1.5 Migrations
- The gear defines `pub struct Migrator; impl MigratorTrait for Migrator { fn migrations() -> Vec<Box<dyn MigrationTrait>> {...} }`. Each migration is `#[derive(DeriveMigrationName)] pub struct Migration;` with `#[async_trait] impl MigrationTrait { async fn up(&self, manager: &SchemaManager) ... }`.
- For SQLite and Postgres, credstore (`m0001_initial_schema.rs`) uses raw SQL chosen by backend: `match manager.get_database_backend() { DatabaseBackend::Postgres => ..., DatabaseBackend::Sqlite => ..., _ => Err(DbErr::Custom(...)) }`, then `manager.get_connection().execute_unprepared(sql)`.
  - Postgres: `UUID`, `TIMESTAMPTZ`, `BYTEA`.
  - SQLite: `BLOB` for UUIDs, `TEXT` for timestamps.
  - It also creates partial unique indexes.
- `toolkit_db::sea_orm_migration` is re-exported.
- Tests: `toolkit_db::migration_runner::run_migrations_for_testing(&db, migrations).await`, with `db = connect_db("sqlite:file:x_{uuid}?mode=memory&cache=shared", ConnectOpts{max_conns:Some(1), min_conns:Some(1), ..Default::default()})`. See credstore `repo_impl/repo_tests.rs`.

## 1.6 Outbox (`toolkit_db::outbox`)
Docs: `/app/libs/toolkit-db/src/outbox/mod.rs` and `README.md`. Real use: `/app/gears/system/types-registry/types-registry/src/infra/outbox.rs`.

Exports:
- `Outbox`, `OutboxBuilder`, `OutboxHandle`, `QueueBuilder`, `LeasedQueueBuilder`
- `Record`, `Records`, `Wake`, `in_transaction`
- `Partitions`, `LeaseConfig`, `OutboxProfile`, `WorkerTuning`, `OutboxError`
- Handler traits `LeasedHandler`, `LeasedMessageHandler`, `TransactionalHandler`, `TransactionalMessageHandler`
- `OutboxMessage`, `Batch`, `HandlerResult`, `MessageResult`
- Dead-letter types `DeadLetterScope`, `DeadLetterFilter`, `DeadLetterMessage`, `DeadLetterStatus`
- `outbox_migrations()`, `outbox_migrations_with_prefix(prefix)`

**Migrations.** Append the outbox migrations to your own, as types-registry does in `gear.rs`:
```rust
let mut migrations = crate::infra::storage::Migrator::migrations();
migrations.extend(toolkit_db::outbox::outbox_migrations_with_prefix(OUTBOX_TABLE_PREFIX)
    .unwrap_or_else(|e| panic!("...{e}")));
```
The prefix must be the same in the migration and in `.table_prefix()`. It must be ASCII, start with a letter, and contain only letters, digits and `_`. The default tables are `toolkit_outbox_*`.

**Start the pipeline** (in `init`, from `db.db()`):
```rust
let handle = Outbox::builder(db)
    .table_prefix(TABLE_PREFIX)?
    .profile(OutboxProfile::low_latency())
    .processor_tuning(WorkerTuning::processor_default().retry_base(..).retry_max(..))
    .queue(QUEUE, Partitions::of(8))
    .leased(handler)
    .lease(LeaseConfig { duration, headroom })   // default 30s / 2s; headroom < duration (panics otherwise)
    .start().await?;                              // -> OutboxHandle
handle.outbox()  // &Arc<Outbox>
```
- **Stop:** `handle.stop().await` in the gear's `serve` after `cancel.cancelled()`. types-registry keeps the handle in a `Mutex<Option<OutboxHandle>>` and uses `lifecycle(entry="serve")` with the `stateful` capability.
- **Transactional queues** use `.transactional(h)` or `.batch_transactional(h)`.
- **Other builder knobs:** `.processors(n)`, `.sequencer_tuning()`, `.vacuum_tuning()`. Profiles: `default_profile()`, `low_latency()`, `high_throughput()`, `relaxed()`.

**Enqueue inside a business transaction:**
- `outbox.enqueue(&tx, Record::to(queue, partition_u32).payload(bytes, "type").build()?) -> Result<Wake, OutboxError>`
- Batches use `enqueue_batch(&tx, Records::to(q).payload_type(t).push(p, bytes).build()?)`. Optional `.trace(id)` for completion tracking.
- **Fire the `Wake` only after commit.** Either use `outbox::in_transaction(&db, |tx| Box::pin(async move { ...; Ok((value, wake)) }))`, or do it by hand as types-registry does: return `(value, wake)` from the transaction closure and call `wake.fire()` on `Ok`. Enqueue **last** in the closure. Use `Wake::discard()` on rollback paths. An unfired wake only delays delivery; the cold reconciler finds the work later.

**Validation:** `MAX_PAYLOAD_SIZE = 64 * 1024` bytes. Larger payloads fail with `OutboxError::PayloadTooLarge{size,max}`. Queue name and payload type must be 1 to 1024 bytes; payload type must be printable ASCII. Trace is at most 256 bytes. Other errors: `QueueNotRegistered`, `PartitionOutOfRange`, `Database(DbErr)`.

**Handlers:**
- `LeasedMessageHandler::handle(&self, msg: &OutboxMessage) -> MessageResult`, returning `Ok`, `Retry` or `Reject(String)`.
- `LeasedHandler::handle(&self, batch: &mut Batch<'_>) -> HandlerResult`, returning `Success`, `Retry{reason}` or `Reject{reason}`. Batch methods: `batch.next_msg()`, `ack()`, `reject(reason)`, `remaining()`, `should_stop()`.
- `OutboxMessage { partition_id, seq, payload: Vec<u8>, payload_type, created_at, attempts: i16 }`. The handler uses `attempts` to decide when to give up.
- **Leased mode** is at-least-once. The handler future is dropped at `duration - headroom`, so handlers must be idempotent; the README suggests deriving an idempotency key from `partition_id:seq`.
- **Transactional mode** is exactly-once with respect to the DB: `handle(&self, txn: &dyn ConnectionTrait, msg, cancel)`.

**Retry and dead letters:**
- `Retry` leaves the cursor in place and retries with exponential backoff (`WorkerTuning::retry_base` and `retry_max`; the processor default is 1s to 60s). It increments `attempts`.
- `Reject` moves the message to the dead-letter table. Nothing is replayed automatically.
- API: `outbox.dead_letter_list`, `_count`, `_replay(&conn, &scope, Duration)`, `_resolve`, `_reject`, `_discard`, `_cleanup`. Status lifecycle: pending → reprocessing → resolved, or pending → discarded.

# 2. Authorization: toolkit-security and authz-resolver-sdk

## 2.1 `SecurityContext`
Source: `/app/libs/toolkit-security/src/context.rs`.

Accessors:
- `subject_id() -> Uuid`
- `subject_type() -> Option<&str>`
- `subject_tenant_id() -> Uuid`
- `token_scopes()` and `has_scope(s)`
- `bearer_token()`
- `is_anonymous()`

Construction: `SecurityContext::builder().subject_id(..).subject_tenant_id(..).subject_type("user").token_scopes(vec![..]).build()?`, or `SecurityContext::anonymous()`.

Handlers receive it as `Extension(ctx): Extension<SecurityContext>`. Dev constants: `constants::DEFAULT_TENANT_ID` and `DEFAULT_SUBJECT_ID`.

## 2.2 `PolicyEnforcer`
Source: `/app/gears/system/authz-resolver/authz-resolver-sdk/src/pep/enforcer.rs`.

Build it in `init`. Either resolve the client eagerly:
```rust
let authz = ctx.client_hub().get::<dyn AuthZResolverApi>()?;
let enforcer = PolicyEnforcer::new(authz);
```
or resolve it lazily with `PolicyEnforcer::from_hub(ctx.client_hub())`. Options: `.with_capabilities(vec![Capability::TenantHierarchy])` and `.with_deadline(d)` (default `DEFAULT_EVAL_DEADLINE` = 30s).

Resource types are `ResourceType::from_static(gts_id!("cf.core.x.y.v1~"), &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID])`, or `ResourceType::new(String, &[...])` for names built at runtime. Actions are plain strings; credstore uses `"read"`, `"write"` and `"delete"`, and the SDK docs use `"get"`, `"list"`, `"create"`, and so on.

Calls:
- `enforcer.access_scope(&ctx, &RES, "get", Some(id)) -> Result<AccessScope, EnforcerError>`. This always sets `require_constraints=true`.
- `enforcer.access_scope_with(&ctx, &RES, "create", None, &AccessRequest::new().context_tenant_id(tid).resource_property(pep_properties::OWNER_TENANT_ID, tid).require_constraints(true))`.
- Other `AccessRequest` options: `tenant_mode`, `barrier_mode`, `tenant_status`, `tenant_context`.
- The request carries `subject.properties["tenant_id"] = ctx.subject_tenant_id()` and `supported_properties` taken from the `ResourceType`.

Credstore's helper (`/app/gears/credstore/credstore/src/domain/authz.rs`):
```rust
let request = AccessRequest::new()
    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
    .require_constraints(true);
enforcer.access_scope_with(ctx, resource, action, None, &request).await.map_err(map_enforcer_err)
```

## 2.3 Error mapping: 403 vs 503
`EnforcerError` variants: `Denied{deny_reason}`, `EvaluationFailed(CanonicalError)` and `CompileFailed(ConstraintCompileError)`. A PDP timeout becomes `EvaluationFailed(service_unavailable)`. Credstore maps them like this:
```rust
EnforcerError::Denied{..} | EnforcerError::CompileFailed(_) => DomainError::AccessDenied{..},   // -> permission_denied().with_reason("ACCESS_DENIED") = 403
EnforcerError::EvaluationFailed(_) => DomainError::ServiceUnavailable{..}                       // -> 503
```
OAGW is different (`oagw/src/domain/error.rs:303`): it maps `EvaluationFailed` and `CompileFailed` to Internal.

## 2.4 What static-authz-plugin returns
Source: `/app/gears/system/authz-resolver/plugins/static-authz-plugin/src/domain/service.rs`.
- **Tenant:** taken from `tenant_context.root_id`, falling back to `subject.properties["tenant_id"]`. A missing tenant or nil UUID gives `decision:false`, which the PEP turns into `Denied` (403). A request carrying `group_ids` or `ancestor_group_ids` properties is also denied.
- **Constraints:** `decision:true` with **`Predicate::In(owner_tenant_id, [tid])`**. This is an `In` predicate, not `Eq`, and it is only emitted if the PEP lists `owner_tenant_id` in its supported properties.
- **Tenant hierarchy:** with `Capability::TenantHierarchy`, the plugin also emits OR-ed `InTenantSubtree(owner_tenant_id | id, tid)`.
- **No owner or user predicate is ever emitted.** If no supported property matches, the constraint list is empty: that compiles to `allow_all()` when constraints are optional, and to a compile failure (deny) under `require_constraints`.
- **Consequence:** a gear that needs per-user rows must narrow the scope itself, for example with `scope.ensure_owner(ctx.subject_id())` plus `owner_col`.

# 3. OData: toolkit-odata, toolkit-odata-macros, and toolkit::api::odata

## 3.1 Extractor
`toolkit::api::canonical_prelude::OData`, defined in `/app/libs/toolkit/src/api/odata.rs`. Handler shape: `OData(query): OData`, which yields `toolkit_odata::ODataQuery { filter, order, limit, cursor, filter_hash, select }`.

Query parameters (`ODataParams`):
- `$filter`, `$orderby`, `$select`
- `limit`, also accepted as `$top`; sending both is a 400
- `cursor`, also accepted as `$skiptoken`

Rejections it produces:
- Any other `$` option (`$skip`, `$count`, ...): `UNSUPPORTED_QUERY_PARAM`
- `$filter` longer than 8 KiB: `FILTER_TOO_LONG`
- More than 2000 nodes: `FILTER_TOO_COMPLEX`
- `cursor` together with `$orderby`: `ORDER_WITH_CURSOR`
- Undecodable cursor: `INVALID_CURSOR`
- `limit=0`: `INVALID_LIMIT`
- Bad `$select`: `INVALID_SELECT`
- Unparsable query string: `INVALID_QUERY_PARAMS`

Limits above the maximum are **clamped, not rejected**, by `clamp_limit` in the pagination helpers.

## 3.2 Page types
`toolkit_odata::{Page<T>{items, page_info}, PageInfo{next_cursor, prev_cursor, limit}}`. With `with-utoipa`, both have hand-written `ToSchema` impls. Response alias: `toolkit::api::response::JsonPage<T> = Json<Page<T>>`.

## 3.3 Cursor pagination over SeaORM (`toolkit_db::odata`)
**Option A: `FieldMap` with `OPager`** (`odata/pager.rs`):
```rust
let fmap = FieldMap::<chat::Entity>::new()
    .insert_with_extractor("id", Column::Id, FieldKind::Uuid, |m| m.id.to_string())
    .insert_with_extractor("created_at", Column::CreatedAt, FieldKind::DateTimeUtc, |m| ...);
OPager::<chat::Entity, _>::new(&scope, &conn, &fmap)
    .tiebreaker("id", SortDir::Desc).limits(20, 100)
    .fetch(&query, |m| Dto::from(m)).await   // -> Result<Page<D>, toolkit_odata::Error>
```
**Gotcha:** every field that can be ordered on, including the tiebreaker, needs `insert_with_extractor`. A field added with plain `insert` has no cursor extractor, so building the next cursor fails with `InvalidOrderByField`. The lower-level equivalent is `paginate_with_odata(select, conn, q, fmap, tiebreaker, LimitCfg{default,max}, map)`.

**Option B: typed path.** `#[derive(ODataFilterable)]` on a DTO, with `#[odata(filter(kind = "Uuid"|"String"|"I64"|"F64"|"Bool"|"DateTimeUtc"|"Date"|"Time"|"Decimal"))]` on each field, generates an enum named `<Dto>FilterField` that implements `FilterField`. You then implement `FieldToColumn<F>` (`type Column`, `fn map_field`, and optionally `map_value` and `is_orderable`) and `ODataFieldMapping<F>` (`type Entity`, `fn extract_cursor_value -> sea_orm::Value`), and call:
```rust
paginate_odata::<F, Mapper, Entity, D, _, _>(Entity::find().secure().scope_with(&scope), &conn, &query,
    ("id", SortDir::Desc), LimitCfg{default: 20, max: 100}, |m| m.into()).await
```
Example: `/app/libs/toolkit-db/tests/odata_null_like.rs`. `paginate_odata_try` is the variant for a fallible mapper. No gear uses either path yet; only the tests in `libs/toolkit-db/tests/` do.

## 3.4 Errors and the Problem they become
`toolkit_odata::Error` (`lib.rs:204`). `impl From<Error> for CanonicalError` is in `problem_mapping.rs` and builds everything from `#[resource_error(gts_id!("cf.core.odata.query.v1~"))] pub struct OdataError;`. The result is `invalid_argument` (400) with `context.resource_type = "gts.cf.core.odata.query.v1~"`:

| Error | field | reason |
|---|---|---|
| `InvalidFilter` | `$filter` | `INVALID_FILTER` |
| `InvalidOrderByField` | `$orderby` | `INVALID_ORDERBY_FIELD` |
| `InvalidCursor`, `CursorInvalid*` | `cursor` | `INVALID_CURSOR` |
| `OrderMismatch` / `FilterMismatch` | `cursor` | `ORDER_MISMATCH` / `FILTER_MISMATCH` |
| `InvalidLimit` | `$top` | `INVALID_LIMIT` |
| `OrderWithCursor` | `$orderby` **and** `cursor` (2 violations) | `ORDER_WITH_CURSOR` |
| `Db` / `ParsingUnavailable` | n/a | internal (500) |

In a handler, `.map_err(CanonicalError::from)?` on a `toolkit_odata::Error` keeps the OData resource type. If you wrap it in your own domain error, you lose that resource type.

# 4. Errors: toolkit-canonical-errors

## 4.1 Resource-type marker and builders
`#[resource_error(gts_id!("cf.core.credstore.secret.v1~"))] pub(crate) struct SecretResource;` generates typestate constructors. `.create()` only compiles once every required piece is set.

| Constructor | Required before `.create()` | Status | Wire context |
|---|---|---|---|
| `not_found(detail)` | `.with_resource(name)` | 404 | `resource_type`, `resource_name` |
| `already_exists(detail)` | `.with_resource(name)` | 409 | same |
| `invalid_argument()` | `.with_field_violation(field, description, reason)` (repeatable), or `.with_format(msg)` / `.with_constraint(msg)` | 400 | `field_violations[{field,description,reason}]` |
| `out_of_range(detail)` | `.with_field_violation(..)` | 400 | `field_violations` |
| `failed_precondition()` | `.with_precondition_violation(subject, description, type_)` | 400 | `violations[{type,subject,description}]` |
| `aborted(detail)` | `.with_reason(r)` | 409 | `reason` |
| `resource_exhausted(detail)` | `.with_quota_violation(subject, description)`, optionally `.with_quota_violation_retry_after_seconds(n)` | 429 | `violations[{subject,description,retry_after_seconds?}]` |
| `permission_denied()` | `.with_reason(r)` | 403 | `reason` |
| `CanonicalError::service_unavailable()` | optional `.with_detail(s)`, `.with_retry_after_seconds(n)` | 503 | `retry_after_seconds`, plus a `Retry-After` header |
| `CanonicalError::internal(diag)` | n/a | 500 | the detail is hidden and replaced by "An internal error occurred. Please retry later." |
| `CanonicalError::unauthenticated()` | `.with_reason()` | 401 | n/a |

`resource_name` is optional on `aborted`, `invalid_argument`, `failed_precondition` and similar categories; set it with `.with_resource(..)`. `.with_override(Http::status_code(413))` changes the status but must stay in the same class (4xx stays 4xx).

Wire shape (`Problem`): `{type:"gts://gts.cf.core.errors.err.v1~cf.core.err.<category>.v1~", title, status, detail, instance?, trace_id?, context{resource_type, ...}}`, served as `application/problem+json`.

Real usage: credstore `/app/gears/credstore/credstore/src/infra/sdk_error_mapping.rs`:
```rust
DomainError::NotFound => SecretResource::not_found("secret not found").with_resource("secret").create(),
DomainError::VersionConflict => SecretResource::aborted("secret version precondition failed").with_reason("OPTIMISTIC_LOCK_FAILURE").create(),
DomainError::UnsupportedTransition{detail} => SecretResource::failed_precondition()
    .with_precondition_violation("sharing", detail, "UNSUPPORTED_TRANSITION").create(),
DomainError::AccessDenied{..} => SecretResource::permission_denied().with_reason("ACCESS_DENIED").create(),
DomainError::ServiceUnavailable{detail, retry_after, ..} => { let mut b = CanonicalError::service_unavailable().with_detail(detail);
    if let Some(d) = retry_after { b = b.with_retry_after_seconds(d.as_secs()); } b.create() }
```
A `resource_exhausted` example is in api-gateway `throttling.rs:921`. Also see `/app/gears/system/types-registry/types-registry/src/api/rest/error.rs`.

## 4.2 Axum integration
- With the `axum` feature, `impl IntoResponse for CanonicalError` renders the Problem and stores the error in the response extensions. `canonical_error_middleware` then logs `diagnostic()` and fills in `instance` and `trace_id`.
- Handlers return `ApiResult<T> = Result<T, CanonicalError>`, from `toolkit::api::canonical_prelude::*`.
- Domain errors convert through `impl From<DomainError> for CanonicalError`.

## 4.3 Platform extractors (`toolkit::api::rest::extract::{Json, Path, Query}`)
**Gotcha:** `canonical_prelude` re-exports **`axum::Json`**, not the platform extractor. To get canonical rejections you have to write `extract::Json(req): extract::Json<T>`, as the types-registry handlers do.

The rejection becomes `invalid_argument` with `resource_type = "gts.cf.core.http.request.v1~"` and a single violation on field `"body"`:

| Rejection | reason | Status |
|---|---|---|
| Malformed JSON | `json_syntax_error` | 400 |
| Deserialization error (unknown field, bad enum, missing field) | `invalid_json_body` | 422 |
| Missing or wrong Content-Type | `missing_json_content_type` | 415 |
| Body could not be read | `json_body_read_error` | axum's status |

`extract::Path` rejections use field `"path"` and reason `invalid_path_params`; a 5xx rejection becomes internal instead. Descriptions are capped at 500 characters. Sources: `/app/libs/toolkit/src/api/rest/extract/{error,json,path}.rs`.

---

I've covered all six areas. The two findings that matter most for tests:

- **Multiple users or tenants require `mode: static_tokens`.** Neither `auth_disabled: true` nor `accept_all` can give different identities.
- **`config/mini-chat.yaml` cannot reach a plain-HTTP mock LLM yet.** It disables SSRF but does not set `allow_http_upstream: true`.

The mini-chat gear is still a stub (`gears/mini-chat/mini-chat/src/lib.rs` has only empty `infra::plugins` modules). However, its `Cargo.toml`, configs, `DESIGN.md` and the generated OpenAPI in `docs/api/api.json` already describe the intended behaviour.

---

## 1. OAGW (`/app/gears/system/oagw/oagw-sdk`, `/app/gears/system/oagw/oagw`)

### Client trait in ClientHub
The trait is in `/app/gears/system/oagw/oagw-sdk/src/api.rs`. The OAGW gear registers it in `oagw/src/gear.rs:160-164` as `ctx.client_hub().register::<dyn ServiceGatewayClientV1>(oagw)`. The OAGW gear declares `deps = [types_registry, authz_resolver, credstore, tenant_resolver]`.

```rust
use oagw_sdk::ServiceGatewayClientV1;
let gw = ctx.client_hub().get::<dyn ServiceGatewayClientV1>()?;
```

Every method takes `ctx: SecurityContext` by value and returns `Result<_, toolkit_canonical_errors::CanonicalError>`:
- `create_upstream(ctx, CreateUpstreamRequest) -> Upstream`, plus `get_upstream`, `list_upstreams(ctx, &ListQuery)`, `update_upstream` and `delete_upstream`.
- `create_route(ctx, CreateRouteRequest) -> Route`, plus `get_route`, `list_routes(ctx, Option<Uuid>, &ListQuery)`, `update_route` and `delete_route`.
- `resolve_proxy_target(ctx, alias, method, path) -> (Upstream, Route)`: a dry-run resolve with no auth or forwarding.
- `proxy_request(ctx, http::Request<Body>) -> http::Response<Body>`.

`ListQuery` defaults to `{ top: 50, skip: 0 }`.

### Types and constants (`oagw_sdk::*`, from `models.rs` and `gts.rs`)
- `Server { endpoints: Vec<Endpoint> }` and `Endpoint { scheme: Scheme, host: String, port: u16 }`.
- `Scheme::{Http, Https (default), Wss, Wt, Grpc}`.
- `AuthConfig { plugin_type: String, sharing: SharingMode, config: Option<HashMap<String,String>> }`.
- `SharingMode::{Private (default), Inherit, Enforce}`.
- `MatchRules { http: Option<HttpMatch>, grpc: Option<GrpcMatch> }`.
- `HttpMatch { methods: Vec<HttpMethod>, path: String, query_allowlist: Vec<String>, path_suffix_mode: PathSuffixMode }`.
- `HttpMethod::{Get, Post, Put, Delete, Patch}` and `PathSuffixMode::{Disabled, Append (default)}`.
- Constants:
  - `HTTP_PROTOCOL_ID` = `gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1`
  - `APIKEY_AUTH_PLUGIN_ID` = `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`
  - `UPSTREAM_SCHEMA` and `ROUTE_SCHEMA` also exist. `gts_id!` adds the `gts.` prefix.

Usage, adapted from `oagw/tests/proxy_integration.rs:135-180`:
```rust
let up = gw.create_upstream(ctx.clone(),
    CreateUpstreamRequest::builder(
        Server { endpoints: vec![Endpoint { scheme: Scheme::Http, host: "127.0.0.1".into(), port: 8080 }] },
        HTTP_PROTOCOL_ID)
    .alias("127.0.0.1:8080")
    .auth(AuthConfig { plugin_type: APIKEY_AUTH_PLUGIN_ID.into(), sharing: SharingMode::Private,
        config: Some([("header".into(),"authorization".into()),("prefix".into(),"Bearer ".into()),
                      ("secret_ref".into(),"cred://openai-key".into())].into_iter().collect()) })
    .build()).await?;
gw.create_route(ctx.clone(), CreateRouteRequest::builder(up.id, MatchRules {
    http: Some(HttpMatch { methods: vec![HttpMethod::Post], path: "/v1/responses".into(),
        query_allowlist: vec![], path_suffix_mode: PathSuffixMode::Append }), grpc: None }).build()).await?;
```
The route builder also has `.plugins/.rate_limit/.cors/.tags/.priority/.enabled`.

### API-key auth plugin (`oagw/src/infra/plugin/apikey_auth.rs`)
- Config keys:
  - `header` (required; lower-cased)
  - `prefix` (optional, default `""`)
  - `secret_ref` (required; a leading `cred://` is stripped)
- At proxy time it calls `credstore.get(&ctx.security_context, &key)`, using the caller's context, and sets `header: prefix+secret`.
- At create time, `validate_secret_ref_accessible` checks that the secret is readable with the creating ctx. If not, it fails with `FailedPrecondition`, which projects to `ServiceGatewayError::FailedPrecondition`. The DESIGN describes a background retry for this case.
- In the static credstore plugin, a secret with no `tenant_id` is global and readable by any tenant. A secret with `tenant_id` and `owner_id` is private to that subject.

### Alias rules (`domain/services/management/alias.rs`)
- **IP endpoints** (such as `127.0.0.1`): an explicit alias is required, otherwise you get "explicit alias is required for IP-based…". Any valid alias is accepted. Allowed characters are `[A-Za-z0-9.:_-]`, up to 253, and the alias is normalized to lowercase.
- **Hostname endpoints**: the alias is derived as `host`, or `host:port` when the port is not 80/443. If you pass a different alias it is rejected as a Validation error ("alias is auto-derived… remove the 'alias' field"). For example, `mock-llm` on port 8080 must use alias `mock-llm:8080`.
- IPv6 is rejected. You cannot mix IP and hostname endpoints.

### Upstream addresses: no string parsing
The SDK has no parser for `"127.0.0.1:8080"` or `"http://host:port"`. The caller builds `Endpoint{scheme,host,port}` itself. The REST DTO defaults are `scheme=https` and `port=443`. Mini-chat's DESIGN (B.1) defines provider fields `host`, `port` (443, or 80 when `use_http`), `use_http` and `upstream_alias` (default = `host`). Allowed `host` characters are letters, digits, `. - _ : [ ]`.

### Already exists
- Upstream storage is in memory (DashMap), so it is empty after every restart.
- A duplicate alias in the same tenant gives `RepositoryError::Conflict` → `CanonicalError::AlreadyExists` with resource type `UPSTREAM_SCHEMA`.
- Detect it with:
  ```rust
  matches!(ServiceGatewayError::from(e), ServiceGatewayError::AlreadyExists{ resource: Resource::Upstream, .. })
  ```
  or match `CanonicalError::AlreadyExists{..}` directly.
- DESIGN line 411 says to "reuse when OAGW reports it already exists": use `list_upstreams` and find by alias.
- Routes have no duplicate check (`route_repo.rs` `create` just inserts).

### Proxying (`infra/proxy/service.rs`)
- The URI is `/{alias}/{path_suffix}?query`. The alias is the text before the first `/`, so `:` is fine (`/127.0.0.1:8080/v1/responses`).
- Routes match by method plus longest `path` prefix.
- Any query key not in `query_allowlist` is rejected (`QUERY_NOT_ALLOWED`). Azure needs `api-version` in the allowlist.
- With `PathSuffixMode::Disabled`, any extra path is rejected.
- Request headers: the default passthrough is `None`, so only `Content-Type` is forwarded, and the auth plugin injects its header.
- Authz: `access_scope_with(PROXY, INVOKE)` runs first. The static authz plugin denies the nil tenant.
- Tenant visibility: the upstream is looked up from `ctx.subject_tenant_id()` up through its ancestors. An ancestor's upstream is visible only if one of auth/rate_limit/plugins/cors has `sharing != Private`. The simplest approach is to create and proxy with the same S2S ctx.
- Response:
  - It is always `Body::Stream`, despite what the README says. Read it with `resp.into_body().into_bytes().await`.
  - Upstream non-2xx comes back as `Ok(resp)` with that status. `resp.extensions().get::<ErrorSource>()` gives `Gateway` or `Upstream`.

SSE consumption:
```rust
use oagw_sdk::{ServerEventsStream, ServerEventsResponse, ServerEvent};
match ServerEventsStream::from_response::<ServerEvent>(resp) {   // or ::<Json<T>>
    ServerEventsResponse::Events(mut s) => while let Some(ev) = s.next().await { let ev = ev?; /* ev.event, ev.data, ev.json::<T>() */ },
    ServerEventsResponse::Response(resp) => { /* non-SSE, e.g. upstream error JSON */ }
}
```
Errors on the stream are `StreamingError::{ServerEventsParse, Stream(..), ..}`. With the `axum` feature, `ServerEventsStream::into_response()` or `oagw_sdk::sse::server_events_response(...)` builds an outgoing SSE response with headers `text/event-stream`, `Cache-Control: no-cache` and `X-Accel-Buffering: no`.

Multipart (`multipart.rs`):
```rust
let mp = MultipartBody::new().text("purpose","assistants")
    .part(Part::bytes("file", bytes).filename("a.pdf").content_type("application/pdf"));
let req = mp.into_request(http::Method::POST, "/alias/v1/files")?;  // sets Content-Type with boundary
```
`Part::stream(name, BodyStream)` makes the body `Body::Stream`. Streaming request bodies are sent chunked and limited by `max_body_size_bytes`.

### Errors (`oagw-sdk/src/error.rs`)
Convert with `ServiceGatewayError::from(canonical)`. Variants:
- `RateLimited{retry_after_secs}`
- `Timeout` (canonical `DeadlineExceeded`, HTTP 504 Problem via `OagwProxyError::deadline_exceeded`)
- `Unavailable{retry_after_secs}` (`DownstreamError`, `ProtocolError` or `StreamAborted` → 502 service_unavailable; also disabled upstream and circuit-breaker open)
- `AuthFailed{reason,detail}` and `PermissionDenied{reason,detail}`
- `PayloadTooLarge{detail}`
- `InvalidTargetHost{code,detail}` and `Validation{field,reason,detail}` (includes HTTP-upstream-forbidden, query-not-allowed and alias errors)
- `NotFound{resource,name}` (unknown alias or route → `NotFound` upstream/route with a nil id)
- `AlreadyExists{resource,name}`
- `FailedPrecondition{..}`, `Aborted{..}`, `Internal{detail}`, `Other{canonical}`

Timeout semantics:
- `proxy_timeout_secs` (default 30) bounds the time until upstream response headers arrive (`RequestTimeout` → `DeadlineExceeded`).
- It is also Pingora's per-read `peer.options.read_timeout` (`pingora_proxy.rs:643`). A gap between SSE chunks longer than `proxy_timeout_secs` will probably break the stream. `e2e-local.yaml` sets it to **2 s**, which matters for slow mock streams.
- SSE responses also get `streaming_idle_timeout_secs` (default 300).
- The connect timeout is fixed at 10 s.

### Config (`oagw/src/config.rs`, `deny_unknown_fields`)
Fields: `proxy_timeout_secs` (30), `max_body_size_bytes` (100 MB), `allow_http_upstream` (false), `token_cache_*`, `websocket_*`, `streaming_idle_timeout_secs` (300), `protocol_cache_ttl_secs`, `management_api_enabled` (true), `ssrf_policy`, `metrics`.
- **Plain HTTP:** an `http` endpoint fails at proxy time with `Validation{field:"endpoint.scheme", reason: HTTP_UPSTREAM_FORBIDDEN}` unless `allow_http_upstream: true`. `e2e-local.yaml` sets it; `config/mini-chat.yaml` does not.
- `ssrf_policy` = `SsrfPolicy { enabled: true, extra_deny_cidrs, allow_cidrs, extra_deny_hostnames, allow_hostnames }`.
  - The built-in list blocks 127/8, RFC1918, link-local and others, plus the hostnames `localhost` and `localhost.localdomain`.
  - It is checked at create time (`validate_endpoints_ssrf`) and at connect time.
  - For local mocks use `enabled: false` (both configs do this) or `allow_cidrs: ["127.0.0.0/8"]`.

---

## 2. AuthN resolver

### SDK (`/app/gears/system/authn-resolver/authn-resolver-sdk`)
```rust
use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
let authn = ctx.client_hub().get::<dyn AuthNResolverClient>()?;
let res = authn.exchange_client_credentials(&ClientCredentialsRequest {
    client_id: "mini-chat".into(), client_secret: SecretString::from("mini-chat-dev-secret"), scopes: vec![] }).await?;
let s2s_ctx = res.security_context;
```
- Errors are `AuthNResolverError::{Unauthorized, NoPluginAvailable, ServiceUnavailable, TokenAcquisitionFailed, Internal}`.
- The plugin is resolved lazily, so you can get "client not registered yet" early on. Retrying is advisable.
- There are no existing callers in the workspace.

### `static-authn-plugin` (`plugins/static-authn-plugin/src/config.rs` and `domain/service.rs`)
- Config: `vendor`, `priority`, `mode: accept_all | static_tokens` (default `accept_all`), `default_identity`, `tokens: [{token, identity}]`, `s2s_credentials: [{client_id, client_secret, identity?, bearer_token?}]`.
- `IdentityConfig { subject_id, subject_tenant_id, token_scopes (default ["*"]), subject_type: Option }`. Defaults are `DEFAULT_SUBJECT_ID = 11111111-6a88-4768-9dfc-6bcd5187d9ed` and `DEFAULT_TENANT_ID = 00000000-df51-5b42-9538-d2b56b7ee953` (`libs/toolkit-security/src/constants.rs`).
- **`accept_all`:** any non-empty bearer maps to `default_identity`. The caller cannot choose subject or tenant; the token is only stored as the context's bearer. **Multiple users or tenants require `mode: static_tokens`**, which maps each exact token string to its identity (unknown token → `None` → 401).
- **S2S:** the `client_id` lookup must match the secret exactly. The context is built from `identity`, which defaults to the default subject and tenant if omitted (as in all the configs). `bearer_token` is set only if configured.

`config/e2e-local.yaml` uses `mode: static_tokens`. Tokens and identities:

| Token | subject_id | tenant |
|---|---|---|
| `e2e-token-tenant-a` | `11111111-…` | `00000000-df51-…` (subject_type `gts.cf.core.security.subject_user.v1~`) |
| `e2e-token-tenant-a-reviewer` | `44444444-6a88-…` | tenant A |
| `e2e-token-tenant-b` | `22222222-6a88-…` | `bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb` |
| `e2e-token-hierarchy-root/l1a/l1b` | — | `…0001`, `…0002`, `…0005` |
| `e2e-token-nil-tenant` | — | nil tenant (authz returns 403) |

The `s2s_credentials` entry is `mini-chat` / `mini-chat-dev-secret`. `config/mini-chat.yaml` and `config/quickstart.yaml` use `accept_all` with the same S2S credentials.

---

## 3. API gateway (`/app/gears/system/api-gateway`)

### `auth_disabled: true` (`src/gear.rs:464-484`)
Every request gets `SecurityContext::builder().subject_id(DEFAULT_SUBJECT_ID).subject_tenant_id(DEFAULT_TENANT_ID).build()`. There is no bearer, no scopes and no subject_type, so there is a single identity. `route_policies.enabled` cannot be combined with it.

### Auth enabled (`middleware/auth.rs`)
- The `Authorization: Bearer <t>` value is passed to `authn.authenticate()`, and the resulting context is inserted into the request extensions.
- Anonymous routes get `SecurityContext::anonymous()`.
- `require_auth_by_default` defaults to true.

### Middleware order (outer → inner)
RequestId → Trace → Timeout → BodyLimit → CORS → MIME → pre-auth throttling → ErrorMapping → Auth → ScopeEnforcement → post-auth throttling → License → Router.

### Timeout
`ServiceBuilder::timeout(Duration::from_secs(30))` (hard-coded) with `timeout_to_canonical` returns a 504 `deadline_exceeded` Problem: "Request exceeded 30s timeout". It is a tower timeout on the handler future, so it covers only time until the response is returned. A streaming SSE body can run longer, but the handler must return the `Response` within 30 s. Nothing in the gateway buffers or compresses bodies.

### Body limit
`defaults.body_limit_bytes` (default 16 MiB; the configs use 64000000) is applied via `RequestBodyLimitLayer` and `DefaultBodyLimit`.

### License (`middleware/license_validation.rs`)
- If an operation declares any feature other than `gts.cf.core.lic.feat.v1~cf.core.global.base.v1`, the gateway returns 403 with reason `LICENSE_FEATURE_REQUIRED`.
- Declaring the base feature, or calling `require_license_features::<L>([])`, passes.
- Pattern: a type `License` with `impl AsRef<str>` and `impl toolkit::api::operation_builder::LicenseFeature for License {}` (see `oagw/src/api/rest/routes/mod.rs:12-22`).

### Throttling
In config:
- `rate_limit_zones.<name>: {rate_limit: "10/s", burst_limit, response_status_code=429, response_retry_after, key:{type: ip|identity}, max_keys}`
- `in_flight_limit_zones.<name>: {in_flight_limit, backlog_limit, backlog_timeout, response_status_code, key, max_keys}`

The binding is done in code with `.with_throttling(ThrottlingSpec { rate_limit_zone: Some("rl_mini_chat_chat".into()), in_flight_limit_zone: Some("ifl_mini_chat_chat".into()), require_security_context: false, dry_run: … })`.
- `validate_throttling()` fails startup for undefined zones.
- Identity keys need `require_security_context: true` on an authenticated operation and auth enabled.
- The in-flight permit is dropped when the handler returns (`throttling.rs:789-790`), not when the SSE body ends.
- `config/quickstart.yaml:108-140` says `create_chat` binds these zones with `dry_run: true`. Both zones are defined in `mini-chat.yaml`, `quickstart.yaml` and `e2e-local.yaml`.

### Mounting
- Gears register full paths through `RestApiCapability::register_rest(&self, ctx, router, openapi)` using `OperationBuilder::post(path)`.
- Typestate chain: `.operation_id().summary().tag().authenticated().require_license_features::<License>([]).json_request::<T>(openapi, ..).handler(h).json_response_with_schema::<R>(openapi, StatusCode, ..).standard_errors(openapi).register(router, openapi)`.
- `url_prefix` is a mini-chat config option (default `/mini-chat`, DESIGN B.1). The toolkit has no such mechanism, so the gear must add it to its own paths. The gateway's `prefix_path` (e.g. `/cf`) is stripped before matching.

---

## 4. `single-tenant-tr-plugin`
`plugins/single-tenant-tr-plugin/src/domain/client.rs` has **no fixed tenant id**. The single tenant is whatever `ctx.subject_tenant_id()` is (name "Default", no parent and no children). A nil tenant gives `TenantNotFound`. Asking for any other id also gives `TenantNotFound`, and `get_ancestors` returns an empty list. So with the default identity the tenant is `00000000-df51-5b42-9538-d2b56b7ee953`.

`e2e-local.yaml` instead uses `static-tr-plugin` with root `00000000-df51-…` and hierarchy tenants `…0001` to `…0006`. Tenant B (`bbbbbbbb-…`) is **not** in that tree, so OAGW's hierarchy walk would fail for a tenant-B context. Proxying with the S2S context avoids that.

---

## 5. SSE-producing routes
- No gear currently serves SSE through `OperationBuilder`. The only SSE producers are the OAGW proxy passthrough and `toolkit::SseBroadcaster` (`libs/toolkit/src/http/sse.rs`, axum `Sse` with a 15 s keepalive).
- For OpenAPI, `OperationBuilder::sse_json::<T>(openapi, "desc")` adds a 200 `text/event-stream` response with `$ref` to `T`. `T` needs `#[toolkit_macros::api_dto(response)]` and `utoipa::ToSchema`.
- `/app/docs/api/api.json` already contains the mini-chat surface:
  - `POST /mini-chat/v1/chats/{id}/messages:stream` (`mini_chat.stream_message`) returns 200 `text/event-stream` with schema `#/components/schemas/MiniChatSseEvent`, `security: bearerAuth`, plus 400/401/403/404/409/422/429/500/503 as `application/problem+json`.
  - The other two SSE endpoints are `PATCH …/turns/{request_id}` (`edit_turn`) and `POST …/turns/{request_id}/retry`.
  - In total it has 19 mini-chat operations (chats CRUD, attachments, messages, reactions, turns, models, `quota/status`).
- An untracked file, `/app/docs/superpowers/specs/2026-10-04-mini-chat-openapi-subset.json`, was created today and is probably from your session.

---

## 6. E2E harness (`/app/testing/e2e`)
- `conftest.py`:
  - `base_url` comes from `E2E_BASE_URL`, default `http://localhost:8086`.
  - `auth_headers` returns `{"Authorization": f"Bearer {E2E_AUTH_TOKEN or 'e2e-token-tenant-a'}"}`. Its docstring says `accept_all`, but `e2e-local.yaml` actually uses `static_tokens`.
  - Other users or tenants are chosen by token, e.g. `Bearer e2e-token-tenant-b` or `Bearer e2e-token-tenant-a-reviewer`.
- `lib/orchestrator.py`:
  - `GearTestEnv` fields: `binary`, `config_path` (default `config/e2e-local.yaml`), `config_patch(text, env) -> text`, `port=8086`, `health_path="/healthz"`, `env`, `sidecars`.
  - `test_env` is a session fixture yielding `RunningTestEnv(base_url, env, sidecars)`. Override the `gear_test_env` fixture in a suite's conftest.
  - Each suite needs `suites/<suite>/e2e.yaml` with `launcher: e2e-launcher|pytest`, `features` and `features_file` (see the README).
- `helpers/mock_server.py` only serves static files for GET (`MockHTTPServer`, `start_mock_server`). **There is no mock LLM, OpenAI or SSE helper.** The `suites/` folder is empty.
- In Rust, `oagw/src/test_support/mock.rs` has `MockUpstream`, `MockGuard` and `MockResponse`, which include SSE mocks.
- `config/e2e-features.txt` includes `mini-chat`. The Makefile sets `GEAR_SERVER_EXTRA_FEATURES_mini-chat ?= static-credstore` (because OAGW checks `secret_ref`). `make mini-chat` runs with `--features mini-chat,static-authn,static-authz,single-tenant,static-credstore,otel --config config/mini-chat.yaml`.
- The example server's `apps/cf-gears-example-server/src/registered_gears.rs` imports `mini_chat`, `mini_chat::infra::plugins::static_audit` and `static_model_policy`.

### Config facts tests depend on
- `e2e-local.yaml`:
  - The mini-chat block has only `client_credentials`.
  - The credstore secret `openai-key` = `sk-test-e2e-fake-key` is private to tenant `00000000-df51-…` and owner `11111111-…`, which matches the default S2S identity.
  - OAGW is set to `proxy_timeout_secs: 2`, `allow_http_upstream: true` and `ssrf_policy.enabled: false`.
  - The gateway has `auth_disabled: false` and binds `0.0.0.0:8086`.
- `config/mini-chat.yaml`:
  - The gateway binds `127.0.0.1:8087` with `auth_disabled: true`.
  - The provider is `providers.azure_openai {kind: openai_responses, storage_kind: azure, host, api_path "/openai/v1/responses", api_version, auth_plugin_type apikey, auth_config {header "api-key", prefix "", secret_ref "azure-openai-key"}}`.
  - OAGW is set to `proxy_timeout_secs: 60` and `ssrf_policy.enabled: false`, with no `allow_http_upstream`.

### Other docs
`grep -ril mini-chat` on `docs/` finds only general architecture docs and `api.json`. The main mini-chat specs are in `/app/gears/mini-chat/docs/` (`DESIGN.md`, `PRD.md`, `acceptance-criteria.md`, ADR 0001–0010). The relevant DESIGN passages:
- Line 411: OAGW provisioning, create-or-reuse on AlreadyExists, and the background retry (2 s doubling to 60 s).
- Line 604: the startup sequence.
- Appendix B.1 (line ~6810): the config schema.