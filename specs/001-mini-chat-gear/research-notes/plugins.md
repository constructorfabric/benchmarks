# Plugin pattern research: mini-chat SDK + bundled plugin gears

All paths are under /app. Line numbers are from the current tree. Nothing in the repo was modified.

## 0. TL;DR and current state

- The macros are NOT `gts_schema!` or `#[struct_to_gts_schema]` directly. SDKs use the **toolkit wrapper**
  `#[toolkit_gts::gts_type_schema(...)]`. It wraps `gts_macros::struct_to_gts_schema` and also emits an
  `inventory::submit!`, so **the schema registers itself with types-registry at boot**. You do not call
  `register` for the spec schema.
- Plugin gear `init()`: `PluginV1::<Spec>::build_registration(segment, vendor, priority)` -> `registry.register(vec![json])`
  -> `RegisterResult::ensure_all_ok` -> `client_hub().register_scoped::<dyn PluginTrait>(ClientScope::gts_id(&instance_id), api)`.
- Host: `toolkit::plugins::{GtsPluginSelector, choose_plugin_instance}` (choose = filter by vendor, pick the LOWEST priority),
  then `hub.try_get_scoped::<dyn PluginTrait>(&ClientScope::gts_id(id))`.
- Already present in the repo for mini-chat:
  - `gears/mini-chat/mini-chat/src/lib.rs` is a stub: `pub mod infra { pub mod plugins { pub mod static_audit {} pub mod static_model_policy {} } }`.
  - `apps/cf-gears-example-server/src/registered_gears.rs:31-38` already does
    `use mini_chat as _; use mini_chat::infra::plugins::static_audit as _; use mini_chat::infra::plugins::static_model_policy as _;`
    behind `#[cfg(feature = "mini-chat")]`. So both modules must stay `pub` at exactly those paths.
  - `config/mini-chat.yaml:231-240+` already has gear sections `static-mini-chat-audit-plugin.config {vendor: "constructorfabric", priority: 100, enabled: true}`
    and `static-mini-chat-model-policy-plugin.config {vendor, priority: 100, model_catalog: [...]}`. The mini-chat host config has `vendor: "constructorfabric"` (line 150).
  - No `mini-chat-sdk` crate exists yet. It is not in the root `Cargo.toml` members or `[workspace.dependencies]`, and not in `Cargo.lock`.
    `specs/001-mini-chat-gear/plan.md:91-101` plans `gears/mini-chat/mini-chat-sdk/` (`cf-gears-mini-chat-sdk` / lib `mini_chat_sdk`)
    with `plugin_api.rs` + `gts.rs`.
  - `gears/mini-chat/docs/DESIGN.md:415` (audit) requires: "no plugin" is NOT cached (look up again on every delivery). A found instance id IS cached.
    If the instance resolves but no client is in ClientHub, return Retry and **reset the cached id** (`selector.reset().await`).
  - `guidelines/GTS.md:1048` gives an example id `...~cf.core.mini_chat_model_policy.plugin.v1~`.

---

## 1. SDK: plugin trait + GTS plugin spec type

### 1a. Plugin API trait (credstore)
`gears/credstore/credstore-sdk/src/plugin_api.rs:1-43`
```rust
use async_trait::async_trait;
use toolkit_security::SecurityContext;
use crate::error::CredStoreError;
use crate::models::{OwnerId, SecretRef, SecretValue, TenantId};

#[async_trait]
pub trait CredStorePluginClientV1: Send + Sync {
    async fn get(&self, ctx: &SecurityContext, tenant_id: &TenantId, key: &SecretRef,
                 owner_id: Option<&OwnerId>) -> Result<Option<SecretValue>, CredStoreError>;
    async fn put(...) -> Result<(), CredStoreError>;
    async fn delete(...) -> Result<(), CredStoreError>;
}
```
Other examples: `gears/system/license-resolver/license-resolver-sdk/src/plugin_api.rs:20-43` (`LicenseResolverPluginClient`, one method),
`tenant-resolver-sdk/src/plugin_api.rs`, `authz-resolver-sdk/src/plugin_api.rs`. Requirements: `#[async_trait]`, `Send + Sync` (object-safe, stored as `Arc<dyn Trait>`).

### 1b. GTS plugin spec type
Minimal form. Copy this from `gears/system/tenant-resolver/tenant-resolver-sdk/src/gts.rs:7-48`
(same as `authz-resolver-sdk/src/gts.rs:3-21` and `license-resolver-sdk/src/gts.rs:21-38`):
```rust
use toolkit::gts::PluginV1;          // or: use toolkit_gts::PluginV1; (license-resolver-sdk:21, no `toolkit` dep needed)
use toolkit_gts::gts_type_schema;   // gts_id! inside the attribute needs NO import (the macro resolves it)

#[derive(Default)]                   // REQUIRED: PluginV1::<P>::build_registration uses P::default()
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.tenant_resolver.plugin.v1~"),
    description = "Tenant Resolver plugin specification",
    properties = "",
)]
pub struct TenantResolverPluginSpecV1;
```
credstore: `gears/credstore/credstore-sdk/src/gts.rs:80-101`. Its `use toolkit_gts::{gts_id, gts_type_schema};` imports gts_id only
because it also builds a `const` with `gts_id!` (line 91).

**Proposed mini-chat equivalents** (`gears/mini-chat/mini-chat-sdk/src/gts.rs`):
```rust
use toolkit_gts::{PluginV1, gts_type_schema};

/// gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_model_policy.plugin.v1~
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_model_policy.plugin.v1~"),
    description = "Mini-chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

/// gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_audit.plugin.v1~
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_audit.plugin.v1~"),
    description = "Mini-chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;
```

### 1c. Base type `PluginV1`
`libs/toolkit-gts/src/plugin.rs:29-47`. Type id `gts.cf.toolkit.plugins.plugin.v1~`. Fields are
`id: GtsInstanceId, vendor: String, priority: i16 (lower = higher priority), properties: P`.
`toolkit::gts::PluginV1` is a re-export (`libs/toolkit/src/gts/mod.rs:14`).
`PluginV1::<P>::build_registration(instance_segment: &str, vendor: impl Into<String>, priority: i16) -> serde_json::Result<(GtsInstanceId, serde_json::Value)>`
is at `plugin.rs:93-107`. The bound is `P: GtsSchema + GtsSerialize + Default`. id = `P::TYPE_ID + instance_segment`.

### 1d. Generated methods on the spec struct (gts-macros 0.12.1, `/usr/local/cargo/registry/src/index.crates.io-*/gts-macros-0.12.1/src/lib.rs:2646-2735`)
- `Spec::gts_type_id() -> &'static gts::GtsTypeId` (Display, used as `format!("{type_id}*")`)
- `Spec::gts_make_instance_id(segment: &str) -> gts::GtsInstanceId`
- `<Spec as gts::GtsSchema>::TYPE_ID: &'static str`
- `Spec::gts_schema_with_refs_as_string()` (used by the inventory)

### 1e. How the schema reaches types-registry (no code needed)
- `libs/toolkit-gts/src/lib.rs:1-49`: the `#[gts_type_schema]` / `gts_instance!` wrappers submit `InventoryTypeSchema` / `InventoryInstance` via `inventory`.
- `gears/system/types-registry/types-registry/src/gear.rs:156-170`: in `init()`, types-registry calls `all_inventory_type_schemas()` + `all_inventory_instances()`
  and `service.register(...)`. This happens in the "configuration" phase, with no validation.
  `post_init` (`gear.rs:237`) calls `switch_to_ready()`, which validates everything.
- The host gears say so in their docs: `authz-resolver/src/gear.rs:23-26`, `tenant-resolver/src/gear.rs:21-24`, `license-resolver/src/gear.rs:16-20`
  ("schema itself reaches types-registry automatically via the toolkit-gts link-time inventory, no per-init registration").
- Test pattern: `toolkit_gts::all_inventory_type_schemas()` then find `s["$id"] == json!(format!("gts://{}", TYPE_ID))`
  (`credstore-sdk/src/gts_tests.rs:12-18`).

### 1f. Macro expansion needs these direct deps in the crate that uses `#[gts_type_schema]`
The expansion references `::gts::…`, `serde::Serialize`/`Deserialize`, `schemars::JsonSchema` and `::serde_json`
(gts-macros lib.rs:489-511, 2236-2244). The toolkit wrapper emits `toolkit_gts::inventory::submit!` (toolkit-gts-macros/src/lib.rs:279), so you need
**`gts`, `toolkit-gts`, `serde`, `serde_json`, `schemars`**. `gts-macros` and `inventory` are NOT needed in the SDK
(tenant-resolver-sdk and license-resolver-sdk have neither).

---

## 2. Plugin gear: register instance + scoped client

Canonical: `gears/credstore/plugins/static-credstore-plugin/src/gear.rs:1-80`. It is the same in
`authz-resolver/plugins/static-authz-plugin/src/gear.rs:1-74`, `authn-resolver/plugins/static-authn-plugin/src/gear.rs:1-92`
and `tenant-resolver/plugins/single-tenant-tr-plugin/src/gear.rs:1-68`.
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
pub struct StaticCredStorePlugin { service: OnceLock<Arc<Service>> }
impl Default for StaticCredStorePlugin { fn default() -> Self { Self { service: OnceLock::new() } } }

#[async_trait]
impl Gear for StaticCredStorePlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticCredStorePluginConfig = ctx.config_expanded_or_default()?; // others: ctx.config_or_default()?
        let service = Arc::new(Service::from_config(&cfg)?);                   // validate before registering
        let (instance_id, instance_json) = PluginV1::<CredStorePluginSpecV1>::build_registration(
            "cf.core._.static_credstore.v1",          // authz uses "cf.builtin.static_authz_resolver.plugin.v1"
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
Config pattern: `static-credstore-plugin/src/config.rs:155-178`.
```rust
#[derive(Debug, Clone, Deserialize)]           // + toolkit_macros::ExpandVars if you use config_expanded_or_default
#[serde(default, deny_unknown_fields)]
pub struct Cfg { pub vendor: String, pub priority: i16, /* ... */ }
impl Default for Cfg { fn default() -> Self { Self { vendor: "constructorfabric".to_owned(), priority: 100, .. } } }
```
Note: `deny_unknown_fields` means your config struct must accept every key in `config/mini-chat.yaml`
(audit: `vendor, priority, enabled`; model policy: `vendor, priority, model_catalog`, and the model entries have many fields).

ClientHub API (`libs/toolkit/src/client_hub.rs`):
- `ClientScope::gts_id(gts_id: &str) -> ClientScope` (:57; prefixes `"gts:"`). `&GtsInstanceId` derefs to `&str`.
- `register_scoped<T: ?Sized + Send + Sync + 'static>(&self, scope: ClientScope, client: Arc<T>)` (:165)
- `try_get_scoped<T>(&self, scope: &ClientScope) -> Option<Arc<T>>` (:300), `get_scoped` -> `Result<_, ClientHubError>` (:273), `remove_scoped` (:326)

types-registry-sdk (`gears/system/types-registry/types-registry-sdk/src`):
- `TypesRegistryClient::register(&self, Vec<serde_json::Value>) -> Result<Vec<RegisterResult>, CanonicalError>` (api.rs:60)
- `list_instances(&self, InstanceQuery) -> Result<Vec<GtsInstance>, CanonicalError>` (api.rs:199). It is NOT cached in the local client (types-registry `domain/local_client.rs:736-760`).
- `RegisterResult::ensure_all_ok(&[RegisterResult]) -> Result<(), CanonicalError>` (models.rs:679)
- `InstanceQuery::new().with_pattern(String)` (models.rs:753-765). `GtsInstance { pub id: GtsInstanceId, pub object: Value, .. }` (models.rs:486-497)
- Re-exports: `lib.rs:43-54`.

---

## 3. Host: lazy plugin selection (vendor, lowest priority), caching, failure cases

### 3a. toolkit primitives: `libs/toolkit/src/plugins/mod.rs`
- `GtsPluginSelector` (:14-100). `new()`, `pre_cached(String)` (for tests),
  `async get_or_init<F,Fut,E>(&self, resolve: F) -> Result<Arc<str>, E>` (single-flight; **caches only on Ok**, so errors are re-resolved on the next call),
  `async reset(&self) -> bool` (clear cached id). Also re-exported as `toolkit::GtsPluginSelector` (lib.rs:180).
- `choose_plugin_instance::<P>(vendor: &str, instances: impl IntoIterator<Item=(&str, &serde_json::Value)>) -> Result<String, ChoosePluginError>` (:150-205).
  It deserializes each item as `PluginV1<P>`, errors if `content.id != gts_id`, skips other vendors, keeps the **lowest `priority`**
  (on a tie the first one wins), and returns `PluginNotFound` if nothing matched.
- `ChoosePluginError::{InvalidPluginInstance{gts_id, reason}, PluginNotFound{type_id, vendor}}` (:103-122).

### 3b. credstore's selector (`gears/credstore/credstore/src/infra/plugin_select.rs:1-103`)
```rust
use std::sync::Arc;
use async_trait::async_trait;
use credstore_sdk::{CredStorePluginClientV1, CredStorePluginSpecV1};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};

pub struct GtsCredStorePluginSelector { hub: Arc<ClientHub>, vendor: String, selector: GtsPluginSelector }

impl GtsCredStorePluginSelector {
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self { hub, vendor, selector: GtsPluginSelector::new() }
    }
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
            toolkit::plugins::ChoosePluginError::PluginNotFound { vendor, .. } => /* ServiceUnavailable "no credstore plugin found for vendor" */,
            toolkit::plugins::ChoosePluginError::InvalidPluginInstance { gts_id, reason } => /* Internal */,
        })?;
        Ok(gts_id)
    }
}

#[async_trait]
impl PluginSelector for GtsCredStorePluginSelector {          // port: domain/ports/plugin.rs:13-17
    async fn resolve(&self) -> Result<Arc<dyn CredStorePluginClientV1>, DomainError> {
        let instance_id = self.selector.get_or_init(|| self.resolve_instance()).await?;
        let scope = ClientScope::gts_id(instance_id.as_ref());
        self.hub.try_get_scoped::<dyn CredStorePluginClientV1>(&scope)
            .ok_or_else(|| DomainError::ServiceUnavailable {
                detail: format!("credstore plugin client not registered yet for '{instance_id}'"),
                retry_after: None, cause: None })
    }
}
```
It is built in `credstore/src/gear.rs:133-136` (`GtsCredStorePluginSelector::new(ctx.client_hub(), cfg.vendor.clone())`). Note: `ctx.client_hub()` returns `Arc<ClientHub>`.

### 3c. Resolver variant with a throttled warning (`gears/system/authz-resolver/authz-resolver/src/domain/service.rs:1-103`)
The same code is in `tenant-resolver/src/domain/service.rs:61-118` and `license-resolver/src/domain/service.rs:143-193`.
```rust
use toolkit::telemetry::ThrottledLog;   // ThrottledLog::new(Duration), .should_log() -> bool
const UNAVAILABLE_LOG_THROTTLE: Duration = Duration::from_secs(10);

async fn get_plugin(&self) -> Result<Arc<dyn AuthZResolverPluginClient>, DomainError> {
    let instance_id = self.selector.get_or_init(|| self.resolve_plugin()).await?;
    let scope = ClientScope::gts_id(instance_id.as_ref());
    if let Some(client) = self.hub.try_get_scoped::<dyn AuthZResolverPluginClient>(&scope) {
        Ok(client)
    } else {
        if self.unavailable_log_throttle.should_log() {
            tracing::warn!(plugin_gts_id = %instance_id, vendor = %self.vendor, "Plugin client not registered yet");
        }
        Err(DomainError::PluginUnavailable { gts_id: instance_id.to_string(), reason: "client not registered yet".into() })
    }
}
async fn resolve_plugin(&self) -> Result<String, DomainError> {
    let registry = self.hub.get::<dyn TypesRegistryClient>()
        .map_err(|e| DomainError::TypesRegistryUnavailable(e.to_string()))?;
    let plugin_type_id = AuthZResolverPluginSpecV1::gts_type_id().clone();
    let instances = registry.list_instances(InstanceQuery::new().with_pattern(format!("{plugin_type_id}*"))).await?;
    let gts_id = choose_plugin_instance::<AuthZResolverPluginSpecV1>(
        &self.vendor, instances.iter().map(|e| (e.id.as_ref(), &e.object)))?;   // From<ChoosePluginError> in error.rs:51-62
    Ok(gts_id)
}
```
Error mapping: `authz-resolver/src/domain/error.rs:9-62` (`TypesRegistryUnavailable`, `PluginNotFound{vendor}`, `InvalidPluginInstance`,
`PluginUnavailable{gts_id, reason}` and `From<ChoosePluginError>`).

### 3d. Semantics summary and the mini-chat specifics
- **Plugin not registered yet** (no instance for this vendor): `choose_plugin_instance` returns `PluginNotFound`. `get_or_init` does NOT cache errors,
  so the next call re-queries types-registry. This is exactly the "no plugin is not cached" rule for audit (map PluginNotFound to ack+drop, `result="dropped"`).
- **Instance resolves but no client in ClientHub**: the id IS cached. Existing resolvers just return `PluginUnavailable` and keep the id.
  mini-chat DESIGN.md:415 additionally requires `self.selector.reset().await` here, then Retry.
- No existing gear calls `reset()` in production (only the toolkit tests use it, plugins/mod.rs:251-275), so this is new code for mini-chat.
- Lazy is mandatory. Host and plugin gears are independent nodes in the topo sort, and Kahn's order among independent nodes comes from `HashMap` key order
  (`libs/toolkit/src/registry.rs:551-575, 636-672`), so it is effectively arbitrary. Resolve on the first call, never in `init()`.
- Types-registry switches to ready mode in `post_init`. Every gear's `init()` (including plugin registration) completes before any request.

### 3e. Proposed generic helper for mini-chat (`infra/plugin_select.rs` or similar)
```rust
pub struct PluginResolver<P> { hub: Arc<ClientHub>, vendor: String, selector: GtsPluginSelector, warn: ThrottledLog, _p: PhantomData<fn() -> P> }
pub enum Resolve<T: ?Sized> { Ready(Arc<T>), NotRegistered /* no instance: not cached */, ClientMissing(String) /* id reset */ }

async fn resolve_id<S>(&self) -> Result<String, ResolveErr>
where S: for<'de> gts::GtsDeserialize<'de> + gts::GtsSchema {
    let reg = self.hub.get::<dyn TypesRegistryClient>()?;
    let pattern = format!("{}*", S::gts_type_id());           // or <S as gts::GtsSchema>::TYPE_ID
    let list = reg.list_instances(InstanceQuery::new().with_pattern(pattern)).await?;
    Ok(choose_plugin_instance::<S>(&self.vendor, list.iter().map(|e| (e.id.as_ref(), &e.object)))?)
}
// on try_get_scoped == None:  self.selector.reset().await; return ClientMissing(id)
```
Note: to name the bound `gts::GtsDeserialize` in mini-chat, `gts` must be a direct dep. mini-chat already has it (Cargo.toml line "gts = { workspace = true }").
Non-generic per-plugin code (copying credstore) avoids the bound entirely.

---

## 4. Cargo manifests

### 4a. SDK crate examples
`gears/credstore/credstore-sdk/Cargo.toml` (full):
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
readme = "README.md"                       # must exist (tools/scripts/check_packaging_metadata.py); lib.rs does #![doc = include_str!("../README.md")]
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
zeroize = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
time = { workspace = true }
schemars = { workspace = true }
gts = { workspace = true }
gts-macros = { workspace = true }
toolkit-gts = { workspace = true }
toolkit = { workspace = true }
toolkit-security = { workspace = true }
tenant-resolver-sdk = { workspace = true }

[features]
test-util = []
```
Leaner template, `gears/system/license-resolver/license-resolver-sdk/Cargo.toml` deps:
`async-trait, thiserror, uuid, serde, serde_json, schemars, gts, toolkit-gts, toolkit-canonical-errors, toolkit-security` (no `toolkit`, no `gts-macros`).
tenant-resolver-sdk (`Cargo.toml` lines 1-35) has `async-trait, thiserror, uuid, serde, serde_json, schemars, utoipa, gts, toolkit-gts, toolkit, toolkit-security`.

**Proposed `gears/mini-chat/mini-chat-sdk/Cargo.toml`:**
```toml
[package]
name = "cf-gears-mini-chat-sdk"
description = "SDK for mini-chat gear: plugin API traits, models, and GTS plugin specs"
version = "0.1.0"
edition.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
rust-version.workspace = true
keywords = ["constructorfabric", "cf-gears"]
categories = ["artificial-intelligence"]
metadata.docs.rs.all-features = true
# omit `readme` unless you create README.md

[lib]
name = "mini_chat_sdk"

[lints]
workspace = true

[dependencies]
async-trait = { workspace = true }
thiserror = { workspace = true }
uuid = { workspace = true }
time = { workspace = true }          # if models carry OffsetDateTime
serde = { workspace = true }
serde_json = { workspace = true }
schemars = { workspace = true }
gts = { workspace = true }
toolkit-gts = { workspace = true }
toolkit-security = { workspace = true }   # if trait methods take &SecurityContext
```

### 4b. Root `Cargo.toml`
- Members: lines 52-103. Add `"gears/mini-chat/mini-chat-sdk",` (mini-chat itself is at line 100).
- `[workspace.dependencies]` starts at line 234. Entries you asked for:
```toml
# line 296
authz-resolver-sdk = { package = "cf-gears-authz-resolver-sdk", version = "0.5.2", path = "gears/system/authz-resolver/authz-resolver-sdk" }
# line 297
oagw-sdk = { package = "cf-gears-oagw-sdk", version = "0.5.15", path = "gears/system/oagw/oagw-sdk" }
# line 326
credstore-sdk = { package = "cf-gears-credstore-sdk", version = "0.2.17", path = "gears/credstore/credstore-sdk" }
```
  Others: types-registry-sdk :294, tenant-resolver-sdk :295, license-resolver-sdk :739. Gear crates are at :281-290 (`types-registry = { package = "cf-gears-types-registry", version = "0.4.0", path = ... }`).
  Proposed: `mini-chat-sdk = { package = "cf-gears-mini-chat-sdk", version = "0.1.0", path = "gears/mini-chat/mini-chat-sdk" }`.
- `[workspace.metadata.cargo-shear] ignored` (lines 742-755): this lists gear crates used only via `#[toolkit::gear(deps=[...])]` re-exports.
  `types-registry` is already there.
- The SDK is a path crate, so adding it changes `Cargo.lock` (new `cf-gears-mini-chat-sdk` package entry) but needs no network.

### 4c. Gear consuming an SDK
- `gears/credstore/credstore/Cargo.toml:22`: `credstore-sdk = { workspace = true }`. Line 32-33: `types-registry-sdk = { workspace = true }`, `types-registry = { workspace = true }`.
  Dev: `types-registry-sdk = { workspace = true, features = ["test-util"] }` (line 67).
- Plugins inside their own crates use explicit path+version instead:
  `static-credstore-plugin/Cargo.toml:20`: `credstore-sdk = { package = "cf-gears-credstore-sdk", version = "0.2.17", path = "../../credstore-sdk" }`.
- Plugin crate extra deps: `types-registry = { workspace = true }` (needed by `deps = [types_registry]`, see §6), `toolkit`, `toolkit-macros`,
  `inventory = { workspace = true }` ("Required by toolkit::gear macro"), `async-trait`, `anyhow`, `serde`, `tracing`.
- mini-chat's `Cargo.toml` already has `types-registry-sdk`, `types-registry`, `toolkit`, `toolkit-gts`, `gts`, `inventory`, `toolkit-macros`, `serde(_json)`, `tracing`.
  Add only `mini-chat-sdk = { workspace = true }` (and for tests: `types-registry-sdk = { workspace = true, features = ["test-util"] }` under dev-dependencies).

---

## 5. Multiple gears in one crate (bundled plugins under `mini_chat::infra::plugins`)

No production crate in this repo registers more than one gear today (every `#[toolkit::gear(` in gears/ is one per crate).
The mechanism supports it, and `libs/toolkit/tests/macro_tests.rs:63,74,109,120,139,154,174,211` declares 8 `#[gear(...)]` structs in one compilation unit.

How `#[toolkit::gear]` registers (`libs/toolkit-macros/src/lib.rs:483-850`):
- Each struct expands to a private `fn __<struct_snake>_registrator(b: &mut ::toolkit::registry::RegistryBuilder)`
  that calls `b.register_core_with_meta(name, &[deps...], Arc<dyn Gear>)`, plus `::toolkit::inventory::submit!{ ::toolkit::registry::Registrator(fn) }` (:820-840).
- It also adds `impl Struct { pub const MODULE_NAME: &'static str = "<name>"; }` (:783-791).
- `GearRegistry::discover_and_build()` iterates `inventory::iter::<Registrator>` (`libs/toolkit/src/registry.rs:306-316`).
  A duplicate name gives the error "Gear '{name}' is already registered" (:351-363).
- The name must be kebab-case (macro-validated): `static-mini-chat-model-policy-plugin`, `static-mini-chat-audit-plugin`.
- Every linked gear is initialized. There is no "enabled" switch. Config comes from `gears.<name>.config` via `ctx.config_or_default()`.

Gotchas when putting 2 or 3 gears in one crate:
1. **`deps = [x]` emits `#[cfg(not(test))] pub use ::x as _gear_dep_x;` in the module where the macro is invoked** (toolkit-macros lib.rs:795-809).
   Two gears in the SAME module with the same dep fail with E0252 (duplicate `_gear_dep_types_registry`).
   Put each gear in its own module, for example `infra/plugins/static_audit/gear.rs` and `infra/plugins/static_model_policy/gear.rs`, or declare the gear struct directly in `static_audit/mod.rs`.
2. `deps` takes **crate identifiers** that are mapped `_`->`-` to gear names (lib.rs:487-503, 521-527). A bundled plugin gear therefore cannot be listed as a dep of the host (no crate named `static_mini_chat_audit_plugin`),
   and the host crate cannot list itself. So host -> plugin ordering cannot be declared, and lazy selection (§3) is required.
3. Registrator fn names derive from the struct name, so use distinct struct names (e.g. `StaticAuditPlugin`, `StaticModelPolicyPlugin`, `MiniChatGear`).
4. Linking: the example server forces linkage with `use mini_chat::infra::plugins::static_audit as _;` etc. (registered_gears.rs:34-38), so keep those modules `pub`.
5. The `#[cfg(not(test))]` on dep re-exports means unit tests inside mini-chat do not need the dep crates linked.
6. Without `deps = [types_registry]` the topo order of the plugin vs types-registry would only come from the system phase (see §6). Keep it explicit like all existing plugins.

Proposed skeleton (`gears/mini-chat/mini-chat/src/infra/plugins/static_audit/mod.rs`):
```rust
use std::sync::Arc;
use async_trait::async_trait;
use mini_chat_sdk::{MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1};
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

#[toolkit::gear(name = "static-mini-chat-audit-plugin", deps = [types_registry])]
pub struct StaticAuditPlugin;
impl Default for StaticAuditPlugin { fn default() -> Self { Self } }

#[async_trait]
impl Gear for StaticAuditPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticAuditPluginConfig = ctx.config_or_default()?;   // {vendor, priority, enabled}
        if !cfg.enabled { return Ok(()); }                              // (choose semantics; no instance => host drops events)
        let (instance_id, json) = PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
            "cf.builtin.static_mini_chat_audit.plugin.v1", cfg.vendor.clone(), cfg.priority)?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        RegisterResult::ensure_all_ok(&registry.register(vec![json]).await?)?;
        let api: Arc<dyn MiniChatAuditPluginClientV1> = Arc::new(LoggingAuditClient);
        ctx.client_hub().register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(&instance_id), api);
        Ok(())
    }
}
```

---

## 6. Dependency ordering / init order

- All existing plugin gears declare `deps = [types_registry]` (static-credstore gear.rs:87-90, static-authz :18-21, static-authn :25-28, single-tenant-tr :21-24).
  Host resolvers do too (authz-resolver :33-37, tenant-resolver :25-29, license-resolver :21-25). credstore uses `deps = [authz_resolver, tenant_resolver, types_registry]` (credstore/src/gear.rs:37-42).
  The plugin crate must then have `types-registry` (the gear crate, lib `types_registry`) as a dependency, because the macro emits `pub use ::types_registry`.
- Runtime order (`libs/toolkit/src/runtime/host_runtime.rs:390-417`): `init()` runs over `registry.gears_by_system_priority()`,
  which puts **system gears first, then non-system gears, each group in topo order** (`registry.rs:286-304`).
  types-registry is `capabilities = [system, db, rest, stateful]` (types-registry/src/gear.rs:31-37), so it always initializes before non-system plugins.
  It registers `dyn TypesRegistryClient` in ClientHub at the end of its init (gear.rs:228).
- After all `init`s: consumer wiring, then `post_init` (system gears only). Here types-registry calls `switch_to_ready()` and validates all entities,
  including plugin instances registered during init against their spec schemas (host_runtime.rs:615-660).
- An unknown dep name gives `RegistryError::UnknownDependency` (registry.rs:565-569). A cycle gives `CycleDetected`.
- Independent gears (mini-chat host vs its plugins) get an arbitrary relative order. Never resolve plugins in the host's `init()`.

## 7. Testing notes
- `types_registry_sdk::testing::MockTypesRegistryClient` (feature `test-util`): `.with_instances([...])`, `.with_list_error(..)`, `.list_instance_calls()`.
  **Its `register()` panics on a non-empty vec** (testing.rs:164-170), so plugin-gear `init()` tests need their own small mock.
- `make_test_instance(gts_id, json!({"id": gts_id, "vendor": v, "priority": 0, "properties": {}}))` builds a selectable instance
  (pattern in `license-resolver/src/domain/test_support.rs:285-337`, incl. `hub_with_registry_and_plugin`, `counting_hub_with_registry_only` for the "client missing" case, and `hub_without_plugin_instances`).
- `GearCtx::new(name, Uuid::nil(), Arc<dyn ConfigProvider>, Arc<ClientHub>, CancellationToken)` (authz-resolver/src/gear.rs:112-127 test).
