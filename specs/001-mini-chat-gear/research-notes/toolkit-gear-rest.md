# Toolkit reference for implementing the `mini-chat` gear

Research target: `/app` (Constructor Fabric gears-rust). Every path below is relative to `/app`; line numbers are from the current tree.
The crate you are filling in: `gears/mini-chat/mini-chat` (package `cf-gears-mini-chat`, lib `mini_chat`). It is already a workspace member (`Cargo.toml:100`), and its `Cargo.toml` already lists every toolkit dep needed below. `src/lib.rs` currently contains only the stub
`pub mod infra { pub mod plugins { pub mod static_audit {} pub mod static_model_policy {} } }`.
The example server already links those stub modules (see section 5).

---

## 1. Declaring and registering a gear

### 1.1 The `#[toolkit::gear(...)]` macro

* Implemented in `libs/toolkit-macros/src/lib.rs`, `pub fn gear` at ~line 503. Re-exported as `toolkit::gear` (`libs/toolkit/src/lib.rs:105`).
* Arguments (parser in `GearConfig`, lines 20-27 and 200-416):
  * `name = "mini-chat"`: the runtime gear name. This is the key under `gears:` in the YAML config.
  * `deps = [types_registry, authz_resolver, ...]`: a list of **crate identifiers** (snake_case). The runtime name is the identifier with `_` replaced by `-` (`authz_resolver` becomes `"authz-resolver"`, lines 521-527). The macro also emits `pub use ::<crate> as _gear_dep_<crate>;` (lines 797-809) so that the dependency's `inventory` registration gets linked. **Each dep crate must therefore be a direct Cargo dependency.** mini-chat already has `types-registry`, `authn-resolver`, `authz-resolver` and `oagw`. Each dep crate must also appear in `[workspace.metadata.cargo-shear] ignored` (`Cargo.toml:742-754`; those four are already listed).
  * `capabilities = [db, rest, stateful, system, rest_host, grpc, grpc_hub]` (lines 41-49). Each one compile-asserts that the matching trait is implemented:
    * `db` requires `toolkit::contracts::DatabaseCapability`
    * `rest` requires `RestApiCapability`
    * `stateful` requires `RunnableCapability`, unless `lifecycle(...)` is also given
    * `system` is a flag. It needs `impl SystemCapability for X {}` (an empty impl is fine; see credstore) and makes the gear initialise before non-system gears.
  * `lifecycle(entry = "serve", stop_timeout = "30s", await_ready)` (parsed at lines 419-480). When present, the macro generates `impl toolkit::lifecycle::Runnable` and wraps the gear in `WithLifecycle` (lines 629-692 and 711-756). The entry method has this signature:
    `async fn serve(self: Arc<Self>, cancel: CancellationToken, ready: ReadySignal) -> anyhow::Result<()>` (with `await_ready`), or `(self: Arc<Self>, cancel: CancellationToken)` (without it).
  * `ctor = Expr` is optional. The default is `<T as Default>::default()`, so the struct must implement `Default`.
  * `client = path::Trait` is optional and only does compile-time object-safety checks (lines 771-794).
* The macro always generates `pub const MODULE_NAME: &'static str = "<name>";` on the struct.
* The generated code references `::async_trait`, `::tokio_util` and `::anyhow`, so the crate needs those deps. mini-chat has them.

### 1.2 Traits (`libs/toolkit/src/contracts.rs`)

```rust
#[async_trait] pub trait Gear: Send + Sync + 'static {            // :37
    async fn init(&self, ctx: &toolkit::GearCtx) -> anyhow::Result<()>;
}
pub trait DatabaseCapability: Send + Sync {                       // :47 (feature "db")
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>>;
}
pub trait RestApiCapability: Send + Sync {                        // :52  sync, runs AFTER migrations
    fn register_rest(&self, ctx: &GearCtx, router: axum::Router,
                     openapi: &dyn OpenApiRegistry) -> anyhow::Result<axum::Router>;
    fn healthcheck(&self, _ctx: &GearCtx) -> Option<Arc<dyn toolkit::healthcheck::Healthcheck>> { None }
}
#[async_trait] pub trait RunnableCapability: Send + Sync {        // :167
    async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()>;
    async fn stop(&self, deadline_token: CancellationToken) -> anyhow::Result<()>;
}
#[async_trait] pub trait SystemCapability: Send + Sync {          // :12  (defaults are no-ops)
    fn pre_init(&self, _sys: &toolkit::runtime::SystemContext) -> anyhow::Result<()> { Ok(()) }
    async fn post_init(&self, _sys: &toolkit::runtime::SystemContext) -> anyhow::Result<()> { Ok(()) }
}
```

Imports used in real code (`gears/credstore/credstore/src/gear.rs:13-17`):
```rust
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::{DatabaseCapability, SystemCapability};
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::DBProvider;
```

### 1.3 `GearCtx` API (`libs/toolkit/src/context.rs`)

| method | line | notes |
|---|---|---|
| `fn config<T: DeserializeOwned>(&self) -> Result<T, ConfigError>` | 278 | Strict. Reads `gears.<name>.config`. |
| `fn config_or_default<T: DeserializeOwned + Default>(&self)` | 304 | Returns `T::default()` if the gear, its object or its `config` key is missing (`libs/toolkit/src/config.rs:55-82`). If the section exists it is parsed with serde, so `deny_unknown_fields` errors propagate. |
| `fn config_expanded<T: DeserializeOwned + ExpandVars>` / `config_expanded_or_default` | 314 / 349 | Expands `${VAR}` in fields marked `#[expand_vars]`. Requires `#[derive(toolkit_macros::ExpandVars)]`. |
| `fn raw_config(&self) -> &serde_json::Value` | 364 | |
| `fn client_hub(&self) -> Arc<toolkit::ClientHub>` | 214 | |
| `fn db(&self) -> Option<toolkit_db::DBProvider<toolkit_db::DbError>>` | 244 | |
| `fn db_required(&self) -> anyhow::Result<DBProvider<DbError>>` | 265 | |
| `fn cancellation_token(&self) -> &CancellationToken` | 220 | |
| `fn gear_name(&self) -> &str` | 191 | |

A DB handle exists only if the YAML has `gears.<name>.database: {...}`. `GearContextBuilder::for_gear` (lines 103-121) calls `ctx.with_db(DBProvider::new(handle))` when the db manager resolves one.

### 1.4 Typed config pattern (`gears/credstore/credstore/src/config.rs:6-85`)

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CredStoreConfig { pub vendor: String, pub hierarchy: HierarchyCfg, pub reaper: ReaperCfg }
impl Default for CredStoreConfig { fn default() -> Self { Self { vendor: "constructorfabric".to_owned(), .. } } }
impl CredStoreConfig { pub fn validate(&self) -> Result<(), String> { /* ... */ } }
// in init():
let cfg: CredStoreConfig = ctx.config_or_default()?;
cfg.validate().map_err(|err| anyhow::anyhow!("credstore config invalid: {err}"))?;
```

Env expansion (`gears/credstore/plugins/static-credstore-plugin/src/config.rs:12-24`):
```rust
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct StaticCredStorePluginConfig { pub vendor: String, pub priority: i16, #[expand_vars] pub secrets: Vec<SecretConfig> }
let cfg: StaticCredStorePluginConfig = ctx.config_expanded_or_default()?;
```
This is relevant for mini-chat: `config/mini-chat.yaml` uses `host: "${AZURE_OPENAI_API_HOST}"`.

### 1.5 ClientHub (`libs/toolkit/src/client_hub.rs`)

* `register<T: ?Sized>(&self, client: Arc<T>)` at :152
* `register_scoped<T>(&self, scope: ClientScope, client: Arc<T>)` at :165
* `get<T>(&self) -> Result<Arc<T>, ClientHubError>` at :182
* `try_get<T>` at :206
* `get_scoped` / `try_get_scoped(&ClientScope)` at :273 / :300
* `ClientScope::gts_id(&str)` at :57

Real calls (credstore `gear.rs:110-169`):
```rust
let authz_client = ctx.client_hub().get::<dyn AuthZResolverApi>()
    .map_err(|e| anyhow::anyhow!("failed to get AuthZResolverApi: {e}"))?;
let enforcer = authz_resolver_sdk::PolicyEnforcer::new(authz_client);
let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
let client: Arc<dyn credstore_sdk::CredStoreClientV1> = Arc::new(CredStoreLocalClient::new(svc));
ctx.client_hub().register::<dyn credstore_sdk::CredStoreClientV1>(client);
```

### 1.6 Full example: credstore (system + db + rest + stateful)

`gears/credstore/credstore/src/lib.rs`:
```rust
#![doc = include_str!("../README.md")]
pub mod api; pub mod client; pub mod config; pub mod domain; pub mod gear; pub mod infra;
pub use gear::CredStoreGear;
```

`gears/credstore/credstore/src/gear.rs:37-205` (abridged; the full file is about 205 lines):
```rust
#[toolkit::gear(
    name = "credstore",
    deps = [authz_resolver, tenant_resolver, types_registry],
    capabilities = [system, db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct CredStoreGear { service: OnceLock<Arc<Service>> }
impl Default for CredStoreGear { fn default() -> Self { Self { service: OnceLock::new() } } }

impl CredStoreGear {
    pub(crate) async fn serve(self: Arc<Self>, cancel: CancellationToken, ready: ReadySignal) -> anyhow::Result<()> {
        let Some(svc) = self.service.get().cloned() else { anyhow::bail!("credstore: serve invoked before init"); };
        let mut interval = tokio::time::interval(Duration::from_secs(svc.reaper_tick_secs()));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ready.notify();
        loop { tokio::select! { biased;
            () = cancel.cancelled() => break,
            _ = interval.tick() => { svc.reap_and_refresh().await; } } }
        Ok(())
    }
}

#[async_trait]
impl Gear for CredStoreGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: CredStoreConfig = ctx.config_or_default()?;
        let db_raw = ctx.db_required()?;
        let db: Arc<DBProvider<crate::domain::error::DomainError>> = Arc::new(DBProvider::new(db_raw.db()));
        // ... resolve clients from hub, build Service ...
        self.service.set(Arc::clone(&svc))
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;
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
        let svc = self.service.get().cloned().ok_or_else(|| anyhow::anyhow!("credstore Service not initialized"))?;
        Ok(crate::api::rest::register_routes(router, openapi, svc))
    }
}
```

Key detail: `DBProvider<E>` is generic over the error type. `DBProvider::new(db_raw.db())` re-wraps the raw `Db` so that `conn()` and `transaction()` return the gear's own `DomainError` (this needs `impl From<toolkit_db::DbError> for DomainError`; see `gears/credstore/credstore/src/infra/canonical_mapping.rs:46-56`).

Other `#[toolkit::gear]` declarations for comparison:
* `gears/system/oagw/oagw/src/gear.rs:41-45`: `name="oagw", deps=[types_registry, authz_resolver, credstore, tenant_resolver], capabilities=[system, rest]`. State is held in `arc_swap::ArcSwapOption<AppState>` and passed to routes with `router.layer(axum::Extension(state))` (`api/rest/routes/mod.rs:29-39`).
* `gears/system/types-registry/types-registry/src/gear.rs:31-37`: `capabilities=[system, db, rest, stateful], lifecycle(entry="serve", stop_timeout="30s", await_ready)`.
* `gears/system/authz-resolver/authz-resolver/src/gear.rs:33-41`: `deps=[types_registry], capabilities=[system, rest]`, plus `#[toolkit::provides(...)]`.

Migrations (`gears/credstore/credstore/src/infra/storage/migrations.rs` and `.../m0001_initial_schema.rs`):
```rust
use sea_orm_migration::prelude::*;
pub struct Migrator;
#[async_trait::async_trait]
impl MigratorTrait for Migrator { fn migrations() -> Vec<Box<dyn MigrationTrait>> { vec![Box::new(m0001_initial_schema::Migration)] } }
// m0001: #[derive(DeriveMigrationName)] pub struct Migration; MigrationTrait::up matches
// manager.get_database_backend() { Postgres => raw SQL, Sqlite => raw SQL, _ => Err }
```
mini-chat runs on **sqlite** in `config/mini-chat.yaml`. UUIDs are BLOBs and timestamps are TEXT there; see the per-backend SQL in m0001.

Entity with secure scoping (`gears/credstore/credstore/src/infra/storage/entity/secrets.rs:7-16`):
```rust
use toolkit_db::secure::Scopable;
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "credstore_secrets")]
#[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
pub struct Model { #[sea_orm(primary_key, auto_increment = false)] pub id: Uuid, pub tenant_id: Uuid, /* time::OffsetDateTime for timestamps */ }
```

Queries (`.../repo_impl/reads.rs`, `writes.rs:20-50`):
```rust
let conn = repo.db.conn()?;                              // DbConn; errors if called inside a tx
Entity::find().secure().scope_with(&scope).filter(...)   // toolkit_db::secure::SecureEntityExt
Entity::insert(am).secure().scope_unchecked(scope).map_err(map_scope_err)?.exec(&conn).await
```
Transactions use `DBProvider::transaction(|tx| Box::pin(async move { ...; Ok::<_, E>(v) }))` (`libs/toolkit-db/src/db_provider.rs:70-77`). Example: `gears/system/types-registry/types-registry/src/domain/registry_service.rs:338`.

Authorization to `AccessScope` (`gears/credstore/credstore/src/domain/authz.rs`):
```rust
use authz_resolver_sdk::pep::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
let rt = ResourceType::new(gts_id.to_owned(), &[pep_properties::OWNER_TENANT_ID]);
let req = AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()).require_constraints(true);
enforcer.access_scope_with(ctx, &rt, "read", None, &req).await  // Denied/CompileFailed => 403, EvaluationFailed => 503
```

### 1.7 Plugin gear example: `static-credstore-plugin`

`gears/credstore/plugins/static-credstore-plugin/src/gear.rs:14-88`:
```rust
use std::sync::{Arc, OnceLock};
use async_trait::async_trait;
use credstore_sdk::{CredStorePluginClientV1, CredStorePluginSpecV1};
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

#[toolkit::gear(name = "static-credstore-plugin", deps = [types_registry])]
pub struct StaticCredStorePlugin { service: OnceLock<Arc<Service>> }
impl Default for StaticCredStorePlugin { fn default() -> Self { Self { service: OnceLock::new() } } }

#[async_trait]
impl Gear for StaticCredStorePlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticCredStorePluginConfig = ctx.config_expanded_or_default()?;
        let service = Arc::new(Service::from_config(&cfg)?);
        let (instance_id, instance_json) = PluginV1::<CredStorePluginSpecV1>::build_registration(
            "cf.core._.static_credstore.v1", cfg.vendor.clone(), cfg.priority)?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;
        self.service.set(service.clone()).map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        let api: Arc<dyn CredStorePluginClientV1> = service;
        ctx.client_hub().register_scoped::<dyn CredStorePluginClientV1>(ClientScope::gts_id(&instance_id), api);
        Ok(())
    }
}
```
The identical pattern is used in `gears/system/authz-resolver/plugins/static-authz-plugin/src/gear.rs:18-71` (instance segment `"cf.builtin.static_authz_resolver.plugin.v1"`, `ctx.config_or_default()`).

Plugin spec type, declared in the SDK (`gears/credstore/credstore-sdk/src/gts.rs:17-38`):
```rust
use toolkit::gts::PluginV1;
use toolkit_gts::{gts_id, gts_type_schema};
#[derive(Default)]
#[gts_type_schema(dir_path = "schemas", base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.credstore.plugin.v1~"),
    description = "CredStore plugin specification", properties = "")]
pub struct CredStorePluginSpecV1;
```
`PluginV1::build_registration(instance_segment, vendor, priority) -> serde_json::Result<(GtsInstanceId, serde_json::Value)>` is at `libs/toolkit-gts/src/plugin.rs:93-109`. The instance id is `P::TYPE_ID + instance_segment`.

Host-side plugin selection (`gears/credstore/credstore/src/infra/plugin_select.rs:19-103`):
```rust
use toolkit::plugins::{GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};
let instances = registry.list_instances(InstanceQuery::new().with_pattern(format!("{}*", CredStorePluginSpecV1::gts_type_id()))).await?;
let gts_id = choose_plugin_instance::<CredStorePluginSpecV1>(&self.vendor, instances.iter().map(|e| (e.id.as_ref(), &e.object)))?; // lowest priority wins
let id = self.selector.get_or_init(|| self.resolve_instance()).await?;  // single-flight cache (libs/toolkit/src/plugins/mod.rs:56)
self.hub.try_get_scoped::<dyn CredStorePluginClientV1>(&ClientScope::gts_id(id.as_ref()))
```
Do the selection lazily, at request time or in `serve`, not in `init`. Plugin init order relative to the host is not guaranteed unless the plugin is listed in `deps`.

For mini-chat, the plugins live **in the same crate** (`mini_chat::infra::plugins::static_audit`, `...::static_model_policy`). Their config keys are `static-mini-chat-audit-plugin` and `static-mini-chat-model-policy-plugin` (`config/mini-chat.yaml:231-241`), so use those strings as the gear `name`s. Each submodule can contain its own `#[toolkit::gear(...)] struct` and they register through `inventory` independently.

---

## 2. REST

### 2.1 `OperationBuilder` (`libs/toolkit/src/api/operation_builder.rs`)

Imports:
```rust
use toolkit::api::{OpenApiRegistry, OperationBuilder, ParamSpec, ThrottlingSpec};
use toolkit::api::operation_builder::{CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilderODataExt,
                                      ResponseHeaderSpec, ResponseHeaderType};
```

Type state is `OperationBuilder<H, R, S, A, L>` (line 676). `register()` (line 2167) is only callable when the handler is set, a response is set, **auth is set**, and **license is set**:
* Constructors: `OperationBuilder::get/post/put/delete/patch(path)` (734-758). Path params use `{id}` syntax (axum 0.8).
* Metadata: `.operation_id("mini_chat.create_chat")`, `.summary()`, `.description()`, `.tag()`, `.path_param("id", "desc")` (818), `.query_param(...)`, `.param(ParamSpec::header("If-Match").required(true).description(..))`.
* Auth: `.authenticated()` (1255) moves to `AuthSet`, after which a license call is **mandatory**. `.anonymous()` (1296) moves to `AuthSet + LicenseSet`.
* License: `.require_license_features([&License])` (1150) or `.no_license_required()` (1183). Passing an empty iterator (e.g. `require_license_features::<License>([])`) means **no** requirement (`license_requirement = None`).
* Visibility: `.exposed()` (1127) marks the route as published by the edge gateway for OoP deployments. The default is internal. Most in-repo gears do not call it; in the in-process example server every registered route is served.
* Request body: `.json_request::<Dto>(openapi, "desc")` (932) requires `Dto: ToSchema + RequestApiDto`. Also `.multipart_file_request("file", Some("desc"))` (1011), `.octet_stream_request(Some(..))` (1072) and `.allow_content_types(&[..])` (1115). Note that the last three install gateway MIME validation.
* Handler: `.handler(fn)` (1340). `.method_router(mr)` (1367) lets you attach per-route layers.
* Responses: `.json_response_with_schema::<Dto>(openapi, StatusCode::OK, "desc")` (1458/1751), `.no_content_response(StatusCode::NO_CONTENT, "desc")`, `.json_response(status, desc)`, `.sse_json::<EventDto>(openapi, "desc")` (1615/1862: `text/event-stream`, status 200), `.multipart_json::<T>`, and `.response_header(ResponseHeaderSpec::new("Location", "desc", ResponseHeaderType::String))` (1695).
* Errors: `.error_400/401/403/404/409/413/415/422/429/500/502/503/504(openapi)` (2032-2150) or `.standard_errors(openapi)` (1947: 400, 401, 403, 404, 409, 429, 500). 413, 415 and 422 are not included in `standard_errors`; add them explicitly when the handler uses `extract::Json`.
* Throttling: `.with_throttling(ThrottlingSpec {..})` (788).
* `.register(router, openapi)` calls `openapi.register_operation(&spec)` and then `router.route(path, method_router)`.

DTOs use `#[toolkit_macros::api_dto(request)]`, `(response)` or `(request, response)`. It derives `Serialize`/`Deserialize` + `utoipa::ToSchema`, adds `#[serde(rename_all = "snake_case")]`, and implements `RequestApiDto`/`ResponseApiDto` (`libs/toolkit-macros/src/api_dto.rs:44-89`). Example (`gears/credstore/credstore/src/api/rest/dto.rs:45-66`):
```rust
#[derive(Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct CreateSecretRequestDto {
    #[schema(pattern = "^[A-Za-z0-9_-]+$", min_length = 1, max_length = 255)] pub reference: String,
    pub value: String,
    #[serde(default)] pub sharing: SharingModeDto,
    #[serde(default, rename = "type")] pub secret_type: Option<String>,
}
```
`Page<T>`, `Vec<T>`, `Option<T>`, `serde_json::Value` and `Problem` already implement the marker traits (`libs/toolkit/src/api/api_dto.rs:28-42`). Never pass `Vec<T>` to `json_response_with_schema`; use `json_array_response_with_schema::<T>` instead.

### 2.2 Complete route-registration example (`gears/credstore/credstore/src/api/rest/routes.rs:31-133`)

```rust
pub fn register_routes(router: Router, openapi: &dyn OpenApiRegistry, svc: Arc<ConcreteService>) -> Router {
    let router = OperationBuilder::post("/credstore/v1/secrets")
        .operation_id("credstore.create_secret")
        .summary("Create a secret")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<CreateSecretRequestDto>(openapi, "Secret reference, value, and sharing mode")
        .handler(handlers::create_secret)
        .no_content_response(StatusCode::CREATED, "Secret created (see Location header)")
        .error_400(openapi).error_401(openapi).error_403(openapi).error_409(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);
    let router = OperationBuilder::get("/credstore/v1/secrets/{ref}")
        .operation_id("credstore.get_secret")
        .authenticated()
        .no_license_required()
        .path_param("ref", "Secret reference")
        .handler(handlers::get_secret)
        .json_response_with_schema::<GetSecretResponseDto>(openapi, StatusCode::OK, "Resolved secret")
        .error_400(openapi).error_404(openapi).error_500(openapi)
        .register(router, openapi);
    router.layer(axum::Extension(svc))     // handlers take Extension(svc): Extension<Arc<Service>>
}
```

### 2.3 License feature (base feature `gts.cf.core.lic.feat.v1~cf.core.global.base.v1`)

* The constant is `toolkit::api::operation_builder::CORE_GLOBAL_BASE_LICENSE_FEATURE` (`operation_builder.rs:66-67`, built with `gts_id!("cf.core.lic.feat.v1~cf.core.global.base.v1")`).
* Pattern from `gears/system/types-registry/types-registry/src/api/rest/routes.rs:26-34` and `gears/system/api-gateway/tests/license_middleware.rs:88-118`:
```rust
struct License;
impl AsRef<str> for License { fn as_ref(&self) -> &'static str { CORE_GLOBAL_BASE_LICENSE_FEATURE } }
impl LicenseFeature for License {}
// actually require it (non-empty list; `impl LicenseFeature for &T` exists at operation_builder.rs:254):
.authenticated().require_license_features([&License])
```
* Enforcement: `gears/system/api-gateway/src/middleware/license_validation.rs:13, 47-79`. This is a stub. If any required feature is something other than `BASE_FEATURE`, the middleware returns `permission_denied().with_reason("LICENSE_FEATURE_REQUIRED")` (403). Requiring exactly the base feature always passes. ADR-0004 and ADR-0008 for mini-chat expect `[base feature]` on every route.
* oagw uses its own feature, `gts_id!("cf.core.lic.feat.v1~cf.core.oagw.base.v1")`, with an empty list (`gears/system/oagw/oagw/src/api/rest/routes/mod.rs:12-22`).

### 2.4 Throttling / rate-limit zones

`ThrottlingSpec` is defined at `operation_builder.rs:517-541`:
```rust
.with_throttling(ThrottlingSpec {
    rate_limit_zone: Some("rl_mini_chat_chat".to_owned()),
    in_flight_limit_zone: Some("ifl_mini_chat_chat".to_owned()),
    require_security_context: false,   // false = pre-auth, IP-keyed zones only
    dry_run: false,                    // quickstart.yaml comments say mini-chat create_chat used dry_run: true
})
```
(Real usage: `gears/system/api-gateway/tests/middleware_order.rs:82-95`.)
* Zones are defined in the api-gateway config under `rate_limit_zones` / `in_flight_limit_zones` (`config/mini-chat.yaml:57-74`, `config/quickstart.yaml:141-159`, `config/e2e-local.yaml:117-126`).
* If an operation references an undefined zone, router build fails: `"throttling: operation .. references undefined rate_limit zone"` (`gears/system/api-gateway/src/middleware/throttling.rs:486-510`, validated at `gear.rs:407`).
* An identity-keyed zone requires `require_security_context: true`, `.authenticated()` and `auth_disabled: false` (`throttling.rs:567-597`).
* A zone must be bound either entirely dry-run or entirely enforced (`throttling.rs:440-460`).
* Rejections become `resource_exhausted` (429) with `with_quota_violation_retry_after_seconds` (`throttling.rs:916-940`).
* OpenAPI output: `x-throttling-rate-limit-zone` / `x-throttling-in-flight-limit-zone` (`libs/toolkit/src/api/openapi_registry.rs:128-145`). The gateway adds `x-rate-limit-rps`, `x-rate-limit-burst` and `x-in-flight-limit` (`gears/system/api-gateway/src/gear.rs:1442-1495`). These match `specs/001-mini-chat-gear/openapi-mini-chat.json:269-274` for `POST /mini-chat/v1/chats`.

### 2.5 `SecurityContext` in handlers

The gateway auth middleware inserts `toolkit_security::SecurityContext` into request extensions:
```rust
use axum::extract::Extension;
use toolkit_security::SecurityContext;
pub async fn create_secret(uri: axum::http::Uri, Extension(ctx): Extension<SecurityContext>,
                           Extension(svc): Extension<Arc<ConcreteService>>, Json(body): Json<Dto>) -> ApiResult<impl IntoResponse>
```
* Accessors (`libs/toolkit-security/src/context.rs:73-125`): `subject_id() -> Uuid`, `subject_tenant_id() -> Uuid`, `subject_type()`, `token_scopes()`, `bearer_token()`, `is_anonymous()`.
* With `auth_disabled: true` (as in mini-chat.yaml), the gateway injects a default context built from `toolkit_security::constants::{DEFAULT_SUBJECT_ID (11111111-6a88-4768-9dfc-6bcd5187d9ed), DEFAULT_TENANT_ID (00000000-df51-5b42-9538-d2b56b7ee953)}` (`gears/system/api-gateway/src/gear.rs:464-485`).

### 2.6 Body and path extractors: 400 / 422 / 415

Use the **canonical** extractors in `toolkit::api::rest::extract::{Json, Path, Query}` (`libs/toolkit/src/api/rest/extract/mod.rs:28-30`). They have the same shape as the axum versions, but their `Rejection = CanonicalError`.

> Gotcha: `toolkit::api::canonical_prelude::*` re-exports **`axum::Json`** as `Json` (`libs/toolkit/src/api/mod.rs:53`). Import `toolkit::api::rest::extract::{Json, Path}` explicitly, or write `extract::Json`. credstore uses plain `axum::Json` and `axum::extract::Path`. With those, rejections reach `canonical_error_middleware`, which wraps them as an `about:blank` Problem without `field_violations`.

The mapping is in `libs/toolkit/src/api/rest/extract/error.rs:43-162`. The resource type is `gts.cf.core.http.request.v1~` (`GenericResourceError`). The category is `invalid_argument`, with a status override taken from axum:

| condition | status | field | reason |
|---|---|---|---|
| malformed JSON | 400 | `body` | `json_syntax_error` |
| serde data error (unknown field under `deny_unknown_fields`, bad enum, missing field) | 422 | `body` | `invalid_json_body` |
| missing or wrong `Content-Type` | 415 | `body` | `missing_json_content_type` |
| body read / length limit | axum status (e.g. 413) | `body` | `json_body_read_error` |
| path parse failure | 400 | `path` | `invalid_path_params` |
| query parse failure (`extract::Query`) | 400 | `query` | `invalid_query_string` |
| path route/type bug | 500 internal | n/a | n/a |

Wire example (test at `extract/json.rs:173-198`):
```json
{"type":"gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~","title":"Invalid Argument","status":422,
 "detail":"Request validation failed","context":{"resource_type":"gts.cf.core.http.request.v1~",
 "field_violations":[{"field":"body","description":"Failed to deserialize the JSON body ...","reason":"invalid_json_body"}]}}
```
Descriptions are truncated to 500 characters.

The gateway MIME middleware (used only for routes with `allow_content_types` / `multipart_file_request` / `octet_stream_request`) returns **400** `invalid_argument` with field `content-type` and reason `UNSUPPORTED_MEDIA_TYPE` or `MISSING_CONTENT_TYPE` (`gears/system/api-gateway/src/middleware/mime_validation.rs:48-53, 91-130`).

### 2.7 Multipart and raw body

* No gear in this repo parses a `multipart/form-data` request yet. The builder side is `.multipart_file_request("file", Some("desc"))` (`operation_builder.rs:1011-1027`, with a doc example at 990-1009). It sets the OpenAPI schema and `allowed_request_content_types = ["multipart/form-data"]`. A gateway test uses it at `gears/system/api-gateway/src/gear.rs:2415`.
* mini-chat's Cargo.toml already enables `axum` `multipart` and `multer = "3.1"`. In the handler, take `axum::extract::Multipart` (or `axum::body::Body` + `multer::Multipart::new(body.into_data_stream(), boundary)`), and map errors to `CanonicalError` yourself.
* Body size: the gateway applies `RequestBodyLimitLayer` + `DefaultBodyLimit::max(config.defaults.body_limit_bytes)` (`gear.rs:531-533`). mini-chat.yaml sets `body_limit_bytes: 64000000`.
* Raw catch-all routes that don't fit OperationBuilder are registered directly, e.g. oagw `router.route("/oagw/v1/proxy/{*path}", any(handlers::proxy::proxy_handler))` (`gears/system/oagw/oagw/src/api/rest/routes/proxy.rs:11-16`). These get no OpenAPI entry and no license/throttle spec.
* `toolkit::http::multipart::MultipartJsonStream` (`libs/toolkit/src/http/multipart.rs`) is for streaming **responses** (`multipart/mixed`), not uploads.

### 2.8 SSE responses

* OpenAPI: `.sse_json::<EventDto>(openapi, "desc")` registers `200 text/event-stream` with `$ref` to EventDto (`operation_builder.rs:1615-1640`). The test `gears/system/api-gateway/src/gear.rs:2191-2240` shows it:
```rust
#[derive(Clone)] #[toolkit_macros::api_dto(request, response)] struct UserEvent { id: u32, message: String }
async fn sse_handler() -> axum::response::sse::Sse<impl futures_core::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>> {
    toolkit::SseBroadcaster::<UserEvent>::new(4).sse_response()
}
OperationBuilder::<Missing, Missing, ()>::get("/tests/v1/demo/sse").handler(sse_handler).anonymous()
    .sse_json::<UserEvent>(&api, "SSE of UserEvent").register(router, &api);
```
* Runtime helpers: `toolkit::SseBroadcaster<T>` (`libs/toolkit/src/http/sse.rs:14-142`) provides `new(cap)`, `send(v)`, `subscribe_stream()`, `sse_response()` (JSON `data:`, keepalive every 15 s with text "keepalive"), `sse_response_named(event)` and `sse_response_with_headers(..)`. For per-request streams with **different `event:` names per frame** (mini-chat's `delta`/`done`/`error`), build `axum::response::sse::Sse::new(stream)` directly, using `Event::default().event("delta").json_data(&payload)` and `.keep_alive(KeepAlive::new().interval(..))`, with the same pattern as `sse.rs:40-91`.
* Gateway timeout: a 30 s `tower::timeout` (`gear.rs:538-544`). It bounds the time until the response future resolves (headers), not body streaming, so return the `Sse` response quickly. Errors raised **before** the stream opens should be returned as `CanonicalError` (Problem JSON). After the stream opens, emit an error event.
* oagw's upstream SSE parsing helpers are in `gears/system/oagw/oagw-sdk/src/sse/{stream.rs,detect.rs,response.rs}`.

### 2.9 Location header on 201

* Helper: `toolkit::api::response::created_json(value, &uri, new_id)`, also in `canonical_prelude` (`libs/toolkit/src/api/response.rs:17-28`):
```rust
pub fn created_json<T: Serialize>(value: T, uri: &Uri, new_id: &str) -> impl IntoResponse + use<T> {
    let location = [uri.path().trim_end_matches('/'), new_id].join("/");
    (StatusCode::CREATED, [(header::LOCATION, location)], Json(value))
}
```
* Credstore does it by hand (`gears/credstore/credstore/src/api/rest/handlers.rs:134-160`):
```rust
let location = format!("{}/{}", uri.path().trim_end_matches('/'), key.as_ref());
Ok((StatusCode::CREATED, [(axum::http::header::LOCATION, location)]).into_response())
```
* types-registry builds a `HeaderMap` with `header::LOCATION` + `RETRY_AFTER` (`types-registry/src/api/rest/handlers.rs:254-285`) and documents it with `.response_header(ResponseHeaderSpec::new("Location", ..., ResponseHeaderType::String))` (`routes.rs:209-213`).
* Note: the gateway `prefix_path` (e.g. `/cf`) is applied with `Router::nest` (`gear.rs:150`), so the handler `Uri` excludes the prefix. Use `axum::extract::OriginalUri` if the full public path is needed.

---

## 3. OData

### 3.1 Extractor (`libs/toolkit/src/api/odata.rs`)

* `pub struct OData(pub ODataQuery)` (line 438) implements `FromRequestParts` with `Rejection = CanonicalError` (469). Usage: `OData(query): OData`. It is also in `canonical_prelude`.
* Wire params, `ODataParams` (18-43): `$filter`, `$orderby`, `$select`, `limit` (alias `$top`), and `cursor` (alias `$skiptoken`). Any other `$`-prefixed key returns 400 with reason `UNSUPPORTED_QUERY_PARAM` (276-316). Non-`$` keys are ignored, so a handler can take an extra `Query<MyParams>`.
* `extract_odata_query` (334-418) runs the checks in this order:
  * filter length > 8 KiB gives `FILTER_TOO_LONG`. The filter is then parsed with `toolkit_odata::parse_filter_string`; a parse error maps to `INVALID_FILTER`. More than 2000 nodes gives `FILTER_TOO_COMPLEX`. The filter hash is stored for cursor binding.
  * `cursor` and `$orderby` together give `ORDER_WITH_CURSOR` (two violations).
  * A cursor that fails `CursorV1::decode` gives `INVALID_CURSOR`.
  * A bad `$orderby` clause gives `INVALID_ORDERBY_FIELD`. Max 10 keys, syntax `field [asc|desc]`.
  * `limit=0` gives `INVALID_LIMIT` (field `$top`).
  * A bad `$select` gives `INVALID_SELECT`.
* Limits: `MAX_FILTER_LEN=8192`, `MAX_NODES=2000`, `MAX_ORDERBY_LEN=1024`, `MAX_ORDER_FIELDS=10`, `MAX_SELECT_LEN=2048`, `MAX_SELECT_FIELDS=100` (lines 87-92).

### 3.2 Types (`libs/toolkit-odata`)

* `toolkit_odata::{Page, PageInfo}` (`src/page.rs:3-15`, re-exported as `toolkit::{Page, PageInfo}`):
  `PageInfo { next_cursor: Option<String>, prev_cursor: Option<String>, limit: u64 }` and `Page<T> { items: Vec<T>, page_info: PageInfo }`. Also `Page::new`, `Page::empty(limit)` and `Page::map_items(f)` (line 105). With feature `with-utoipa` (enabled in mini-chat), `Page<T>: ToSchema` with name `Page_<T>`.
* `ODataQuery { filter, order: ODataOrderBy, limit: Option<u64>, cursor: Option<CursorV1>, filter_hash, select }` (`src/lib.rs:472-545`). Methods: `filter()`, `has_filter()`, `selected_fields()`, `with_*`.
* `CursorV1 { k: Vec<String>, o: SortDir, s: String, f: Option<String>, d: "fwd"|"bwd" }` (`src/lib.rs:363-454`) is encoded as base64url (no padding) JSON `{"v":1,"k":[..],"o":"asc","s":"+created_at,-id","f":"<hash>","d":"fwd"}`. `validate_cursor_against` gives `ORDER_MISMATCH` / `FILTER_MISMATCH`.
* `toolkit_odata::Error` (`src/lib.rs:283-334`) converts to `CanonicalError` in `src/problem_mapping.rs`. All variants become `invalid_argument` with resource type **`gts.cf.core.odata.query.v1~`** (`src/errors.rs:5-6`, `#[resource_error(gts_id!("cf.core.odata.query.v1~"))] pub struct OdataError;`):
  * `InvalidFilter` → (`$filter`, `INVALID_FILTER`)
  * `InvalidOrderByField` → (`$orderby`, `INVALID_ORDERBY_FIELD`)
  * `InvalidCursor` and every `Cursor*` variant → (`cursor`, `INVALID_CURSOR`)
  * `OrderMismatch` → `ORDER_MISMATCH`
  * `FilterMismatch` → `FILTER_MISMATCH`
  * `InvalidLimit` → (`$top`, `INVALID_LIMIT`)
  * `OrderWithCursor`
  * `Db(_)` → internal 500
* `FieldKind` values: `String, I64, F64, Bool, Uuid, DateTimeUtc, Date, Time, Decimal` (`src/filter.rs:10-20`). There is no `I32`, even though the web doc claims one.

### 3.3 Declaring filterable / orderable fields

`#[derive(toolkit_odata_macros::ODataFilterable)]` (`libs/toolkit-odata-macros/src/lib.rs:20-44`, expansion in `odata_filterable.rs`) generates `pub enum <Struct>FilterField { CamelCaseVariants }` implementing `toolkit_odata::filter::FilterField` (`FIELDS`, `name()`, `kind()`):
```rust
#[derive(ODataFilterable)]
pub struct ChatQuery {
    #[odata(filter(kind = "Uuid"))]        pub id: uuid::Uuid,
    #[odata(filter(kind = "String"))]      pub title: String,
    #[odata(filter(kind = "DateTimeUtc"))] pub updated_at: time::OffsetDateTime,
}   // => ChatQueryFilterField::{Id, Title, UpdatedAt}
```
* mini-chat's Cargo.toml does **not** yet depend on `toolkit-odata-macros` (workspace dep `toolkit-odata-macros`, `Cargo.toml:253`). Either add it, or implement `FilterField` by hand as in `libs/toolkit-db/tests/odata_null_like.rs:73-98` (that version also overrides `nullable()`).
* Mapping to columns uses `toolkit_db::odata::{FieldToColumn, ODataFieldMapping}` (`libs/toolkit-db/src/odata/sea_orm_filter.rs:47-209`). `FieldToColumn::map_field`, an optional `map_value`, and an optional `is_orderable(field) -> bool` (false rejects `$orderby` with `INVALID_ORDERBY_FIELD`). `ODataFieldMapping` adds `type Entity`, `extract_cursor_value(&Model, F) -> sea_orm::Value` and an optional `cursor_kind`.
* Cursor value codecs support `time::OffsetDateTime` (`Value::TimeDateTimeWithTimeZone`) for `DateTimeUtc` (`sea_orm_filter.rs:419-459, 463-510`).
* OpenAPI: `.with_odata_filter::<F>()`, `.with_odata_orderby::<F>()` and `.with_odata_select()` (trait `OperationBuilderODataExt`, `operation_builder.rs:551-670`; imported in types-registry routes.rs:8-11).

### 3.4 Pagination against SeaORM (`toolkit_db::odata`)

```rust
pub async fn paginate_odata<F, M, E, D, Mapper, C>(               // sea_orm_filter.rs:589
    select: toolkit_db::secure::SecureSelect<E, Scoped>, conn: &C, query: &toolkit_odata::ODataQuery,
    tiebreaker: (&str, SortDir), limit_cfg: toolkit_db::odata::LimitCfg /* {default, max} */,
    model_to_domain: Mapper) -> Result<Page<D>, toolkit_odata::Error>
where F: FilterField, M: ODataFieldMapping<F, Entity = E>, E: EntityTrait, Mapper: Fn(E::Model) -> D, C: DBRunner;
// paginate_odata_try (630) for fallible mappers -> PaginateOdataTryError<MapErr>::{OData, MapError}
```
* The limit is clamped: `None` uses the default, `0` becomes 1, and anything above max becomes max. It fetches `limit+1` rows to detect a further page. The tiebreaker is appended to the order.
* There is also the legacy `FieldMap`-based `OPager::<E,_>::new(&scope, &conn, &FIELD_MAP).tiebreaker("id", SortDir::Desc).limits(25, 1000).fetch(&q, |m| dto)` (`libs/toolkit-db/src/odata/pager.rs:140-301`).

Executed example (`libs/toolkit-db/tests/odata_null_like.rs:217-330`):
```rust
use toolkit_db::odata::{LimitCfg, paginate_odata};
use toolkit_db::secure::{AccessScope, SecureEntityExt};
use toolkit_odata::{ODataQuery, SortDir};
let page = paginate_odata::<Field, Mapper, ent::Entity, String, _, _>(
    ent::Entity::find().secure().scope_with(&scope), &conn, &query,
    ("id", SortDir::Asc), LimitCfg { default: 50, max: 50 }, |m| m.name).await?;
```

### 3.5 Complete list-endpoint shape

No gear in this repo uses the OData extractor end to end yet. This is the canonical composition, assembled from `docs/web-docs/build-with-gears/add-pagination-odata.md` plus the real APIs above:
```rust
// routes
OperationBuilder::get("/mini-chat/v1/chats")
    .operation_id("mini_chat.list_chats").tag("Mini Chat Chats")
    .authenticated().require_license_features([&License])
    .handler(handlers::list_chats)
    .json_response_with_schema::<toolkit_odata::Page<dto::ChatDto>>(openapi, StatusCode::OK, "Page of chats")
    .with_odata_filter::<ChatQueryFilterField>()
    .with_odata_orderby::<ChatQueryFilterField>()
    .with_odata_select()   // only if $select is supported
    .error_400(openapi).error_401(openapi).error_403(openapi).error_500(openapi)
    .register(router, openapi)
// handler
use toolkit::api::canonical_prelude::*;   // ApiResult, CanonicalError, OData, JsonPage, StatusCode
pub async fn list_chats(Extension(ctx): Extension<SecurityContext>, Extension(svc): Extension<Arc<Service>>,
                        OData(query): OData) -> ApiResult<JsonPage<dto::ChatDto>> {
    let page = svc.list_chats(&ctx, &query).await?;       // DomainError -> CanonicalError via From
    Ok(Json(page.map_items(dto::ChatDto::from)))
}
// with $select: Ok(Json(toolkit::api::page_to_projected_json(&page, query.selected_fields())))  (select.rs:126)
// repo: map toolkit_odata::Error with `CanonicalError::from(e)` (or carry it in DomainError) so
//       INVALID_FILTER / INVALID_CURSOR keep resource_type gts.cf.core.odata.query.v1~
```

---

## 4. Canonical errors (`libs/toolkit-canonical-errors`)

### 4.1 Types

* `toolkit_canonical_errors::{CanonicalError, Problem, resource_error, Http, ResourceErrorBuilder, ServiceUnavailableBuilder}` (`src/lib.rs:12-32`). The crate is also re-exported in `toolkit::api::canonical_prelude::{CanonicalError, Problem, resource_error, ApiResult}`, where `ApiResult<T=()> = Result<T, CanonicalError>`.
* `CanonicalError` is a `#[non_exhaustive]` enum with 16 categories (`src/error.rs:30-157`). Category constructors are **not public**. You construct errors through a resource marker (`#[resource_error]`) or the three global constructors.

### 4.2 Declaring a resource type

```rust
use toolkit_canonical_errors::{CanonicalError, resource_error};
#[resource_error(gts_id!("cf.core.credstore.secret.v1~"))]     // gears/credstore/credstore/src/infra/sdk_error_mapping.rs:11-12
pub(crate) struct SecretResource;
// for mini-chat:  #[resource_error(gts_id!("cf.core.mini_chat.chat.v1~"))] pub struct ChatResource;
```
`gts_id!("x")` expands to `"gts." + x`. The macro validates the string at compile time and requires a unit struct (`libs/toolkit-canonical-errors-macro/src/lib.rs:73-140`). It generates (lines 140-305):

| constructor | builder state | required before `.create()` |
|---|---|---|
| `not_found(detail)` | ResourceMissing | `.with_resource(name)` |
| `already_exists(detail)` | ResourceMissing | `.with_resource(name)` |
| `data_loss(detail)` | ResourceMissing | `.with_resource(name)` |
| `aborted(detail)` | NeedsReason | `.with_reason("REASON")` |
| `permission_denied()` | NeedsReason | `.with_reason("REASON")` |
| `invalid_argument()` | NeedsFieldViolation | `.with_field_violation(field, description, reason)` (repeatable), or `.with_format(msg)` / `.with_constraint(msg)` |
| `out_of_range(detail)` | NeedsFieldViolation | `.with_field_violation(..)` |
| `failed_precondition()` | NeedsPreconditionViolation | `.with_precondition_violation(subject, description, type_)` (repeatable) |
| `resource_exhausted(detail)` | NeedsQuotaViolation | `.with_quota_violation(subject, description)` (+ `.with_quota_violation_retry_after_seconds(n)`) |
| `unknown(detail)`, `deadline_exceeded(detail)`, `unimplemented(detail)`, `cancelled()` | n/a | none |

* Optional `.with_resource(name)` on optional-resource builders and `.with_override(Http::status_code(422))` (same status class only, debug-asserted) (`src/builder.rs:400-454`).
* Global constructors (`src/builder.rs:641-677`): `CanonicalError::internal(diagnostic).create()` (the diagnostic is not on the wire; the detail is "An internal error occurred. Please retry later."), `CanonicalError::service_unavailable().with_retry_after_seconds(n).with_detail("safe text").create()`, and `CanonicalError::unauthenticated().with_reason(..).create()`.
* `failed_precondition()` and `permission_denied()` take **no custom detail**. Their detail is fixed ("Operation precondition not met" / "You do not have permission ...").

### 4.3 Status, type URI and context JSON

* HTTP mapping (`src/error.rs:614-633`):
  * invalid_argument, failed_precondition and out_of_range: 400
  * unauthenticated: 401
  * permission_denied: 403
  * not_found: 404
  * already_exists and aborted: 409
  * resource_exhausted: 429
  * cancelled: 499
  * internal, unknown and data_loss: 500
  * unimplemented: 501
  * service_unavailable: 503
  * deadline_exceeded: 504
* `type` is `gts://gts.cf.core.errors.err.v1~cf.core.err.<category>.v1~`. Titles look like "Invalid Argument", "Not Found" and "Failed Precondition".
* Context shapes (`src/context.rs`):
  * `field_violations[{field,description,reason}]`
  * failed_precondition `violations[{type,subject,description}]`
  * resource_exhausted `violations[{subject,description,retry_after_seconds?}]`
  * aborted `{reason}`, permission_denied `{reason}`
  * service_unavailable `{retry_after_seconds?}`
  * `resource_type` and `resource_name` are merged into `context` (`src/problem.rs:273-302`).
* `IntoResponse for CanonicalError` (`src/problem.rs:784-796`, feature `axum`, which mini-chat enables) renders `Problem` as `application/problem+json`. It sets the `Retry-After` header **only for service_unavailable** with `retry_after_seconds` (lines 715-749), and stashes the error in response extensions. `canonical_error_middleware` (`libs/toolkit/src/api/canonical_error_layer.rs:107+`) fills `instance` and `trace_id` and logs; the gateway installs it.

### 4.4 Real DomainError to CanonicalError mapping (`gears/credstore/credstore/src/infra/sdk_error_mapping.rs:18-80`)

```rust
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
Handlers return `ApiResult<impl IntoResponse>` and use `?` on `Result<_, DomainError>` (`handlers.rs:134-160`). For DB errors, classify `DbErr` (unique violation → Conflict, serialization failure → ServiceUnavailable, else Internal) as in `infra/canonical_mapping.rs:12-44`. There is also a blanket `From<sea_orm::DbErr> for CanonicalError` (`src/error.rs:730`).

---

## 5. Example server, config and manifests

* `apps/cf-gears-example-server/Cargo.toml` defines feature `mini-chat = ["dep:mini-chat"]` (`mini-chat = { package = "cf-gears-mini-chat", path = "../../gears/mini-chat/mini-chat", optional = true }`) and `k8s = ["mini-chat/k8s"]`. The other features used by mini-chat.yaml are `static-authn`, `static-authz`, `single-tenant`, `static-credstore` and `oagw`. `oagw` is optional (`dep:api_egress`); mini-chat pulls it in transitively through its own `oagw` dep.
* `src/registered_gears.rs:31-38` links gears with `use X as _;` so their `inventory::submit!` survive linking:
```rust
#[cfg(feature = "mini-chat")] use mini_chat as _;
#[cfg(feature = "mini-chat")] use mini_chat::infra::plugins::static_audit as _;
#[cfg(feature = "mini-chat")] use mini_chat::infra::plugins::static_model_policy as _;
```
  **These module paths must stay valid** (`pub mod infra { pub mod plugins { pub mod static_audit; pub mod static_model_policy; } }`).
* `src/main.rs:95-...`: `AppConfig::load_or_default(cli.config)` layers defaults, then YAML, then `APP__*` env, then CLI. After that it calls `toolkit::bootstrap::run_server(config)` / `run_migrate`. Commands are `run`, `check` and `migrate`, and the `--print-config`, `--list-gears` and `--dump-gears-config-yaml` flags are available.
* There is no `[[bin]]`, so the binary is named `cf-gears-example-server`. The `config/mini-chat.yaml` header comment says `--bin cf-gears-server`, which is stale.
* Config schema (`libs/toolkit/src/bootstrap/config/mod.rs:44-130`). `AppConfig { server, database: Option<GlobalDatabaseConfig>, logging, opentelemetry, gears_dir, gears: HashMap<String, Value>, vendor, oop_http }` uses `deny_unknown_fields`. Each `gears.<name>` entry is `GearConfig { database: Option<DbConnConfig>, config: Value, runtime: Option<GearRuntime>, metadata }`, also with `deny_unknown_fields`.
* `DbConnConfig` (`libs/toolkit-db/src/config.rs:67-140`): `server` (a reference to `database.servers.<name>`), `file` (relative to `home_dir/<gear>`), `path`, `engine`, `dsn`, `host`, `port`, `user`, `password`, `dbname`, `params`, `pool`.
* mini-chat's section (`config/mini-chat.yaml:145-229`):
```yaml
mini-chat:
  database: { server: "sqlite_mini_chat", file: "mini_chat.db" }
  config:
    vendor: "constructorfabric"
    orphan_watchdog: { scan_interval_secs: 10, timeout_secs: 90 }
    client_credentials: { client_id: "mini-chat", client_secret: "mini-chat-dev-secret" }
    thread_summary_worker: { enabled: true, summary_model_id: "gpt-4.1-mini", compression_threshold_pct: 80, summary_system_prompt: "..." }
    providers: { azure_openai: { kind: openai_responses, storage_kind: azure, host: "${AZURE_OPENAI_API_HOST}", api_path: ..., api_version: ..., auth_plugin_type: ..., auth_config: {header, prefix, secret_ref} } }
static-mini-chat-audit-plugin: { config: { vendor, priority: 100, enabled: true } }
static-mini-chat-model-policy-plugin: { config: { vendor, priority: 100, model_catalog: [ {id, provider_model_id, ..., preference: {is_default, sort_order}} ] } }
```
  Under `deny_unknown_fields`, every key above (and `upload_reaper`, which is commented out) must be accepted by the typed config. The same YAML also configures gears that don't exist in this repo (`simple-user-settings`, `file-parser`, `resource-group`). Check how unknown gear sections behave before relying on that file at runtime.
* `Gears.toml` (repo root) is the manifest for the external `cargo gears` CLI (lint and dylint config: `[apps.gears-rust.dev] config = "lint.yml"`, plus a skip list). The runtime does not read it.
* `gear.toml` (per gear dir, e.g. `gears/mini-chat/gear.toml`, `gears/credstore/plugins/static-credstore-plugin/gear.toml`) is catalogue metadata only: `[gear] name, description, category, is_plugin, has_plugins, has_extension_point`. No Rust code reads it.

---

## Quick gotchas checklist

1. `.authenticated()` must be followed by `.require_license_features([&License])` or `.no_license_required()`, otherwise `.register()` does not compile.
2. Use `toolkit::api::rest::extract::{Json, Path}` to get the `json_syntax_error` / `invalid_json_body` / `missing_json_content_type` / `invalid_path_params` problems. The prelude's `Json` is axum's.
3. Each `deps = [x]` entry must be a direct Cargo dep and must be in the cargo-shear ignore list.
4. Throttling zone names in code must exist in the api-gateway config, or gateway startup fails.
5. `not_found` / `already_exists` need `.with_resource(..)`. `aborted` / `permission_denied` need `.with_reason(..)`. `failed_precondition` needs at least one precondition violation.
6. The sqlite backend is used in dev: migrations need a Sqlite branch.
7. `resource_exhausted` never sets the `Retry-After` header (only `service_unavailable` does). Put the retry hint in the quota violation.
