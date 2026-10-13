# 01 — Gear lifecycle, plugins (GTS / types-registry), SDK crates, linking

Reference for implementing `mini-chat` (gear name `mini-chat`) plus two bundled plugin gears
(`static-mini-chat-model-policy-plugin`, `static-mini-chat-audit-plugin`) and a new SDK crate in
the `/app` workspace (Constructor Fabric "gears-rust").

All paths are relative to `/app` unless they are absolute. Every excerpt below was copied from code
in this repo (the source file is cited for each one). Signatures come from `libs/toolkit*`.

---

## 0. Current state of mini-chat in the repo (what already exists)

| Item | State |
|---|---|
| `gears/mini-chat/gear.toml` | exists (`name = "MiniChat"`, `is_plugin = false`, `has_plugins = true`, `has_extension_point = true`, `category = "gen-ai"`) |
| `gears/mini-chat/mini-chat/Cargo.toml` | exists: package `cf-gears-mini-chat`, `[lib] name = "mini_chat"`, feature `k8s = ["dep:kube", "dep:k8s-openapi"]`. Deps include `authn-resolver-sdk` (path), `authz-resolver-sdk`, `oagw-sdk`, `types-registry-sdk`, `types-registry`, `authn-resolver`, `authz-resolver`, `oagw`, `toolkit`, `toolkit-gts`, `gts`, `toolkit-db` (`sqlite`,`pg`), `toolkit-db-macros`, `toolkit-macros`, `toolkit-security`, `toolkit-odata`, `toolkit-canonical-errors` (`axum`), `sea-orm`, `sea-orm-migration`, `inventory`, `async-trait`, `tokio`, `tokio-util`, `anyhow`, `axum` (`macros`,`multipart`), `utoipa`, `secrecy`, ... |
| `gears/mini-chat/mini-chat/src/lib.rs` | placeholder only: `pub mod infra { pub mod plugins { pub mod static_audit {} pub mod static_model_policy {} } }` |
| Root `Cargo.toml` `members` | contains `"gears/mini-chat/mini-chat"`. **There is no `mini-chat-sdk` member or `[workspace.dependencies]` entry yet.** You must add both, e.g. `"gears/mini-chat/mini-chat-sdk"` to members and `mini-chat-sdk = { package = "cf-gears-mini-chat-sdk", version = "0.1.0", path = "gears/mini-chat/mini-chat-sdk" }` to deps |
| `apps/cf-gears-example-server/Cargo.toml` | `mini-chat = ["dep:mini-chat"]`, `k8s = ["mini-chat/k8s"]`, dep `mini-chat = { package = "cf-gears-mini-chat", path = "../../gears/mini-chat/mini-chat", optional = true }` |
| `apps/cf-gears-example-server/src/registered_gears.rs` | `#[cfg(feature = "mini-chat")] use mini_chat as _;` `use mini_chat::infra::plugins::static_audit as _;` `use mini_chat::infra::plugins::static_model_policy as _;` |
| `config/mini-chat.yaml`, `config/quickstart.yaml`, `config/e2e-local.yaml` | contain `gears: mini-chat:`, `static-mini-chat-audit-plugin:`, `static-mini-chat-model-policy-plugin:` sections (see §5) |
| `Makefile` target `mini-chat` | `cargo run --bin cf-gears-example-server --features mini-chat,static-authn,static-authz,single-tenant,static-credstore,otel -- --config config/mini-chat.yaml run` |
| `dylint.toml` | `"gears/mini-chat"` is in `excluded_paths` (DE1101 tests-in-separate-files is relaxed for everything under that dir, SDK included) |
| `Cargo.toml [workspace.metadata.cargo-shear] ignored` | already lists `authn-resolver`, `authz-resolver`, `credstore`, `grpc_hub`, `oagw`, `tenant-resolver`, `types-registry` |

Names used in `gears/mini-chat/docs/DESIGN.md` (the spec): plugin client traits
`MiniChatModelPolicyPluginClientV1` and `MiniChatAuditPluginClientV1`. Plugin spec types
`MiniChatModelPolicyPluginSpecV1` and `MiniChatAuditPluginSpecV1`. Bundled plugin gear names
`static-mini-chat-model-policy-plugin` and `static-mini-chat-audit-plugin`. Outbox queues
`mini-chat.audit` and `mini-chat.usage_snapshot`.

---

## 1. Declaring and registering a gear

### 1.1 The `#[toolkit::gear(...)]` attribute

Defined in `libs/toolkit-macros/src/lib.rs` (`pub fn gear`) and re-exported as `toolkit::gear`
(`libs/toolkit/src/lib.rs`: `pub use toolkit_macros::{ExpandVars, domain_model, gear, lifecycle};`).

Accepted arguments. Anything else is a compile error ("unknown attribute parameter"):

| Arg | Form | Meaning |
|---|---|---|
| `name` | **required** string literal | Runtime gear name. Must be kebab-case: lowercase/digits/`-`, starts with a letter, no `_`, no trailing or double `-` (`validate_kebab_case`). This name is the YAML key under `gears:` and the DB key. |
| `deps` | `[crate_ident, ...]` | **Crate identifiers** (snake_case) of gears this gear depends on. The runtime dep name is `ident.replace('_','-')` (`authn_resolver` -> `"authn-resolver"`). The macro also emits `#[cfg(not(test))] #[doc(hidden)] pub use ::authn_resolver as _gear_dep_authn_resolver;` so the dependency's `inventory::submit!` survives linking. **So every dep must be a real crate in `[dependencies]`.** You cannot list a gear that lives in the same crate. |
| `capabilities` | `[db, rest, rest_host, stateful, system, grpc_hub, grpc]` (identifiers or string literals) | Each one adds a compile-time `where Self: Trait` assertion and a registration call on the registry builder. |
| `ctor` | Rust expression (not a string) | Constructor. Default is `<T as Default>::default()`. |
| `client` | trait path | Only a compile-time object-safety check. It does **not** register anything. |
| `lifecycle(entry = "serve", stop_timeout = "30s", await_ready)` | list | Generates `impl toolkit::lifecycle::Runnable` and wraps the gear in `WithLifecycle` for the `stateful` capability. Without it, `stateful` requires a manual `impl RunnableCapability`. |

Capability -> required trait (`libs/toolkit/src/contracts.rs`):

| cap | trait the struct must implement |
|---|---|
| (always) | `toolkit::contracts::Gear` (`async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()>`) |
| `db` | `toolkit::contracts::DatabaseCapability` (`fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>>`) |
| `rest` | `toolkit::contracts::RestApiCapability` (`fn register_rest(&self, ctx: &GearCtx, router: axum::Router, openapi: &dyn OpenApiRegistry) -> anyhow::Result<axum::Router>`, plus optional `fn healthcheck(&self, ctx: &GearCtx) -> Option<Arc<dyn toolkit::Healthcheck>>`) |
| `stateful` | with `lifecycle(...)`: an inherent `async fn <entry>(self: Arc<Self>, cancel: CancellationToken[, ready: ReadySignal]) -> anyhow::Result<()>`. Without it: `RunnableCapability { async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()>; async fn stop(&self, deadline_token: CancellationToken) -> anyhow::Result<()>; }` |
| `system` | `toolkit::contracts::SystemCapability` (default `pre_init(&self, &SystemContext) -> Result<()>` and `async post_init(&self, &SystemContext) -> Result<()>`). Use an empty `impl SystemCapability for X {}`. System gears run `init` before all non-system gears. |
| `rest_host`, `grpc_hub`, `grpc` | API-gateway and gRPC hub internals. Not needed for mini-chat. |

What the macro expands to (simplified from `libs/toolkit-macros/src/lib.rs`):

```rust
#[doc(hidden)]
fn __my_gear_registrator(b: &mut ::toolkit::registry::RegistryBuilder) {
    let gear: Arc<MyGear> = Arc::new(<MyGear as Default>::default());
    b.register_core_with_meta("my-gear", &["types-registry", ...], gear.clone() as Arc<dyn ::toolkit::contracts::Gear>);
    b.register_db_with_meta("my-gear", gear.clone() as Arc<dyn ::toolkit::contracts::DatabaseCapability>);
    b.register_rest_with_meta("my-gear", gear.clone() as Arc<dyn ::toolkit::contracts::RestApiCapability>);
    // stateful + lifecycle:
    let wl = ::toolkit::lifecycle::WithLifecycle::from_arc_with_name(gear.clone(), "my-gear")
        .with_stop_timeout(<30s>).with_ready_mode(true, true, Some(__toolkit_run_ready_shim_for_my_gear));
    b.register_stateful_with_meta("my-gear", Arc::new(wl) as Arc<dyn ::toolkit::contracts::RunnableCapability>);
    b.register_system_with_meta(...); // if `system`
}
::toolkit::inventory::submit! { ::toolkit::registry::Registrator(__my_gear_registrator) }
impl MyGear { pub const MODULE_NAME: &'static str = "my-gear"; }
// + for lifecycle: impl Runnable, a ready shim fn, and `pub fn into_gear(self) -> WithLifecycle<Self>`
```

The generated code refers to `::async_trait`, `::tokio_util::sync::CancellationToken`, `::anyhow` and
`::toolkit`. The gear crate therefore needs `async-trait`, `tokio-util`, `anyhow` and `toolkit` as
direct deps when it uses `lifecycle(...)`. Gears also list `inventory` with the comment
"Required by toolkit::gear macro".

### 1.2 Discovery and phase order

`GearRegistry::discover_and_build()` (`libs/toolkit/src/registry.rs`) iterates
`inventory::iter::<Registrator>`. It then topologically sorts on `deps`. An unknown dep name fails
with `RegistryError::UnknownDependency { gear, depends_on }`, and a cycle fails with `CycleDetected`.
`gears_by_system_priority()` puts all `system` gears first and keeps topological order inside each group.

Phase order (`libs/toolkit/src/runtime/host_runtime.rs` header):
`pre_init` (system only) -> **DB migrations** (gears with `db`, run *before* init, each gear has its own
migration-history table) -> **`init`** (all gears) -> proxy-wiring -> `post_init` (system only, after
*all* inits) -> **REST wiring** (`register_rest`) -> gRPC -> **start** (stateful) ... **stop**.

Every gear compiled into the binary takes part. There is no "enabled" list. A gear without a YAML
section reads defaults via `config_or_default`. Note that `rest` gears need the `api-gateway` gear
(the rest host) to be present, and it is always linked in the example server.

### 1.3 Full gear example: gateway gear with system + db + rest + stateful

From `gears/credstore/credstore/src/gear.rs`:

```rust
use std::sync::{Arc, OnceLock};
use std::time::Duration;
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

impl Default for CredStoreGear {
    fn default() -> Self { Self { service: OnceLock::new() } }
}

impl CredStoreGear {
    pub(crate) async fn serve(self: Arc<Self>, cancel: CancellationToken, ready: ReadySignal) -> anyhow::Result<()> {
        let Some(svc) = self.service.get().cloned() else {
            anyhow::bail!("credstore: serve invoked before init");
        };
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
        let cfg: CredStoreConfig = ctx.config_or_default()?;
        cfg.validate().map_err(|err| anyhow::anyhow!("credstore config invalid: {err}"))?;

        let db_raw = ctx.db_required()?;                                   // DBProvider<DbError>
        let db: Arc<DBProvider<crate::domain::error::DomainError>> =
            Arc::new(DBProvider::new(db_raw.db()));                        // re-typed error

        let authz_client = ctx.client_hub().get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZResolverApi: {e}"))?;
        let enforcer = PolicyEnforcer::new(authz_client);
        // ... build Service ...
        let plugins = Arc::new(GtsCredStorePluginSelector::new(ctx.client_hub(), cfg.vendor.clone()));
        // ...
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

Patterns to copy:
- State lives in `OnceLock<Arc<...>>`, which is set in `init` and read in `register_rest` / `serve`. OAGW uses
  `arc_swap::ArcSwapOption<AppState>` instead. Types-registry uses `parking_lot::Mutex<Option<OutboxHandle>>`
  for things that must be `take()`n in `serve` at shutdown.
- `init` must not depend on migrated schema at type level. Migrations run before `init`, so using the DB in
  `init` is fine (types-registry starts its outbox in `init`).
- Do all fallible steps first, then commit to the `OnceLock` (see the static-credstore plugin in §2.4).

### 1.4 `GearCtx` API (`libs/toolkit/src/context.rs`)

```rust
impl GearCtx {
    pub fn new(gear_name: impl Into<Arc<str>>, instance_id: Uuid, config_provider: Arc<dyn ConfigProvider>,
               client_hub: Arc<ClientHub>, cancellation_token: CancellationToken) -> Self; // for tests
    pub fn gear_name(&self) -> &str;
    pub fn instance_id(&self) -> Uuid;
    pub fn client_hub(&self) -> Arc<toolkit::client_hub::ClientHub>;
    pub fn cancellation_token(&self) -> &CancellationToken;
    pub fn db(&self) -> Option<toolkit_db::DBProvider<toolkit_db::DbError>>;           // feature "db" (default)
    pub fn db_required(&self) -> anyhow::Result<toolkit_db::DBProvider<toolkit_db::DbError>>;
    pub fn config<T: DeserializeOwned>(&self) -> Result<T, ConfigError>;              // strict: section must exist
    pub fn config_or_default<T: DeserializeOwned + Default>(&self) -> Result<T, ConfigError>;
    pub fn config_expanded<T: DeserializeOwned + ExpandVars>(&self) -> Result<T, ConfigError>;
    pub fn config_expanded_or_default<T: DeserializeOwned + Default + ExpandVars>(&self) -> Result<T, ConfigError>;
    pub fn raw_config(&self) -> &serde_json::Value;   // gears.<name>.config or {}
    pub fn without_db(&self) -> GearCtx;
}
```

Import paths: `toolkit::GearCtx` and `toolkit::context::GearCtx` are the same type. `toolkit::Gear` and
`toolkit::contracts::Gear` are the same. `toolkit::RestApiCapability`. `toolkit::contracts::{DatabaseCapability, SystemCapability}`.

`ConfigError` variants: `GearNotFound`, `InvalidGearStructure`, `MissingConfigSection`,
`InvalidConfig { gear, cause: serde_json::Error }`, `VarExpand { gear, cause }`. The lenient loader returns
`T::default()` when the gear key, the object, or the `config` key is missing. An invalid section is still an error.

### 1.5 Config structs: `deny_unknown_fields` + defaults

All gear and plugin configs in the repo use this shape. From `gears/credstore/credstore/src/config.rs`:

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CredStoreConfig {
    pub vendor: String,
    pub hierarchy: HierarchyCfg,
    pub reaper: ReaperCfg,
}
impl Default for CredStoreConfig {
    fn default() -> Self {
        Self { vendor: "constructorfabric".to_owned(), hierarchy: HierarchyCfg::default(), reaper: ReaperCfg::default() }
    }
}
impl CredStoreConfig {
    /// # Errors
    pub fn validate(&self) -> Result<(), String> { if self.vendor.trim().is_empty() { return Err("vendor must be non-empty".to_owned()); } Ok(()) }
}
```

The default vendor everywhere is `"constructorfabric"`. The gateway's `vendor` must equal the plugin's
`vendor`, otherwise no plugin is found.

### 1.6 `${VAR}` env expansion

Helper: `toolkit::var_expand` (re-export of `toolkit_utils::var_expand`):
- `pub fn expand_env_vars(input: &str) -> Result<String, ExpandVarsError>` supports `${VAR}` (error if
  unset) and `${VAR:-default}`. It is single-pass, so values are not re-expanded.
- `pub trait ExpandVars { fn expand_vars(&mut self) -> Result<(), ExpandVarsError>; }` has impls for
  `String`, `Option<T>`, `Vec<T>`, `HashMap<K, V>`, and `secrecy::SecretString`.
- Derive: `#[derive(toolkit_macros::ExpandVars)]` (also `toolkit::ExpandVars`). Mark fields with
  `#[expand_vars]`. A nested struct field needs its own type to derive `ExpandVars` too. The generated impl
  refers to `::toolkit::var_expand`.
- Load it with `ctx.config_expanded_or_default::<T>()` or `ctx.config_expanded::<T>()`.

From `gears/credstore/plugins/static-credstore-plugin/src/config.rs`:

```rust
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct StaticCredStorePluginConfig {
    pub vendor: String,
    pub priority: i16,
    #[expand_vars]
    pub secrets: Vec<SecretConfig>,
}

#[derive(Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(deny_unknown_fields)]
pub struct SecretConfig {
    pub tenant_id: Option<Uuid>,
    pub owner_id: Option<Uuid>,
    pub key: String,
    #[expand_vars]
    pub value: String,
    pub sharing: Option<SharingMode>,
}
```

mini-chat config uses `host: "${AZURE_OPENAI_API_HOST}"`. Per DESIGN, `providers.<id>.host`,
`auth_config` and `tenant_overrides.*.host/auth_config` support `${VAR}`. Model this as
`#[expand_vars] providers: HashMap<String, ProviderEntry>` with `ProviderEntry: ExpandVars`. Only the
marked fields expand. `auth_config` is a map, so `HashMap<String, String>` works.

### 1.7 ClientHub (`libs/toolkit/src/client_hub.rs`)

```rust
pub struct ClientHub { /* RwLock maps */ }
impl ClientHub {
    pub fn register<T: ?Sized + Send + Sync + 'static>(&self, client: Arc<T>);                 // overwrites
    pub fn get<T: ?Sized + Send + Sync + 'static>(&self) -> Result<Arc<T>, ClientHubError>;
    pub fn try_get<T: ?Sized + Send + Sync + 'static>(&self) -> Option<Arc<T>>;
    pub fn register_scoped<T: ?Sized + ...>(&self, scope: ClientScope, client: Arc<T>);
    pub fn get_scoped<T: ?Sized + ...>(&self, scope: &ClientScope) -> Result<Arc<T>, ClientHubError>;
    pub fn try_get_scoped<T: ?Sized + ...>(&self, scope: &ClientScope) -> Option<Arc<T>>;
    pub fn remove<T>(&self) -> Option<Arc<T>>;  pub fn remove_scoped<T>(&self, scope: &ClientScope) -> Option<Arc<T>>;
}
pub struct ClientScope(Arc<str>);
impl ClientScope { pub fn new(scope: impl Into<Arc<str>>) -> Self; pub fn gts_id(gts_id: &str) -> Self /* "gts:" + id */; pub fn as_str(&self) -> &str; }
pub enum ClientHubError { NotFound{..}, TypeMismatch{..}, ScopedNotFound{..}, ScopedTypeMismatch{..} }
```

Keys come from `std::any::type_name::<T>()`, so always register and get with the same `dyn Trait`:
`hub.register::<dyn MiniChatClientV1>(arc)` and `hub.get::<dyn MiniChatClientV1>()?`.
Import path: `toolkit::client_hub::{ClientHub, ClientScope}`. `toolkit::ClientHub` is also re-exported.

Resolving dependencies in `init`. From `gears/system/oagw/oagw/src/gear.rs`:
```rust
let tenant_resolver = ctx.client_hub().get::<dyn TenantResolverClient>()?;
let credstore = ctx.client_hub().get::<dyn CredStoreClientV1>()?;
let authz = ctx.client_hub().get::<dyn AuthZResolverApi>()?;
let policy_enforcer = PolicyEnforcer::new(authz);
...
ctx.client_hub().register::<dyn ServiceGatewayClientV1>(oagw.clone());
```
`get` in `init` only works for clients registered by gears that are **earlier in init order**. That
means a system gear, or a gear named in `deps`. mini-chat's deps (per its Cargo deps) are
`types_registry, authn_resolver, authz_resolver, oagw`. All four are system gears, so their clients
(`TypesRegistryClient`, `AuthNResolverClient`, `AuthZResolverApi`, `ServiceGatewayClientV1`) exist when
mini-chat (non-system) runs `init`.

S2S auth (mini-chat `client_credentials`). The trait is in
`gears/system/authn-resolver/authn-resolver-sdk/src/api.rs`:
```rust
#[async_trait]
pub trait AuthNResolverClient: Send + Sync {
    async fn authenticate(&self, bearer_token: &str) -> Result<AuthenticationResult, AuthNResolverError>;
    async fn exchange_client_credentials(&self, request: &ClientCredentialsRequest) -> Result<AuthenticationResult, AuthNResolverError>;
}
pub struct ClientCredentialsRequest { pub client_id: String, pub client_secret: secrecy::SecretString, pub scopes: Vec<String> }
// AuthenticationResult has `security_context`
```
The static-authn plugin maps these with its `s2s_credentials: [{client_id, client_secret}]` config.

---

## 2. Plugin pattern (gateway + plugin gears via types-registry / GTS)

### 2.1 Building blocks

| Piece | Location | Signature / use |
|---|---|---|
| Base plugin type | `libs/toolkit-gts/src/plugin.rs`, re-exported as `toolkit::gts::PluginV1` and `toolkit_gts::PluginV1` | `pub struct PluginV1<P: gts::GtsSchema> { pub id: GtsInstanceId, pub vendor: String, pub priority: i16, pub properties: P }`, GTS type `gts.cf.toolkit.plugins.plugin.v1~` |
| Registration helper | same | `impl<P: GtsSchema + GtsSerialize + Default> PluginV1<P> { pub fn build_registration(instance_segment: &str, vendor: impl Into<String>, priority: i16) -> serde_json::Result<(GtsInstanceId, serde_json::Value)> }`. id = `P::TYPE_ID + instance_segment` |
| Selector cache | `libs/toolkit/src/plugins/mod.rs`, `toolkit::plugins::GtsPluginSelector` (also `toolkit::GtsPluginSelector`) | `new()`, `pre_cached(String)`, `async fn get_or_init<F,Fut,E>(&self, resolve: F) -> Result<Arc<str>, E>` (single-flight, **caches only Ok**), `async fn reset(&self) -> bool` |
| Chooser | `toolkit::plugins::choose_plugin_instance` | `pub fn choose_plugin_instance<'a, P>(vendor: &str, instances: impl IntoIterator<Item=(&'a str, &'a serde_json::Value)>) -> Result<String, ChoosePluginError> where P: for<'de> gts::GtsDeserialize<'de> + gts::GtsSchema`. Deserializes each as `PluginV1<P>`, checks `content.id == gts_id`, filters `vendor`, and picks the **lowest `priority`** |
| Chooser errors | `toolkit::plugins::ChoosePluginError` | `InvalidPluginInstance { gts_id, reason }`, `PluginNotFound { type_id, vendor }` |
| Schema macro | `toolkit_gts::gts_type_schema` (wraps upstream `gts_macros::struct_to_gts_schema` and submits `InventoryTypeSchema` to `inventory`) | args: `dir_path`, `type_id`, `description`, `properties`, `base` (all required by upstream), optional `traits_schema`, `traits`, `gts_abstract` |
| ID macro | `toolkit_gts::gts_id!("<suffix>")` | prepends `gts.`. Gives a `&'static str` const |
| Instance macros | `toolkit_gts::{gts_instance!, gts_instance_raw!}` | static instances submitted to inventory. Only `libs/toolkit-gts` uses them in-tree. Plugins in this repo register **at runtime** instead (below) |
| Generated by schema macro | on the struct | `pub fn gts_type_id() -> &'static ::gts::gts::GtsTypeId`, `pub fn gts_make_instance_id(segment: &str) -> ::gts::GtsInstanceId`, `const TYPE_ID: &str` (via `GtsSchema`) |
| Registry client | `types_registry_sdk::TypesRegistryClient` | `async fn register(&self, entities: Vec<serde_json::Value>) -> Result<Vec<RegisterResult>, CanonicalError>`, `async fn list_instances(&self, query: InstanceQuery) -> Result<Vec<GtsInstance>, CanonicalError>` |
| Helpers | `types_registry_sdk::{RegisterResult, InstanceQuery, GtsInstance}` | `RegisterResult::ensure_all_ok(&results) -> Result<(), CanonicalError>`. `InstanceQuery::new().with_pattern(format!("{type_id}*"))`. `GtsInstance { id: GtsInstanceId, object: serde_json::Value, .. }` |

How schemas reach types-registry: `types-registry` `init` calls
`toolkit_gts::all_inventory_type_schemas()` and `all_inventory_instances()` and registers them. Every
`#[gts_type_schema]` in any linked crate (for example the new SDK) is registered automatically. No
`register` call is needed for the spec type. In `post_init` types-registry switches to "ready" mode and
validates everything (`gears/system/types-registry/types-registry/src/gear.rs`). Plugin *instances* are
registered by the plugin gears in their own `init` with `registry.register(vec![payload])`.

### 2.2 SDK side: plugin spec + plugin client trait

Spec. From `gears/system/authn-resolver/authn-resolver-sdk/src/gts.rs` (credstore, authz and tenant-resolver are identical in shape):

```rust
use toolkit::gts::PluginV1;
use toolkit_gts::gts_type_schema;

#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.authn_resolver.plugin.v1~"),
    description = "AuthN Resolver plugin specification",
    properties = "",
)]
pub struct AuthNResolverPluginSpecV1;
```
(Inside `#[gts_type_schema(...)]` the bare `gts_id!(...)` is recognised by the wrapper, so no import is
needed there. credstore-sdk writes `use toolkit_gts::{gts_id, gts_type_schema};` because it also uses
`gts_id!` in a `const`.) Spec structs must be unit structs with `#[derive(Default)]` so that
`build_registration` can build `P::default()`. No `schemas/` directory exists on disk in this repo, and
none is needed.

Plugin client trait. From `gears/credstore/credstore-sdk/src/plugin_api.rs`:

```rust
use async_trait::async_trait;
use toolkit_security::SecurityContext;

#[async_trait]
pub trait CredStorePluginClientV1: Send + Sync {
    async fn get(&self, ctx: &SecurityContext, tenant_id: &TenantId, key: &SecretRef, owner_id: Option<&OwnerId>)
        -> Result<Option<SecretValue>, CredStoreError>;
    async fn put(&self, ctx: &SecurityContext, tenant_id: &TenantId, key: &SecretRef, value: SecretValue, owner_id: Option<&OwnerId>)
        -> Result<(), CredStoreError>;
    async fn delete(&self, ctx: &SecurityContext, tenant_id: &TenantId, key: &SecretRef, owner_id: Option<&OwnerId>)
        -> Result<(), CredStoreError>;
}
```

Applied to mini-chat. Proposed SDK file `gears/mini-chat/mini-chat-sdk/src/gts.rs`. The GTS segment format is
`<vendor>.<package>.<namespace>.<type>.v<N>`, with `_` when no namespace is needed (guidelines/GTS.md §3.3):

```rust
use toolkit::gts::PluginV1;          // or toolkit_gts::PluginV1 if the SDK avoids depending on `toolkit`
use toolkit_gts::gts_type_schema;

#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~"),
    description = "Mini-Chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~"),
    description = "Mini-Chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;
```
(The exact type ids above are a proposal. The spec docs only name the Rust types. Existing ids follow
`cf.core.<gear>.plugin.v1~`, but mini-chat has two plugin kinds, so they need distinct segments.)

Plugin trait sketch (method set comes from DESIGN.md §B.2 / §4355: policy snapshot, user limits,
`publish_usage`, `check_user_license` with a default body. Audit plugin: receive an audit event):

```rust
#[async_trait::async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    async fn get_current_policy_version(&self, ...) -> Result<..., MiniChatModelPolicyPluginError>;
    async fn get_policy_snapshot(&self, ...) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError>;
    async fn get_user_limits(&self, ...) -> Result<UserLimits, MiniChatModelPolicyPluginError>;
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), MiniChatModelPolicyPluginError>;
    async fn check_user_license(&self, ...) -> Result<..., ...> { /* default: active=false */ }
}
#[async_trait::async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    async fn emit(&self, event: AuditEvent) -> Result<(), MiniChatAuditPluginError>;
}
```

### 2.3 Gateway side: lazy plugin selection by vendor

Selector. From `gears/credstore/credstore/src/infra/plugin_select.rs`. The authn-resolver
`domain/service.rs` is the same, minus the error mapping:

```rust
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};

pub struct GtsCredStorePluginSelector { hub: Arc<ClientHub>, vendor: String, selector: GtsPluginSelector }

impl GtsCredStorePluginSelector {
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self { Self { hub, vendor, selector: GtsPluginSelector::new() } }

    async fn resolve_instance(&self) -> Result<String, DomainError> {
        let registry = self.hub.get::<dyn TypesRegistryClient>().map_err(/* -> ServiceUnavailable */)?;
        let type_id = CredStorePluginSpecV1::gts_type_id();
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await.map_err(/* -> ServiceUnavailable */)?;
        let gts_id = choose_plugin_instance::<CredStorePluginSpecV1>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        ).map_err(|e| match e {
            toolkit::plugins::ChoosePluginError::PluginNotFound { vendor, .. } => DomainError::ServiceUnavailable { .. },
            toolkit::plugins::ChoosePluginError::InvalidPluginInstance { gts_id, reason } => DomainError::Internal { .. },
        })?;
        Ok(gts_id)
    }
}

#[async_trait]
impl PluginSelector for GtsCredStorePluginSelector {
    async fn resolve(&self) -> Result<Arc<dyn CredStorePluginClientV1>, DomainError> {
        let instance_id = self.selector.get_or_init(|| self.resolve_instance()).await?;
        let scope = ClientScope::gts_id(instance_id.as_ref());
        self.hub.try_get_scoped::<dyn CredStorePluginClientV1>(&scope)
            .ok_or_else(|| DomainError::ServiceUnavailable { /* "plugin client not registered yet" */ .. })
    }
}
```

Domain port that hides ClientHub and types-registry (`gears/credstore/credstore/src/domain/ports/plugin.rs`):
```rust
#[async_trait]
pub trait PluginSelector: Send + Sync {
    async fn resolve(&self) -> Result<Arc<dyn CredStorePluginClientV1>, DomainError>;
}
```

Rules:
- Resolve **lazily** on first use, never in the gateway's `init`. types-registry only becomes "ready" in
  `post_init`, and plugin gears may `init` after the gateway. For mini-chat this is required: the plugins
  are in the same crate, so they **cannot** be listed in `deps`, and init order between them is not
  defined.
- `GtsPluginSelector::get_or_init` caches only a successful id. A `PluginNotFound` is retried on the
  next call, which matches DESIGN's "no-plugin is not cached" rule for audit. Call `selector.reset().await`
  when the cached instance's scoped client is missing (DESIGN: "cached instance id is reset" and the
  handler returns `Retry`).
- authn-resolver throttles the "client not registered yet" warning with
  `toolkit::telemetry::ThrottledLog::new(Duration)` and `.should_log()`.
- Domain structs in `domain/` need `#[toolkit_macros::domain_model]` (DE0309, see §6). authn-resolver's
  `Service` holding `Arc<ClientHub>` + `GtsPluginSelector` is annotated `#[domain_model]`.

### 2.4 Plugin side: register instance + scoped client

From `gears/credstore/plugins/static-credstore-plugin/src/gear.rs`. static-authn, static-authz and
single-tenant follow the same pattern:

```rust
use std::sync::{Arc, OnceLock};
use async_trait::async_trait;
use credstore_sdk::{CredStorePluginClientV1, CredStorePluginSpecV1};
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use tracing::info;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

#[toolkit::gear(
    name = "static-credstore-plugin",
    deps = [types_registry]
)]
pub struct StaticCredStorePlugin {
    service: OnceLock<Arc<Service>>,
}

impl Default for StaticCredStorePlugin {
    fn default() -> Self { Self { service: OnceLock::new() } }
}

#[async_trait]
impl Gear for StaticCredStorePlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticCredStorePluginConfig = ctx.config_expanded_or_default()?;
        let service = Arc::new(Service::from_config(&cfg)?);           // validate before registering

        let (instance_id, instance_json) = PluginV1::<CredStorePluginSpecV1>::build_registration(
            "cf.core._.static_credstore.v1",                            // instance segment
            cfg.vendor.clone(),
            cfg.priority,
        )?;

        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        self.service.set(service.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let api: Arc<dyn CredStorePluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn CredStorePluginClientV1>(ClientScope::gts_id(&instance_id), api);
        info!(instance_id = %instance_id);
        Ok(())
    }
}
```

Instance segments used in the repo: `"cf.builtin.static_authn_resolver.plugin.v1"`,
`"cf.builtin.static_authz_resolver.plugin.v1"`, `"cf.builtin.single_tenant_resolver.plugin.v1"`,
`"cf.core._.static_credstore.v1"`. For mini-chat, use for example
`"cf.core._.static_mini_chat_model_policy.v1"` and `"cf.core._.static_mini_chat_audit.v1"`.

Plugin config shape. From `gears/system/authn-resolver/plugins/static-authn-plugin/src/config.rs`:
```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticAuthNPluginConfig {
    pub vendor: String,     // default "constructorfabric"
    pub priority: i16,      // default 100 (lower = preferred)
    pub mode: AuthNMode,
    ...
}
```
The DESIGN for `static-mini-chat-audit-plugin` is `{ enabled: bool = true, vendor, priority }` with
`deny_unknown_fields`. For `static-mini-chat-model-policy-plugin` it is `{ vendor, priority,
model_catalog: Vec<ModelCatalogEntry> (required key when the section is present), kill_switches
(deny_unknown_fields, all default false), default_standard_limits, default_premium_limits }`.

### 2.5 Minimal checklist for a new plugin kind

1. SDK: `#[derive(Default)] #[gts_type_schema(base = PluginV1, type_id = gts_id!("cf.toolkit.plugins.plugin.v1~<...>.v1~"), properties = "", dir_path = "schemas", description = "...")] pub struct XPluginSpecV1;`
2. SDK: `#[async_trait] pub trait XPluginClientV1: Send + Sync { ... }`, plus models and error types. Re-export them at the crate root.
3. Gateway: hold `GtsPluginSelector` + `Arc<ClientHub>` + `vendor`. Resolve lazily with `list_instances` -> `choose_plugin_instance::<XPluginSpecV1>` -> `try_get_scoped::<dyn XPluginClientV1>(&ClientScope::gts_id(id))`.
4. Plugin gear: `#[toolkit::gear(name = "...", deps = [types_registry])]`. In `init`, call `PluginV1::<XPluginSpecV1>::build_registration(segment, vendor, priority)`, then `registry.register(vec![json])` + `RegisterResult::ensure_all_ok`, then `register_scoped::<dyn XPluginClientV1>(ClientScope::gts_id(&instance_id), api)`.
5. YAML: `gears.<plugin-name>.config.{vendor, priority, ...}` and `gears.<gateway>.config.vendor` must match.

---

## 3. SDK crate conventions

### 3.1 Cargo.toml shape

`gears/credstore/credstore-sdk/Cargo.toml`:
```toml
[package]
name = "cf-gears-credstore-sdk"
description = "SDK for credstore gear: API traits, models, and error definitions"
version = "0.2.17"
edition.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
rust-version.workspace = true
readme = "README.md"                 # if declared, the file MUST exist (check_packaging_metadata.py)
keywords = ["constructorfabric", "cf-gears", "cf-gears-system"]
categories = ["security"]
metadata.docs.rs.all-features = true

[lib]
name = "credstore_sdk"

[lints]
workspace = true

[dependencies]
async-trait = { workspace = true }
thiserror = { workspace = true }
uuid = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
time = { workspace = true }
schemars = { workspace = true }      # needed by #[gts_type_schema] expansion
gts = { workspace = true }           # needed by #[gts_type_schema] expansion (::gts::...)
gts-macros = { workspace = true }
toolkit-gts = { workspace = true }
toolkit = { workspace = true }       # for toolkit::gts::PluginV1 (can use toolkit_gts::PluginV1 instead)
toolkit-security = { workspace = true }  # SecurityContext in trait signatures

[features]
test-util = []
```
Minimal set for a GTS-schema SDK (authn-resolver-sdk and tenant-resolver-sdk): `async-trait`,
`thiserror`, `serde`, `serde_json`, `schemars`, `gts`, `toolkit-gts`, `toolkit`, `toolkit-security`.
`oagw-sdk` shows that an SDK can avoid `toolkit` and use only `toolkit-gts`, `toolkit-security` and
`toolkit-canonical-errors`.

Conventions:
- The package name is `cf-gears-<gear>-sdk` and `[lib] name` is `<gear_snake>_sdk`. For mini-chat that is
  package `cf-gears-mini-chat-sdk` and lib `mini_chat_sdk`, in directory `gears/mini-chat/mini-chat-sdk/`.
- `[lints] workspace = true`. Publishable crates need a `version` on every normal dependency: either
  `workspace = true` with a versioned workspace entry, or `path` + `version`. A path-only dependency
  fails `check_packaging_metadata.py`. If `readme = "README.md"` is declared, the file must exist.
  mini-chat's Cargo.toml declares no `readme`, and lib.rs `#![doc = include_str!("../README.md")]` also
  requires the file.
- `gears/**` crates must have **no** entry in `release-plz.toml` (`check_release_config.py`).

### 3.2 What goes in an SDK

`gears/credstore/credstore-sdk/src/lib.rs`:
```rust
#![doc = include_str!("../README.md")]
pub mod api;        // public client trait(s) used via ClientHub (e.g. CredStoreClientV1)
pub mod error;      // transport-agnostic error enum (thiserror)
pub mod gts;        // GTS schema types: plugin spec(s), resource types
pub mod models;     // transport-agnostic models
pub mod plugin_api; // plugin client trait(s) (CredStorePluginClientV1)
#[cfg(feature = "test-util")]
pub mod test_util;
pub mod types;

pub use api::CredStoreClientV1;
pub use error::CredStoreError;
pub use gts::{CredStorePluginSpecV1, SECRET_RESOURCE_TYPE, SecretTypeTraits, SecretV1};
pub use models::{...};
pub use plugin_api::CredStorePluginClientV1;
```
authn-resolver-sdk uses the same split: `api` (`AuthNResolverClient`), `plugin_api`
(`AuthNResolverPluginClient`), `gts` (`AuthNResolverPluginSpecV1`), `models`, `error`.
The doc `docs/toolkit_unified_system/02_gear_layout_and_sdk_pattern.md` says to keep REST DTOs
(serde/utoipa) out of the SDK. However, credstore and authn SDK models do derive serde. DESIGN.md
expects serde on SDK types like `ModelCatalogEntry` and `UsageEvent` (JSON payloads, plugin config).
Client traits end in `V1` (DE0504 client-versioning lint, currently skipped in Gears.toml).

Resource-type constants. From `gears/credstore/credstore-sdk/src/gts.rs`:
```rust
pub const SECRET_RESOURCE_TYPE: &str = gts_id!("cf.core.credstore.secret.v1~");
```
For mini-chat: chat `gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~`, model `gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~`, user quota `gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~` (DESIGN.md ~line 2523-2543). Use
`gts_id!` rather than raw `"gts...."` strings (DE0901 gts-string-pattern lint).

### 3.3 How the gear crate depends on its SDK

Workspace entry: `credstore-sdk = { package = "cf-gears-credstore-sdk", version = "0.2.17", path = "gears/credstore/credstore-sdk" }`.
Gear crate: `credstore-sdk = { workspace = true }`. Plugin crate (path form):
`credstore-sdk = { package = "cf-gears-credstore-sdk", version = "0.2.17", path = "../../credstore-sdk" }`.
OAGW's `lib.rs` re-exports SDK items: `pub use oagw_sdk::{CreateRouteRequest, ..., api::ServiceGatewayClientV1};`.
The gear registers its local impl as `dyn SdkTrait` in `init` (§1.7).

---

## 4. Example server linking and inventory registration

`apps/cf-gears-example-server/src/registered_gears.rs`:
```rust
// This file is used to ensure that all gears are linked and registered via inventory
#![allow(unused_imports)]
#[cfg(feature = "oagw")]
use api_egress as _;
use api_gateway as _;
...
#[cfg(feature = "mini-chat")]
use mini_chat as _;
#[cfg(feature = "mini-chat")]
use mini_chat::infra::plugins::static_audit as _;
#[cfg(feature = "mini-chat")]
use mini_chat::infra::plugins::static_model_policy as _;
```
`main.rs` has `mod registered_gears;`. `toolkit::bootstrap::run_server` calls
`GearRegistry::discover_and_build()`, which walks `inventory::iter::<Registrator>`. Any `#[toolkit::gear]`
in a linked crate is registered. `use x as _;` forces the crate to link. The two extra `use` lines only
need `mini_chat::infra::plugins::static_audit` and `...::static_model_policy` to be **public modules**.
The inventory submissions inside them are already linked through `use mini_chat as _`.

Features (example server `Cargo.toml`): `mini-chat = ["dep:mini-chat"]` and `k8s = ["mini-chat/k8s"]`, so the
mini-chat crate must keep a `k8s` feature. The binary is `cf-gears-example-server`.

### 4.1 Multiple gears in one crate

**No crate in the repo does this today.** Each plugin is its own crate (`static-credstore-plugin`,
`static-authn-plugin`, ...). The macro supports it:
- Each `#[toolkit::gear]` emits its own `fn __<struct_snake>_registrator` and its own `inventory::submit!`,
  plus `impl <Struct> { pub const MODULE_NAME }`. Separate structs in separate modules do not collide.
- **Gotcha:** `deps = [types_registry]` emits `pub use ::types_registry as _gear_dep_types_registry;` in
  the module where the struct is declared. Two gears with the same dep **in the same module** would
  define that alias twice and fail to compile. Put each gear in its own module:
  `crate::gear` (main), `crate::infra::plugins::static_audit` (audit plugin gear),
  `crate::infra::plugins::static_model_policy` (policy plugin gear).
- The main gear cannot list its in-crate plugins in `deps`. `deps` takes crate idents, and the plugin
  modules are not crates. Use lazy resolution (§2.3).
- The runtime names must be unique: `mini-chat`, `static-mini-chat-model-policy-plugin`,
  `static-mini-chat-audit-plugin`.

Suggested layout inside `gears/mini-chat/mini-chat/src/`:
```
lib.rs                      pub mod gear; pub mod config; pub mod api; pub mod domain; pub mod infra;
gear.rs                     #[toolkit::gear(name = "mini-chat", deps = [types_registry, authn_resolver, authz_resolver, oagw],
                                            capabilities = [db, rest, stateful], lifecycle(entry = "serve", stop_timeout = "30s", await_ready))]
infra/plugins/mod.rs        pub mod static_audit; pub mod static_model_policy;
infra/plugins/static_audit/mod.rs         #[toolkit::gear(name = "static-mini-chat-audit-plugin", deps = [types_registry])] pub struct StaticAuditPlugin {...}
infra/plugins/static_model_policy/mod.rs  #[toolkit::gear(name = "static-mini-chat-model-policy-plugin", deps = [types_registry])] pub struct StaticModelPolicyPlugin {...}
```
Each plugin module can have its own `config.rs` / `service.rs` submodules.

---

## 5. YAML config keys, DB handle, migrations, outbox

### 5.1 YAML layout (`config/mini-chat.yaml`)

```yaml
database:
  servers:
    sqlite_mini_chat:
      engine: "sqlite"
      params: { WAL: "true", synchronous: "NORMAL", busy_timeout: "5000" }
      pool: { max_conns: 5, acquire_timeout: "30s" }

logging:
  mini_chat: { console_level: debug, file: "logs/mini-chat.log", file_level: debug }   # key = tracing target (crate name)

gears:
  mini-chat:                       # == #[toolkit::gear(name = "mini-chat")]
    database:
      server: "sqlite_mini_chat"   # reference to database.servers.<name>
      file: "mini_chat.db"         # sqlite file under home_dir/<gear>
    config:                        # -> ctx.config*::<MiniChatConfig>()
      vendor: "constructorfabric"
      orphan_watchdog: { scan_interval_secs: 10, timeout_secs: 90 }
      client_credentials: { client_id: "mini-chat", client_secret: "mini-chat-dev-secret" }
      thread_summary_worker: { enabled: true, summary_model_id: "gpt-4.1-mini", compression_threshold_pct: 80, summary_system_prompt: "..." }
      providers:
        azure_openai:
          kind: openai_responses
          storage_kind: azure
          host: "${AZURE_OPENAI_API_HOST}"
          api_path: "/openai/v1/responses"
          api_version: "2025-03-01-preview"
          auth_plugin_type: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
          auth_config: { header: "api-key", prefix: "", secret_ref: "azure-openai-key" }
  static-mini-chat-audit-plugin:
    config: { vendor: "constructorfabric", priority: 100, enabled: true }
  static-mini-chat-model-policy-plugin:
    config:
      vendor: "constructorfabric"
      priority: 100
      model_catalog: [ { id: "gpt-4.1", provider_model_id: "gpt-4.1", ..., tier: Premium, ... } ]
```
The api-gateway section of the same file defines the throttling zones `rl_mini_chat_chat` and
`ifl_mini_chat_chat`. The comment says these "must be defined or gateway startup fails validation".
mini-chat's `create_chat` operation references them through
`OperationBuilder::...with_throttling(toolkit::api::ThrottlingSpec { rate_limit_zone: Some("rl_mini_chat_chat".into()), in_flight_limit_zone: Some("ifl_mini_chat_chat".into()), require_security_context: false, dry_run: true })`
(per the `config/quickstart.yaml` comments).

Per-gear `database:` fields (`toolkit_db::DbConnConfig`, `libs/toolkit-db/src/config.rs`): `engine`, `dsn`,
`host`, `port`, `user`, `password`, `dbname`, `params`, `file`, `path`, `pool`, `lock_keepalive`, `server`.
The runtime looks the DB up by gear name: `DbManager::get(gear_name)`.

### 5.2 DB handle in the gear

- `ctx.db() -> Option<DBProvider<DbError>>` and `ctx.db_required() -> anyhow::Result<DBProvider<DbError>>`.
- `DBProvider<E>` (`libs/toolkit-db/src/db_provider.rs`): `new(db: Db)`, `db() -> Db`,
  `conn() -> Result<DbConn<'_>, E>`, `async transaction(..)`, `async transaction_with_config(..)`.
- To re-type to a domain error, use `Arc::new(DBProvider::<DomainError>::new(ctx.db_required()?.db()))`
  (credstore). Requires `impl From<DbError> for DomainError`.
- Secure ORM entities (`toolkit_db::secure::Scopable`). From `gears/credstore/credstore/src/infra/storage/entity/secrets.rs`:
  ```rust
  #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
  #[sea_orm(table_name = "credstore_secrets")]
  #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
  pub struct Model { #[sea_orm(primary_key, auto_increment = false)] pub id: Uuid, pub tenant_id: Uuid, ... }
  ```
- If the DB is optional (types-registry), check `if let Some(db) = ctx.db()`. If a `db` gear has no
  DB configured, the runtime only logs debug and skips migrations.

### 5.3 Migrations (sea-orm-migration `MigratorTrait`)

`gears/credstore/credstore/src/infra/storage/migrations.rs`:
```rust
use sea_orm_migration::prelude::*;
pub mod m0001_initial_schema;
pub struct Migrator;
#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> { vec![Box::new(m0001_initial_schema::Migration)] }
}
```
`m0001_initial_schema.rs`:
```rust
use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;
#[derive(DeriveMigrationName)]
pub struct Migration;
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        let conn = manager.get_connection();
        let sql = match backend {
            sea_orm::DatabaseBackend::Postgres => "CREATE TABLE IF NOT EXISTS ...",
            sea_orm::DatabaseBackend::Sqlite => "CREATE TABLE IF NOT EXISTS ...",
            sea_orm::DatabaseBackend::MySql => return Err(DbErr::Custom(MYSQL_NOT_SUPPORTED.to_owned())),
        };
        conn.execute_unprepared(sql).await?;
        Ok(())
    }
}
```
Hookup: `impl DatabaseCapability { fn migrations(&self) -> Vec<Box<dyn MigrationTrait>> { use sea_orm_migration::MigratorTrait; Migrator::migrations() } }`.
The runtime runs them before `init` through `toolkit_db::migration_runner::run_migrations_for_gear(db, gear_name, migrations)`
with a per-gear history table. `cargo run ... -- migrate` runs only `pre_init` + migrations.

### 5.4 Outbox (platform transactional outbox: `toolkit_db::outbox`)

Migrations are owned by the toolkit and appended to the gear's own list. From
`gears/system/types-registry/types-registry/src/gear.rs`:
```rust
impl DatabaseCapability for TypesRegistryGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        use sea_orm_migration::MigratorTrait;
        let mut migrations = crate::infra::storage::Migrator::migrations();
        let outbox = match toolkit_db::outbox::outbox_migrations_with_prefix(OUTBOX_TABLE_PREFIX) {
            Ok(outbox) => outbox,
            Err(e) => panic!("types-registry outbox migration prefix '{OUTBOX_TABLE_PREFIX}' is invalid: {e}"),
        };
        migrations.extend(outbox);
        migrations
    }
}
```
API (`libs/toolkit-db/src/outbox/mod.rs` re-exports):
- `pub fn outbox_migrations() -> Vec<Box<dyn MigrationTrait>>` (default table names) and
  `pub fn outbox_migrations_with_prefix(prefix: impl Into<String>) -> Result<Vec<Box<dyn MigrationTrait>>, OutboxError>`.
  The prefix must be a portable SQL identifier, for example `"types_registry__outbox"`. For mini-chat use
  something like `"mini_chat__outbox"`. Use the same prefix at runtime.
- Start (in `init`, after the DB exists). From `types-registry/src/infra/outbox.rs`:
  ```rust
  let handle: OutboxHandle = Outbox::builder(db /* toolkit_db::Db */)
      .table_prefix(TABLE_PREFIX)?                  // -> Result<OutboxBuilder, OutboxError>
      .profile(OutboxProfile::low_latency())        // or OutboxProfile::default_profile()
      .processor_tuning(WorkerTuning::processor_low_latency().batch_size(1))
      .queue(QUEUE, Partitions::of(8))              // power of 2 in 1..=64 (panics otherwise)
          .leased(handler)                          // impl LeasedHandler; or .transactional(..)
          .lease(LeaseConfig { duration, headroom })
      // .queue("mini-chat.usage_snapshot", Partitions::of(4)).leased(h2)  // chain more queues
      .start().await?;
  // keep `handle` (e.g. Mutex<Option<OutboxHandle>>); `handle.outbox() -> &Arc<Outbox>`; on shutdown `handle.stop().await`
  ```
  Queue names match `[a-zA-Z0-9._-]{1,1024}` and must start and end alphanumeric, so `mini-chat.audit`
  is valid. Payloads are at most 64 KiB.
- Enqueue inside a business transaction: `outbox.enqueue(tx, Record::to(queue, partition_u32).payload(bytes, "payload.type").build()?) .await? -> Wake`.
  Fire the `Wake` after commit. `toolkit_db::outbox::in_transaction(&db, |tx| Box::pin(async move { ...; Ok((value, wake)) }))`
  does commit-then-fire for you.
- Handlers (all `#[async_trait]`):
  ```rust
  pub trait LeasedHandler: Send + Sync { async fn handle(&self, batch: &mut Batch<'_>) -> HandlerResult; }
  pub trait LeasedMessageHandler: Send + Sync { async fn handle(&self, msg: &OutboxMessage) -> MessageResult; } // blanket -> LeasedHandler
  pub trait TransactionalHandler: Send + Sync { async fn handle(&self, txn: &DatabaseExecutor<'_>, msgs: &[OutboxMessage]) -> HandlerResult; }
  pub enum HandlerResult { Success, Retry { reason: String }, Reject { reason: String } }
  pub enum MessageResult { Ok, Retry, Reject(String) }
  pub struct OutboxMessage { pub partition_id: i64, pub seq: i64, pub payload: Vec<u8>, pub payload_type: String, pub created_at: chrono::DateTime<Utc>, pub attempts: i16, .. }
  // Batch: next_msg(), ack(), reject(String), remaining() -> Duration, should_stop() -> bool
  ```
  DESIGN's "bounded Retry: dead-letter on the 120th attempt" maps to checking `msg.attempts` in the
  handler and returning `MessageResult::Reject(..)`. Attempts are counted per partition batch, so use
  `batch_size(1)` for a per-message budget, as types-registry does.
- Stop the outbox in the lifecycle `serve` after `cancel.cancelled().await`. From types-registry:
  ```rust
  pub(crate) async fn serve(self: Arc<Self>, cancel: CancellationToken, ready: ReadySignal) -> anyhow::Result<()> {
      ready.notify();
      cancel.cancelled().await;
      let admission = self.outbox.lock().take();          // parking_lot::Mutex, released before await
      if let Some(admission) = admission { admission.stop().await; }
      Ok(())
  }
  ```

---

## 6. gear.toml, Gears.toml, lints and validation a new gear must pass

- **`gear.toml`** (one per gear *directory*, consumed by the external `cargo-gears` CLI; nothing in-repo parses it):
  ```toml
  [gear]
  name = "MiniChat"
  description = "..."
  category = "gen-ai"
  is_plugin = false
  has_plugins = true
  has_extension_point = true
  ```
  Separate plugin crates have their own (static-credstore-plugin: `is_plugin = true`, `has_plugins = false`,
  `has_extension_point = false`). Not every plugin has one: static-authn-plugin has none. mini-chat's
  in-crate plugins need no extra gear.toml. The existing `gears/mini-chat/gear.toml` already covers them.
- **`Gears.toml`** (repo root): `[apps.gears-rust.dev] config = "lint.yml"`, lint fmt and clippy off,
  `dylint.enabled = true`. It skips `de0504_client_versioning`, `de1101_tests_in_separate_files`,
  `de0101_no_serde_in_contract` and `de0102_no_toschema_in_contract`. It does not list gears.
- **`make dylint`** = `cargo gears lint --dylint` (architecture lints; config in `dylint.toml`). Lint names
  referenced in the repo: `de0205_operation_builder` (use `OperationBuilder` for REST),
  `de0301_no_infra_in_domain`, `de0309_must_have_domain_model` (every non-private struct or enum in a
  `domain/` path needs `#[toolkit_macros::domain_model]`, which also rejects sqlx/sea_orm/http/axum/hyper/
  reqwest/tonic/fs field types), `de0706_no_direct_sqlx`, `de0803_api_snake_case`,
  `de0901_gts_string_pattern` (use `gts_id!`), `de1301_no_print_macros`,
  `de1302_error_from_to_string` (repo uses `#[allow(unknown_lints, de1302_error_from_to_string)]` where needed),
  and DE0708 (no sha2/sha1/md5 outside the allow-list; use `aws-lc-rs`). `dylint.toml` excludes
  `gears/mini-chat` from DE1101, so inline tests up to any size are fine there. Elsewhere the limit is
  100 lines, and the repo uses `#[cfg(test)] #[path = "x_tests.rs"] mod x_tests;`.
- **`make validate-gear-names`** (`tools/scripts/validate_gear_names.py`): directories directly under
  `gears/` must be kebab-case. `mini-chat` passes.
- **`make check-packaging-metadata`**: declared readme paths must exist, and every normal dependency of a
  publishable crate needs a version.
- **`make check-release-config`**: no `release-plz.toml` entries for `gears/**`.
- **`make shear`** (`cargo shear --expand`): dependency crates used only through `deps = [...]` must be in
  `[workspace.metadata.cargo-shear] ignored`. The ones mini-chat needs are already listed. Any other
  unused dep in a new SDK fails.
- **`make gts-docs`**: `gts-validator --vendor cf,vendor,example,fabrikam ... docs gears libs examples`.
  GTS ids in `.md`/`.json` must be valid and use one of those vendors.
- **`make clippy`**: `cargo clippy --workspace --all-targets --all-features -- -D warnings -D clippy::perf`.
  Workspace lints include `clippy::pedantic` deny, `unwrap_used`, `expect_used`, `str_to_string`,
  `redundant_pub_crate`, `must_use_candidate`, `missing_errors_doc`, `missing_panics_doc`,
  `too_many_lines`, `cognitive_complexity`, `integer_division`, `cast_*`, `non_ascii_literal`,
  `wildcard_imports`, `use_debug`, `unused_async` (trait impls exempt) and `similar_names`.
  `rust`: `unsafe_code = forbid`, `non_ascii_idents = forbid`. These are clippy and rustc lints, so plain
  `cargo build` passes unless a rustc-level lint is set to deny or forbid. CI runs clippy.
- `make lint` = `RUSTFLAGS="-D warnings" cargo check --workspace --all-targets --all-features`.
  The new SDK and mini-chat must compile warning-free with `--all-features`, which includes `k8s`.

---

## 7. Testing helpers (for gear `init` unit tests)

```rust
// gears/system/authz-resolver/authz-resolver/src/gear.rs (tests)
struct EmptyConfig;
impl toolkit::config::ConfigProvider for EmptyConfig {
    fn get_gear_config(&self, _gear_name: &str) -> Option<&serde_json::Value> { None }
}
fn test_ctx() -> GearCtx {
    GearCtx::new("authz-resolver", Uuid::nil(), Arc::new(EmptyConfig), Arc::new(ClientHub::default()), CancellationToken::new())
}
```
`types-registry-sdk` feature `test-util` provides `types_registry_sdk::testing::MockTypesRegistryClient`
(`new()`, `with_instances(..)`, `with_type_schemas(..)`, `with_list_error(..)`, `list_instance_calls()`).
For a plugin round trip in a test, register the mock in a `ClientHub`, run plugin `init`, then check
`hub.try_get_scoped::<dyn XPluginClientV1>(&ClientScope::gts_id(&id))`.
`toolkit_db` has test helpers: `toolkit_db::test_support` and `toolkit_db::migration_runner::run_migrations_for_testing(..)`.
