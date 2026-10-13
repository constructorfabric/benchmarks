# Research: Platform conventions for the `mini-chat` gear (REST, errors, OData, DB, outbox, tests)

Scope: everything a developer needs to build `gears/mini-chat/mini-chat` (package `cf-gears-mini-chat`,
lib `mini_chat`) on the Constructor Fabric "gears-rust" platform. Every snippet comes from source in
this repo; the path and line numbers are given so you can check them. "NOTE" marks traps that are
easy to miss.

Starting state of the crate: `gears/mini-chat/mini-chat/src/lib.rs` contains only empty
`pub mod infra { pub mod plugins { pub mod static_audit {} pub mod static_model_policy {} } }`.
`Cargo.toml` is already present (deps listed in section 6.4). `apps/cf-gears-example-server`
already references `mini_chat`, `mini_chat::infra::plugins::static_audit` and
`mini_chat::infra::plugins::static_model_policy` behind the `mini-chat` feature.

---

## 1. Gear anatomy and registration

### 1.1 The `#[toolkit::gear(...)]` attribute macro

Source: `libs/toolkit-macros/src/lib.rs` (`GearConfig` parse 19-416, macro body 502-848).

Accepted parameters (lines 222-398):

| param | form | notes |
|---|---|---|
| `name` | `name = "mini-chat"` | **required**, kebab-case only (no `_`, starts with a lowercase letter; `validate_kebab_case` 135-185). This is the runtime gear name and the YAML key under `gears:`. |
| `deps` | `deps = [types_registry, authz_resolver]` | **crate identifiers** (snake_case lib names). The runtime dependency name is the ident with `_` replaced by `-` (line 527-531). The macro emits `#[cfg(not(test))] pub use ::types_registry as _gear_dep_types_registry;` (797-807), so every dep **must be a direct Cargo dependency of the crate** and must also be listed in `[workspace.metadata.cargo-shear] ignored` (root `Cargo.toml` 742-754: already has `authn-resolver`, `authz-resolver`, `credstore`, `grpc_hub`, `oagw`, `tenant-resolver`, `types-registry`). |
| `capabilities` | `capabilities = [db, rest, stateful]` | allowed: `db`, `rest`, `rest_host`, `stateful`, `system`, `grpc_hub`, `grpc` (41-49). Compile-time asserts check that the matching trait is implemented (556-626). |
| `ctor` | `ctor = MyGear::new()` | optional Rust expression. Without it the macro uses `<T as Default>::default()` (534-540). |
| `client` | `client = path::Trait` | optional, compile-time object-safety check only (757-785). |
| `lifecycle(...)` | `lifecycle(entry = "serve", stop_timeout = "30s", await_ready)` | optional; generates `impl Runnable` that calls `self.serve(cancel[, ready])`, and wraps the gear in `WithLifecycle`, so you **do not** implement `RunnableCapability` yourself (638-747). |

Capability to trait (registration code 690-747, traits in `libs/toolkit/src/contracts.rs`):

| capability | trait you implement | signature |
|---|---|---|
| (always) | `toolkit::Gear` (contracts.rs:37) | `async fn init(&self, ctx: &toolkit::GearCtx) -> anyhow::Result<()>` |
| `db` | `toolkit::contracts::DatabaseCapability` (47) | `fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>>` |
| `rest` | `toolkit::RestApiCapability` (52) | `fn register_rest(&self, ctx: &GearCtx, router: axum::Router, openapi: &dyn OpenApiRegistry) -> anyhow::Result<axum::Router>`, plus optional `fn healthcheck(&self, ctx) -> Option<Arc<dyn toolkit::Healthcheck>>` |
| `stateful` | with `lifecycle(...)`: an inherent `async fn serve(self: Arc<Self>, cancel: CancellationToken[, ready: ReadySignal]) -> anyhow::Result<()>`; without it: `toolkit::contracts::RunnableCapability` (167): `async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()>` and `async fn stop(&self, deadline_token: CancellationToken) -> anyhow::Result<()>` |
| `system` | `toolkit::contracts::SystemCapability` (12), `pre_init`/`post_init` have defaults. System gears are initialised before non-system ones. |

The macro also generates `impl <Struct> { pub const MODULE_NAME: &'static str = "<name>"; }` (780-784)
and an `inventory::submit!` registrator called `__<struct_snake>_registrator` (749, 829-841). Two gears
in one crate never collide, because the registrator name comes from the struct name.

Runtime phase order (`libs/toolkit/src/runtime/host_runtime.rs:6-15`):
`pre_init` (system only) -> **DB migrations** -> `init` (topological order by deps) -> proxy-wiring ->
`post_init` (system only) -> **REST wiring** (`register_rest`) -> gRPC -> **start** (stateful) -> stop.
NOTE: migrations run **before** `init`, so `migrations()` must not depend on init state. Return a
static `Migrator::migrations()`. `register_rest` runs **after** `init`, so the service is usually kept
in a `OnceLock` that `init` fills.

### 1.2 Canonical example: credstore (system, db, rest, stateful)

`gears/credstore/credstore/src/gear.rs:37-205`:

```rust
use std::sync::{Arc, OnceLock};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::{DatabaseCapability, SystemCapability};
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::DBProvider;

#[toolkit::gear(
    name = "credstore",
    deps = [authz_resolver, tenant_resolver, types_registry],
    capabilities = [system, db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct CredStoreGear {
    service: OnceLock<Arc<Service>>,
}
impl Default for CredStoreGear { fn default() -> Self { Self { service: OnceLock::new() } } }

impl CredStoreGear {
    pub(crate) async fn serve(self: Arc<Self>, cancel: CancellationToken, ready: ReadySignal)
        -> anyhow::Result<()>
    {
        let Some(svc) = self.service.get().cloned() else { anyhow::bail!("serve invoked before init"); };
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
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: CredStoreConfig = ctx.config_or_default()?;            // gears.credstore.config
        cfg.validate().map_err(|err| anyhow::anyhow!("credstore config invalid: {err}"))?;
        let db_raw = ctx.db_required()?;                                 // DBProvider<DbError>
        let db: Arc<DBProvider<DomainError>> = Arc::new(DBProvider::new(db_raw.db()));
        let authz_client = ctx.client_hub().get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZResolverApi: {e}"))?;
        let enforcer = PolicyEnforcer::new(authz_client);
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
        -> anyhow::Result<axum::Router>
    {
        let svc = self.service.get().cloned()
            .ok_or_else(|| anyhow::anyhow!("credstore Service not initialized"))?;
        Ok(crate::api::rest::register_routes(router, openapi, svc))
    }
}
```

Other examples:
- `gears/system/types-registry/types-registry/src/gear.rs:31-37`: `capabilities = [system, db, rest, stateful]`, `lifecycle(entry = "serve", stop_timeout = "30s", await_ready)`. `serve` waits for cancellation, then drains an `OutboxHandle` (96-112). `migrations()` appends outbox migrations (277-297, see 5.6).
- `gears/system/oagw/oagw/src/gear.rs:41-45`: `name = "oagw", deps = [types_registry, authz_resolver, credstore, tenant_resolver], capabilities = [system, rest]`.
- `gears/system/license-resolver/license-resolver/src/gear.rs:21-25`: `deps = [types_registry], capabilities = [system]`.
- Direct `RunnableCapability` impl (no `lifecycle`): `libs/toolkit/tests/macro_tests.rs:74, 99-106`.

Suggested declaration for mini-chat. Its Cargo deps already include `types-registry`, `authz-resolver`, `authn-resolver` and `oagw`, which are all valid `deps` idents:

```rust
#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authz_resolver, authn_resolver, oagw],
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct MiniChatGear { /* OnceLocks for service, outbox handle Mutex<Option<OutboxHandle>>, ... */ }
```

### 1.3 `GearCtx` API (config, DB, ClientHub)

`libs/toolkit/src/context.rs`:

| method | line | notes |
|---|---|---|
| `ctx.config::<T>() -> Result<T, ConfigError>` | 278 | requires `gears.<name>.config` to be present |
| `ctx.config_or_default::<T: DeserializeOwned + Default>()` | 304 | lenient: missing gear/config gives `T::default()` |
| `ctx.config_expanded::<T: DeserializeOwned + ExpandVars>()` | 314 | expands `${VAR}` / `${VAR:-default}` in fields marked `#[expand_vars]`. An **unset** `${VAR}` with no default is an **error** (`libs/toolkit-utils/src/var_expand.rs:43-46`) |
| `ctx.config_expanded_or_default::<T>()` | 349 | same, lenient |
| `ctx.raw_config() -> &serde_json::Value` | 364 | |
| `ctx.db() -> Option<toolkit_db::DBProvider<toolkit_db::DbError>>` | 244 | `None` when no `database:` section is configured for the gear |
| `ctx.db_required() -> anyhow::Result<DBProvider<DbError>>` | 265 | |
| `ctx.client_hub() -> Arc<toolkit::ClientHub>` | 214 | |
| `ctx.cancellation_token() -> &CancellationToken` | 220 | root child token |
| `ctx.gear_name()`, `ctx.instance_id()` | 191, 201 | |
| `GearCtx::new(name, instance_id: Uuid, Arc<dyn ConfigProvider>, Arc<ClientHub>, CancellationToken)` + `.with_db(DBProvider<DbError>)` | 128, 182 | for tests (see 6.3) |

Config struct convention (`gears/credstore/credstore/src/config.rs:8-24`): `#[derive(Debug, Clone, Deserialize)] #[serde(default, deny_unknown_fields)]` plus a manual `impl Default`, and a `validate()` that `init` calls.
For env expansion (the mini-chat YAML uses `${AZURE_OPENAI_API_HOST}`), see `gears/credstore/plugins/static-credstore-plugin/src/config.rs:11-33`:

```rust
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct StaticCredStorePluginConfig {
    pub vendor: String,
    pub priority: i16,
    #[expand_vars]
    pub secrets: Vec<SecretConfig>,   // nested struct also derives ExpandVars
}
// in init:  let cfg: StaticCredStorePluginConfig = ctx.config_expanded_or_default()?;
```
`ExpandVars` is implemented for `String`, `Option<T>`, `Vec<T>`, `HashMap<K, V: ExpandVars>`, and `SecretString` (`libs/toolkit-utils/src/var_expand.rs:112-150`). The derive expands to `::toolkit::var_expand::...` (`libs/toolkit-macros/src/expand_vars.rs:40-52`).

NOTE (`deny_unknown_fields`): `config/mini-chat.yaml` `gears.mini-chat.config` has `vendor`, `orphan_watchdog`, `upload_reaper` (commented out), `client_credentials`, `thread_summary_worker` and `providers`. The model-policy plugin config has a large `model_catalog`. Your config structs must accept every key that appears there.

### 1.4 ClientHub

`libs/toolkit/src/client_hub.rs`:

```rust
pub fn register<T: ?Sized + Send + Sync + 'static>(&self, client: Arc<T>)                    // :152
pub fn register_scoped<T: ?Sized + ...>(&self, scope: ClientScope, client: Arc<T>)            // :165
pub fn get<T: ?Sized + ...>(&self) -> Result<Arc<T>, ClientHubError>                          // :182
pub fn try_get<T: ?Sized + ...>(&self) -> Option<Arc<T>>                                       // :206
pub fn get_scoped<T: ?Sized + ...>(&self, scope: &ClientScope) -> Result<Arc<T>, ClientHubError> // :273
pub fn try_get_scoped<T: ?Sized + ...>(&self, scope: &ClientScope) -> Option<Arc<T>>          // :300
impl ClientScope { pub fn new(scope: impl Into<Arc<str>>) -> Self /*:49*/; pub fn gts_id(gts_id: &str) -> Self /*:57*/ }
```
Usage: `ctx.client_hub().get::<dyn types_registry_sdk::TypesRegistryClient>()?`,
`ctx.client_hub().get::<dyn authz_resolver_sdk::AuthZResolverApi>()?`,
`ctx.client_hub().get::<dyn oagw_sdk::ServiceGatewayClientV1>()?`. Clients are registered by the
providing gear in its `init`, so they are available when your `init` runs, provided that gear is in
your `deps` or is a `system` gear.

### 1.5 Example server linkage, Cargo features, gear.toml

`apps/cf-gears-example-server/src/registered_gears.rs:31-38` (already present):
```rust
#[cfg(feature = "mini-chat")]
use mini_chat as _;
#[cfg(feature = "mini-chat")]
use mini_chat::infra::plugins::static_audit as _;
#[cfg(feature = "mini-chat")]
use mini_chat::infra::plugins::static_model_policy as _;
```
`apps/cf-gears-example-server/Cargo.toml`: `mini-chat = ["dep:mini-chat"]`, `k8s = ["mini-chat/k8s"]`, dependency
`mini-chat = { package = "cf-gears-mini-chat", path = "../../gears/mini-chat/mini-chat", optional = true }`.
So `static_audit` and `static_model_policy` must be **`pub mod`** paths that exist (any contents).
Linking the crate is enough to submit every `inventory` registrator in it.

Run command (header of `config/mini-chat.yaml`):
`cargo run --bin cf-gears-server --features mini-chat,static-authn,static-authz,single-tenant,static-credstore -- --config config/mini-chat.yaml run`.
NOTE: that YAML also configures `simple-user-settings`, `file-parser` and `resource-group`. Those gears
do not exist in this repo, and unknown YAML gear entries are just unused config.

`gears/mini-chat/gear.toml` already exists (`name = "MiniChat"`, `category = "gen-ai"`, `is_plugin = false`,
`has_plugins = true`, `has_extension_point = true`). It is catalog metadata only (compare
`gears/credstore/gear.toml`) and has no runtime effect.

### 1.6 Plugin gears inside the mini-chat crate (`static-mini-chat-audit-plugin`, `static-mini-chat-model-policy-plugin`)

Pattern to copy: `gears/credstore/plugins/static-credstore-plugin/src/gear.rs:20-80` and
`gears/system/authz-resolver/plugins/static-authz-plugin/src/gear.rs:17-74`. These are standalone
crates, but nothing in the macro requires that. A second and third `#[toolkit::gear]` struct in
`mini_chat::infra::plugins::{static_audit, static_model_policy}` works the same way.

Step 1: declare a plugin **spec type** (GTS schema chained under `PluginV1`) and a **client trait**.
Credstore keeps both in its SDK crate (`gears/credstore/credstore-sdk/src/gts.rs:30-38`,
`plugin_api.rs`). mini-chat has no SDK crate, so put them in e.g. `mini_chat::domain::ports` or
`mini_chat::plugin_api`:

```rust
// gears/credstore/credstore-sdk/src/gts.rs:17-38 (copy shape)
use toolkit::gts::PluginV1;                       // = toolkit_gts::PluginV1
use toolkit_gts::{gts_id, gts_type_schema};

#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~"),   // choose id
    description = "Mini-Chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;

// client trait (shape of gears/credstore/credstore-sdk/src/plugin_api.rs)
#[async_trait::async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    async fn emit(&self, event: AuditEvent) -> Result<(), AuditPluginError>;
}
```
NOTE (deps): upstream `gts_macros::struct_to_gts_schema` (wrapped by `gts_type_schema`) adds
`#[derive(schemars::JsonSchema)]` to the struct
(`/usr/local/cargo/registry/src/.../gts-macros-0.12.1/src/lib.rs:482-505`) and references `::gts::*`.
mini-chat `Cargo.toml` has `gts` and `toolkit-gts` but **not `schemars`**, so add
`schemars = { workspace = true }`, as SDK crates such as `authz-resolver-sdk` do. `dir_path` is metadata
only and writes no files at compile time. The macro also submits the schema to the link-time inventory,
which types-registry seeds at boot (`types-registry/src/gear.rs:156-170`), so you do **not** register
the type schema yourself. You only register the **instance**.

DESIGN names: `MiniChatAuditPluginSpecV1` / `MiniChatAuditPluginClientV1` and
`MiniChatModelPolicyPluginSpecV1` / `MiniChatModelPolicyPluginClientV1` (`gears/mini-chat/docs/DESIGN.md:1405-1407`).

Step 2: the plugin gear (copy of static-credstore-plugin `gear.rs:6-79`):

```rust
// src/infra/plugins/static_audit/mod.rs (or static_audit.rs)
use std::sync::{Arc, OnceLock};
use async_trait::async_trait;
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

#[toolkit::gear(name = "static-mini-chat-audit-plugin", deps = [types_registry])]
pub struct StaticMiniChatAuditPlugin { service: OnceLock<Arc<StaticAuditService>> }
impl Default for StaticMiniChatAuditPlugin { fn default() -> Self { Self { service: OnceLock::new() } } }

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticAuditPluginConfig { pub vendor: String, pub priority: i16, pub enabled: bool }
impl Default for StaticAuditPluginConfig {
    fn default() -> Self { Self { vendor: "constructorfabric".to_owned(), priority: 100, enabled: true } }
}

#[async_trait]
impl Gear for StaticMiniChatAuditPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticAuditPluginConfig = ctx.config_or_default()?;
        let service = Arc::new(StaticAuditService::new(cfg.enabled));
        // instance id = <SpecV1::TYPE_ID> + segment, e.g. "...~cf.core._.static_mini_chat_audit.v1"
        let (instance_id, instance_json) = PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
            "cf.core._.static_mini_chat_audit.v1", cfg.vendor.clone(), cfg.priority)?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;
        self.service.set(service.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        let api: Arc<dyn MiniChatAuditPluginClientV1> = service;
        ctx.client_hub().register_scoped::<dyn MiniChatAuditPluginClientV1>(
            ClientScope::gts_id(&instance_id), api);
        Ok(())
    }
}
```
`PluginV1::<P>::build_registration(instance_segment: &str, vendor: impl Into<String>, priority: i16)
-> serde_json::Result<(GtsInstanceId, serde_json::Value)>` is at `libs/toolkit-gts/src/plugin.rs:93-107`.
`P` must be `GtsSchema + GtsSerialize + Default`. Segments used elsewhere:
`"cf.core._.static_credstore.v1"` and `"cf.builtin.static_authz_resolver.plugin.v1"`.

Step 3: resolve the plugin from the main gear, lazily, by vendor and lowest priority
(`gears/credstore/credstore/src/infra/plugin_select.rs:19-103`):

```rust
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{GtsPluginSelector, choose_plugin_instance, ChoosePluginError};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};

pub struct AuditPluginSelector { hub: Arc<ClientHub>, vendor: String, selector: GtsPluginSelector }

async fn resolve_instance(&self) -> Result<String, DomainError> {
    let registry = self.hub.get::<dyn TypesRegistryClient>().map_err(/* -> ServiceUnavailable */)?;
    let type_id = MiniChatAuditPluginSpecV1::gts_type_id();
    let instances = registry
        .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*"))).await?;
    choose_plugin_instance::<MiniChatAuditPluginSpecV1>(
        &self.vendor, instances.iter().map(|e| (e.id.as_ref(), &e.object)))
        .map_err(|e| match e {
            ChoosePluginError::PluginNotFound { .. } => /* "no plugin" */,
            ChoosePluginError::InvalidPluginInstance { .. } => /* internal */,
        })
}
pub async fn resolve(&self) -> Result<Arc<dyn MiniChatAuditPluginClientV1>, DomainError> {
    let id = self.selector.get_or_init(|| self.resolve_instance()).await?;   // single-flight cache
    self.hub.try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(&id))
        .ok_or_else(|| /* ServiceUnavailable; also `self.selector.reset().await` to re-resolve */)
}
```
API: `GtsPluginSelector::new()`, `get_or_init(resolve) -> Result<Arc<str>, E>`, `reset().await -> bool`,
`pre_cached(String)` for tests (`libs/toolkit/src/plugins/mod.rs:14-100`).
`choose_plugin_instance::<P>(vendor, iter<(&str, &Value)>) -> Result<String, ChoosePluginError>`
filters by `vendor` and picks the **lowest** `priority` (`mod.rs:150-204`).
NOTE: the main gear cannot list its in-crate plugin gears in `deps` (deps must be external crates).
That is why resolution is **lazy** at first use rather than in `init`. The mini-chat DESIGN (line 415)
also requires that "no plugin" is not cached: call `selector.reset()` on failure, or only cache hits.

---

## 2. REST: `OperationBuilder`, extractors, SSE, multipart, throttling, licensing

### 2.1 Route registration via `toolkit::api::OperationBuilder`

`libs/toolkit/src/api/operation_builder.rs`. The type-state builder only allows `.register(router, openapi)`
(2167) once a handler, a response, the auth axis (`.authenticated()` / `.anonymous()`) **and** the license
axis (`.require_license_features(..)` / `.no_license_required()`) are all set.

Key methods (line numbers):
- constructors `get/post/put/delete/patch(path)` 734-758. **The path is used verbatim**: no prefix is derived
  from the gear name. Write `"/mini-chat/v1/chats/{id}"` (axum 0.8 `{param}` syntax; a literal segment
  like `messages:stream` is fine).
- `.operation_id(id)` 779, `.summary` 794, `.description` 800, `.tag` 806
- `.path_param(name, desc)` 818, `.query_param(name, required, desc)` 826,
  `.query_param_typed(name, required, desc, "integer")` 841, `.param(ParamSpec::header("If-Match").required(true))` 812
- `.json_request::<T>(openapi, desc)` 932 (T: `ToSchema + RequestApiDto`), `.request_optional()` 967
- `.multipart_file_request("file", Some("File to upload"))` 1011: sets the `multipart/form-data` body schema
  and the allowed content type. The OpenAPI output is
  `"File to upload (expects field 'file' with file data)"`, matching the contract.
- `.allow_content_types(&[...])` 1115: the gateway answers 415 for other content types
- `.exposed()` 1127: visibility for an **OoP edge proxy** only (`x-toolkit-visibility`). In-process,
  every registered route is served. The mini-chat contract has no `x-toolkit-visibility`, so do **not**
  call `.exposed()`.
- `.with_throttling(ThrottlingSpec {..})` 788
- `.authenticated()` 1255 / `.anonymous()` 1296 (the latter also sets the license axis)
- `.require_license_features::<L>(iter)` 1150 / `.no_license_required()` 1183
- `.handler(fn)` 1340, or `.method_router(MethodRouter)` 1367 to attach per-route layers
- responses: `.json_response(status, desc)` 1404, `.json_response_with_schema::<T>(openapi, status, desc)` 1458
  (T: `ToSchema + ResponseApiDto`), `.json_array_response_with_schema::<T>` 1497, `.no_content_response(status, desc)` 1434,
  `.problem_response(openapi, status, desc)` 1586, `.sse_json::<T>(openapi, desc)` 1615 (200 `text/event-stream`),
  `.response_header(ResponseHeaderSpec::new("Location", "Path of the created chat", ResponseHeaderType::String))` 1695
  (attaches to the **last** response)
- errors: `.error_400/401/403/404/409/413/415/422/429/500/502/503/504(openapi)` 2030-2120 with descriptions
  "Bad Request", "Unauthorized", "Forbidden", "Not Found", "Conflict", "Payload Too Large", "Unsupported Media Type",
  "Unprocessable Entity", "Too Many Requests", "Internal Server Error", "Bad Gateway", "Service Unavailable", "Gateway Timeout"
  (all `application/problem+json`, schema `Problem`). `.standard_errors(openapi)` 1947 adds 400,401,403,404,409,429,500.
  The mini-chat contract lists exact per-route status sets (e.g. create_chat: 201,400,401,403,422,500,503), so use
  individual `error_*` calls.
- OData (trait `toolkit::api::operation_builder::OperationBuilderODataExt`, 551-664):
  `.with_odata_filter::<F: FilterField>()`, `.with_odata_orderby::<F>()`, `.with_odata_select()`.

Exports: `toolkit::api::{OperationBuilder, OpenApiRegistry, OpenApiRegistryImpl, ParamSpec, ResponseHeaderSpec,
ResponseHeaderType, ThrottlingSpec}` (`libs/toolkit/src/api/mod.rs:25-29`);
`toolkit::api::operation_builder::{LicenseFeature, OperationBuilderODataExt, CORE_GLOBAL_BASE_LICENSE_FEATURE}`.

Full credstore example (`gears/credstore/credstore/src/api/rest/routes.rs:31-133`):

```rust
use axum::Router;
use axum::http::StatusCode;
use toolkit::api::{OpenApiRegistry, OperationBuilder, ParamSpec};

pub fn register_routes(router: Router, openapi: &dyn OpenApiRegistry, svc: Arc<Service>) -> Router {
    let router = OperationBuilder::post("/credstore/v1/secrets")
        .operation_id("credstore.create_secret")
        .summary("Create a secret")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<CreateSecretRequestDto>(openapi, "Secret reference, value, and sharing mode")
        .handler(handlers::create_secret)
        .no_content_response(StatusCode::CREATED, "Secret created (see Location header)")
        .error_400(openapi).error_401(openapi).error_403(openapi)
        .error_409(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);
    let router = OperationBuilder::get("/credstore/v1/secrets/{ref}")
        .operation_id("credstore.get_secret")
        .authenticated().no_license_required()
        .path_param("ref", "Secret reference")
        .handler(handlers::get_secret)
        .json_response_with_schema::<GetSecretResponseDto>(openapi, StatusCode::OK, "Resolved secret")
        .error_400(openapi).error_401(openapi).error_403(openapi).error_404(openapi)
        .error_500(openapi).error_503(openapi)
        .register(router, openapi);
    router.layer(axum::Extension(svc))        // services are injected as Extension<Arc<..>>
}
```

### 2.2 License requirement

`gears/system/types-registry/types-registry/src/api/rest/routes.rs:26-34, 111-113`:
```rust
use toolkit::api::operation_builder::{CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature};
struct License;
impl AsRef<str> for License { fn as_ref(&self) -> &'static str { CORE_GLOBAL_BASE_LICENSE_FEATURE } }
impl LicenseFeature for License {}
// ...
    .authenticated()
    .require_license_features::<License>([])         // [] = none; [License] = base feature
```
`CORE_GLOBAL_BASE_LICENSE_FEATURE = gts_id!("cf.core.lic.feat.v1~cf.core.global.base.v1")` (`operation_builder.rs:66`).
The gateway middleware (`gears/system/api-gateway/src/middleware/license_validation.rs:48-77`) is a stub:
any required feature **other than** the base feature gives 403 `permission_denied`, reason `LICENSE_FEATURE_REQUIRED`.
For mini-chat (DESIGN line 261, spec line 258) use `.require_license_features::<License>([License])` with the base feature.

### 2.3 Rate-limit / in-flight zones

`ThrottlingSpec` (`operation_builder.rs:517-541`):
```rust
pub struct ThrottlingSpec {
    pub rate_limit_zone: Option<String>,
    pub in_flight_limit_zone: Option<String>,
    pub require_security_context: bool,   // false = pre-auth, IP-keyed zones only
    pub dry_run: bool,
}
```
Usage for create_chat. The zones in `config/mini-chat.yaml` are `key: { type: ip }`, so
`require_security_context: false`:
```rust
OperationBuilder::post("/mini-chat/v1/chats")
    .with_throttling(ThrottlingSpec {
        rate_limit_zone: Some("rl_mini_chat_chat".to_owned()),
        in_flight_limit_zone: Some("ifl_mini_chat_chat".to_owned()),
        require_security_context: false,
        dry_run: false,
    })
```
Gateway validation (`gears/system/api-gateway/src/middleware/throttling.rs:469-526, 567-598`): an
undefined zone name fails **gateway startup** ("references undefined rate_limit zone"). An identity-keyed
zone requires `require_security_context: true`, an authenticated route and `auth_disabled=false`. The
gateway adds `x-rate-limit-rps`/`x-rate-limit-burst`/`x-in-flight-limit` to the OpenAPI document from
zone config. The OpenAPI vendor keys emitted are `x-throttling-rate-limit-zone` and
`x-throttling-in-flight-limit-zone` (`libs/toolkit/src/api/openapi_registry.rs:140-157`); both appear in
the contract for `POST /mini-chat/v1/chats`.

### 2.4 Handlers: `SecurityContext`, extractors, responses

`SecurityContext` is inserted into request extensions by the gateway auth middleware. With
`auth_disabled: true` a default context is used: `subject_id = DEFAULT_SUBJECT_ID`,
`subject_tenant_id = DEFAULT_TENANT_ID = 00000000-df51-5b42-9538-d2b56b7ee953`
(`gears/system/api-gateway/src/gear.rs:463-484`, `libs/toolkit-security/src/constants.rs:12`).
Handlers take `Extension(ctx): Extension<toolkit_security::SecurityContext>`. Accessors:
`ctx.subject_id() -> Uuid`, `ctx.subject_tenant_id() -> Uuid`, `ctx.subject_type()`, `ctx.token_scopes()`,
`ctx.bearer_token()`. Builder: `SecurityContext::builder().subject_id(u).subject_tenant_id(t).build()?`
(`libs/toolkit-security/src/context.rs:52-200`).

Canonical prelude (`libs/toolkit/src/api/mod.rs:33-54`):
```rust
use toolkit::api::canonical_prelude::*;
// brings: CanonicalError, Problem, resource_error, ApiResult<T = ()> = Result<T, CanonicalError>,
//         OData, JsonBody, JsonPage, created_json, no_content, ok_json, extract, apply_select,
//         axum::{Json, http::StatusCode, response::IntoResponse}
```

**Use the canonical extractors** `toolkit::api::rest::extract::{Json, Path, Query}`
(`libs/toolkit/src/api/rest/extract/mod.rs:28-30`). Axum's own extractors give plain-text rejections.
These ones reject with a `CanonicalError` (problem+json), `resource_type = "gts.cf.core.http.request.v1~"`
(`extract/error.rs:13-14`):

| case | status | `field_violations[0]` |
|---|---|---|
| malformed JSON (`JsonSyntaxError`) | 400 | `{field:"body", reason:"json_syntax_error"}` |
| schema/type mismatch, missing field, unknown field under `deny_unknown_fields` (`JsonDataError`) | 422 | `{field:"body", reason:"invalid_json_body"}` |
| missing/wrong `Content-Type` | 415 | `{field:"body", reason:"missing_json_content_type"}` |
| body over `DefaultBodyLimit` (`BytesRejection`) | 413 | `{field:"body", reason:"json_body_read_error"}` |
| `Path<Uuid>` not parseable | 400 | `{field:"path", reason:"invalid_path_params"}` (route-definition bugs give 500 `internal`) |
| `Query<T>` not parseable | 400 | `{field:"query", reason:"invalid_query_string"}` |

(`extract/error.rs:33-51`, `path.rs:55-71`, `query.rs:47-61`; status = axum's own rejection status;
`detail = "Request validation failed"`; description = axum text, capped at 500 chars.)
Exact wire JSON (`libs/toolkit/src/api/rest/extract/json.rs:145-170`):
```json
{"type":"gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~","title":"Invalid Argument",
 "status":400,"detail":"Request validation failed",
 "context":{"resource_type":"gts.cf.core.http.request.v1~",
            "field_violations":[{"field":"body","description":"Failed to parse the request body as JSON: key must be a string at line 1 column 2","reason":"json_syntax_error"}]}}
```
When served through the api-gateway, `canonical_error_middleware` (`libs/toolkit/src/api/canonical_error_layer.rs:111`,
layered at `api-gateway/src/gear.rs:554`) adds `instance` (request path) and `trace_id`. In unit tests on a bare
router those two fields are absent.

Handler shape (types-registry `gears/system/types-registry/types-registry/src/api/rest/handlers.rs:12, 41, 109`
uses `extract::Json(req): extract::Json<RegisterEntitiesRequest>` and `extract::Path(gts_id): extract::Path<String>`):
```rust
use std::sync::Arc;
use axum::extract::Extension;
use axum::http::{StatusCode, Uri, header};
use axum::response::IntoResponse;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;

pub async fn create_chat(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ChatService>>,
    extract::Json(body): extract::Json<CreateChatReq>,
) -> ApiResult<impl IntoResponse> {
    let chat = svc.create_chat(&ctx, body.into()).await?;      // DomainError -> CanonicalError via From
    let dto = ChatDetailDto::from(chat);
    Ok(created_json(dto, &uri, &chat_id.to_string()))          // 201 + Location: {uri.path()}/{id}
}

pub async fn delete_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ChatService>>,
    extract::Path(id): extract::Path<uuid::Uuid>,
) -> ApiResult<impl IntoResponse> {
    svc.delete_chat(&ctx, id).await?;
    Ok(no_content())                                           // 204
}
// Multi-param path:  extract::Path((id, msg_id)): extract::Path<(Uuid, Uuid)>
```
`created_json(value, &uri, new_id)` builds `Location = uri.path().trim_end_matches('/') + "/" + new_id`
(`libs/toolkit/src/api/response.rs:17-28`). Credstore builds the header by hand
(`handlers.rs:153-158`): `Ok((StatusCode::CREATED, [(axum::http::header::LOCATION, location)]).into_response())`.

DTOs: `#[toolkit_macros::api_dto(request)]` / `(response)` / `(request, response)` derives
`Serialize`/`Deserialize` + `utoipa::ToSchema`, adds `#[serde(rename_all = "snake_case")]` and implements the
`RequestApiDto`/`ResponseApiDto` markers needed by `json_request`/`json_response_with_schema`
(`libs/toolkit-macros/src/api_dto.rs:42-100`; doc `lib.rs:1190-1228`). You may add `#[serde(deny_unknown_fields)]`
etc. on top. Marker impls already exist for `Vec<T>`, `Option<T>`, `Box<T>`, `toolkit_odata::Page<T>`,
`serde_json::Value`, `Problem` (`libs/toolkit/src/api/api_dto.rs:26-41`). So
`.json_response_with_schema::<toolkit_odata::Page<ChatDto>>(...)` works and names the schema `Page_ChatDto`
(`libs/toolkit-odata/src/page.rs:53-60`). This needs the `toolkit-odata` feature `with-utoipa`, which mini-chat
already enables.

### 2.5 Multipart uploads

The toolkit has **no** multipart request extractor. `libs/toolkit/src/http/multipart.rs` covers only `multipart/mixed`
*responses*. mini-chat already enables `axum = { features = ["macros", "multipart"] }` and depends on `multer = "3.1"`.
Pattern:
```rust
use axum::extract::{Multipart, multipart::MultipartRejection};
pub async fn upload_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AttachmentService>>,
    extract::Path(chat_id): extract::Path<Uuid>,
    multipart: Result<Multipart, MultipartRejection>,      // map rejection yourself to CanonicalError
) -> ApiResult<impl IntoResponse> {
    let mut mp = multipart.map_err(|e| AttachmentResource::invalid_argument()
        .with_field_violation("file", e.body_text(), "INVALID_MULTIPART").create())?;
    while let Some(field) = mp.next_field().await.map_err(/* ... */)? {
        if field.name() == Some("file") { let filename = field.file_name().map(str::to_owned);
            let ct = field.content_type().map(str::to_owned); let bytes = field.bytes().await?; /* ... */ }
    }
    // ...
}
```
OpenAPI: `.multipart_file_request("file", Some("File to upload"))`. NOTE: axum's `Multipart` is bounded by
`DefaultBodyLimit` (2 MB by default in a bare test router). In production the gateway applies
`DefaultBodyLimit::max(config.defaults.body_limit_bytes)` and `RequestBodyLimitLayer` globally
(`api-gateway/src/gear.rs:533-534`). For large-file tests add `.layer(axum::extract::DefaultBodyLimit::max(n))`.

### 2.6 SSE (`text/event-stream`) responses

The platform has no per-request SSE helper with multiple event types. What exists:
- `toolkit::SseBroadcaster<T>` (`libs/toolkit/src/http/sse.rs:13-140`): a broadcast fan-out with a 15 s keepalive
  (`KeepAlive::new().interval(15s).text("keepalive")`). It suits pub/sub, not per-request streams.
- `oagw_sdk::ServerEventsStream` (`gears/system/oagw/oagw-sdk/src/sse/stream.rs:56-160`) **parses** upstream SSE from
  an OAGW proxy response: `ServerEventsStream::from_response::<ServerEvent>(resp)` returns
  `ServerEventsResponse::{Events(stream), Response(resp)}`, and `ServerEvent { id, event, data, retry }` has `.json::<T>()`.
  `oagw_sdk::sse::server_events_response(...)` (`sse/response.rs:18-34`) sets `Content-Type: text/event-stream`,
  `Cache-Control: no-cache`, `Connection: keep-alive`, `X-Accel-Buffering: no`.

So build a mini-chat stream with axum directly (axum 0.8; see the `Sse::new(...)` usages in
`libs/toolkit-contract/tests/rest_client_codegen.rs:202-262`):
```rust
use std::convert::Infallible;
use axum::response::{IntoResponse, Response};
use axum::response::sse::{Event, KeepAlive, Sse};

pub async fn stream_message(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<StreamService>>,
    extract::Path(chat_id): extract::Path<Uuid>,
    extract::Json(req): extract::Json<StreamMessageReq>,
) -> ApiResult<Response> {
    // Pre-stream failures (authz, 404, quota, validation, 409 running turn) -> Err(CanonicalError) -> JSON problem.
    let turn = svc.start_turn(&ctx, chat_id, req).await?;
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
    tokio::spawn(async move {
        // Event::default().event("delta").json_data(&payload)  -> Result<Event, axum::Error>
        // a closed `tx` (send() Err) means the client disconnected -> cancel provider stream
        turn.run(tx).await;
    });
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|ev| (Ok::<Event, Infallible>(ev), rx))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()).into_response())
}
```
OpenAPI: `.sse_json::<MiniChatSseEvent>(openapi, "SSE stream of chat response events")`, which declares a 200
`text/event-stream` response with `$ref MiniChatSseEvent` (contract `POST /mini-chat/v1/chats/{id}/messages:stream`).
Gateway notes: the 30 s `tower` timeout (`api-gateway/src/gear.rs:536-543`) only bounds producing the response head.
`canonical_error_middleware` passes 2xx responses through untouched, so the body is not buffered.
In-band errors after the 200 must be SSE `error` events, not HTTP statuses.

### 2.7 OpenAPI schema registration

`json_request::<T>`, `json_response_with_schema::<T>` and `sse_json::<T>` call
`toolkit::api::ensure_schema::<T>(registry)` (`openapi_registry.rs:~90-108`), which registers `T` and every schema
it references (`ToSchema::schemas`). Enums used in DTOs need `#[derive(utoipa::ToSchema)]`. The `Problem` component
comes from `toolkit_canonical_errors::Problem: ToSchema` (feature `utoipa`, enabled transitively by toolkit).

---

## 3. Canonical errors (`libs/toolkit-canonical-errors`)

### 3.1 Declaring a resource type: `#[resource_error(...)]`

`libs/toolkit-canonical-errors-macro/src/lib.rs:78-305`. It works only on a unit struct. The argument is a string
literal or `gts_id!("<suffix>")` (the `gts.` prefix is prepended). It is validated at compile time as a GTS
**type** id, which must end with `~`. Requires `toolkit-canonical-errors` as a **direct** dependency
(mini-chat has it with `features = ["axum"]`).
```rust
use toolkit_canonical_errors::{CanonicalError, resource_error};
#[resource_error(gts_id!("cf.core.mini_chat.chat.v1~"))]       pub struct ChatResource;
#[resource_error(gts_id!("cf.core.mini_chat.message.v1~"))]    pub struct MessageResource;
#[resource_error(gts_id!("cf.core.mini_chat.turn.v1~"))]       pub struct TurnResource;
#[resource_error(gts_id!("cf.core.mini_chat.attachment.v1~"))] pub struct AttachmentResource;
#[resource_error(gts_id!("cf.core.mini_chat.model.v1~"))]      pub struct ModelResource;
```
(ADR-0004 line 44 names exactly `gts.cf.core.mini_chat.{chat,message,turn,attachment,model}.v1~` for `not_found`.)

Generated constructors (macro lines 168-288). Typestate: you **must** finish each chain to reach `.create()`:

| constructor | required next step(s) | category / HTTP |
|---|---|---|
| `X::not_found(detail)` | `.with_resource(name)` (mandatory) | not_found / 404 |
| `X::already_exists(detail)` | `.with_resource(name)` (mandatory) | already_exists / 409 |
| `X::data_loss(detail)` | `.with_resource(name)` | data_loss / 500 |
| `X::aborted(detail)` | `.with_reason("REASON")` (mandatory), `.with_resource` optional | aborted / 409 |
| `X::unknown(detail)` / `X::deadline_exceeded(detail)` / `X::unimplemented(detail)` | none | 500 / 504 / 501 |
| `X::permission_denied()` | `.with_reason("REASON")` | permission_denied / 403, detail fixed "You do not have permission to perform this operation" |
| `X::cancelled()` | none | 499 |
| `X::invalid_argument()` | `.with_field_violation(field, description, reason)` (repeatable) **or** `.with_format(msg)` **or** `.with_constraint(msg)` | invalid_argument / 400, detail "Request validation failed" (format/constraint: detail = msg) |
| `X::out_of_range(detail)` | `.with_field_violation(..)` | out_of_range / 400 |
| `X::resource_exhausted(detail)` | `.with_quota_violation(subject, description)` (repeatable), `.with_quota_violation_retry_after_seconds(s)` | resource_exhausted / 429 |
| `X::failed_precondition()` | `.with_precondition_violation(subject, description, type_)` (repeatable) | failed_precondition / 400, detail "Operation precondition not met" |

Non-resource constructors on `CanonicalError` (`libs/toolkit-canonical-errors/src/builder.rs:614-650, 753-834`):
`CanonicalError::internal(diagnostic).create()` (wire detail is always "An internal error occurred. Please retry later.";
the diagnostic is logged only, `context: {}`), `CanonicalError::service_unavailable().with_retry_after_seconds(30).with_detail("..").create()`
(503 + **`Retry-After` header**, `problem.rs:713-745`), `CanonicalError::unauthenticated().with_reason("..").create()` (401).
Any builder can take `.with_override(toolkit_canonical_errors::Http::status_code(422))`. The status must stay in the same class (4xx/5xx)
(`builder.rs:442-450`, `transport.rs`).

### 3.2 Wire format (RFC 9457 Problem)

`Problem` (`problem.rs:236-263`): `{type, title, status, detail, instance?, trace_id?, context, error_code?, error_domain?}`.
`type = "gts://gts.cf.core.errors.err.v1~cf.core.err.<category>.v1~"`. `context` is the category payload plus
`resource_type` / `resource_name` (`problem.rs:273-298`). Content-Type is `application/problem+json`.
Titles come from `error.rs:655-674`: "Invalid Argument", "Not Found", "Already Exists", "Permission Denied",
"Resource Exhausted", "Failed Precondition", "Aborted", "Out of Range", "Internal", "Service Unavailable",
"Unauthenticated", "Deadline Exceeded", "Unimplemented", "Cancelled", "Data Loss", "Unknown".

Exact `context` shapes, from `libs/toolkit-canonical-errors/tests/showcase.rs`:
```jsonc
// not_found (29-49)
{"type":"gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~","title":"Not Found","status":404,
 "detail":"User not found","context":{"resource_type":"gts.cf.core.users.user.v1~","resource_name":"user-123"}}
// invalid_argument (98-131)
"context":{"resource_type":"…","field_violations":[{"field":"email","description":"…","reason":"INVALID_FORMAT"}]}
// invalid_argument with_format / with_constraint (134-179)
"context":{"resource_type":"…","format":"Request body is not valid JSON"}   |   "context":{"resource_type":"…","constraint":"…"}
// out_of_range (208-238)
"context":{"resource_type":"…","field_violations":[{"field":"page","description":"…","reason":"OUT_OF_RANGE"}]}
// permission_denied (241-261)
"context":{"resource_type":"…","reason":"CROSS_TENANT_ACCESS"}
// aborted (264-284)  status 409
"context":{"resource_type":"…","reason":"OPTIMISTIC_LOCK_FAILURE"}
// failed_precondition (307-337)  status 400
"context":{"resource_type":"…","violations":[{"type":"STATE","subject":"tenant.users","description":"…"}]}
// resource_exhausted (424-452)  status 429
"context":{"resource_type":"…","violations":[{"subject":"requests_per_minute","description":"…"}]}   // + "retry_after_seconds" per violation if set
// service_unavailable (455-473)  status 503, no resource_type
"context":{"retry_after_seconds":30}
// internal (340-356)
"context":{}
// unauthenticated (403-421)
"context":{"reason":"TOKEN_EXPIRED"}
```
Context structs: `libs/toolkit-canonical-errors/src/context.rs` (`FieldViolationV1{field,description,reason}`,
`QuotaViolationV1{subject,description,retry_after_seconds?}`, `PreconditionViolationV1{type,subject,description}`,
`InvalidArgumentV1` is untagged `FieldViolations{field_violations}|Format{format}|Constraint{constraint}`).

### 3.3 axum integration and the per-gear mapping

With feature `axum`: `impl IntoResponse for CanonicalError` (`problem.rs:779-793`) renders the Problem and stores the
original error in response extensions so the middleware can log `diagnostic()`. Handlers return
`ApiResult<T> = Result<T, CanonicalError>`, and `?` works through `impl From<DomainError> for CanonicalError`.
Example: `gears/credstore/credstore/src/infra/sdk_error_mapping.rs:11-79`:
```rust
#[resource_error(gts_id!("cf.core.credstore.secret.v1~"))]
pub(crate) struct SecretResource;

impl From<DomainError> for CanonicalError {
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::InvalidSecretRef { detail } => SecretResource::invalid_argument()
                .with_field_violation("reference", detail, "INVALID_SECRET_REF").create(),
            DomainError::NotFound => SecretResource::not_found("secret not found").with_resource("secret").create(),
            DomainError::Conflict => SecretResource::already_exists("secret already exists").with_resource("secret").create(),
            DomainError::VersionConflict => SecretResource::aborted("secret version precondition failed")
                .with_reason("OPTIMISTIC_LOCK_FAILURE").create(),
            DomainError::UnsupportedTransition { detail } => SecretResource::failed_precondition()
                .with_precondition_violation("sharing", detail, "UNSUPPORTED_TRANSITION").create(),
            DomainError::AccessDenied { .. } => SecretResource::permission_denied().with_reason("ACCESS_DENIED").create(),
            DomainError::ServiceUnavailable { detail, retry_after, .. } => {
                let mut b = CanonicalError::service_unavailable().with_detail(detail);
                if let Some(d) = retry_after { b = b.with_retry_after_seconds(d.as_secs()); }
                b.create()
            }
            DomainError::Internal { diagnostic, .. } => CanonicalError::internal(diagnostic).create(),
        }
    }
}
```
DB error classification example: `gears/credstore/credstore/src/infra/canonical_mapping.rs:12-56`
(`impl From<toolkit_db::DbError> for DomainError`; `DbError::Sea(DbErr)` holds the SeaORM error;
`toolkit_db::secure::is_unique_violation(&DbErr)` is exported).

---

## 4. OData pagination

### 4.1 Extractor: `toolkit::api::odata::OData`

`libs/toolkit/src/api/odata.rs`. `pub struct OData(pub ODataQuery)` (438) implements `FromRequestParts` with
`Rejection = CanonicalError`. It derefs to `ODataQuery`. Query parameters bound (`ODataParams` 18-44): `$filter`,
`$orderby`, `$select`, `limit` (alias `$top`), `cursor` (alias `$skiptoken`).
Behaviour (`extract_odata_query` 334-435):
- any other `$`-prefixed key: 400, `UNSUPPORTED_QUERY_PARAM` (one violation per key, field = the key), e.g. `$skip`, `$count`
- query string not parseable / both `limit` and `$top`: 400 `INVALID_QUERY_PARAMS` (field `query`)
- `$filter` longer than 8 KiB: `FILTER_TOO_LONG`; more than 2000 nodes: `FILTER_TOO_COMPLEX`; parse error: `INVALID_FILTER` (field `$filter`)
- `cursor` together with `$orderby`: `ORDER_WITH_CURSOR` (two violations: `$orderby` and `cursor`)
- cursor not decodable: `INVALID_CURSOR` (field `cursor`)
- `$orderby` syntax error, or more than 10 fields: `INVALID_ORDERBY_FIELD`
- `limit=0`: `INVALID_LIMIT` (field `$top`). Other limits are **not** clamped here; the repository clamps them.
- `$select`: `INVALID_SELECT`
All of these use `resource_type = "gts.cf.core.odata.query.v1~"` (`libs/toolkit-odata/src/errors.rs`:
`#[resource_error(gts_id!("cf.core.odata.query.v1~"))] pub struct OdataError;`). The full mapping
`impl From<toolkit_odata::Error> for CanonicalError` is in `libs/toolkit-odata/src/problem_mapping.rs:15-140`:
`InvalidFilter` -> `$filter`/`INVALID_FILTER`; `InvalidOrderByField` -> `$orderby`/`INVALID_ORDERBY_FIELD`;
`InvalidCursor` and `CursorInvalid*` -> `cursor`/`INVALID_CURSOR`; `OrderMismatch` -> `cursor`/`ORDER_MISMATCH`;
`FilterMismatch` -> `cursor`/`FILTER_MISMATCH`; `InvalidLimit` -> `$top`/`INVALID_LIMIT`; `OrderWithCursor` -> two violations;
`Db(_)` -> internal. So the repository's `toolkit_odata::Error` maps to the same contract with a plain `CanonicalError::from(e)`.

`ODataQuery` (`libs/toolkit-odata/src/lib.rs` ~472-545): `filter: Option<Box<ast::Expr>>`, `order: ODataOrderBy`, `limit: Option<u64>`,
`cursor: Option<CursorV1>`, `filter_hash`, `select`; builders `ODataQuery::new().with_filter(expr).with_order(..).with_limit(n)`;
`toolkit_odata::parse_filter_string(raw) -> Result<ParsedFilter, Error>` then `.into_expr()`.

`Page<T>` / `PageInfo` (`libs/toolkit-odata/src/page.rs:3-15`, re-exported as `toolkit::{Page, PageInfo}`):
```json
{"items":[...],"page_info":{"next_cursor":"<b64url>|null","prev_cursor":"<b64url>|null","limit":20}}
```

### 4.2 Filterable field enum (`FilterField`)

Trait (`libs/toolkit-odata/src/filter.rs:38-56`): `const FIELDS: &'static [Self]; fn name(&self) -> &'static str;
fn kind(&self) -> FieldKind; fn nullable(&self) -> bool { false }` (+ `from_name`, case-insensitive).
`FieldKind` = `String | I64 | F64 | Bool | Uuid | DateTimeUtc | Date | Time | Decimal` (10-20). Allowed operators per kind come
from `kind.allows(op)`, and `with_odata_filter::<F>()` publishes them. The contract for `GET /mini-chat/v1/chats` expects
`updated_at: eq|ne|gt|ge|lt|le|in` (DateTimeUtc), `id: eq|ne|in` (Uuid), `title: eq|ne|contains|startswith|endswith|in` (String).
The `$filter` description lists fields **in `FIELDS` order** (updated_at, id, title), so declare them in that order.

Option A: derive (`libs/toolkit-odata-macros/src/odata_filterable.rs`; generates `pub enum <Struct>FilterField`, variants
UpperCamel, referencing `::toolkit_odata`). **Requires adding** `toolkit-odata-macros = { workspace = true }`:
```rust
#[derive(toolkit_odata_macros::ODataFilterable)]
pub struct ChatQuery {
    #[odata(filter(kind = "DateTimeUtc"))] pub updated_at: time::OffsetDateTime,
    #[odata(filter(kind = "Uuid"))]        pub id: uuid::Uuid,
    #[odata(filter(kind = "String"))]      pub title: String,
}   // -> ChatQueryFilterField::{UpdatedAt, Id, Title}
```
Option B: hand-written, with no extra dependency (as in `libs/toolkit-db/tests/odata_null_like.rs:75-100`):
```rust
use toolkit_odata::filter::{FieldKind, FilterField};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatField { UpdatedAt, Id, Title }
impl FilterField for ChatField {
    const FIELDS: &'static [Self] = &[Self::UpdatedAt, Self::Id, Self::Title];
    fn name(&self) -> &'static str { match self { Self::UpdatedAt => "updated_at", Self::Id => "id", Self::Title => "title" } }
    fn kind(&self) -> FieldKind { match self { Self::UpdatedAt => FieldKind::DateTimeUtc, Self::Id => FieldKind::Uuid, Self::Title => FieldKind::String } }
}
```

### 4.3 SeaORM mapping and `paginate_odata`

`libs/toolkit-db/src/odata/sea_orm_filter.rs` (re-exported from `toolkit_db::odata`, `odata/mod.rs:35-38`):
```rust
pub trait FieldToColumn<F: FilterField> {                                       // :47
    type Column: Clone + sea_orm::Iden + sea_orm::ColumnTrait + sea_orm::IntoSimpleExpr + 'static;
    fn map_field(field: F) -> Self::Column;
    fn map_value(_field: F, _op: FilterOp, value: &ODataValue) -> Result<ODataValue, String> { Ok(value.clone()) }
    fn is_orderable(_field: F) -> bool { true }
}
pub trait ODataFieldMapping<F: FilterField>: FieldToColumn<F> {                 // :135
    type Entity: EntityTrait;
    fn extract_cursor_value(model: &<Self::Entity as EntityTrait>::Model, field: F) -> sea_orm::Value;
    fn cursor_kind(field: F) -> FieldKind { field.kind() }
}
#[derive(Clone, Copy)] pub struct LimitCfg { pub default: u64, pub max: u64 }  // :529
// clamp_limit (:534): None -> default; 0 -> 1; > max -> max
pub async fn paginate_odata<F, M, E, D, Mapper, C>(                             // :589
    select: SecureSelect<E, Scoped>, conn: &C, query: &toolkit_odata::ODataQuery,
    tiebreaker: (&str, SortDir), limit_cfg: LimitCfg, model_to_domain: Mapper,
) -> Result<Page<D>, toolkit_odata::Error>
where F: FilterField, M: ODataFieldMapping<F, Entity = E>, E: EntityTrait, Mapper: Fn(E::Model) -> D, C: DBRunner;
pub async fn paginate_odata_try<..>(..) -> Result<Page<D>, PaginateOdataTryError<MapErr>>   // :630 fallible mapper
```
Effective order: the cursor's own order when a cursor is present, otherwise `$orderby` + tiebreaker. An unknown or
non-orderable `$orderby` field gives `InvalidOrderByField`. A cursor/query mismatch gives `OrderMismatch`/`FilterMismatch`.
It fetches `limit + 1` rows and builds next/prev cursors.
Cursor value types (`encode_cursor_value` :419-453): `Uuid` -> `Value::Uuid(Some(u))`, `String` -> `Value::String(Some(s))`,
`DateTimeUtc` -> `Value::TimeDateTimeWithTimeZone(Some(odt))` (time crate) **or** `Value::ChronoDateTimeUtc(Some(dt))`, `I64` -> `Value::BigInt/Int/SmallInt`.

Full example for mini-chat chats. The shape is taken from `libs/toolkit-db/tests/odata_null_like.rs:102-130, 310-328`;
no gear in the repo uses `paginate_odata` yet:
```rust
use toolkit_db::odata::{FieldToColumn, LimitCfg, ODataFieldMapping, paginate_odata};
use toolkit_db::secure::SecureEntityExt;
use toolkit_odata::{ODataQuery, Page, SortDir};

pub struct ChatMapper;
impl FieldToColumn<ChatField> for ChatMapper {
    type Column = chat::Column;
    fn map_field(f: ChatField) -> chat::Column {
        match f { ChatField::UpdatedAt => chat::Column::UpdatedAt, ChatField::Id => chat::Column::Id, ChatField::Title => chat::Column::Title }
    }
}
impl ODataFieldMapping<ChatField> for ChatMapper {
    type Entity = chat::Entity;
    fn extract_cursor_value(m: &chat::Model, f: ChatField) -> sea_orm::Value {
        match f {
            ChatField::UpdatedAt => sea_orm::Value::TimeDateTimeWithTimeZone(Some(m.updated_at)),
            ChatField::Id => sea_orm::Value::Uuid(Some(m.id)),
            ChatField::Title => sea_orm::Value::String(Some(m.title.clone())),
        }
    }
}

pub async fn list_chats(&self, scope: &AccessScope, q: &ODataQuery) -> Result<Page<ChatModel>, DomainError> {
    let conn = self.db.conn()?;
    let base = chat::Entity::find()
        .filter(chat::Column::DeletedAt.is_null())            // plain SeaORM filters before .secure()
        .secure()
        .scope_with(scope);
    paginate_odata::<ChatField, ChatMapper, chat::Entity, ChatModel, _, _>(
        base, &conn, q,
        ("updated_at", SortDir::Desc),                         // tiebreaker / default order (pick per DESIGN)
        LimitCfg { default: 20, max: 100 },                    // DESIGN.md:662 default 20, max 100 (clamped)
        ChatModel::from,
    )
    .await
    .map_err(DomainError::OData)                               // then CanonicalError::from(odata_err) in the REST mapping
}
```
NOTE: `ODataOrderBy::ensure_tiebreaker` (`libs/toolkit-odata/src/lib.rs:225-233`) only **appends** the tiebreaker
when it is missing, and with no `$orderby` the effective order is just `[tiebreaker]`. A non-unique tiebreaker
(`updated_at`) makes ties ambiguous. To get "updated_at desc, id desc" by default, pass `("id", SortDir::Desc)` as the
tiebreaker and, when `q.cursor.is_none() && q.order.is_empty()`, set
`q.order = ODataOrderBy(vec![OrderKey { field: "updated_at".into(), dir: SortDir::Desc }])` before calling
`paginate_odata` (`toolkit_odata::{ODataOrderBy, OrderKey, SortDir}`).

Route declaration matching the contract (`GET /mini-chat/v1/chats`: params `limit`, `cursor`, `$filter`, `$orderby`, in that order):
```rust
use toolkit::api::operation_builder::OperationBuilderODataExt;
OperationBuilder::get("/mini-chat/v1/chats")
    .operation_id("mini_chat.list_chats")
    .tag("Mini Chat Chats")
    .authenticated()
    .require_license_features::<License>([License])
    .query_param_typed("limit", false, "Maximum number of chats to return", "integer")
    .query_param("cursor", false, "Cursor for pagination")
    .with_odata_filter::<ChatField>()
    .with_odata_orderby::<ChatField>()
    .handler(handlers::list_chats)
    .json_response_with_schema::<toolkit_odata::Page<ChatDto>>(openapi, StatusCode::OK, "...")
    .error_400(openapi).error_401(openapi).error_403(openapi).error_500(openapi).error_503(openapi)
    .register(router, openapi)

// handler
pub async fn list_chats(Extension(ctx): Extension<SecurityContext>, Extension(svc): Extension<Arc<ChatService>>,
                        OData(query): OData) -> ApiResult<Json<Page<ChatDto>>> { ... }
```

---

## 5. Database (`libs/toolkit-db`)

### 5.1 Declaring the database (config)

Per-gear YAML (`config/mini-chat.yaml`): a global `database.servers.<name>` (engine/params/pool) plus
`gears.mini-chat.database: { server: "sqlite_mini_chat", file: "mini_chat.db" }`. `DbConnConfig` fields:
`server`, `file` (relative to home_dir/gear), `path`, `dsn`, `host/port/user/password/dbname`, `params`, `pool`
(`libs/toolkit-db/src/config.rs:67-140`). The runtime builds the gear's `Db`, runs the gear's `migrations()` with a
per-gear history table `toolkit_migrations__<prefix>__<hash8>` (`migration_runner.rs:1-20`), then exposes the DB
through `ctx.db()`. mini-chat enables `toolkit-db = { features = ["sqlite", "pg"] }`.

### 5.2 Migrations

`gears/credstore/credstore/src/infra/storage/migrations.rs:9-20`:
```rust
use sea_orm_migration::prelude::*;
pub mod m0001_initial_schema;
pub struct Migrator;
#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> { vec![Box::new(m0001_initial_schema::Migration)] }
}
// m0001_initial_schema.rs
#[derive(DeriveMigrationName)] pub struct Migration;
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();   // per-backend raw SQL (Postgres/Sqlite), MySQL -> error
        let conn = manager.get_connection();            // conn.execute_unprepared(sql).await?
        ...
    }
}
```
Credstore uses raw SQL per backend to keep CHECKs and partial indexes. SQLite stores UUIDs as `BLOB` and timestamps
as `TEXT` (`m0001_initial_schema.rs:50-75`). The SeaORM schema builder (`Table::create()...`) is also fine
(`libs/toolkit-db/tests/odata_null_like.rs:234-266`).

### 5.3 Entities and secure ORM scoping

`#[derive(Scopable)]` (`libs/toolkit-db-macros/src/lib.rs:1-110`, re-exported as `toolkit_db::secure::Scopable`).
All four dimensions are **mandatory**: `tenant_col = ".."|no_tenant`, `resource_col = ".."|no_resource`,
`owner_col = ".."|no_owner`, `type_col = ".."|no_type`; alternatively `unrestricted`; plus repeatable
`pep_prop(name = "col")`. Property mapping: tenant_col -> `owner_tenant_id`, resource_col -> `id`, owner_col -> `owner_id`.
The derive expands to `::toolkit_db::secure::ScopableEntity`.
`gears/credstore/credstore/src/infra/storage/entity/secrets.rs:7-48`:
```rust
use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db::secure::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "credstore_secrets")]
#[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub owner_id: Uuid,
    pub created_at: OffsetDateTime,
    pub expires_at: Option<OffsetDateTime>,
    // ...
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)] pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
```
For a mini-chat chat the natural shape is `#[secure(tenant_col = "tenant_id", resource_col = "id", owner_col = "user_id", no_type)]`.

`AccessScope` (`toolkit_security::AccessScope`, re-exported in `toolkit_db::secure`; constructors at
`libs/toolkit-security/src/access_scope.rs:826-893`): `allow_all()`, `deny_all()`, `for_tenant(id)`, `for_tenants(vec)`,
`for_resource(id)`, `for_resources(vec)`, `from_constraints(vec![ScopeConstraint::new(vec![ScopeFilter::..])])`.
The production scope comes from the PDP: `authz_resolver_sdk::PolicyEnforcer::access_scope(&ctx, &ResourceType, action: &str,
resource_id: Option<Uuid>) -> Result<AccessScope, EnforcerError>` (`gears/system/authz-resolver/authz-resolver-sdk/src/pep/enforcer.rs:517-542`);
`ResourceType::from_static(name, &[pep_properties::OWNER_TENANT_ID, ...])` (187).
An empty scope denies everything; tenant + resource constraints are ANDed.

Query and write APIs. Note: `DbConn`/`DbTx` expose **no** raw `ConnectionTrait`; every statement goes through the secure wrappers:
```rust
use toolkit_db::secure::{SecureEntityExt, SecureInsertExt, SecureUpdateExt, SecureDeleteExt,
                         secure_insert, secure_update_with_scope, ScopeError};
// select (select.rs:132-300)
let rows = chat::Entity::find().filter(chat::Column::DeletedAt.is_null())
    .secure().scope_with(&scope)                      // -> SecureSelect<E, Scoped> (NOT a Result)
    .order_by(chat::Column::UpdatedAt, sea_orm::Order::Desc).limit(10)
    .all(&conn).await?;                               // .one(&runner) / .count(&runner); .and_id(uuid)? ; .filter(Condition)
// insert (db_ops.rs:179) — validates tenant_id is in scope; tenant_id must be Set
let m = secure_insert::<chat::Entity>(active_model, &scope, &conn).await?;
// or builder form (credstore writes.rs:45-51)
chat::Entity::insert(am).secure().scope_unchecked(&scope)?.exec(&conn).await?;
// update one by id (db_ops.rs:480): row must exist in scope; tenant_id immutable
secure_update_with_scope::<chat::Entity>(am, &scope, id, &conn).await?;
// bulk update/delete (db_ops.rs:1130-1323)
chat::Entity::update_many().col_expr(chat::Column::Title, Expr::value(t)).filter(chat::Column::Id.eq(id))
    .secure().scope_with(&scope).exec(&conn).await?;          // -> sea_orm::UpdateResult (rows_affected)
chat::Entity::delete_many().secure().scope_with(&scope).filter(cond).exec(&conn).await?;
```
`ScopeError` = `Db(DbErr) | Invalid(&'static str) | TenantNotInScope{tenant_id} | Denied(&'static str) | UnresolvedScopeProperty{..}`
(`secure/error.rs:14-44`). `DBRunner` is implemented by `DbConn<'_>`, `DbTx<'_>`, `SecureConn`, `SecureTx<'_>` (`secure/runner.rs:71-107`).
For background jobs that are not tenant-bound, use `AccessScope::allow_all()` (credstore `reads.rs:55-56`).

### 5.4 `DBProvider` and transactions

`libs/toolkit-db/src/db_provider.rs:18-100`:
```rust
pub struct DBProvider<E> { /* Arc<Db> */ }            // Clone; E: From<DbError> + Send + 'static
impl<E> DBProvider<E> {
    pub fn new(db: Db) -> Self;
    pub fn db(&self) -> Db;                           // cheap clone, e.g. for Outbox::builder
    pub fn conn(&self) -> Result<DbConn<'_>, E>;      // fails inside a transaction (guard)
    pub async fn transaction<T, F>(&self, f: F) -> Result<T, E>
    where T: Send + 'static,
          F: for<'a> FnOnce(&'a DbTx<'a>) -> Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>> + Send;
    pub async fn transaction_with_config<T, F>(&self, tx_config: TxConfig, f: F) -> Result<T, E>;  // isolation/access mode
}
```
Usage pattern (closure must be `'static`-safe, so clone `Arc`s in; `gears/system/types-registry/types-registry/src/domain/admission/acceptance.rs:525-574`):
```rust
let repo = Arc::clone(&self.repo);
let scope = scope.clone();
let out = self.db.transaction(move |tx| Box::pin(async move {
    let chat = repo.insert_chat(tx, &scope, new).await?;
    repo.insert_message(tx, &scope, msg).await?;
    Ok(chat)
})).await?;
```
`TxConfig { isolation: Option<TxIsolationLevel>, access_mode: Option<TxAccessMode> }`, `toolkit_db::secure::{TxConfig, TxIsolationLevel, TxAccessMode}`.
`Db` also has `transaction_with_retry` / `transaction_with_retry_max` (`secure/db.rs:703-760`) for serialization-failure retries.

### 5.5 Advisory locks (single-runner background jobs)

There is no leader-election library. Mini-chat DESIGN (line 595) uses a gear-local Kubernetes Lease behind the `k8s`
feature (`kube`/`k8s-openapi` optional deps are already declared) and a no-op elector otherwise. The DB alternative:
`Db::lock(gear, key) -> Result<DbLockGuard>` and `Db::try_lock(gear, key, LockConfig) -> Result<Option<DbLockGuard>>`
(`secure/db.rs:434-449`; `LockConfig` defaults: `max_wait: Some(30s)`, exponential backoff, `advisory_locks.rs:193-230`);
`DbLockGuard::release().await`.

### 5.6 Transactional OUTBOX (`toolkit_db::outbox`)

Docs: `libs/toolkit-db/src/outbox/README.md` (partly stale, see the NOTEs) and `mod.rs`. Exports (`mod.rs:243-256`):
`Outbox, OutboxBuilder, OutboxHandle, Record, Records, Wake, in_transaction, outbox_migrations, outbox_migrations_with_prefix,
LeasedHandler, LeasedMessageHandler, TransactionalHandler, TransactionalMessageHandler, HandlerResult, MessageResult, OutboxMessage,
Batch, LeaseConfig, OutboxError, OutboxProfile, Partitions, WorkerTuning, DeadLetterFilter, DeadLetterScope, DeadLetterMessage, ...`.

**Migrations.** Use a per-gear table prefix and append to your own migrations. Real code from
`gears/system/types-registry/types-registry/src/gear.rs:277-297`:
```rust
fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
    use sea_orm_migration::MigratorTrait;
    let mut migrations = crate::infra::storage::Migrator::migrations();
    let outbox = match toolkit_db::outbox::outbox_migrations_with_prefix(OUTBOX_TABLE_PREFIX) {   // e.g. "mini_chat__outbox"
        Ok(outbox) => outbox,
        Err(e) => panic!("outbox migration prefix '{OUTBOX_TABLE_PREFIX}' is invalid: {e}"),
    };
    migrations.extend(outbox);
    migrations
}
```
Prefix rules: ASCII identifier, starts with a letter, `[A-Za-z0-9_]` only, short (README "Custom table prefix").

**Start the pipeline in `init`** and keep the handle for shutdown (`types-registry/src/infra/outbox.rs:536-555`, `gear.rs:60-112`):
```rust
use toolkit_db::outbox::{Outbox, OutboxHandle, OutboxProfile, Partitions, LeaseConfig, WorkerTuning};
let handle: OutboxHandle = Outbox::builder(db.db())           // db: DBProvider<_>  -> Db
    .table_prefix(TABLE_PREFIX)?                              // same prefix as migrations
    .profile(OutboxProfile::low_latency())
    .processor_tuning(WorkerTuning::processor_low_latency().batch_size(1))   // per-message attempt budget, see NOTE
    .queue("mini-chat.audit", Partitions::of(4))              // power of 2, 1..=64 (types.rs:35-44)
        .leased(AuditHandler::new(..))
        .lease(LeaseConfig { duration: Duration::from_secs(30), headroom: Duration::from_secs(2) })  // defaults 30s/2s
    .queue("mini-chat.usage", Partitions::of(4))
        .leased(UsageHandler::new(..))
    .start().await?;
let outbox: Arc<Outbox> = Arc::clone(handle.outbox());        // manager.rs:751
// shutdown (in serve(), after cancel): handle.stop().await;   // manager.rs:759
```
Queue names (`outbox/validation.rs:27-47`) must be 1-1024 bytes of `[A-Za-z0-9._-]` and must start and end with an
alphanumeric, so `mini-chat.audit` is valid (DESIGN line 415 makes it configurable as `outbox.audit_queue_name`).
Payload type: 1-1024 printable ASCII characters (`validation.rs:50`).

**Enqueue inside the business transaction, fire the wake after commit:**
```rust
// Record builder (record.rs:28-125)
let record = Record::to(QUEUE, partition)                     // partition: u32 < Partitions
    .payload(serde_json::to_vec(&event)?, "application/json") // payload_type: printable ASCII
    .trace("optional-unique-id")                              // optional
    .build()?;                                                // validation, no DB statement yet
// Outbox::enqueue (core.rs:452-484) / enqueue_batch(db, Records) (496)
pub async fn enqueue(&self, db: &(impl DBRunner + Sync + ?Sized), msg: Record<'_>) -> Result<Wake, OutboxError>;

// Option 1: helper (transaction.rs:32-44)
pub async fn in_transaction<F, T, E>(db: &Db, f: F) -> Result<T, E>
where E: From<DbError> + Send + 'static,
      F: for<'a> FnOnce(&'a DbTx<'a>) -> Pin<Box<dyn Future<Output = Result<(T, Wake), E>> + Send + 'a>> + Send,
      T: Send + 'static;
// Option 2: DBProvider::transaction returning (T, Wake), then wake.fire() (acceptance.rs:528-582)
let (value, wake) = self.db.transaction(move |tx| Box::pin(async move {
    repo.finalize_turn(tx, &scope, ..).await?;
    let wake = outbox.enqueue(tx, record).await.map_err(DomainError::from)?;   // enqueue LAST
    Ok((value, wake))
})).await?;
wake.fire();                                                  // only after commit; Wake::empty(), `+=` combines (wake.rs:55,131)
```
NOTE: the README/doc comments still mention `Wake::flush`; the real method is `Wake::fire(self)`, and `Wake::discard(self)`
drops it intentionally. A `Wake` dropped without firing logs a warning (`impl Drop`, wake.rs:114).

**Handlers** (`outbox/handler.rs`):
```rust
pub struct OutboxMessage { pub partition_id: i64, pub seq: i64, pub payload: Vec<u8>, pub payload_type: String,
                           pub created_at: chrono::DateTime<chrono::Utc>, pub attempts: i16 }          // :12
pub enum MessageResult { Ok, Retry, Reject(String) }                                                    // :40
pub enum HandlerResult { Success, Retry { reason: String }, Reject { reason: String } }                  // :25
#[async_trait::async_trait] pub trait LeasedMessageHandler: Send + Sync {                               // :76
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult; }
#[async_trait::async_trait] pub trait LeasedHandler: Send + Sync {                                      // :63 (batch)
    async fn handle(&self, batch: &mut Batch<'_>) -> HandlerResult; }
#[async_trait::async_trait] pub trait TransactionalMessageHandler: Send + Sync {                       // :145
    async fn handle(&self, txn: &sea_orm::DatabaseExecutor<'_>, msg: &OutboxMessage) -> HandlerResult; }
```
- Leased mode: at-least-once, so handlers must be idempotent. The handler future is dropped at `lease.duration - headroom` or
  `stop_grace` (5 s) after `stop()`. Builder: `.leased(h)` / `.transactional(h)` / `.batch_transactional(h)` (`builder.rs:121-212`).
- Retry: exponential backoff, `WorkerTuning.retry_base` / `retry_max` (`types.rs:252-282`).
- **Max attempts / dead letter**: the outbox has **no** built-in max-attempts or dedupe key. The handler decides:
  `if msg.attempts >= MAX { return MessageResult::Reject(..) }`. Reject moves the message to `<prefix>_dead_letters`.
  NOTE (types-registry `infra/outbox.rs:157-170`): `attempts` is counted **per partition batch**, so per-message budgets need
  `processor_tuning(WorkerTuning::processor_low_latency().batch_size(1))`. DESIGN requires dead-lettering on the 120th attempt.
- **Dedupe**: there is no `dedupe_key`. Use a business-level unique key or an idempotency id in the payload (e.g.
  `Idempotency-Key = format!("{}:{}", msg.partition_id, msg.seq)`, README "Handler makes a remote HTTP call").
- Dead-letter API on `Outbox` (core.rs:657-740): `dead_letter_list(db, &DeadLetterFilter)`, `dead_letter_count`,
  `dead_letter_replay(db, &DeadLetterScope, timeout)`, `dead_letter_resolve(db, &[ids])`, `dead_letter_reject`, `dead_letter_discard`, `dead_letter_cleanup`.
- Real leased handler: `types-registry/src/infra/outbox.rs:141-260` (`AdmissionHandler`, `impl LeasedHandler`), dispatch with
  `OnceLock<Weak<Outbox>>` to avoid ownership cycles (105-139).

---

## 6. Testing conventions

### 6.1 In-memory SQLite repository tests

`gears/credstore/credstore/src/infra/storage/repo_impl/repo_tests.rs:14-51`:
```rust
use sea_orm_migration::MigratorTrait;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::{ConnectOpts, DBProvider, connect_db};

async fn setup() -> SecretRepoImpl {
    let dsn = format!("sqlite:file:credstore_repo_{}?mode=memory&cache=shared", Uuid::new_v4());
    let db = connect_db(&dsn, ConnectOpts { max_conns: Some(1), min_conns: Some(1), ..Default::default() })
        .await.expect("connect sqlite");
    run_migrations_for_testing(&db, Migrator::migrations()).await.expect("run migrations");  // migration_runner.rs:479
    SecretRepoImpl::new(Arc::new(DBProvider::<DomainError>::new(db)))
}
```
Outbox tables in tests: `run_migrations_for_testing(&db, outbox_migrations_with_prefix(PREFIX)?)`, or pass the gear's full
`migrations()` list (types-registry `tests/outbox_test.rs` fires wakes manually at lines 833 and 1079).
`toolkit-db` feature `test-support` gives a SQL `QueryRecorder` (`libs/toolkit-db/src/test_support.rs`).

### 6.2 REST tests with `tower::ServiceExt::oneshot`

`gears/credstore/credstore/src/api/rest/routes_tests.rs:1-140`:
```rust
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;

fn build_router(svc: Arc<Service>) -> Router {
    let openapi = OpenApiRegistryImpl::new();
    register_routes(Router::new(), &openapi, svc)
}
fn json_request(method: &str, uri: &str, body: Option<serde_json::Value>, ctx: SecurityContext) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    if body.is_some() { b = b.header("content-type", "application/json"); }
    let body = body.map_or_else(Body::empty, |j| Body::from(serde_json::to_vec(&j).unwrap()));
    let mut req = b.body(body).unwrap();
    req.extensions_mut().insert(ctx);                         // what the gateway auth layer would do
    req
}
async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}
#[tokio::test]
async fn post_create_returns_201_with_location() {
    let resp = build_router(svc).oneshot(json_request("POST", "/credstore/v1/secrets", Some(json!({..})), ctx)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert!(resp.headers().get("location").is_some());
}
```
SecurityContext for tests (`gears/credstore/credstore/src/domain/secret/test_support.rs:41-47`):
`SecurityContext::builder().subject_id(subject).subject_tenant_id(tenant).build().expect("ctx")`.
A mock PDP that grants `owner_tenant_id == subject tenant` (`test_support.rs:51-91`): implement
`authz_resolver_sdk::AuthZResolverApi::evaluate(&self, _ctx: PlatformSecurityContext, request: EvaluationRequest)
-> Result<EvaluationResponse, CanonicalError>` returning `EvaluationResponse { decision: true, context: EvaluationResponseContext {
constraints: vec![Constraint { predicates: vec![Predicate::Eq(EqPredicate::new(pep_properties::OWNER_TENANT_ID, root))] }], ..Default::default() } }`,
then `PolicyEnforcer::new(Arc::new(MockAuthZResolver))`. A deny variant is at lines 93-114.
Types-registry mock: `types-registry-sdk = { workspace = true, features = ["test-util"] }` gives
`types_registry_sdk::testing::MockTypesRegistryClient::new().with_instances(..)` (its `register_*` methods are not implemented).

To exercise the canonical error middleware (`instance`/`trace_id`) in tests, add
`.layer(axum::middleware::from_fn(toolkit::api::canonical_error_middleware))`
(as in `libs/toolkit/src/api/canonical_error_layer_tests.rs:31`).

### 6.3 Full gear `init` in tests (`GearCtx` by hand)

`gears/system/api-gateway/tests/throttling_tests.rs:36-67`:
```rust
struct TestConfigProvider { config: serde_json::Value }
impl toolkit::config::ConfigProvider for TestConfigProvider {
    fn get_gear_config(&self, gear: &str) -> Option<&serde_json::Value> { (gear == "mini-chat").then_some(&self.config) }
}
let hub = Arc::new(toolkit::ClientHub::new());
hub.register::<dyn types_registry_sdk::TypesRegistryClient>(Arc::new(mock_registry));
let ctx = toolkit::GearCtx::new("mini-chat", Uuid::new_v4(),
        Arc::new(TestConfigProvider { config: serde_json::json!({ "config": { /* gear config */ } }) }),
        hub, tokio_util::sync::CancellationToken::new())
    .with_db(toolkit_db::DBProvider::<toolkit_db::DbError>::new(db));   // context.rs:182 (toolkit default feature "db")
gear.init(&ctx).await?;
let router = gear.register_rest(&ctx, axum::Router::new(), &toolkit::api::OpenApiRegistryImpl::new())?;
```

### 6.4 Dependencies

Already in `gears/mini-chat/mini-chat/Cargo.toml`: `toolkit`, `toolkit-canonical-errors (axum)`, `toolkit-gts`, `gts`,
`toolkit-db (sqlite, pg)`, `toolkit-db-macros`, `toolkit-security`, `toolkit-macros`, `toolkit-odata (with-utoipa)`,
`sea-orm`, `sea-orm-migration`, `axum (macros, multipart)`, `http`, `utoipa (time)`, `serde`, `serde_json`, `time`, `uuid`,
`tokio`, `tokio-util`, `futures`, `bytes`, `multer`, `async-trait`, `anyhow`, `thiserror`, `tracing`, `inventory`,
`types-registry(-sdk)`, `authz-resolver(-sdk)`, `authn-resolver(-sdk)`, `oagw(-sdk)`, `secrecy`, `image`, `base64`, `regex`, `heck`,
`opentelemetry`, `tokio-retry`; optional `kube`, `k8s-openapi` (feature `k8s`). Dev: `chrono`, `opentelemetry_sdk (testing)`, `temp-env`.

Likely additions:
- `schemars = { workspace = true }`: **required** for `#[gts_type_schema(base = PluginV1, ...)]` plugin specs (section 1.6).
- `toolkit-odata-macros = { workspace = true }`: only if you use `#[derive(ODataFilterable)]`.
- `[dev-dependencies]`: `tower = { workspace = true }` (for `ServiceExt::oneshot`; axum enables `util`),
  `tokio = { workspace = true, features = ["rt", "macros", "test-util"] }`,
  `types-registry-sdk = { workspace = true, features = ["test-util"] }`; optional `http-body-util`, `tempfile`, `httpmock`, `serial_test`, `tracing-test`.

Present in `Cargo.lock` (usable as dev-deps; the workspace version is in root `Cargo.toml`): `tower 0.5.3`,
`http-body-util 0.1.3`, `tempfile 3.27`, `httpmock 0.8.3`, `serial_test 4.0.1`, `tokio-test 0.4.5`, `tracing-test 0.2.6`,
`temp-env 0.3.6`, `async-stream 0.3.6`, `tokio-stream 0.1.18`, `eventsource-stream 0.2.3` (transitive only, not a workspace dep),
`reqwest 0.13`, `hyper 1.11`, `serde_urlencoded 0.7`, `mime 0.3`, `multer 3.1`. **Not available**: `wiremock`, `mockall`,
`insta`, `rstest`, `pretty_assertions`, `axum-test` (`wiremock`/`mockall` are declared in workspace deps but absent from the lockfile).

Lints: workspace `clippy::pedantic = deny`, plus `expect_used`/`unwrap_used`-style denies. Test modules use
`#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]` (`routes_tests.rs:1`, `repo_tests.rs:7-12`).
Tests usually live in sibling files wired with `#[cfg(test)] #[path = "x_tests.rs"] mod tests;` (`routes.rs:135-137`).
