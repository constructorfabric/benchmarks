# Platform note 01: gear wiring, REST, plugins, ClientHub, background tasks, tests

Audience: the engineer implementing the `mini-chat` gear (`gears/mini-chat/mini-chat`, package
`cf-gears-mini-chat`, lib `mini_chat`) and its SDK crate, without re-reading platform source.
Everything below was checked against the code at the commit this note was written on. Each
section cites the file it comes from.

Related notes in this directory:
- `00-openapi-minichat.md`: the target REST contract (every operation, schema, and header) taken from `docs/api/api.json`.
- `03-authz-authn-errors.md`: `SecurityContext`, `PolicyEnforcer`, AuthN S2S exchange, canonical errors.

---

## 0. Current state of the repo (what exists, what you create)

- `gears/mini-chat/mini-chat/Cargo.toml` already exists and is already a workspace member
  (`Cargo.toml` `members`). Its `src/lib.rs` is a stub:
  ```rust
  pub mod infra { pub mod plugins { pub mod static_audit {} pub mod static_model_policy {} } }
  ```
- **There is no `mini-chat-sdk` crate yet.** You must create it (for example
  `gears/mini-chat/mini-chat-sdk`, package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`), add it to
  the workspace `members`, and add it as a dependency of `cf-gears-mini-chat`. Optionally add a
  `[workspace.dependencies]` entry for it.
- `apps/cf-gears-example-server/src/registered_gears.rs` already links three gears from the crate:
  ```rust
  #[cfg(feature = "mini-chat")]
  use mini_chat as _;
  #[cfg(feature = "mini-chat")]
  use mini_chat::infra::plugins::static_audit as _;
  #[cfg(feature = "mini-chat")]
  use mini_chat::infra::plugins::static_model_policy as _;
  ```
  So the modules `mini_chat::infra::plugins::static_audit` and
  `mini_chat::infra::plugins::static_model_policy` **must stay `pub`**. Each holds one plugin gear.
  The example server's `Cargo.toml` has `mini-chat = ["dep:mini-chat"]` and `k8s = ["mini-chat/k8s"]`.
- Run command, from `config/mini-chat.yaml`:
  `cargo run --bin cf-gears-server --features mini-chat,static-authn,static-authz,single-tenant,static-credstore -- --config config/mini-chat.yaml run`.
  The real binary or package name is `cf-gears-example-server`.
- Gear names used in config (and therefore in `#[toolkit::gear(name = ...)]`):
  `mini-chat`, `static-mini-chat-audit-plugin`, `static-mini-chat-model-policy-plugin`.

### 0.1 Dependencies already declared in `gears/mini-chat/mini-chat/Cargo.toml`

`authn-resolver-sdk` (path dep, lib `authn_resolver_sdk`), `authz-resolver-sdk`, `oagw-sdk`,
`types-registry-sdk`, `types-registry`, `authn-resolver`, `authz-resolver`, `oagw` (gear crates, for
`deps = [...]`), `anyhow`, `async-trait`, `tokio`, `tracing`, `tracing-opentelemetry`, `inventory`,
`tokio-retry`, `tokio-util`, `futures`, `bytes`, `regex`, `multer = "3.1"` (direct, not workspace),
`base64`, `image`, `serde`, `serde_json`, `utoipa` (+`time`), `axum` (+`macros`,`multipart`), `http`, `time`,
`uuid`, `sea-orm`, `sea-orm-migration`, `thiserror`, `opentelemetry`, `toolkit`,
`toolkit-canonical-errors` (+`axum`), `toolkit-gts`, `gts`, `toolkit-db` (+`sqlite`,`pg`),
`toolkit-db-macros`, `toolkit-security`, `toolkit-macros`, `toolkit-odata` (+`with-utoipa`), `heck`,
`secrecy`, optional `kube`/`k8s-openapi` (feature `k8s`). Dev: `chrono`, `opentelemetry_sdk`(+`testing`),
`temp-env`.

**Not yet declared, but you will probably need:**
- your SDK crate;
- `toolkit-odata-macros = { workspace = true }` if you want `#[derive(ODataFilterable)]`. The derive is **not**
  re-exported by `toolkit-odata` or `toolkit`.
- `schemars = { workspace = true }` if the SDK or the gear declares GTS schemas (`credstore-sdk` and `authn-resolver-sdk` both depend on it);
- dev: `tower = { workspace = true }` for `ServiceExt::oneshot` in router tests,
  `tokio = { workspace = true, features = ["rt","macros","test-util"] }`,
  `types-registry-sdk = { workspace = true, features = ["test-util"] }` (for `MockTypesRegistryClient`).
  `toolkit-db` already has `sqlite`.

### 0.2 Workspace dependency names (`/app/Cargo.toml` `[workspace.dependencies]`)

| dep key (use `{ workspace = true }`) | package | lib name (`use ...`) |
|---|---|---|
| `toolkit` | cf-gears-toolkit 0.10.0 | `toolkit` (default features `otel`,`db`) |
| `toolkit-macros` | cf-gears-toolkit-macros | `toolkit_macros` |
| `toolkit-db` | cf-gears-toolkit-db | `toolkit_db` (features `sqlite`,`pg`,`mysql`) |
| `toolkit-db-macros` | cf-gears-toolkit-db-macros | `toolkit_db_macros` |
| `toolkit-odata` | cf-gears-toolkit-odata | `toolkit_odata` (feature `with-utoipa`) |
| `toolkit-odata-macros` | cf-gears-toolkit-odata-macros | `toolkit_odata_macros` |
| `toolkit-canonical-errors` | cf-gears-toolkit-canonical-errors | `toolkit_canonical_errors` (feature `axum`) |
| `toolkit-canonical-errors-macro` | ... | `toolkit_canonical_errors_macro` |
| `toolkit-security` | cf-gears-toolkit-security | `toolkit_security` |
| `toolkit-gts` | cf-gears-toolkit-gts | `toolkit_gts` |
| `toolkit-gts-macros` | cf-gears-toolkit-gts-macros | `toolkit_gts_macros` (re-exported by `toolkit_gts`) |
| `toolkit-sdk` | cf-gears-toolkit-sdk | `toolkit_sdk` |
| `toolkit-http` | cf-gears-toolkit-http | `toolkit_http` |
| `toolkit-utils` | cf-gears-toolkit-utils | `toolkit_utils` |
| `toolkit-auth` | cf-gears-toolkit-auth | `toolkit_auth` |
| `toolkit-contract`, `toolkit-contract-macros` | ... | `toolkit_contract`, `toolkit_contract_macros` |
| `gts`, `gts-macros` | upstream 0.12.1 | `gts`, `gts_macros` |
| `types-registry` / `types-registry-sdk` | cf-gears-types-registry(-sdk) | `types_registry` / `types_registry_sdk` (sdk feature `test-util`) |
| `authn-resolver` | cf-gears-authn-resolver | `authn_resolver` (SDK has **no** workspace entry: path `gears/system/authn-resolver/authn-resolver-sdk`, lib `authn_resolver_sdk`) |
| `authz-resolver` / `authz-resolver-sdk` | ... | `authz_resolver` / `authz_resolver_sdk` |
| `tenant-resolver` / `tenant-resolver-sdk` | ... | `tenant_resolver` / `tenant_resolver_sdk` |
| `oagw` / `oagw-sdk` | cf-gears-oagw(-sdk) | `oagw` / `oagw_sdk` |
| `credstore` / `credstore-sdk` | cf-gears-credstore(-sdk) | `credstore` / `credstore_sdk` |
| `api-gateway` | cf-gears-api-gateway | `api_gateway` |
| `static-authn-plugin`, `static-authz-plugin` | ... | `static_authn_plugin`, `static_authz_plugin` |
| `grpc_hub` | cf-gears-grpc-hub | `grpc_hub` |

Also relevant third-party workspace deps: `axum 0.8`, `utoipa 5.5`, `tokio`, `tokio-util` (`rt`),
`tokio-stream`, `futures`, `async-stream`, `async-trait`, `sea-orm 2.0`, `sea-orm-migration 2.0`,
`inventory`, `serde_html_form`, `schemars 1.2`, `arc-swap`, `dashmap`, `parking_lot`, `secrecy`,
`humantime-serde`, `temp-env`, `bytes`, `base64`, `image`, `http`, `heck`.

**cargo-shear:** gear crates that are used only through `deps = [...]` re-exports must be listed in
`[workspace.metadata.cargo-shear] ignored` (`Cargo.toml` around line 742). The list already contains `authn-resolver`, `authz-resolver`,
`credstore`, `grpc_hub`, `oagw`, `tenant-resolver`, and `types-registry`.

---

## 1. Declaring and registering a gear

### 1.1 The `#[toolkit::gear(...)]` attribute (`libs/toolkit-macros/src/lib.rs`)

Re-exported as `toolkit::gear`. Syntax:

```rust
#[toolkit::gear(
    name = "mini-chat",                              // REQUIRED, kebab-case string literal (compile-time validated)
    deps = [types_registry, authn_resolver, authz_resolver, oagw], // crate IDENTS (snake_case), NOT strings
    capabilities = [db, rest, stateful],             // idents or string literals
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready), // optional; needed for `stateful` unless you impl RunnableCapability
    // ctor = MyGear::new(),                         // optional Rust expr; default `<T as Default>::default()`
    // client = crate::SomeTrait,                    // optional; only adds an object-safety compile check
)]
pub struct MiniChatGear { /* OnceLock<...> fields */ }
```

- Valid capabilities: `db`, `rest`, `rest_host`, `stateful`, `system`, `grpc_hub`, `grpc`.
  A compile-time assertion requires the matching trait impl:
  `db`→`toolkit::contracts::DatabaseCapability`, `rest`→`RestApiCapability`,
  `rest_host`→`ApiGatewayCapability`, `grpc`→`GrpcServiceCapability`, `grpc_hub`→`GrpcHubCapability`.
  `stateful` requires `RunnableCapability` **only when `lifecycle(...)` is absent**. `system` needs no
  trait, but every system gear in the repo still writes `impl SystemCapability for X {}` (it is
  registered as `Arc<dyn SystemCapability>`, so the impl must exist).
- `Gear` (`toolkit::contracts::Gear`) is always required.
- `deps`: each ident becomes runtime dep name `ident.replace('_','-')` (`authn_resolver` → `"authn-resolver"`). The macro emits
  `#[cfg(not(test))] #[doc(hidden)] pub use ::<ident> as _gear_dep_<ident>;` so the dep crate must be a
  **direct Cargo dependency**. An unregistered dep name fails startup with `RegistryError::UnknownDependency`.
  The alias is emitted in the module where the struct lives, so **two gears with the same dep must
  not be declared in the same module** (duplicate `_gear_dep_types_registry`). Keep each plugin gear in its own module.
  Note: `docs/toolkit_unified_system/03_clienthub_and_plugins.md` shows `deps = ["types-registry"]`
  (string literals). **That doc is stale.** The macro rejects strings.
- `lifecycle(entry = "...", stop_timeout = "45s", await_ready)`: `await_ready` can also be written `await_ready = true`.
  Defaults: entry `serve`, timeout `30s`, await_ready false.
- Generated items: `impl MiniChatGear { pub const MODULE_NAME: &'static str = "mini-chat"; }`, a
  registrator `fn __<struct_snake>_registrator(b: &mut toolkit::registry::RegistryBuilder)` submitted via
  `toolkit::inventory::submit!`. With `lifecycle`, it also emits `impl toolkit::lifecycle::Runnable`, a ready shim, and
  `pub fn into_gear(self) -> WithLifecycle<Self>`.
- The macro expansion references `::async_trait`, `::tokio_util`, `::anyhow`, and `::toolkit`. The crate needs these
  as direct deps (it has them). Plugin crates also list `inventory` with the comment "Required by toolkit::gear macro".

### 1.2 Lifecycle traits (`libs/toolkit/src/contracts.rs`)

```rust
#[async_trait] pub trait Gear: Send + Sync + 'static {
    async fn init(&self, ctx: &toolkit::context::GearCtx) -> anyhow::Result<()>;
}
pub trait DatabaseCapability: Send + Sync {                    // cfg(feature="db"), on by default
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>>;
}
pub trait RestApiCapability: Send + Sync {                     // sync; runs AFTER migrations and init
    fn register_rest(&self, ctx: &GearCtx, router: axum::Router, openapi: &dyn OpenApiRegistry)
        -> anyhow::Result<axum::Router>;
    fn healthcheck(&self, _ctx: &GearCtx) -> Option<Arc<dyn toolkit::Healthcheck>> { None } // optional /readyz probe
}
#[async_trait] pub trait RunnableCapability: Send + Sync {
    async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()>;
    async fn stop(&self, deadline_token: CancellationToken) -> anyhow::Result<()>;
}
#[async_trait] pub trait SystemCapability: Send + Sync {
    fn pre_init(&self, _sys: &toolkit::runtime::SystemContext) -> anyhow::Result<()> { Ok(()) }
    async fn post_init(&self, _sys: &toolkit::runtime::SystemContext) -> anyhow::Result<()> { Ok(()) }
}
```
Imports used in real gears: `use toolkit::{Gear, GearCtx, RestApiCapability};`,
`use toolkit::contracts::{DatabaseCapability, SystemCapability};`, `use toolkit::api::OpenApiRegistry;`,
`use toolkit::lifecycle::ReadySignal;`.

**Runtime phase order** (`libs/toolkit/src/runtime/host_runtime.rs::run_phases_internal`):
1. `pre_init` (system gears only)
2. **DB migrations**, before init. All gears with `DatabaseCapability` and a `database:` config are migrated.
3. `init`. System gears go first, then the rest in topological `deps` order (`gears_by_system_priority`).
4. Proxy wiring, then `post_init` (system gears only; types-registry switches to "ready" mode here).
5. REST: `register_rest` for every `rest` gear, on one **accumulated** router.
6. gRPC
7. `start` for runnable gears
8. Wait for shutdown
9. `stop` in reverse order

Every linked gear runs. There is no config-based enablement, and a missing config section means
`config_or_default()` returns defaults.

mini-chat does **not** need `system`. All of its dependencies are system gears and are
initialized before it: `types-registry`, `authn-resolver`, `authz-resolver`, `oagw` (oagw is
`capabilities = [system, rest]`, deps `[types_registry, authz_resolver, credstore, tenant_resolver]`).
Because oagw depends on credstore, credstore is linked transitively, and a `static-credstore-plugin`
must exist at runtime or credstore operations return 503.

### 1.3 `GearCtx` (`libs/toolkit/src/context.rs`, re-exported `toolkit::GearCtx`)

```rust
impl GearCtx {
    pub fn gear_name(&self) -> &str;
    pub fn instance_id(&self) -> Uuid;
    pub fn client_hub(&self) -> Arc<toolkit::ClientHub>;
    pub fn cancellation_token(&self) -> &CancellationToken;          // child of the runtime root token
    pub fn db(&self) -> Option<toolkit_db::DBProvider<toolkit_db::DbError>>;     // None if no `database:` for this gear
    pub fn db_required(&self) -> anyhow::Result<toolkit_db::DBProvider<toolkit_db::DbError>>;
    pub fn config<T: DeserializeOwned>(&self) -> Result<T, ConfigError>;              // gears.<name>.config REQUIRED
    pub fn config_or_default<T: DeserializeOwned + Default>(&self) -> Result<T, ConfigError>;
    pub fn config_expanded<T: DeserializeOwned + ExpandVars>(&self) -> Result<T, ConfigError>;
    pub fn config_expanded_or_default<T: DeserializeOwned + Default + ExpandVars>(&self) -> Result<T, ConfigError>;
    pub fn raw_config(&self) -> &serde_json::Value;
    pub fn without_db(&self) -> GearCtx;
    pub fn new(name: impl Into<Arc<str>>, instance_id: Uuid, cfg: Arc<dyn ConfigProvider>,
               hub: Arc<ClientHub>, cancel: CancellationToken) -> Self;     // for tests
}
```
YAML shape read by these methods:
`gears: { mini-chat: { database: { server: "...", file: "mini_chat.db" }, config: { ... } } }`.

**Typed config pattern** (`gears/credstore/credstore/src/config.rs`): `#[serde(default, deny_unknown_fields)]`
on every struct, a hand-written `Default`, and a `validate(&self) -> Result<(), String>` that `init`
calls:
```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CredStoreConfig { pub vendor: String, pub hierarchy: HierarchyCfg, pub reaper: ReaperCfg }
impl Default for CredStoreConfig { fn default() -> Self { Self { vendor: "constructorfabric".to_owned(), /*...*/ } } }
// init:
let cfg: CredStoreConfig = ctx.config_or_default()?;
cfg.validate().map_err(|err| anyhow::anyhow!("credstore config invalid: {err}"))?;
```
Because of `deny_unknown_fields`, every key used in `config/*.yaml` must exist in your struct.
The mini-chat keys used today are:
- `vendor`
- `url_prefix` (DESIGN default `/mini-chat`)
- `orphan_watchdog{scan_interval_secs,timeout_secs}`
- `upload_reaper{enabled,scan_interval_secs,stale_after_secs}`
- `client_credentials{client_id,client_secret}`
- `thread_summary_worker{enabled,summary_model_id,compression_threshold_pct,summary_system_prompt}`
- `providers.<id>{kind,storage_kind,host,upstream_alias,api_path,api_version,auth_plugin_type,auth_config{header,prefix,secret_ref},tenant_overrides}`
- `metrics.prefix`

See `gears/mini-chat/docs/DESIGN.md` around line 6817 for the full table.

**Env-var expansion** (`${VAR}`): derive `toolkit_macros::ExpandVars` (also `toolkit::ExpandVars`), mark
fields with `#[expand_vars]`, and load with `ctx.config_expanded_or_default()`. Impls exist for `String`,
`Option<T>`, `Vec<T>`, `HashMap<K,V>`, and `secrecy::SecretString`. There is **no impl for `BTreeMap`**. A
missing env var is an `ExpandVarsError::Var`, which **fails `init`**. Example
(`gears/credstore/plugins/static-credstore-plugin/src/config.rs`):
```rust
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct StaticCredStorePluginConfig {
    pub vendor: String, pub priority: i16,
    #[expand_vars] pub secrets: Vec<SecretConfig>,   // nested struct also derives ExpandVars
}
```
`config/mini-chat.yaml` uses `host: "${AZURE_OPENAI_API_HOST}"` under `providers.azure_openai`. Mark
`providers` and the nested `host` (plus `auth_config.secret_ref` if desired) with `#[expand_vars]`.

### 1.4 Complete DB + REST + background-task gear (credstore, verbatim, trimmed comments)

`gears/credstore/credstore/src/gear.rs`:
```rust
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use async_trait::async_trait;
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use tenant_resolver_sdk::TenantResolverClient;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::{DatabaseCapability, SystemCapability};
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::DBProvider;
use types_registry_sdk::TypesRegistryClient;

#[toolkit::gear(
    name = "credstore",
    deps = [authz_resolver, tenant_resolver, types_registry],
    capabilities = [system, db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct CredStoreGear { service: OnceLock<Arc<Service>> }

impl Default for CredStoreGear { fn default() -> Self { Self { service: OnceLock::new() } } }

impl CredStoreGear {
    pub(crate) async fn serve(self: Arc<Self>, cancel: CancellationToken, ready: ReadySignal)
        -> anyhow::Result<()> {
        let Some(svc) = self.service.get().cloned() else { anyhow::bail!("credstore: serve invoked before init"); };
        let mut interval = tokio::time::interval(Duration::from_secs(svc.reaper_tick_secs()));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ready.notify();
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                _ = interval.tick() => { svc.reap_and_refresh().await; }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Gear for CredStoreGear {
    #[tracing::instrument(skip_all, fields(module = "credstore"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: CredStoreConfig = ctx.config_or_default()?;
        cfg.validate().map_err(|err| anyhow::anyhow!("credstore config invalid: {err}"))?;

        let db_raw = ctx.db_required()?;                                     // DBProvider<DbError>
        let db: Arc<DBProvider<DomainError>> = Arc::new(DBProvider::new(db_raw.db())); // re-type the error
        let repo = Arc::new(SecretRepoImpl::new(Arc::clone(&db)));

        let authz_client = ctx.client_hub().get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZResolverApi: {e}"))?;
        let enforcer = PolicyEnforcer::new(authz_client);
        let tr_client = ctx.client_hub().get::<dyn TenantResolverClient>()
            .map_err(|e| anyhow::anyhow!("failed to get TenantResolverClient: {e}"))?;
        let plugins = Arc::new(GtsCredStorePluginSelector::new(ctx.client_hub(), cfg.vendor.clone()));
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()
            .map_err(|e| anyhow::anyhow!("failed to get TypesRegistryClient: {e}"))?;
        // ... build Service ...
        self.service.set(Arc::clone(&svc))
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;

        let client: Arc<dyn credstore_sdk::CredStoreClientV1> = Arc::new(CredStoreLocalClient::new(svc));
        ctx.client_hub().register::<dyn credstore_sdk::CredStoreClientV1>(client);
        Ok(())
    }
}

impl SystemCapability for CredStoreGear {}

impl DatabaseCapability for CredStoreGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        use sea_orm_migration::MigratorTrait;
        crate::infra::storage::migrations::Migrator::migrations()
    }
}

impl RestApiCapability for CredStoreGear {
    fn register_rest(&self, _ctx: &GearCtx, router: axum::Router, openapi: &dyn OpenApiRegistry)
        -> anyhow::Result<axum::Router> {
        let svc = self.service.get().cloned()
            .ok_or_else(|| anyhow::anyhow!("credstore Service not initialized"))?;
        Ok(crate::api::rest::register_routes(router, openapi, svc))
    }
}
```
`lib.rs` is `pub mod api; pub mod client; pub mod config; pub mod domain; pub mod gear; pub mod infra; pub use gear::CredStoreGear;`.

Migrations (`gears/credstore/credstore/src/infra/storage/migrations.rs`):
```rust
use sea_orm_migration::prelude::*;
pub mod m0001_initial_schema;
pub struct Migrator;
#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> { vec![Box::new(m0001_initial_schema::Migration)] }
}
// m0001: #[derive(DeriveMigrationName)] pub struct Migration; impl MigrationTrait { async fn up(&self, manager: &SchemaManager) ... }
// uses manager.get_database_backend() to branch Postgres vs Sqlite raw SQL; conn.execute_unprepared(...)
```
If you use the transactional outbox (mini-chat audit/usage queues), append its DDL to `migrations()`
the way `gears/system/types-registry/types-registry/src/gear.rs` does:
```rust
let outbox = toolkit_db::outbox::outbox_migrations_with_prefix("mini_chat__outbox").expect("valid prefix");
migrations.extend(outbox);
```

Entity with secure-ORM scoping (`gears/credstore/credstore/src/infra/storage/entity/secrets.rs`):
```rust
use sea_orm::entity::prelude::*;
use toolkit_db::secure::Scopable;
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "credstore_secrets")]
#[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
pub struct Model { #[sea_orm(primary_key, auto_increment = false)] pub id: Uuid, pub tenant_id: Uuid, /* ... */ }
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)] pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
```
Queries look like `Entity::find().secure().scope_with(&scope).filter(...)` (`toolkit_db::secure::SecureEntityExt`). They
run on `let conn = db.conn()?;` or inside
`db.transaction(|tx| Box::pin(async move { ... })).await`. `DBProvider<E>` API
(`libs/toolkit-db/src/db_provider.rs`): `new(Db)`, `db() -> Db`, `conn() -> Result<DbConn<'_>, E>`,
`transaction(..)`, and `transaction_with_config(TxConfig, ..)`, with `E: From<toolkit_db::DbError>`.

### 1.5 How the example server links gears (inventory)

Each `#[toolkit::gear]` does `toolkit::inventory::submit!{ toolkit::registry::Registrator(...) }`. The
binary has to make the linker keep the crate (and the plugin modules), hence `use mini_chat as _;` plus
one `use` line per plugin module in `apps/cf-gears-example-server/src/registered_gears.rs`. The runtime
collects every registrator at startup. GTS types and instances declared with `toolkit_gts` macros are
collected the same way (`toolkit_gts::all_inventory_type_schemas()`, `all_inventory_instances()`), and
types-registry seeds them during its `init`.

---

## 2. REST

### 2.1 Imports

```rust
use axum::{Extension, Router};
use axum::http::StatusCode;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder, OperationBuilderODataExt,
    ParamSpec, ResponseHeaderSpec, ResponseHeaderType, ThrottlingSpec,
};
use toolkit::api::canonical_prelude::*;     // CanonicalError, Problem, resource_error, ApiResult<T>, OData,
                                            // JsonBody, JsonPage, created_json, no_content, ok_json,
                                            // extract (module), apply_select, axum::Json, StatusCode, IntoResponse
use toolkit::api::rest::extract::{Json, Path, Query};   // canonical-error-aware extractors (prefer these)
use toolkit_security::SecurityContext;
```
`toolkit::api` also re-exports `OperationBuilder, ParamSpec, ResponseHeaderSpec, ResponseHeaderType,
ThrottlingSpec, OpenApiRegistryImpl, ensure_schema`. `LicenseFeature`, `CORE_GLOBAL_BASE_LICENSE_FEATURE`
and `OperationBuilderODataExt` are only available under `toolkit::api::operation_builder`.

### 2.2 `OperationBuilder` (type-state; `libs/toolkit/src/api/operation_builder.rs`)

`OperationBuilder<H=Missing, R=Missing, S=(), A=AuthNotSet, L=LicenseNotSet>`. `.register(router, openapi)`
compiles **only** when a handler and at least one response are set, auth is decided, and the license is decided.

- Constructors: `OperationBuilder::get|post|put|delete|patch(path: impl Into<String>)`. Path syntax is
  axum 0.8 `{param}` (OpenAPI-identical); in `format!` write `{{id}}`.
- Descriptive (any stage): `.operation_id(s)`, `.summary(s)`, `.description(s)`, `.tag(s)`,
  `.param(ParamSpec)`, `.path_param(name, desc)`, `.query_param(name, required, desc)`,
  `.query_param_typed(name, required, desc, "integer")`, `.query_param_array(name, req, desc, item_type)`,
  `.json_request::<T>(openapi, desc)`, `.json_request_no_desc::<T>(openapi)`, `.request_optional()`,
  `.multipart_file_request(field_name, Option<&str>)`, `.octet_stream_request(Option<&str>)`,
  `.allow_content_types(&["..."])`, `.with_throttling(ThrottlingSpec)`, `.exposed()`.
- Auth (from `AuthNotSet`): `.authenticated()` (sets `spec.authenticated = true`; the license is still unset) or
  `.anonymous()` (sets both auth and license). `.public()` is a deprecated alias for `.anonymous().exposed()`.
- License (only after `.authenticated()`): `.require_license_features::<F>(iter)` or `.no_license_required()`.
  An empty iterator means no requirement.
- Handler: `.handler(fn)` (method router picked from the HTTP method) or `.method_router(MethodRouter<S>)` (per-route layers).
- First response (from `R=Missing`): `.response(ResponseSpec)`, `.json_response(status, desc)`,
  `.no_content_response(status, desc)` (no `content` block, so use it for 204, or for 201 without a body),
  `.json_response_with_schema::<T>(openapi, status, desc)`,
  `.json_array_response_with_schema::<Item>(openapi, status, desc)` (never pass `Vec<T>` to the plain
  variant: `ensure_schema` panics on the component name `Vec`), `.text_response(status, desc, "text/plain")`,
  `.html_response`, `.problem_response(openapi, status, desc)`, `.sse_json::<T>(openapi, desc)` (200
  `text/event-stream`), `.multipart_json::<T>(openapi, desc)` (200 `multipart/mixed`).
- More responses: the same methods are available again, plus `.response_header(ResponseHeaderSpec)`, which
  attaches the header to the **most recently declared** response and panics on a duplicate header name.
- Errors (problem+json, `Problem` schema): `.error_400 … .error_504(openapi)` (400, 401, 403, 404, 409, 413, 415, 422, 429, 500, 502, 503, 504),
  `.standard_errors(openapi)` (400, 401, 403, 404, 409, 429, 500), `.with_400_validation_error(openapi)`.
  413, 415, and 422 are never included automatically. Add them when the handler uses `extract::Json`.
- `.register(router, openapi)` calls `openapi.register_operation(&spec)` and then `router.route(path, method_router)`.
  A duplicate `(method, path)` triggers a warning in the registry and an axum panic.

**DTO trait bounds:** `json_request::<T>` needs `T: utoipa::ToSchema + utoipa::PartialSchema +
toolkit::api::api_dto::RequestApiDto + 'static`. Response helpers need `ResponseApiDto`. These marker traits
come from `#[toolkit_macros::api_dto(request)]`, `(response)`, or `(request, response)`. That attribute derives
`serde::Serialize`/`Deserialize` and `utoipa::ToSchema` (written as `utoipa::`, so a direct `utoipa` dep is required),
adds `#[serde(rename_all = "snake_case")]`, and implements the marker. Built-in markers are already implemented for `Vec<T>`,
`Option<T>`, `Box<T>`, `toolkit_odata::Page<T>`, `serde_json::Value`, and `Problem` (`libs/toolkit/src/api/api_dto.rs`).
Example (`gears/credstore/credstore/src/api/rest/dto.rs`):
```rust
#[derive(Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct CreateSecretRequestDto {
    #[schema(pattern = "^[A-Za-z0-9_-]+$", min_length = 1, max_length = 255)]
    pub reference: String,
    pub value: String,
    #[serde(default)] pub sharing: SharingModeDto,
    #[serde(default, rename = "type")] pub secret_type: Option<String>,
    #[serde(default)] #[schema(format = DateTime)] pub expires_at: Option<String>,
}
```

**OpenAPI component names and schema aliases.** The component name is `T::name()`, which by default is the bare
Rust ident. Two different types with the same name **panic** at registration (`OpenApiRegistryImpl::ensure_schema_raw`:
"OpenAPI schema name collision"). Rename with utoipa's alias attribute, as in
`gears/system/authz-resolver/authz-resolver-sdk/src/models.rs`:
```rust
#[derive(..., utoipa::ToSchema)]
#[schema(as = AuthzBarrierMode)]
pub enum BarrierMode { ... }
```
The target contract (`docs/api/api.json`) uses these mini-chat component names: `ChatDetailDto`, `CreateChatReq`,
`UpdateChatReq`, `MiniChatMessageDto`, `MiniChatReactionDto`, `MiniChatSseEvent`, `SetReactionReq`,
`StreamMessageRequest`, `EditTurnRequest`, `TurnStatusResponse`, `AttachmentDetailDto`, `ModelDto`,
`ModelListDto`, `QuotaStatusResponse`, and so on (full list in note 00). The `MiniChat*` names are aliases that
avoid collisions. `Page<T>`'s hand-written `ToSchema::name()` is `format!("Page_{}", T::name())`
(`libs/toolkit-odata/src/page.rs`), so `Page<MessageDto>` with `#[schema(as = MiniChatMessageDto)]` produces
`Page_MiniChatMessageDto`.

### 2.3 Full route example (credstore `routes.rs`, verbatim, abridged)

```rust
pub fn register_routes(router: Router, openapi: &dyn OpenApiRegistry, svc: Arc<ConcreteService>) -> Router {
    let router = OperationBuilder::post("/credstore/v1/secrets")
        .operation_id("credstore.create_secret")
        .summary("Create a secret")
        .tag("Credential Store")
        .authenticated()
        .no_license_required()
        .json_request::<CreateSecretRequestDto>(openapi, "Secret reference, value, and sharing mode")
        .handler(handlers::create_secret)
        .no_content_response(StatusCode::CREATED, "Secret created (see Location header)")
        .error_400(openapi).error_401(openapi).error_403(openapi)
        .error_409(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/credstore/v1/secrets/{ref}")
        .operation_id("credstore.put_secret")
        .authenticated().no_license_required()
        .path_param("ref", "Secret reference")
        .param(ParamSpec::header("If-Match").required(true).description("..."))
        .json_request::<UpdateSecretRequestDto>(openapi, "Secret value and sharing mode")
        .handler(handlers::put_secret)
        .no_content_response(StatusCode::NO_CONTENT, "Secret stored")
        .error_400(openapi) /* ... */ .register(router, openapi);

    let router = OperationBuilder::get("/credstore/v1/secrets/{ref}")
        .operation_id("credstore.get_secret")
        .authenticated().no_license_required()
        .path_param("ref", "Secret reference")
        .handler(handlers::get_secret)
        .json_response_with_schema::<GetSecretResponseDto>(openapi, StatusCode::OK, "Resolved secret value and metadata")
        .error_404(openapi) /* ... */ .register(router, openapi);

    router.layer(axum::Extension(svc))
}
```
**Caution:** the `router` passed to `register_rest` is the accumulated router of every gear registered
before yours (`host_runtime.rs::run_rest_phase`). `router.layer(...)` therefore also wraps the earlier gears'
routes. credstore and types-registry accept this. To keep your layers local, build
`let mine = Router::new(); /* register ops on mine */; let mine = mine.layer(Extension(state)); Ok(router.merge(mine))`.
Either way, give the extension a unique type (for example `Arc<MiniChatAppState>`).

### 2.4 License requirement (`gts.cf.core.lic.feat.v1~cf.core.global.base.v1`)

`toolkit::api::operation_builder::CORE_GLOBAL_BASE_LICENSE_FEATURE: &str =
gts_id!("cf.core.lic.feat.v1~cf.core.global.base.v1")`, which expands to the literal above. Pattern
(`gears/system/types-registry/types-registry/src/api/rest/routes.rs`; oagw does the same):
```rust
struct License;
impl AsRef<str> for License { fn as_ref(&self) -> &'static str { CORE_GLOBAL_BASE_LICENSE_FEATURE } }
impl LicenseFeature for License {}
// ...
OperationBuilder::get(format!("{V1}/entities"))
    .authenticated()
    .require_license_features::<License>([])        // NOTE: `[]` (empty) => spec.license_requirement = None
```
`require_license_features::<License>([])` declares **no** requirement. To actually require the base
feature, pass a value: `.require_license_features([License])`. Enforcement lives in
`gears/system/api-gateway/src/middleware/license_validation.rs`, keyed by `(method, matched path)`.
It is a stub: any required feature **other than** the base feature gets 403 `permission_denied` with
reason `LICENSE_FEATURE_REQUIRED`, and the base feature always passes. DESIGN (ADR-0008) says mini-chat
routes are gated on the base license feature, so use `.require_license_features([License])`.

### 2.5 Throttling: binding a route to `rl_mini_chat_chat` / `ifl_mini_chat_chat`

Zones are defined in the api-gateway config (`gears.api-gateway.config.rate_limit_zones` /
`in_flight_limit_zones`; see `config/mini-chat.yaml` and `config/quickstart.yaml` lines 108-160). A route
references them **in code** through `ThrottlingSpec` (`operation_builder.rs`):
```rust
#[derive(Clone, Debug, Default)]
pub struct ThrottlingSpec {
    pub rate_limit_zone: Option<String>,
    pub in_flight_limit_zone: Option<String>,
    pub require_security_context: bool,   // false => throttled BEFORE auth, IP-keyed zones only
    pub dry_run: bool,                    // true => only warn-log would-be rejections, never 429
}
```
The quickstart config documents that the expected `create_chat` binding is:
```rust
OperationBuilder::post(format!("{prefix}/v1/chats"))
    .operation_id("mini_chat.create_chat")
    .with_throttling(ThrottlingSpec {
        rate_limit_zone: Some("rl_mini_chat_chat".to_owned()),
        in_flight_limit_zone: Some("ifl_mini_chat_chat".to_owned()),
        require_security_context: false,   // zones are `key: { type: ip }`
        dry_run: true,                     // per quickstart.yaml comment: observe-only
    })
```
The registry emits the `x-throttling-rate-limit-zone` and `x-throttling-in-flight-limit-zone` extensions, and the gateway adds
`x-rate-limit-rps`, `x-rate-limit-burst`, and `x-in-flight-limit` to the final document. Referencing a zone that is not
defined in gateway config fails gateway startup validation. An identity-keyed zone on an anonymous operation, or with
`auth_disabled: true`, is rejected at router build.

### 2.6 URL prefix

The toolkit has **no** per-gear prefix mechanism. Every gear writes absolute paths (`"/credstore/v1/secrets"`,
`format!("{V1}/entities")` in types-registry). For mini-chat, `url_prefix` is a gear config field (default
`/mini-chat`, DESIGN about line 6819). Build paths as `format!("{}/v1/chats", cfg.url_prefix)` and so on. The gateway
strips its own `prefix_path` (e.g. `/cf`) before routing, so the runtime path is `/mini-chat/v1/chats`
and the public URL is `/cf/mini-chat/v1/chats` (quickstart.yaml comment).

`.exposed()` sets `x-toolkit-visibility: exposed`. That only matters to the out-of-process `GatewayProvider`.
The in-process api-gateway serves every registered route either way. Routes are internal by default.

### 2.7 SecurityContext in handlers

The api-gateway inserts `toolkit_security::SecurityContext` into request extensions
(`gears/system/api-gateway/src/gear.rs` around line 464 and `middleware/auth.rs`):
- `auth_disabled: true` (all dev configs): every request gets
  `SecurityContext::builder().subject_id(DEFAULT_SUBJECT_ID).subject_tenant_id(DEFAULT_TENANT_ID).build()`.
- Otherwise: bearer → `AuthNResolverClient::authenticate` → `result.security_context`. Anonymous routes and
  CORS preflight get `SecurityContext::anonymous()`.

Handler signature (`gears/credstore/credstore/src/api/rest/handlers.rs`):
```rust
pub async fn create_secret(
    uri: axum::http::Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Json(body): Json<CreateSecretRequestDto>,
) -> ApiResult<impl IntoResponse> {
    // `?` on DomainError works because `impl From<DomainError> for CanonicalError` exists
    svc.put(&ctx, &key, /*...*/).await?;
    let location = format!("{}/{}", uri.path().trim_end_matches('/'), key.as_ref());
    Ok((StatusCode::CREATED, [(axum::http::header::LOCATION, location)]).into_response())
}
```
`ApiResult<T> = Result<T, CanonicalError>`. `CanonicalError: IntoResponse` renders `application/problem+json`, and the
canonical middleware (installed by api-gateway, `gear.rs` around line 554) fills in `instance` and `trace_id`.

### 2.8 Body, query, and path extractors and their errors

`toolkit::api::rest::extract::{Json, Path, Query}` (`libs/toolkit/src/api/rest/extract/*.rs`) are drop-in replacements
for the axum extractors whose `Rejection = CanonicalError`. `extract::Json<T>` is also usable as a response body.
Rejections map as follows (axum status kept as override, resource_type `gts.cf.core.http.request.v1~`, one field
violation with field `"body"`/`"path"`/`"query"`):

| case | status | `field_violations[0].reason` |
|---|---|---|
| malformed JSON syntax | 400 | `json_syntax_error` |
| valid JSON, wrong shape / unknown field (`deny_unknown_fields`) / bad enum | 422 | `invalid_json_body` |
| missing or non-JSON `Content-Type` | 415 | `missing_json_content_type` |
| body read error / too large | 413 (etc.) | `json_body_read_error` |
| bad path param | 400 | `invalid_path_params` (5xx route/type bugs become `internal`) |
| bad query | 400 | `invalid_query_string` |

The wire shape for a 422 (from `json.rs` tests):
```json
{"type":"gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~","title":"Invalid Argument","status":422,
 "detail":"Request validation failed","context":{"resource_type":"gts.cf.core.http.request.v1~",
 "field_violations":[{"field":"body","description":"Failed to deserialize ... unknown field `extra` ...","reason":"invalid_json_body"}]}}
```
If you use plain `axum::Json` (credstore does), the rejection is text/plain. The canonical middleware wraps it
into a minimal Problem with `type: "about:blank"` and the same status, and loses the reason codes. Prefer `extract::Json`
and declare `.error_413/.error_415/.error_422` on the route.

Handler-side validation errors should be domain errors mapped through `#[resource_error(gts_id!("..."))]`
markers (`X::invalid_argument().with_field_violation(field, detail, REASON).create()` → 400). See note 03.

### 2.9 Query params, OData, pagination

- Plain query: `Query(params): Query<MyParams>` with `#[derive(Deserialize)]`, documented with `.query_param(...)`.
- OData extractor: `toolkit::api::odata::OData` (also in `canonical_prelude`). `pub struct OData(pub ODataQuery)`
  implements `Deref<Target = ODataQuery>` and `into_inner()`, with `Rejection = CanonicalError`. It binds `$filter`, `$orderby`, `$select`,
  `limit` (alias `$top`), and `cursor` (alias `$skiptoken`). Any other `$`-prefixed key is rejected with 400. `$skip` is not supported.
  ```rust
  pub async fn list_chats(Extension(ctx): Extension<SecurityContext>, Extension(st): Extension<Arc<AppState>>,
                          OData(query): OData) -> ApiResult<Json<Page<ChatDetailDto>>> { ... }
  ```
- Declaring OData in OpenAPI (`OperationBuilderODataExt`, trait import required):
  `.with_odata_filter::<F>()`, `.with_odata_orderby::<F>()`, `.with_odata_select()`, where `F: toolkit_odata::filter::FilterField`.
  Allowed operators come from `FieldKind`. The order of `F::FIELDS` controls the description order (the contract lists
  `updated_at, id, title` for chats and `created_at, id, role` for messages). The contract declares `limit` and `cursor` first:
  ```rust
  .query_param_typed("limit", false, "Maximum number of chats to return", "integer")
  .query_param("cursor", false, "Cursor for pagination")
  .with_odata_filter::<ChatFilterField>()
  .with_odata_orderby::<ChatFilterField>()
  .handler(handlers::list_chats)
  .json_response_with_schema::<Page<ChatDetailDto>>(openapi, StatusCode::OK, "Paginated list of chats")
  ```
- `FilterField`: `trait FilterField: Copy+Eq+Hash+Debug+'static { const FIELDS: &'static [Self]; fn name(&self)->&'static str; fn kind(&self)->FieldKind; }`
  with `FieldKind::{String,I64,F64,Bool,Uuid,DateTimeUtc,Date,Time,Decimal}`. Generate it with
  `#[derive(toolkit_odata_macros::ODataFilterable)]` and `#[odata(filter(kind = "Uuid"))]` on fields (this produces
  `<Struct>FilterField`), or implement it by hand.
- DB pagination: `toolkit_db::odata::sea_orm_filter::paginate_odata::<F, M, E, D, _, _>(secure_select, &conn, &odata_query,
  ("id", SortDir::Desc), LimitCfg { default, max }, |m| m.into()) -> Result<Page<D>, toolkit_odata::Error>`, where
  `M: ODataFieldMapping<F, Entity = E>` maps fields to columns and extracts cursors. Use `paginate_odata_try` for a fallible mapper.
- `Page<T> { items: Vec<T>, page_info: PageInfo { next_cursor: Option<String>, prev_cursor: Option<String>, limit: u64 } }`.

### 2.10 Response headers, 201 with Location, 204, 503 Retry-After

OpenAPI side (this matches `docs/api/api.json` for `mini_chat.create_chat` exactly):
```rust
.json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::CREATED, "Created chat")
.response_header(ResponseHeaderSpec::new("Location", "Path of the created chat", ResponseHeaderType::String))
.error_400(openapi).error_401(openapi).error_403(openapi).error_422(openapi).error_500(openapi)
.error_503(openapi)
.response_header(ResponseHeaderSpec::new("Retry-After", "Seconds to wait before retrying", ResponseHeaderType::Integer))
```
(`ResponseHeaderType::{String, Integer, Boolean}`; there is also `ResponseHeaderSpec::without_description(name, ty)`.)
Runtime side: `toolkit::api::response::created_json(value, &uri, &new_id)` returns `(201, [(LOCATION, "<uri.path()>/<id>")], Json(value))`.
Other helpers: `ok_json(v)` and `no_content()` (`StatusCode::NO_CONTENT`). For 204 declare
`.no_content_response(StatusCode::NO_CONTENT, "Chat deleted")`. For a 503 with Retry-After at runtime use
`CanonicalError::service_unavailable().with_detail(d).with_retry_after_seconds(secs).create()`.

### 2.11 SSE responses

OpenAPI: `.sse_json::<MiniChatSseEvent>(openapi, "SSE of chat response events")` declares 200 `text/event-stream` with
that schema. The contract uses this for `stream_message` (POST `.../messages:stream`, JSON body
`StreamMessageRequest`), `edit_turn` (PATCH, `EditTurnRequest`), and `retry_turn` (POST, no body). The event type only documents the
wire format. It still needs `ToSchema` and `ResponseApiDto`.

Runtime: there is no per-request SSE helper in toolkit. `toolkit::SseBroadcaster<T>` (`libs/toolkit/src/http/sse.rs`) is a
fan-out broadcaster: `new(cap)`, `send(v)`, `subscribe_stream()`, `sse_response()`,
`sse_response_named(name)`, `sse_response_with_headers(iter)`. It uses JSON `data`, a 15 s keepalive, and the keepalive comment text `keepalive`.
For a per-request stream, use axum directly:
```rust
use axum::response::sse::{Event, KeepAlive, Sse};
use std::convert::Infallible;
pub async fn stream_message(Extension(ctx): Extension<SecurityContext>, Extension(st): Extension<Arc<AppState>>,
                            Path(chat_id): Path<uuid::Uuid>, Json(req): Json<StreamMessageRequest>)
    -> ApiResult<axum::response::Response> {
    // Validate and authorize BEFORE starting the stream so errors render as problem+json with a proper status.
    let rx = st.svc.start_turn(&ctx, chat_id, req).await?;               // e.g. mpsc::Receiver<StreamEvent>
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx)
        .map(|ev| Ok::<_, Infallible>(Event::default().event(ev.name()).json_data(&ev.data()).unwrap_or_else(|_| Event::default().data("serialization_error"))));
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response())
}
```
(`tokio-stream` and `futures::StreamExt` are available.) DESIGN names events `stream_started`, `ping`, `delta`, `tool`,
`citations`, `done`, `error`, each with an `event:` name and JSON `data:`. The contract schema `MiniChatSseEvent` is a
`oneOf` of `{event: enum[...], data: $ref}`. Client disconnect drops the stream. Detect it with a guard type in the
stream whose `Drop` signals your cancellation token. The gateway applies `RequestBodyLimitLayer` and
`DefaultBodyLimit::max(defaults.body_limit_bytes)`. It also applies a hard-coded **30 s `tower` timeout**
(`gears/system/api-gateway/src/gear.rs` around line 542, `.timeout(Duration::from_secs(30))`, mapped to canonical
`deadline_exceeded`). A tower timeout bounds the handler future, which ends when the response head is returned,
not the streamed body. So the SSE handler must return the `Sse` response promptly: do the validation, authorization,
and DB preflight, then stream. Never await the LLM call before returning. The same applies to multipart uploads,
whose handler future must finish in under 30 s.

### 2.12 Multipart and raw body

- OpenAPI: `.multipart_file_request("file", Some("File to upload"))` produces a `multipart/form-data` body
  `{type: object, properties: {file: {type: string, format: binary}}, required: [file]}`, a description of
  `"File to upload (expects field 'file' with file data)"` (matching the contract for `upload_attachment`), and an allow-list of
  `multipart/form-data` content types. The gateway returns 415 for anything else.
- `.octet_stream_request(Some(desc))` declares the `application/octet-stream` binary body (allow-list set accordingly).
- Runtime: `axum::extract::Multipart` (axum feature `multipart`, already enabled) or `multer` directly on
  `axum::body::Body`. `Multipart` must be the **last** extractor. Its rejections are plain text, so the canonical
  middleware wraps them into an `about:blank` Problem. Map field errors yourself. Body size is capped by the gateway limits above.
- For raw bytes use `body: axum::body::Bytes` or `axum::body::Body` (streaming, e.g. `body.into_data_stream()`).

### 2.13 Router-level tests (pattern from `gears/credstore/credstore/src/api/rest/routes_tests.rs`)

```rust
let openapi = toolkit::api::OpenApiRegistryImpl::new();
let router = register_routes(axum::Router::new(), &openapi, svc);
let mut req = Request::builder().method("POST").uri("/credstore/v1/secrets")
    .header("content-type", "application/json").body(Body::from(serde_json::to_vec(&json)?))?;
req.extensions_mut().insert(SecurityContext::builder().subject_id(s).subject_tenant_id(t).build()?); // gateway normally does this
let resp = router.oneshot(req).await?;          // tower::ServiceExt
let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await?;
```
The canonical middleware is **not** in this router, so `instance` and `trace_id` are absent, and plain-axum rejections
stay text/plain. Test with `extract::Json` to get Problem bodies.

---

## 3. Plugins

### 3.1 SDK side: plugin spec (GTS type) and plugin client trait

GTS base type: `toolkit::gts::PluginV1<P>` (= `toolkit_gts::PluginV1`, `libs/toolkit-gts/src/plugin.rs`), type id
`gts.cf.toolkit.plugins.plugin.v1~`, with fields `{ id: GtsInstanceId, vendor: String, priority: i16, properties: P }`.
A lower `priority` value wins.

`gears/system/authn-resolver/authn-resolver-sdk/src/gts.rs` (verbatim):
```rust
use toolkit::gts::PluginV1;
use toolkit_gts::gts_type_schema;

#[derive(Default)]                                  // REQUIRED by PluginV1::<P>::build_registration
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.authn_resolver.plugin.v1~"),
    description = "AuthN Resolver plugin specification",
    properties = "",
)]
pub struct AuthNResolverPluginSpecV1;
```
credstore uses the same shape (`gears/credstore/credstore-sdk/src/gts.rs`):
`type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.credstore.plugin.v1~")`, `pub struct CredStorePluginSpecV1;`.
For mini-chat, DESIGN names `MiniChatModelPolicyPluginSpecV1` / `MiniChatModelPolicyPluginClientV1` and
`MiniChatAuditPluginSpecV1` / `MiniChatAuditPluginClientV1`. Use type ids like
`gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_model_policy.plugin.v1~")` (your choice; the
segment must be valid GTS: lowercase, `_` allowed, `.v1~` suffix).

- `gts_id!("x")` is a const `&str` with `gts.` prepended.
- The macro output provides `<Spec as gts::GtsSchema>::TYPE_ID` and `SCHEMA_ID`, plus `Spec::gts_type_id()` (Display; used as `format!("{type_id}*")`).
- The macro also submits the schema to the inventory, so types-registry registers it at boot. **No per-init schema registration is needed.**
- SDK crate deps for this: `serde`, `serde_json`, `schemars`, `gts`, `toolkit-gts`, `toolkit` (for `PluginV1`), `async-trait`,
  `thiserror`, `toolkit-security`. `gts-macros` is needed only if you call upstream macros directly.

Other GTS macros (`toolkit_gts`): `gts_instance! { Type { id: gts_id!("...~..."), ... } }` (typed well-known instance,
optional `#[gts_static(NAME)]`), `gts_instance_raw!({ "id": gts_id!(...), ... })`, `gts_uri!`. A crate using
`gts_instance!` needs a direct `gts` dep, which mini-chat already has. Examples are in `libs/toolkit-gts/tests/macro_integration.rs`.

Plugin client trait (`gears/credstore/credstore-sdk/src/plugin_api.rs`):
```rust
#[async_trait::async_trait]
pub trait CredStorePluginClientV1: Send + Sync {
    async fn get(&self, ctx: &SecurityContext, tenant_id: &TenantId, key: &SecretRef, owner_id: Option<&OwnerId>)
        -> Result<Option<SecretValue>, CredStoreError>;
    async fn put(&self, ctx: &SecurityContext, tenant_id: &TenantId, key: &SecretRef, value: SecretValue,
                 owner_id: Option<&OwnerId>) -> Result<(), CredStoreError>;
    async fn delete(&self, ctx: &SecurityContext, tenant_id: &TenantId, key: &SecretRef, owner_id: Option<&OwnerId>)
        -> Result<(), CredStoreError>;
}
```
Default method bodies are allowed and used (DESIGN: `check_user_license` has a default returning `active: false`). The SDK
`lib.rs` re-exports everything at the root:
`pub use gts::CredStorePluginSpecV1; pub use plugin_api::CredStorePluginClientV1; pub use api::CredStoreClientV1;`.
The consumer-facing (public) client trait is a separate trait (`CredStoreClientV1`, `AuthNResolverClient`) that the main gear
registers unscoped.

### 3.2 Plugin gear side: register GTS instance and scoped client (static-authn-plugin, verbatim)

`gears/system/authn-resolver/plugins/static-authn-plugin/src/gear.rs`:
```rust
use std::sync::{Arc, OnceLock};
use async_trait::async_trait;
use authn_resolver_sdk::{AuthNResolverPluginClient, AuthNResolverPluginSpecV1};
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

#[toolkit::gear(name = "static-authn-plugin", deps = [types_registry])]
pub struct StaticAuthNPlugin { service: OnceLock<Arc<Service>> }

impl Default for StaticAuthNPlugin { fn default() -> Self { Self { service: OnceLock::new() } } }

#[async_trait]
impl Gear for StaticAuthNPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticAuthNPluginConfig = ctx.config_or_default()?;      // { vendor, priority, ... } deny_unknown_fields

        let (instance_id, instance_json) = PluginV1::<AuthNResolverPluginSpecV1>::build_registration(
            "cf.builtin.static_authn_resolver.plugin.v1",   // instance segment appended to TYPE_ID
            cfg.vendor.clone(),
            cfg.priority,
        )?;

        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        let service = Arc::new(Service::from_config(&cfg));
        self.service.set(service.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let api: Arc<dyn AuthNResolverPluginClient> = service;   // Service: impl AuthNResolverPluginClient
        ctx.client_hub().register_scoped::<dyn AuthNResolverPluginClient>(ClientScope::gts_id(&instance_id), api);
        Ok(())
    }
}
```
`gears/credstore/plugins/static-credstore-plugin/src/gear.rs` is identical in shape. It uses
`#[toolkit::gear(name = "static-credstore-plugin", deps = [types_registry])]`, loads config with
`ctx.config_expanded_or_default()?`, validates **before** registering, uses instance segment `"cf.core._.static_credstore.v1"`,
and calls `register_scoped::<dyn CredStorePluginClientV1>(ClientScope::gts_id(&instance_id), api)`.

`build_registration` signature (`libs/toolkit-gts/src/plugin.rs`):
```rust
impl<P: gts::GtsSchema + gts::GtsSerialize + Default> PluginV1<P> {
    pub fn build_registration(instance_segment: &str, vendor: impl Into<String>, priority: i16)
        -> serde_json::Result<(gts::GtsInstanceId, serde_json::Value)>;
}
// resulting id: "<P::TYPE_ID><segment>", e.g.
// gts.cf.toolkit.plugins.plugin.v1~cf.core.authn_resolver.plugin.v1~cf.builtin.static_authn_resolver.plugin.v1
```
Plugin config convention: `{ vendor: String (default "constructorfabric"), priority: i16 (default 100), ... }` with
`#[serde(default, deny_unknown_fields)]`. mini-chat's plugin config keys today (`config/mini-chat.yaml`):
- `static-mini-chat-audit-plugin`: `vendor`, `priority`, `enabled`.
- `static-mini-chat-model-policy-plugin`: `vendor`, `priority`, `model_catalog: [ModelCatalogEntry...]`.

Plugin crate layout used by both static plugins: `lib.rs` (`pub mod config; pub mod domain; pub mod gear; pub use gear::X;`),
`config.rs`, `domain/{mod.rs, service.rs, client.rs}`. Here `client.rs` holds `#[async_trait] impl PluginClientTrait for Service`
delegating to inherent methods, and tests go in `*_tests.rs` files included via `#[cfg(test)] #[path = "client_tests.rs"] mod client_tests;`.

### 3.3 Main gear side: lazy plugin resolution by vendor

Helpers in `toolkit::plugins` (`libs/toolkit/src/plugins/mod.rs`; `GtsPluginSelector` is also `toolkit::GtsPluginSelector`):
```rust
pub struct GtsPluginSelector;   // single-flight cached instance id
impl GtsPluginSelector {
    pub fn new() -> Self;
    pub fn pre_cached(value: String) -> Self;                 // tests
    pub async fn get_or_init<F, Fut, E>(&self, resolve: F) -> Result<Arc<str>, E>
        where F: FnOnce() -> Fut, Fut: Future<Output = Result<String, E>>;
    pub async fn reset(&self) -> bool;                        // drop cached id (e.g. client vanished)
}
pub fn choose_plugin_instance<'a, P>(vendor: &str, instances: impl IntoIterator<Item = (&'a str, &'a serde_json::Value)>)
    -> Result<String, ChoosePluginError>
    where P: for<'de> gts::GtsDeserialize<'de> + gts::GtsSchema;
pub enum ChoosePluginError { InvalidPluginInstance { gts_id: String, reason: String }, PluginNotFound { type_id: String, vendor: String } }
```
`choose_plugin_instance` deserializes each entry as `PluginV1<P>`, checks `content.id == gts_id`, filters by `vendor`, and returns the
id with the lowest `priority`.

Full selector (`gears/credstore/credstore/src/infra/plugin_select.rs`, verbatim minus comments):
```rust
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};

pub struct GtsCredStorePluginSelector { hub: Arc<ClientHub>, vendor: String, selector: GtsPluginSelector }

impl GtsCredStorePluginSelector {
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self { Self { hub, vendor, selector: GtsPluginSelector::new() } }

    async fn resolve_instance(&self) -> Result<String, DomainError> {
        let registry = self.hub.get::<dyn TypesRegistryClient>().map_err(|e| DomainError::ServiceUnavailable { /*...*/ })?;
        let type_id = CredStorePluginSpecV1::gts_type_id();
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await.map_err(|e| DomainError::ServiceUnavailable { /*...*/ })?;
        let gts_id = choose_plugin_instance::<CredStorePluginSpecV1>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),   // GtsInstance { id: GtsInstanceId, object: Value, .. }
        ).map_err(|e| match e {
            toolkit::plugins::ChoosePluginError::PluginNotFound { vendor, .. } => DomainError::ServiceUnavailable {
                detail: format!("no credstore plugin found for vendor '{vendor}'"), retry_after: None, cause: None },
            toolkit::plugins::ChoosePluginError::InvalidPluginInstance { gts_id, reason } => DomainError::Internal {
                diagnostic: format!("invalid credstore plugin instance '{gts_id}': {reason}"), cause: None },
        })?;
        Ok(gts_id)
    }
}

#[async_trait]
impl PluginSelector for GtsCredStorePluginSelector {      // domain port: async fn resolve(&self) -> Result<Arc<dyn Plugin>, DomainError>
    async fn resolve(&self) -> Result<Arc<dyn CredStorePluginClientV1>, DomainError> {
        let instance_id = self.selector.get_or_init(|| self.resolve_instance()).await?;
        let scope = ClientScope::gts_id(instance_id.as_ref());
        self.hub.try_get_scoped::<dyn CredStorePluginClientV1>(&scope)
            .ok_or_else(|| DomainError::ServiceUnavailable {
                detail: format!("credstore plugin client not registered yet for '{instance_id}'"), retry_after: None, cause: None })
    }
}
```
authn-resolver (`gears/system/authn-resolver/authn-resolver/src/domain/service.rs`) is the same, plus
`toolkit::telemetry::ThrottledLog::new(Duration::from_secs(10))` to rate-limit the "plugin client not registered yet" warning.

Rules:
- Resolve **lazily** (on first use), never in `init`. Plugin instances are registered in plugin `init`s, whose order relative to
  your gear is not guaranteed. The main gear does **not** list plugin gears in `deps`.
- The main gear takes `vendor` from its config (`mini-chat.config.vendor`, default `"constructorfabric"`). It must equal the plugin's `vendor`.
- DESIGN-specific audit semantics: "no plugin" is not cached (look up again on every delivery). A found id is cached. If the
  id resolves but the ClientHub has no client, call `selector.reset().await` and return Retry.

### 3.4 Several gears in one crate (main gear plus bundled static plugins)

There is no in-repo precedent with more than one gear per crate, but the macro supports it:
- Put each plugin gear in its own **public** module: `pub mod infra { pub mod plugins { pub mod static_audit; pub mod static_model_policy; } }`.
  Each module contains one `#[toolkit::gear(name = "static-mini-chat-audit-plugin", deps = [types_registry])] pub struct ...`
  (and likewise for `name = "static-mini-chat-model-policy-plugin"`), plus a `Default` impl, `impl Gear`, and a config struct.
- Struct names must differ, because the registrator fn is `__<snake_struct>_registrator`. Use one gear per module, because of the
  `_gear_dep_<dep>` re-export.
- The plugin gears register **scoped** clients for the SDK plugin traits exactly as in 3.2. The main gear resolves them as in 3.3.
  Plugins depend only on the SDK traits, never on main-gear internals, so they could be split out later.
- The example server links them with `use mini_chat::infra::plugins::static_audit as _;` (section 0).
- A plugin gear's `deps` cannot name its own crate (`::mini_chat` is not resolvable from inside the crate), and it does not need to.

---

## 4. ClientHub (`libs/toolkit/src/client_hub.rs`; `toolkit::ClientHub`, `toolkit::client_hub::{ClientHub, ClientScope, ClientHubError}`)

The key is `std::any::type_name::<T>()`, where `T` is usually `dyn Trait`. The value is an `Arc<T>`. Re-registering overwrites.
```rust
impl ClientHub {
    pub fn new() -> Self;                                                     // also Default
    pub fn register<T: ?Sized + Send + Sync + 'static>(&self, client: Arc<T>);
    pub fn get<T: ?Sized + ...>(&self) -> Result<Arc<T>, ClientHubError>;      // NotFound / TypeMismatch
    pub fn try_get<T>(&self) -> Option<Arc<T>>;
    pub fn try_get_local<T>(&self) -> Option<Arc<T>>;                         // ignores remote proxies
    pub fn register_scoped<T>(&self, scope: ClientScope, client: Arc<T>);
    pub fn get_scoped<T>(&self, scope: &ClientScope) -> Result<Arc<T>, ClientHubError>;   // ScopedNotFound
    pub fn try_get_scoped<T>(&self, scope: &ClientScope) -> Option<Arc<T>>;
    pub fn remove<T>(&self) -> Option<Arc<T>>;
    pub fn remove_scoped<T>(&self, scope: &ClientScope) -> Option<Arc<T>>;
}
impl ClientScope { pub fn new(s: impl Into<Arc<str>>) -> Self; pub fn gts_id(id: &str) -> Self /* "gts:"+id */; pub fn as_str(&self) -> &str; }
```
`ClientHubError` implements `std::error::Error`, so `?` into `anyhow` works:
`let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;`.

Clients mini-chat consumes (all registered by system gears before mini-chat `init`):
- `dyn types_registry_sdk::TypesRegistryClient`
- `dyn authn_resolver_sdk::AuthNResolverClient` (`authenticate`, `exchange_client_credentials(&ClientCredentialsRequest { client_id, client_secret: SecretString, scopes })`)
- `dyn authz_resolver_sdk::AuthZResolverApi`, wrapped as `PolicyEnforcer::new(arc)`; `PolicyEnforcer::from_hub(hub)` also exists
- `dyn oagw_sdk::api::ServiceGatewayClientV1` (registered by oagw)

mini-chat should register its own public SDK client the same way:
`ctx.client_hub().register::<dyn mini_chat_sdk::MiniChatClientV1>(Arc::new(LocalClient::new(svc)))`. That trait name is
illustrative; DE lint `de0504_client_versioning` expects a `V1` suffix.

Testing: register mocks under the same `dyn Trait` on a fresh `ClientHub::new()`, then build
`GearCtx::new("mini-chat", Uuid::new_v4(), Arc::new(TestConfigProvider{..}), hub, CancellationToken::new())`. `ConfigProvider` is
`toolkit::config::ConfigProvider { fn get_gear_config(&self, gear: &str) -> Option<&serde_json::Value>; }` and returns
`{"config": {...}}` per gear (pattern in `gears/system/api-gateway/tests/license_middleware.rs`).

---

## 5. Background tasks and shutdown

### 5.1 Recommended: `stateful` + `lifecycle(...)` + an entry method

```rust
#[toolkit::gear(name = "mini-chat", deps = [...], capabilities = [db, rest, stateful],
                lifecycle(entry = "serve", stop_timeout = "30s", await_ready))]
pub struct MiniChatGear { /* OnceLock<Arc<...>> fields filled in init */ }

impl MiniChatGear {
    pub(crate) async fn serve(self: Arc<Self>, cancel: CancellationToken, ready: toolkit::lifecycle::ReadySignal)
        -> anyhow::Result<()> { /* ... */ }
    // Without await_ready the signature is: async fn serve(self: Arc<Self>, cancel: CancellationToken) -> anyhow::Result<()>
}
```
The macro wraps the gear in `toolkit::lifecycle::WithLifecycle` (`libs/toolkit/src/lifecycle.rs`) and registers it as
`RunnableCapability`. On `start`, it spawns `serve(cancel_child, ready)`. With `await_ready`, start waits until you call `ready.notify()`
(`ReadySignal::notify(self)`). On `stop`, it cancels the token and waits up to `stop_timeout`, then aborts the task. `serve` runs in the **start**
phase, after REST registration. Two shapes exist in the repo:

- **Periodic loop inside `serve`** (credstore, §1.4): `tokio::select! { biased; () = cancel.cancelled() => break, _ = interval.tick() => ... }`.
- **Workers started in `init`, drained in `serve`** (types-registry, `gears/system/types-registry/types-registry/src/gear.rs`):
  ```rust
  // init: let handle = Outbox::builder(db.db()).table_prefix(PREFIX)?.profile(OutboxProfile::low_latency())
  //         .queue(QUEUE, Partitions::of(N)).leased(handler).start().await?;   *self.outbox.lock() = Some(handle);
  pub(crate) async fn serve(self: Arc<Self>, cancel: CancellationToken, ready: ReadySignal) -> anyhow::Result<()> {
      ready.notify();
      cancel.cancelled().await;
      let admission = self.outbox.lock().take();          // release the lock before awaiting
      if let Some(admission) = admission { admission.stop().await; }   // OutboxHandle::stop(self)
      Ok(())
  }
  ```
  Outbox API: `toolkit_db::outbox::{Outbox, OutboxHandle, OutboxProfile, Partitions, LeasedHandler, Batch, HandlerResult}`.
  `OutboxHandle::outbox() -> &Arc<Outbox>` is the enqueue entry point (`Outbox::enqueue(...)` inside your transaction, then `Wake::fire()` after commit,
  or use the `in_transaction` helper). `HandlerResult::Reject` dead-letters a message. SQLite is single-process only.
  Read `libs/toolkit-db/src/outbox/mod.rs` and `README.md` there before using it.

For several workers (orphan watchdog, upload reaper, thread-summary worker, outbox), spawn them from `serve` with child tokens
and join them on cancellation:
```rust
let mut set = tokio::task::JoinSet::new();
set.spawn(orphan_watchdog(svc.clone(), cancel.child_token()));
set.spawn(upload_reaper(svc.clone(), cancel.child_token()));
ready.notify();
cancel.cancelled().await;
while let Some(res) = set.join_next().await { if let Err(e) = res { tracing::warn!(error = %e, "worker join error"); } }
```
Every loop must observe its token (`select!` on `cancel.cancelled()`).

### 5.2 Alternative: implement `RunnableCapability` yourself (`capabilities = [stateful]` without `lifecycle`)

```rust
#[async_trait]
impl RunnableCapability for MyGear {
    async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()> { /* spawn tasks; return quickly */ Ok(()) }
    async fn stop(&self, deadline_token: CancellationToken) -> anyhow::Result<()> {
        // deadline_token is FRESH (not cancelled) => begin graceful stop; it is cancelled when the
        // runtime shutdown deadline (default 35s, `toolkit::DEFAULT_SHUTDOWN_DEADLINE`) expires => abort.
        Ok(())
    }
}
```
Other tools: `GearCtx::cancellation_token()` (child of the root token; usable for tasks spawned in `init`, though prefer `serve`),
`toolkit::lifecycle::Lifecycle` (`start`, `start_with_ready`, `stop(timeout)`, `status`) for hand-managed tasks, and `RestApiCapability::healthcheck`.
The k8s leader election in mini-chat is behind the `k8s` feature (`kube`, `k8s-openapi`). There is no toolkit helper for it.

---

## 6. Testing conventions

- Lints: the workspace denies clippy `pedantic`, `unwrap_used`/`expect_used` (see `[workspace.lints.clippy]`). Test files start with
  `#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]`. DE1101 (`dylint.toml`): more than 100 inline test lines
  must move into a separate `*_tests.rs` file, included with
  `#[cfg(test)] #[cfg_attr(coverage_nightly, coverage(off))] #[path = "x_tests.rs"] mod x_tests;`.
- **In-memory SQLite repo tests** (`gears/credstore/credstore/src/infra/storage/repo_impl/repo_tests.rs`):
  ```rust
  use sea_orm_migration::MigratorTrait;
  use toolkit_db::migration_runner::run_migrations_for_testing;
  use toolkit_db::{ConnectOpts, DBProvider, connect_db};
  async fn setup() -> SecretRepoImpl {
      let dsn = format!("sqlite:file:credstore_repo_{}?mode=memory&cache=shared", Uuid::new_v4()); // isolated per test
      let db = connect_db(&dsn, ConnectOpts { max_conns: Some(1), min_conns: Some(1), ..Default::default() }).await.expect("connect sqlite");
      run_migrations_for_testing(&db, Migrator::migrations()).await.expect("run migrations");
      SecretRepoImpl::new(Arc::new(DBProvider::<DomainError>::new(db)))
  }
  ```
  A bare `sqlite::memory:` needs `max_conns: Some(1)`, because each pooled connection otherwise gets its own empty DB. More than one
  connection needs the `sqlite:file:<unique>?mode=memory&cache=shared` form
  (`gears/system/types-registry/types-registry/tests/common/mod.rs`). `run_migrations_for_testing(&Db, Vec<Box<dyn MigrationTrait>>)`
  is public (no feature gate). Pass `MiniChatGear::default().migrations()` (or `Migrator::migrations()` plus outbox migrations).
  `toolkit_db::test_support::QueryRecorder` / `connect_with_recorder` (feature `test-support`) records SQL for DB-behavior audits.
- **Service and domain tests with fakes** (`gears/credstore/credstore/src/domain/secret/test_support.rs`, a `pub` module under
  `#[cfg(test)]`): `make_ctx(subject, tenant) -> SecurityContext`, `mock_enforcer()` (a PDP fake returning an `Eq` constraint on
  `pep_properties::OWNER_TENANT_ID`), `deny_enforcer()`, `FakeSecretRepo`, `FakePlugin`, `FakePluginSelector`, `FakeDir`, `FakeMetrics`.
  Domain ports are traits (`PluginSelector`, `SecretRepo`, `TenantDirectory`, metrics port), so tests inject fakes
  without ClientHub. Copy this structure into `mini_chat::...::test_support`.
- **types-registry mock**: `types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance, make_test_type_schema}`
  (feature `test-util`): `MockTypesRegistryClient::new().with_instances([make_test_instance(id, json!({...}))])`, with
  `.with_list_error(err)`, `.list_instance_calls()`, and `.received_instance_queries()`. `register*` with non-empty input **panics** in
  the mock. To test plugin resolution, put
  `PluginV1` JSON (`{"id": id, "vendor": "...", "priority": 100, "properties": {}}`) in the instances.
  `GtsPluginSelector::pre_cached(id)` skips resolution.
- **Plugin gear tests**: test `Service` and the `impl PluginClient for Service` directly
  (`static-authn-plugin/src/domain/{service_tests.rs, client_tests.rs}`, `static-credstore-plugin/src/config_tests.rs`, which
  parses YAML with `serde-saphyr`). Dev-deps: `tokio = { features = ["rt","macros"] }`, `serde-saphyr`.
- **Router tests**: see §2.13 (`OpenApiRegistryImpl::new()`, `tower::ServiceExt::oneshot`, inject `SecurityContext` into extensions).
- **Config tests**: `serde_json::from_str::<Cfg>(r#"{...}"#)` for partial and default cases, plus `validate()` rejection cases (credstore `config.rs`).
- Env-var expansion tests use the `temp-env` dev-dep (`temp_env::with_var(...)` / `async_with_vars`).
- Gear-name folder validation is `make validate-gear-names`. E2E tests (python, `testing/e2e`) run against the example server.

---

## 7. Skeleton for mini-chat (assembled from the patterns above)

```rust
// gears/mini-chat/mini-chat/src/lib.rs
pub mod api; pub mod config; pub mod domain; pub mod gear; pub mod infra;
pub use gear::MiniChatGear;
// infra/mod.rs must contain: pub mod plugins;  plugins/mod.rs: pub mod static_audit; pub mod static_model_policy;

// gears/mini-chat/mini-chat/src/gear.rs
#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct MiniChatGear { state: OnceLock<Arc<AppState>>, cfg: OnceLock<MiniChatConfig> }

#[async_trait::async_trait]
impl toolkit::Gear for MiniChatGear {
    async fn init(&self, ctx: &toolkit::GearCtx) -> anyhow::Result<()> {
        let cfg: MiniChatConfig = ctx.config_expanded_or_default()?;           // ${VAR} support
        cfg.validate().map_err(|e| anyhow::anyhow!("mini-chat config invalid: {e}"))?;
        let db = ctx.db_required()?;                                           // needs gears.mini-chat.database
        let hub = ctx.client_hub();
        let authz = authz_resolver_sdk::PolicyEnforcer::new(hub.get::<dyn authz_resolver_sdk::AuthZResolverApi>()?);
        let authn = hub.get::<dyn authn_resolver_sdk::AuthNResolverClient>()?;
        let oagw  = hub.get::<dyn oagw_sdk::api::ServiceGatewayClientV1>()?;
        let policy = Arc::new(GtsModelPolicySelector::new(Arc::clone(&hub), cfg.vendor.clone())); // lazy, §3.3
        let audit  = Arc::new(GtsAuditSelector::new(Arc::clone(&hub), cfg.vendor.clone()));
        // ... build services and outbox; store them in OnceLocks ...
        // hub.register::<dyn mini_chat_sdk::MiniChatClientV1>(Arc::new(LocalClient::new(svc)));
        Ok(())
    }
}
impl toolkit::contracts::DatabaseCapability for MiniChatGear { fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> { /* Migrator + outbox */ } }
impl toolkit::RestApiCapability for MiniChatGear {
    fn register_rest(&self, _ctx: &toolkit::GearCtx, router: axum::Router, openapi: &dyn toolkit::api::OpenApiRegistry)
        -> anyhow::Result<axum::Router> {
        let st = self.state.get().cloned().ok_or_else(|| anyhow::anyhow!("mini-chat not initialized"))?;
        Ok(crate::api::rest::routes::register_routes(router, openapi, st, &self.cfg.get().expect("init").url_prefix))
    }
}

// gears/mini-chat/mini-chat/src/infra/plugins/static_audit/mod.rs (or static_audit.rs)
#[toolkit::gear(name = "static-mini-chat-audit-plugin", deps = [types_registry])]
pub struct StaticMiniChatAuditPlugin { service: OnceLock<Arc<AuditService>> }
// init: cfg {vendor, priority, enabled}; PluginV1::<MiniChatAuditPluginSpecV1>::build_registration("cf.core._.static_mini_chat_audit.v1", ...)
//       -> registry.register(vec![json]) -> ensure_all_ok -> hub.register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(&id), svc)
```

Checklist of gotchas:
1. `deps` uses crate idents, not strings. Each dep must be a direct Cargo dep (already true for the four above).
2. `deny_unknown_fields` on every config struct, and match every key used in `config/*.yaml`.
3. Plugin spec structs need `#[derive(Default)]` for `build_registration`.
4. Resolve plugins lazily. Do not call them from `init`.
5. An authenticated route must call `.require_license_features(...)` or `.no_license_required()`, or `register` will not compile.
   To actually require the base license, pass `[License]`, not `[]`.
6. Use `toolkit::api::rest::extract::Json` for canonical 400/415/422 bodies, and declare `error_413/415/422`.
7. For 503, add `.response_header(Retry-After, Integer)` after `.error_503(openapi)` to match the contract.
8. Use `json_array_response_with_schema::<Item>` for top-level arrays, never `Vec<T>`. Watch schema name collisions (`#[schema(as = ...)]`).
9. Keep plugin modules `pub` at `mini_chat::infra::plugins::{static_audit, static_model_policy}`, one gear per module.
10. `ExpandVars` with a missing env var fails `init`, and `BTreeMap` has no `ExpandVars` impl (use `HashMap`).
