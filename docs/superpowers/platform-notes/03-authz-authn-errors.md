# Platform note 03: SecurityContext, AuthZ (PEP), AuthN S2S, canonical errors, GTS

This is a reference for implementing the `mini-chat` gear. Everything below was checked against the
source at the time of writing. Paths are relative to `/app`. Crate names are given as
**package name -> lib name**, e.g. `cf-gears-toolkit-security` -> `toolkit_security`.

Crates mini-chat already depends on (`gears/mini-chat/mini-chat/Cargo.toml`):

```toml
authn-resolver-sdk = { package = "cf-gears-authn-resolver-sdk", version = "0.3.37", path = "../../system/authn-resolver/authn-resolver-sdk" } # NOT a workspace dep
authz-resolver-sdk = { workspace = true }          # lib: authz_resolver_sdk
toolkit = { workspace = true }
toolkit-canonical-errors = { workspace = true, features = ["axum"] }   # lib: toolkit_canonical_errors
toolkit-gts = { workspace = true }                 # lib: toolkit_gts
gts = { workspace = true }                         # needed by gts_instance! expansions
toolkit-security = { workspace = true }            # lib: toolkit_security
toolkit-db = { workspace = true, features = ["sqlite", "pg"] }
secrecy = { workspace = true }
```

---

## 1. toolkit-security: `SecurityContext`

Source: `libs/toolkit-security/src/context.rs`, `src/constants.rs`, `src/lib.rs`, `src/prelude.rs`.

### 1.1 Type

```rust
// toolkit_security::SecurityContext  (re-exported at crate root; also in toolkit_security::prelude)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SecurityContext {
    subject_id: Uuid,                 // user / service / system
    subject_type: Option<String>,     // e.g. "user", "service" (opaque; static-authn suggests a GTS id)
    subject_tenant_id: Uuid,          // subject's home tenant (required)
    #[serde(default)] token_scopes: Vec<String>,   // ["*"] = first-party/unrestricted; [] = NOTHING granted
    #[serde(skip)] bearer_token: Option<SecretString>, // for PDP forwarding, never serialized
}
```

All fields are private. Accessors:

| Method | Returns |
|---|---|
| `subject_id(&self)` | `Uuid` |
| `subject_type(&self)` | `Option<&str>` |
| `subject_tenant_id(&self)` | `Uuid` |
| `token_scopes(&self)` | `&[String]` |
| `has_scope(&self, scope: &str)` | `bool`. `"*"` matches everything. An empty list matches nothing |
| `is_anonymous(&self)` | `bool`: `subject_id.is_nil() \|\| subject_tenant_id.is_nil()` |
| `bearer_token(&self)` | `Option<&SecretString>` |
| `SecurityContext::anonymous()` | nil subject, nil tenant, no scopes, no token |
| `SecurityContext::builder()` | `SecurityContextBuilder` |

There is no separate "context tenant" field. The tenant to use is `ctx.subject_tenant_id()`.
(DESIGN.md's "user_id / tenant_id" correspond to `subject_id()` / `subject_tenant_id()`.)

Builder (`SecurityContextBuilder`, `#[derive(Debug, Default)]`): `.subject_id(Uuid)`,
`.subject_type(&str)`, `.subject_tenant_id(Uuid)`, `.token_scopes(Vec<String>)`,
`.bearer_token(impl Into<SecretString>)`, then `.build() -> Result<SecurityContext, SecurityContextBuildError>`.
Errors: `SecurityContextBuildError::MissingSubjectId` and `::MissingSubjectTenantId`. Both ids are
required. A nil value is accepted by the builder, but the resulting context then reports
`is_anonymous() == true`.

### 1.2 Well-known ids (`toolkit_security::constants`, not re-exported at the crate root)

```rust
pub const DEFAULT_TENANT_ID: Uuid  = uuid!("00000000-df51-5b42-9538-d2b56b7ee953");
pub const DEFAULT_SUBJECT_ID: Uuid = uuid!("11111111-6a88-4768-9dfc-6bcd5187d9ed");
pub const INTERNAL_TOKEN_HEADER: &str = "x-toolkit-internal-token"; // platform-plane (S2S infra) header
```

Where these ids show up:
- **api-gateway with `auth_disabled: true`** (`gears/system/api-gateway/src/gear.rs:462`) injects
  `SecurityContext::builder().subject_id(DEFAULT_SUBJECT_ID).subject_tenant_id(DEFAULT_TENANT_ID).build()`
  on every request. That context has **no token scopes** (empty).
- **static-authn-plugin** `IdentityConfig::default()` (`plugins/static-authn-plugin/src/config.rs`) uses
  `subject_id = DEFAULT_SUBJECT_ID`, `subject_tenant_id = DEFAULT_TENANT_ID`,
  `token_scopes = ["*"]`, `subject_type = None`. This is the identity for `mode: accept_all` and for any
  `s2s_credentials` entry without an `identity:` block, which includes mini-chat's entry in
  `config/mini-chat.yaml`.

### 1.3 How handlers obtain it

The api-gateway authn middleware (`gears/system/api-gateway/src/middleware/auth.rs::authn_middleware`)
inserts the context into the request extensions:
- `AuthRequirement::Required`: it calls `AuthNResolverClient::authenticate(token)` and inserts
  `result.security_context`. A missing bearer gives 401 `unauthenticated` with reason `MISSING_BEARER`.
  A rejected token gives 401 with reason `AUTHN_FAILED`.
- Public/anonymous routes and CORS preflight get `SecurityContext::anonymous()`.

The route opts in through the OperationBuilder: `.authenticated()` or `.anonymous()`
(`libs/toolkit/src/api/operation_builder.rs:1255/1296`). The handler extracts it with axum's `Extension`:

```rust
// gears/system/oagw/oagw/src/api/rest/handlers/route.rs
use axum::extract::Extension;
use toolkit_security::SecurityContext;

pub async fn get_route(
    Extension(state): Extension<AppState>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, Box<Problem>> { /* state.cp.get_route(&ctx, uuid) */ }
```

Pass `&SecurityContext` down into the domain service. Every authorized call takes it.

### 1.4 Test construction (copy-paste)

```rust
// gears/credstore/credstore/src/domain/secret/test_support.rs
pub fn make_ctx(subject_id: Uuid, tenant_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(subject_id)
        .subject_tenant_id(tenant_id)
        .build()
        .expect("test ctx")
}
// fuller form (libs/toolkit-security/src/context.rs tests)
let ctx = SecurityContext::builder()
    .subject_id(Uuid::from_u128(1))
    .subject_type("user")
    .subject_tenant_id(Uuid::from_u128(2))
    .token_scopes(vec!["*".to_owned()])
    .bearer_token("test-token-123".to_owned())
    .build()
    .unwrap();
```

### 1.5 `AccessScope` (also in toolkit-security, consumed by toolkit-db)

Source: `libs/toolkit-security/src/access_scope.rs`. It is re-exported as `toolkit_db::secure::AccessScope`.

```rust
pub mod pep_properties {   // toolkit_security::pep_properties
    pub const OWNER_TENANT_ID: &str = "owner_tenant_id";
    pub const RESOURCE_ID: &str = "id";
    pub const OWNER_ID: &str = "owner_id";
}
```

An `AccessScope` is an OR of `ScopeConstraint`s. Each `ScopeConstraint` is an AND of `ScopeFilter`s
(`Eq`, `In`, `InGroup`, `InGroupSubtree`, `InTenantSubtree`; the enum is `#[non_exhaustive]`).
- Constructors: `allow_all()` (unconstrained), `deny_all()` (also `Default`), `for_tenant(id)`,
  `for_tenants(ids)`, `for_resource(id)`, `for_resources(ids)`, `from_constraints(vec)`, `single(c)`.
- Predicates: `is_unconstrained()`, `is_deny_all()`, `has_property(p)`, `all_uuid_values_for(p)`,
  `contains_uuid(p, id)` (searches values only; it is **not** an authorization decision).
- Transformations:
  - `ensure_owner(owner_id: Uuid) -> AccessScope`. Defence in depth for user-owned rows: it injects or
    narrows `owner_id = owner_id` into every constraint. Unconstrained becomes owner-only. Deny-all
    stays deny-all.
  - `tenant_only()`. Keeps only the `owner_tenant_id` filters. This **widens** the scope. Use it only
    for child entities with no owner column, after the parent was checked against the full scope.
  - `tenant_and_owner()`.
- Applying it in the DB: `Entity::find().secure().scope_with(&scope)?.all(conn).await?`.
  The entity needs `#[derive(Scopable)] #[secure(tenant_col = "...", resource_col = "...", owner_col = "..." | no_owner, no_type)]`
  (see `libs/toolkit-db/src/secure/mod.rs`, `gears/credstore/credstore/src/infra/storage/entity/secrets.rs`).
  An empty or deny-all scope compiles to `WHERE 1=0`.

---

## 2. authz-resolver-sdk: `PolicyEnforcer` (PEP)

Source: `gears/system/authz-resolver/authz-resolver-sdk/src/{lib.rs,api.rs,models.rs,error.rs,constraints.rs,pep/enforcer.rs,pep/compiler.rs}`.

### 2.1 Public surface (crate root re-exports)

```rust
use authz_resolver_sdk::{
    AuthZResolverApi,                          // PDP contract trait (in ClientHub as Arc<dyn AuthZResolverApi>)
    PolicyEnforcer, ResourceType, AccessRequest, EnforcerError, IntoPropertyValue, // == pep::*
    EvaluationRequest, EvaluationResponse, EvaluationResponseContext, EvaluationRequestContext,
    Subject, Action, Resource, TenantContext, TenantMode, BarrierMode, Capability, DenyReason,
    Constraint, Predicate, EqPredicate, InPredicate, InTenantSubtreePredicate, // ...
    AuthZResolverError, AuthZResolverPluginClient, AuthZResolverPluginSpecV1,
};
use authz_resolver_sdk::pep::{compile_to_access_scope, ConstraintCompileError};
use authz_resolver_sdk::pep::enforcer::DEFAULT_EVAL_DEADLINE; // Duration::from_secs(30)
```

PDP contract (`api.rs`):

```rust
#[toolkit::contract(gear = "authz-resolver", version = "v1")]
pub trait AuthZResolverApi: Send + Sync {
    #[idempotency(SafeRead)]
    async fn evaluate(&self, ctx: PlatformSecurityContext, req: EvaluationRequest)
        -> Result<EvaluationResponse, CanonicalError>;
}
```

A deny is `Ok(EvaluationResponse { decision: false, .. })`. `Err(CanonicalError)` means an
infrastructure or transport failure. The gateway (`authz-resolver/src/domain/local_client.rs`) maps
"types registry unavailable / plugin not found / plugin unavailable" to `CanonicalError::service_unavailable()`
and "invalid plugin instance / internal" to `CanonicalError::internal(..)`.

### 2.2 `ResourceType`

```rust
pub struct ResourceType { name: Cow<'static, str>, supported_properties: &'static [&'static str], native_group_predicates: bool }
impl ResourceType {
    pub const fn from_static(name: &'static str, supported_properties: &'static [&'static str]) -> Self;
    pub fn new(name: impl Into<Cow<'static, str>>, supported_properties: &'static [&'static str]) -> Self;
    pub const fn with_native_group_predicates(self) -> Self; // not needed for mini-chat
    pub fn name(&self) -> &str;
    pub fn supported_properties(&self) -> &'static [&'static str];
}
```

The name is a GTS type id (ends with `~`). Use `gts_id!` from `toolkit_gts`, which is also re-exported
as `toolkit_canonical_errors::gts_id`. Example for mini-chat:

```rust
use authz_resolver_sdk::pep::ResourceType;
use toolkit_gts::gts_id;
use toolkit_security::pep_properties;

pub const CHAT: ResourceType = ResourceType::from_static(
    gts_id!("cf.core.mini_chat.chat.v1~"),   // -> "gts.cf.core.mini_chat.chat.v1~"
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID, pep_properties::RESOURCE_ID],
);
pub const MODEL: ResourceType = ResourceType::from_static(gts_id!("cf.core.mini_chat.model.v1~"), &[]);
```

### 2.3 `AccessRequest` (per-call overrides; all optional)

```rust
#[derive(Debug, Clone, Default)]
pub struct AccessRequest { resource_properties: HashMap<String, Value>, tenant_context: Option<TenantContext>, require_constraints: Option<bool> }
AccessRequest::new()
    .resource_property(key: impl Into<String>, value: impl IntoPropertyValue) // Uuid,&Uuid,String,&str,i64,bool,Value
    .resource_properties(HashMap<String, serde_json::Value>)                  // replaces all
    .context_tenant_id(Uuid)        // TenantContext.root_id (default: unset -> PDP falls back to subject tenant)
    .tenant_mode(TenantMode::RootOnly | TenantMode::Subtree)   // default Subtree
    .barrier_mode(BarrierMode::Respect | BarrierMode::Ignore)  // default Respect
    .tenant_status(Vec<String>)
    .tenant_context(TenantContext { mode, root_id, barrier_mode, tenant_status })
    .require_constraints(bool)      // default true
```

### 2.4 `PolicyEnforcer`

```rust
#[derive(Clone)] pub struct PolicyEnforcer { /* authz source, capabilities, deadline */ }
impl PolicyEnforcer {
    pub fn new(authz: Arc<dyn AuthZResolverApi>) -> Self;          // eager (in-process, tests)
    pub fn from_hub(hub: Arc<ClientHub>) -> Self;                  // lazy: resolves dyn AuthZResolverApi per call
    pub fn with_capabilities(self, Vec<Capability>) -> Self;       // TenantHierarchy | GroupMembership | GroupHierarchy
    pub fn with_deadline(self, Duration) -> Self;                  // default 30 s
    pub fn build_request(&self, ctx, resource: &ResourceType, action: &str, resource_id: Option<Uuid>, require_constraints: bool) -> EvaluationRequest;
    pub fn build_request_with(&self, ctx, resource, action, resource_id, require_constraints, request: &AccessRequest) -> EvaluationRequest;
    pub async fn access_scope(&self, ctx: &SecurityContext, resource: &ResourceType, action: &str, resource_id: Option<Uuid>)
        -> Result<AccessScope, EnforcerError>;                      // require_constraints = true
    pub async fn access_scope_with(&self, ctx, resource, action, resource_id, request: &AccessRequest)
        -> Result<AccessScope, EnforcerError>;
}
```

What `build_request_with` puts on the wire (`EvaluationRequest`, AuthZEN shape):
- `subject = Subject { id: ctx.subject_id(), subject_type: ctx.subject_type(), properties: {"tenant_id": "<ctx.subject_tenant_id()>"} }`
- `action = Action { name: action }`
- `resource = Resource { resource_type: resource.name(), id: resource_id, properties: request.resource_properties }`
- `context = EvaluationRequestContext { tenant_context: request.tenant_context (None unless set), token_scopes: ctx.token_scopes(), require_constraints, capabilities (filtered), supported_properties: resource.supported_properties(), bearer_token: ctx.bearer_token() (serde-skipped) }`

Flow inside `access_scope_with`:
1. Build the request.
2. Call `evaluate(PlatformSecurityContext::outbound_marker(), req)` under the deadline. A timeout
   becomes `EvaluationFailed(service_unavailable "authz-resolver did not respond within ..")`.
3. If `!response.decision`, return `Err(EnforcerError::Denied { deny_reason: response.context.deny_reason })`.
4. Compile the constraints (`compiler.rs` matrix):

| require_constraints | constraints | result |
|---|---|---|
| false | empty | `Ok(AccessScope::allow_all())` |
| false | present | compiled scope |
| true | empty | `Err(CompileFailed(ConstraintsRequiredButAbsent))` |
| true | present | compiled scope. Each PDP `Constraint` becomes a `ScopeConstraint` (AND), multiple are ORed. A constraint that names a property not in `supported_properties` fails on its own. If all of them fail you get `AllConstraintsFailed { reason }` |

### 2.5 Errors and how to classify them

```rust
pub enum EnforcerError {
    Denied { deny_reason: Option<DenyReason> },              // PDP said decision=false  -> "denied"
    EvaluationFailed(#[from] CanonicalError),                // PDP unreachable / timeout / no plugin -> "PDP unavailable"
    CompileFailed(#[from] ConstraintCompileError),           // constraints missing/unsupported -> "compile failure" (fail-closed)
}
#[non_exhaustive] pub enum ConstraintCompileError {
    ConstraintsRequiredButAbsent,
    AllConstraintsFailed { reason: String },
    UnadvertisedCapabilities { predicate: &'static str, missing: Vec<&'static str> },
}
pub struct DenyReason { pub error_code: String, pub details: Option<String> }
pub enum AuthZResolverError { NoPluginAvailable, ServiceUnavailable(String), Internal(String) } // plugin-side only
```

Mini-chat DESIGN line 1414 says: denial or compile failure -> 403 `permission_denied`, evaluation failure
-> 503 `service_unavailable` with `Retry-After`. Credstore does the same thing:

```rust
// gears/credstore/credstore/src/domain/authz.rs
pub fn map_enforcer_err(err: EnforcerError) -> DomainError {
    match err {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => DomainError::AccessDenied { cause: Some(Box::new(err)) },
        EnforcerError::EvaluationFailed(source) => DomainError::ServiceUnavailable {
            detail: "authorization evaluation failed".to_owned(),
            retry_after: None,
            cause: Some(Box::new(EnforcerError::EvaluationFailed(source))),
        },
    }
}
pub async fn scope_for(enforcer: &PolicyEnforcer, ctx: &SecurityContext, resource: &ResourceType, action: &str)
    -> Result<AccessScope, DomainError> {
    let tenant = ctx.subject_tenant_id();
    let request = AccessRequest::new()
        .resource_property(pep_properties::OWNER_TENANT_ID, tenant)
        .require_constraints(true);
    enforcer.access_scope_with(ctx, resource, action, None, &request).await.map_err(map_enforcer_err)
}
```

oagw (`gears/system/oagw/oagw/src/domain/error.rs:303`) is different. It maps `Denied` to
`Forbidden { reason: deny_reason.error_code or "AUTHZ_DENIED" }` and maps both `CompileFailed` and
`EvaluationFailed` to `Internal`. Don't copy oagw. Mini-chat needs 403/503 as described above.

### 2.6 Wiring in a gear

```rust
// gears/credstore/credstore/src/gear.rs (init)
#[toolkit::gear(name = "credstore", deps = [authz_resolver, tenant_resolver, types_registry],
                capabilities = [system, db, rest, stateful], lifecycle(entry = "serve", stop_timeout = "30s", await_ready))]
...
let authz_client = ctx.client_hub().get::<dyn AuthZResolverApi>()
    .map_err(|e| anyhow::anyhow!("failed to get AuthZResolverApi: {e}"))?;
let enforcer = PolicyEnforcer::new(authz_client);  // no capabilities -> PDP sends flat predicates
// oagw: let authz = ctx.client_hub().get::<dyn AuthZResolverApi>()?; let pe = PolicyEnforcer::new(authz);
```

`PolicyEnforcer` is cheap to `Clone`. Build it once in `init` and store it in the service.

### 2.7 Mini-chat call shapes

```rust
// list / read / update / delete / sub-actions on a chat (resource id = chat_id when present)
let scope = enforcer.access_scope(ctx, &CHAT, "read", Some(chat_id)).await.map_err(map_enforcer_err)?;
let scope = scope.ensure_owner(ctx.subject_id());  // static PDP never emits owner_id (see 2.8)
// create: send the properties of the row to be created
let scope = enforcer.access_scope_with(ctx, &CHAT, "create", None,
    &AccessRequest::new()
        .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
        .resource_property(pep_properties::OWNER_ID, ctx.subject_id())).await?;
// pure permission check (Model catalog): no constraints required
let _ = enforcer.access_scope_with(ctx, &MODEL, "list", None,
    &AccessRequest::new().require_constraints(false)).await?;  // static PDP -> allow_all()
```

### 2.8 What static-authz-plugin returns

Source: `gears/system/authz-resolver/plugins/static-authz-plugin/src/domain/service.rs`.
**The action and the resource type are ignored.** The plugin's decision logic:

1. `tid = context.tenant_context.root_id`, otherwise `subject.properties["tenant_id"]` parsed as a UUID.
   The enforcer always sets the latter to `ctx.subject_tenant_id()`.
2. If no tid can be resolved -> `decision: false`, no constraints (the enforcer returns `Denied { deny_reason: None }`).
3. If `tid == Uuid::nil()` -> `decision: false`. An anonymous context is therefore always denied.
4. If `resource.properties` contains `"group_ids"` or `"ancestor_group_ids"` -> `decision: false`.
5. If `"owner_tenant_id"` is in `supported_properties`, it pushes
   `Constraint { predicates: [In("owner_tenant_id", [tid])] }`.
6. Only if `Capability::TenantHierarchy` was advertised, it also pushes one
   `InTenantSubtree(prop, tid)` constraint for each of `owner_tenant_id` / `id` that is supported.
7. It returns `decision: true` with those constraints. **No `owner_id` predicate is ever emitted. No
   `eq` either: tenant scoping is an `in` with a single value.** `resource.id` and the resource
   properties (other than the group keys) are not inspected.

What this means for mini-chat (supported `[owner_tenant_id, owner_id, id]`, no capabilities):
- The scope is `owner_tenant_id IN (subject_tenant_id)`.
- Owner isolation must come from `scope.ensure_owner(ctx.subject_id())` or from an explicit
  `user_id` filter. DESIGN expects `eq(owner_tenant_id)+eq(owner_id)` from a real PDP, and
  `ensure_owner` handles both cases.
- A resource with `supported_properties = &[]` gets empty constraints. That is `allow_all()` with
  `require_constraints(false)` and `CompileFailed(ConstraintsRequiredButAbsent)` with the default `true`.
  The plugin logs a warning in the second case.
- `config/mini-chat.yaml` enables it with `vendor: constructorfabric`, `priority: 100`. The config
  type `StaticAuthZPluginConfig { vendor, priority }` has `deny_unknown_fields`.

### 2.9 PDP fakes for unit tests (copy from credstore)

```rust
// gears/credstore/credstore/src/domain/secret/test_support.rs
struct MockAuthZResolver;
#[async_trait]
impl AuthZResolverApi for MockAuthZResolver {
    async fn evaluate(&self, _ctx: PlatformSecurityContext, request: EvaluationRequest)
        -> Result<EvaluationResponse, CanonicalError> {
        let root = request.subject.properties.get("tenant_id")
            .and_then(serde_json::Value::as_str).and_then(|s| Uuid::parse_str(s).ok()).expect("tenant_id");
        Ok(EvaluationResponse { decision: true, context: EvaluationResponseContext {
            constraints: vec![Constraint { predicates: vec![Predicate::Eq(EqPredicate::new(pep_properties::OWNER_TENANT_ID, root))] }],
            ..Default::default() } })
    }
}
pub fn mock_enforcer() -> PolicyEnforcer { PolicyEnforcer::new(Arc::new(MockAuthZResolver)) }
// deny: Ok(EvaluationResponse { decision: false, context: EvaluationResponseContext::default() })
// unavailable: Err(CanonicalError::service_unavailable().create())
```

Imports for these fakes: `authz_resolver_sdk::constraints::{Constraint, EqPredicate, Predicate}`,
`authz_resolver_sdk::models::{EvaluationRequest, EvaluationResponse, EvaluationResponseContext}`,
`toolkit_security::{PlatformSecurityContext, SecurityContext, pep_properties}`,
`toolkit_canonical_errors::CanonicalError` (or `toolkit::api::canonical_prelude::CanonicalError`).
oagw has an allow-all fake as well: `gears/system/oagw/oagw/src/domain/test_support.rs::allow_all_enforcer()`.

---

## 3. authn-resolver-sdk: S2S client-credentials exchange

Source: `gears/system/authn-resolver/authn-resolver-sdk/src/{api.rs,models.rs,error.rs,plugin_api.rs,gts.rs}`.

```rust
use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest, AuthenticationResult, AuthNResolverError};

#[async_trait]
pub trait AuthNResolverClient: Send + Sync {
    async fn authenticate(&self, bearer_token: &str) -> Result<AuthenticationResult, AuthNResolverError>;
    async fn exchange_client_credentials(&self, request: &ClientCredentialsRequest)
        -> Result<AuthenticationResult, AuthNResolverError>;
}
pub struct ClientCredentialsRequest {      // no derives (no Debug/Clone)
    pub client_id: String,
    pub client_secret: secrecy::SecretString,
    pub scopes: Vec<String>,               // forwarded to IdP; static plugin ignores
}
#[derive(Debug, Clone)] pub struct AuthenticationResult { pub security_context: SecurityContext }
#[derive(Debug, thiserror::Error)] pub enum AuthNResolverError {
    Unauthorized(String), NoPluginAvailable, ServiceUnavailable(String),
    TokenAcquisitionFailed(String),       // bad credentials / IdP down
    Internal(String),
}
```

The client is registered in ClientHub by the `authn-resolver` gear as `Arc<dyn AuthNResolverClient>`
(`authn-resolver/src/gear.rs`). It is a system gear, so it initialises before ordinary gears. Usage:

```rust
let authn = ctx.client_hub().get::<dyn AuthNResolverClient>()?;
let req = ClientCredentialsRequest {
    client_id: cfg.client_credentials.client_id.clone(),
    client_secret: SecretString::from(cfg.client_credentials.client_secret.clone()),
    scopes: vec![],
};
let service_ctx: SecurityContext = authn.exchange_client_credentials(&req).await?.security_context;
```

Mini-chat config already carries this block (`config/mini-chat.yaml`, `gears.mini-chat.config.client_credentials`):
`client_id: "mini-chat"`, `client_secret: "mini-chat-dev-secret"`.

**static-authn-plugin and `s2s_credentials`** (`plugins/static-authn-plugin/src/{config.rs,domain/service.rs,domain/client.rs}`):

```rust
pub struct StaticAuthNPluginConfig {      // #[serde(default, deny_unknown_fields)]
    pub vendor: String, pub priority: i16, pub mode: AuthNMode /* accept_all | static_tokens */,
    pub default_identity: IdentityConfig, pub tokens: Vec<TokenMapping>,
    pub s2s_credentials: Vec<S2sCredentialMapping>,
}
pub struct S2sCredentialMapping {         // #[serde(deny_unknown_fields)]
    pub client_id: String,
    pub client_secret: SecretString,
    #[serde(default)] pub identity: IdentityConfig,     // default = DEFAULT_SUBJECT_ID / DEFAULT_TENANT_ID / ["*"] / None
    #[serde(default)] pub bearer_token: Option<SecretString>,
}
pub struct IdentityConfig { pub subject_id: Uuid, pub subject_tenant_id: Uuid, pub token_scopes: Vec<String>, pub subject_type: Option<String> }
```

`exchange_client_credentials` looks up `client_id`, compares `client_secret` exactly, and builds the
`SecurityContext` from `identity`, plus `bearer_token` when one is configured. An unknown id or a wrong
secret returns `Err(AuthNResolverError::TokenAcquisitionFailed("invalid client credentials"))`.
`scopes` is ignored. In the mini-chat dev config the service context comes out as
`subject_id=11111111-6a88-4768-9dfc-6bcd5187d9ed`, `subject_tenant_id=00000000-df51-5b42-9538-d2b56b7ee953`,
`token_scopes=["*"]`, no `subject_type`, no bearer token.
`authenticate` in `accept_all` mode accepts any non-empty token and returns `default_identity`.

Gateway error passthrough (`authn-resolver/src/domain/error.rs`): plugin not found becomes
`NoPluginAvailable`, an unavailable plugin becomes `ServiceUnavailable`, and `TokenAcquisitionFailed`
is passed through unchanged. Types-registry or internal failures become `Internal`.

---

## 4. toolkit-canonical-errors

Source: `libs/toolkit-canonical-errors/src/{lib.rs,error.rs,builder.rs,context.rs,problem.rs,transport.rs}`,
macro `libs/toolkit-canonical-errors-macro/src/lib.rs`, examples `tests/showcase.rs`.

### 4.1 Exports

```rust
use toolkit_canonical_errors::{
    CanonicalError, Problem, ProblemCategory, resource_error, gts_id,
    ResourceErrorBuilder, ServiceUnavailableBuilder, Http, TransportOverride, ForeignPassthrough,
    FieldViolation, PreconditionViolation, QuotaViolation, /* per-category ctx types *V1 */
};
// Handler prelude (libs/toolkit/src/api/mod.rs):
use toolkit::api::canonical_prelude::*;
//   CanonicalError, Problem, resource_error, ApiResult<T = ()> = Result<T, CanonicalError>,
//   OData, JsonBody, JsonPage, created_json, no_content, ok_json, extract (module), apply_select,
//   axum::Json (!! the plain axum one), StatusCode, IntoResponse
```

### 4.2 Categories, HTTP status, title, GTS type

`type` = `gts://gts.cf.core.errors.err.v1~cf.core.err.<category>.v1~`. Status comes from
`CanonicalError::default_http_status()`. Titles below are from `CanonicalError::title()`, which is what
goes on the wire. `ProblemCategory::title()` uses sentence case ("Invalid argument"); that is only
relevant to contract errors.

| Category (variant) | HTTP | `title` | Default `detail` | Wire `context` keys |
|---|---|---|---|---|
| `invalid_argument` (`InvalidArgument`) | 400 | Invalid Argument | "Request validation failed" (or the format/constraint text) | `field_violations[{field,description,reason}]` **or** `format` **or** `constraint` |
| `out_of_range` | 400 | Out of Range | caller-supplied | `field_violations[...]` |
| `failed_precondition` | 400 | Failed Precondition | "Operation precondition not met" (fixed) | `violations[{type,subject,description}]` |
| `unauthenticated` | 401 | Unauthenticated | "Authentication required" | `reason` (optional) |
| `permission_denied` | 403 | Permission Denied | "You do not have permission to perform this operation" (fixed) | `reason` |
| `not_found` | 404 | Not Found | caller-supplied | (none) |
| `already_exists` | 409 | Already Exists | caller-supplied | (none) |
| `aborted` | 409 | Aborted | caller-supplied | `reason` |
| `resource_exhausted` | 429 | Resource Exhausted | caller-supplied | `violations[{subject,description,retry_after_seconds?}]` |
| `cancelled` | 499 | Cancelled | "Operation cancelled by the client" | (none) |
| `internal` | 500 | Internal | always "An internal error occurred. Please retry later." | `{}` (the description is `#[serde(skip)]`, server-side only) |
| `unknown` | 500 | Unknown | "An unknown error occurred" | `{}` |
| `data_loss` | 500 | Data Loss | caller-supplied | (none) |
| `unimplemented` | 501 | Unimplemented | caller-supplied | (none) |
| `service_unavailable` | 503 | Service Unavailable | "Service temporarily unavailable" or `.with_detail` | `retry_after_seconds` (optional) |
| `deadline_exceeded` | 504 | Deadline Exceeded | caller-supplied | (none) |

Every category except `internal` also gets `context.resource_type` (set by the `#[resource_error]`
type) and `context.resource_name` (set by `.with_resource(..)`). These are merged into `context` by
`Problem::from_error`.

### 4.3 Wire shape (`Problem`, RFC 9457, `application/problem+json`)

```rust
pub struct Problem {
    #[serde(rename = "type")] pub problem_type: String,
    pub title: String,
    pub status: Option<u16>,
    pub detail: String,
    pub instance: Option<String>,      // skip if None; filled by canonical_error_middleware = request path
    pub trace_id: Option<String>,      // skip if None; filled by middleware (traceparent -> x-trace-id -> x-request-id -> span)
    pub context: serde_json::Value,    // always an object
    pub error_code: Option<String>,    // contract errors only; skip if None
    pub error_domain: Option<String>,  // contract errors only; skip if None
}
```

```json
{
  "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
  "title": "Not Found",
  "status": 404,
  "detail": "Chat not found",
  "instance": "/mini-chat/v1/chats/0193...",
  "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736",
  "context": { "resource_type": "gts.cf.core.mini_chat.chat.v1~", "resource_name": "0193..." }
}
```

`Problem` helpers: `Problem::from(err)` / `Problem::from_error(&err) -> Result<Problem, serde_json::Error>`,
`.with_instance(..)`, `.with_trace_id(..)`, `TryFrom<Problem> for CanonicalError` (lossy),
`Problem::contract_error(category, error_code, error_domain, detail, data)`.

### 4.4 Declaring a resource error type (macro)

```rust
use toolkit_canonical_errors::resource_error;   // also via toolkit::api::canonical_prelude
#[resource_error(gts_id!("cf.core.mini_chat.chat.v1~"))]      pub struct ChatResourceError;
#[resource_error(gts_id!("cf.core.mini_chat.message.v1~"))]   pub struct MessageResourceError;
#[resource_error(gts_id!("cf.core.mini_chat.turn.v1~"))]      pub struct TurnResourceError;
#[resource_error(gts_id!("cf.core.mini_chat.attachment.v1~"))] pub struct AttachmentResourceError;
#[resource_error(gts_id!("cf.core.mini_chat.model.v1~"))]     pub struct ModelResourceError;
```

Rules: unit struct only, no generics. The id must be a GTS **type** id (ends with `~`); this is checked
at compile time. `cf-gears-toolkit-canonical-errors` must be a direct dependency. A plain literal
(`"gts.cf.core.mini_chat.chat.v1~"`) is also accepted. Real declarations:
`gears/system/types-registry/types-registry/src/api/rest/error.rs:12`,
`gears/credstore/credstore/src/infra/sdk_error_mapping.rs:11`, `gears/system/oagw/oagw/src/api/rest/error.rs:86-106`.

Generated constructors and the builder typestate (`.create()` compiles only once the required parts are set):

| Constructor | resource name | required next call |
|---|---|---|
| `not_found(detail)`, `already_exists(detail)`, `data_loss(detail)` | **required** `.with_resource(name)` | none |
| `aborted(detail)` | optional | **`.with_reason(r)`** |
| `unknown(detail)`, `deadline_exceeded(detail)`, `unimplemented(detail)` | optional | none |
| `permission_denied()` | not allowed (no `.with_resource`) | **`.with_reason(r)`** |
| `cancelled()` | not allowed | none |
| `invalid_argument()` | optional | **one of** `.with_field_violation(field, description, reason)` (repeatable), `.with_format(msg)`, `.with_constraint(msg)`. format/constraint also replace `detail` |
| `out_of_range(detail)` | optional | **`.with_field_violation(..)`** (repeatable; `with_format`/`with_constraint` compile but are dropped for out_of_range) |
| `resource_exhausted(detail)` | optional | **`.with_quota_violation(subject, description)`** (repeatable), optional `.with_quota_violation_retry_after_seconds(u64)` (applies to the last one pushed) |
| `failed_precondition()` | optional | **`.with_precondition_violation(subject, description, type_)`** (repeatable). Note the argument order: subject, description, type |

Any builder also accepts `.with_override(Http::status_code(u16))`. The override must keep the same
status class; this is a `debug_assert`. The JSON extractor uses it for 415/422.
There is **no** `.with_detail` on `ResourceErrorBuilder`. The detail is either the constructor
argument or fixed.

System-level constructors on `CanonicalError` (no resource type):

```rust
CanonicalError::internal(diagnostic: impl Into<String>).create()      // diagnostic is logged, never on the wire
CanonicalError::unauthenticated().with_reason("TOKEN_EXPIRED").create()
CanonicalError::service_unavailable()                                  // ServiceUnavailableBuilder
    .with_retry_after_seconds(10)        // -> context.retry_after_seconds + HTTP `Retry-After: 10`
    .with_detail("authorization evaluation failed")   // must be safe for the public body
    .create()
```

Accessors on `CanonicalError`: `status_code() -> u16`, `title()`, `detail()`, `gts_type()`,
`resource_type()`, `resource_name()`, `diagnostic()` (Internal/Unknown only), `http_status_override()`.
`Display` renders `"<category>: <detail>"`. `From<serde_json::Error>` gives `invalid_argument` with
`format`. `From<std::io::Error>` and `From<sea_orm::DbErr>` (feature `sea-orm`) give `internal`.
The enum and its variants are `#[non_exhaustive]`, so use `matches!(e, CanonicalError::NotFound { .. })`.

### 4.5 Copy-paste examples (expected JSON from `tests/showcase.rs`)

```rust
ChatResourceError::not_found("Chat not found").with_resource(chat_id.to_string()).create();
// 404 {"context":{"resource_type":"gts.cf.core.mini_chat.chat.v1~","resource_name":"<id>"}}

ChatResourceError::invalid_argument()
    .with_field_violation("title", "title must be 1..=255 characters after trim", "INVALID_TITLE")
    .create();
// 400 detail "Request validation failed", context.field_violations=[{"field":"title","description":"...","reason":"INVALID_TITLE"}]

ChatResourceError::invalid_argument().with_format("outbox payload too large").create();
// 400 detail == context.format == "outbox payload too large"

AttachmentResourceError::out_of_range("File too large")
    .with_field_violation("content_length", "max 25 MiB", "FILE_TOO_LARGE").create();          // 400

AttachmentResourceError::failed_precondition()
    .with_precondition_violation("images", "image uploads are disabled", "FEATURE_DISABLED").create();
// 400 context.violations=[{"type":"FEATURE_DISABLED","subject":"images","description":"..."}]

ChatResourceError::permission_denied().with_reason("ACCESS_DENIED").create();               // 403 context.reason

TurnResourceError::aborted("Turn already in progress").with_reason("TURN_IN_PROGRESS").create();  // 409 context.reason

AttachmentResourceError::already_exists("vector store provider mismatch")
    .with_resource("provider_mismatch").create();                                           // 409

ChatResourceError::resource_exhausted("Per-chat document limit reached")
    .with_quota_violation("document_limit", "at most 50 documents per chat").create();       // 429 context.violations

CanonicalError::service_unavailable().with_retry_after_seconds(10).create();                 // 503 + Retry-After: 10
CanonicalError::internal(format!("db: {e}")).create();                                       // 500, opaque detail
```

The real `DomainError -> CanonicalError` pattern (types-registry, `api/rest/error.rs`):

```rust
impl From<DomainError> for CanonicalError {
    fn from(e: DomainError) -> Self {
        match e {
            DomainError::InvalidGtsId(msg) => TypeRegistryError::invalid_argument()
                .with_field_violation(field::GTS_ID_FIELD, msg, field::INVALID_GTS_ID).create(),
            DomainError::NotFound { kind, target } =>
                TypeRegistryError::not_found(format!("No entity with {kind}: {target}")).with_resource(target).create(),
            DomainError::NotInReadyMode => CanonicalError::service_unavailable().create(),
            DomainError::Internal(e) => { tracing::error!(error = ?e, "..."); CanonicalError::internal(e.to_string()).create() }
            /* ... */
        }
    }
}
```

Credstore (`infra/sdk_error_mapping.rs`) maps `AccessDenied` to
`SecretResource::permission_denied().with_reason("ACCESS_DENIED").create()` and
`ServiceUnavailable { detail, retry_after }` to
`CanonicalError::service_unavailable().with_detail(detail)` plus
`.with_retry_after_seconds(d.as_secs())` when it is set.

### 4.6 axum integration (feature `axum`)

- `impl IntoResponse for CanonicalError`: converts to `Problem` and stashes a clone of the error in the
  response extensions so the middleware can log `diagnostic()`.
- `impl IntoResponse for Problem` and `for Box<Problem>`: status from `problem.status` (fallback 500),
  header `Content-Type: application/problem+json`. **If the category is `service_unavailable` and
  `context.retry_after_seconds` is set, it also adds a `Retry-After: <seconds>` header** (test:
  `tests/axum_integration.rs::canonical_error_into_response_sets_retry_after_header`).
- `toolkit::api::canonical_error_middleware` (`libs/toolkit/src/api/canonical_error_layer.rs`) is
  layered by api-gateway (`gear.rs:554`) and by the OoP bootstrap. It fills `instance` (request path)
  and `trace_id`, logs 4xx at warn and 5xx at error, and wraps unshaped `text/plain` error responses
  into a `Problem`. Handlers should **not** set `instance`/`trace_id` themselves.
- Handler signature: `async fn h(..) -> ApiResult<Json<Dto>>` (i.e. `Result<_, CanonicalError>`), and
  use `?` on any domain error that has `From<DomainError> for CanonicalError`. oagw's older style
  returns `Result<impl IntoResponse, Box<Problem>>`.

### 4.7 Request extractors that produce canonical 400/415/422 automatically

Use `toolkit::api::rest::extract::{Json, Path, Query}` (`libs/toolkit/src/api/rest/extract/`).
**Do not use `axum::Json` / `axum::extract::Path` as extractors.** Watch out: `canonical_prelude`
re-exports `axum::Json`, which is fine as a response body only. Follow types-registry:

```rust
// gears/system/types-registry/types-registry/src/api/rest/handlers.rs
use axum::Json;                         // response body
use axum::extract::Extension;
use toolkit::api::canonical_prelude::*;
use toolkit::api::rest::extract;

pub async fn register_entities(
    Extension(service): Extension<Arc<TypesRegistryService>>,
    extract::Json(req): extract::Json<RegisterEntitiesRequest>,
) -> ApiResult<(StatusCode, Json<RegisterEntitiesResponse>)> { ... }

pub async fn get_entity(
    Extension(service): Extension<Arc<TypesRegistryService>>,
    extract::Path(gts_id): extract::Path<String>,
) -> ApiResult<Json<GtsEntityDto>> { ... }
```

Every rejection becomes `invalid_argument` on resource type `gts.cf.core.http.request.v1~`
(`GenericResourceError` in `extract/error.rs`), with one field violation and the status overridden to
axum's status. Bodies are verified in `extract/json.rs` and `extract/path.rs` tests:

| Situation | HTTP | `field` | `reason` |
|---|---|---|---|
| malformed JSON (`{not-json}`) | 400 | `body` | `json_syntax_error` |
| schema mismatch (missing/unknown field with `deny_unknown_fields`, bad enum variant, wrong type, non-UUID in a `Uuid` field) | 422 | `body` | `invalid_json_body` |
| no/wrong `Content-Type` (not `application/json`) | 415 | `body` | `missing_json_content_type` |
| body read failure / length limit | axum status (e.g. 413) | `body` | `json_body_read_error` |
| path segment fails to deserialize (e.g. non-UUID into `Uuid`) | 400 | `path` | `invalid_path_params` |
| route/type mismatch in path params (programming bug, axum 500) | 500 | n/a | `internal` |
| query string fails to deserialize | 400 | `query` | `invalid_query_string` |

```json
{"type":"gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~","title":"Invalid Argument","status":422,
 "detail":"Request validation failed",
 "context":{"resource_type":"gts.cf.core.http.request.v1~",
  "field_violations":[{"field":"body","description":"Failed to deserialize the JSON body into the target type: extra: unknown field `extra`, expected `name` at line 1 column 19","reason":"invalid_json_body"}]}}
```

Descriptions are axum/serde text, capped at 500 characters. For OData list endpoints use
`toolkit::api::canonical_prelude::OData`. Its errors carry resource type `gts.cf.core.odata.query.v1~`.

---

## 5. toolkit-gts / gts: ids, type schemas, instances, plugin specs

Source: `libs/toolkit-gts/src/{lib.rs,plugin.rs,permission.rs}`, `libs/toolkit-gts-macros/src/lib.rs`,
`libs/toolkit-gts/tests/macro_integration.rs`.

### 5.1 Id macros

```rust
use toolkit_gts::{gts_id, gts_uri, GTS_ID_PREFIX, GtsId, GtsInstanceId};
const CHAT_TYPE: &str = gts_id!("cf.core.mini_chat.chat.v1~");  // "gts.cf.core.mini_chat.chat.v1~" (prefix configurable via GTS_ID_PREFIX at build time)
let uri: &str = gts_uri!("cf.core.mini_chat.chat.v1~");         // "gts://gts.cf.core.mini_chat.chat.v1~"
```

The id grammar is `gts.<vendor>.<package>.<namespace>.<type>.v<N>~`. A type id ends with `~`. An
instance id is `<type id><vendor>.<package>.<namespace>.<name>.v<N>` (no trailing `~`). Chained types
look like `base~derived~`. `gts_id!` validates the id at compile time and does not double an existing
prefix. Prefer it over hard-coded `"gts.…"` literals everywhere: `ResourceType`, `#[resource_error]`,
`type_id =`, `id:`.

### 5.2 Plugin spec (for mini-chat's audit / model-policy plugins)

Base: `toolkit_gts::PluginV1<P>`, also re-exported as `toolkit::gts::PluginV1`. Type id
`gts.cf.toolkit.plugins.plugin.v1~`. Fields: `id: GtsInstanceId, vendor: String, priority: i16, properties: P`.

```rust
// pattern: gears/system/authz-resolver/authz-resolver-sdk/src/gts.rs (identical in authn/credstore sdk)
use toolkit::gts::PluginV1;
use toolkit_gts::gts_type_schema;

#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~"),
    description = "Mini-chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;
```

Plugin gear side (`plugins/static-authz-plugin/src/gear.rs`):

```rust
#[toolkit::gear(name = "static-authz-plugin", deps = [types_registry])]
...
let (instance_id, instance_json) = PluginV1::<AuthZResolverPluginSpecV1>::build_registration(
    "cf.builtin.static_authz_resolver.plugin.v1",   // instance segment
    cfg.vendor.clone(), cfg.priority)?;             // -> (GtsInstanceId, serde_json::Value)
let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;   // types_registry_sdk
let results = registry.register(vec![instance_json]).await?;
RegisterResult::ensure_all_ok(&results)?;
let api: Arc<dyn AuthZResolverPluginClient> = service;
ctx.client_hub().register_scoped::<dyn AuthZResolverPluginClient>(ClientScope::gts_id(&instance_id), api);
```

Consumer side (`authz-resolver/src/domain/service.rs`): selection is lazy and happens on first use,
not in `init`.

```rust
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};

let plugin_type_id = AuthZResolverPluginSpecV1::gts_type_id().clone();
let instances = registry.list_instances(InstanceQuery::new().with_pattern(format!("{plugin_type_id}*"))).await?;
let gts_id = choose_plugin_instance::<AuthZResolverPluginSpecV1>(&self.vendor,
    instances.iter().map(|e| (e.id.as_ref(), &e.object)))?;     // Err(ChoosePluginError::{PluginNotFound,InvalidPluginInstance})
// cache with GtsPluginSelector::new().get_or_init(|| self.resolve_plugin()).await?
let client = hub.try_get_scoped::<dyn AuthZResolverPluginClient>(&ClientScope::gts_id(&gts_id));
```

### 5.3 Resource type schemas and permission instances

Static authz does not need registered resource types, because it ignores `resource.type`. A real
RBAC PDP validates role `target_type` against types-registry, so with a real PDP an unregistered type
means every request is denied (see the `credstore-sdk/src/gts.rs` doc). A minimal type schema that is
picked up automatically through the link-time inventory:

```rust
// shape from libs/toolkit-gts/tests/macro_integration.rs
use toolkit_gts::{gts_id, gts_type_schema};
use gts::GtsInstanceId;
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.mini_chat.chat.v1~"),
    description = "Mini-chat chat resource",
    properties = "id,name",
    base = true
)]
pub struct ChatV1 { pub id: GtsInstanceId, pub name: String }
```

The required attributes are `dir_path`, `type_id`, `description`, `properties` and `base`. Optional
ones include `traits_schema`, `traits` and `gts_abstract`. Instances: `toolkit_gts::gts_instance! { Type { id: gts_id!("<type>~<inst>.v1"), .. } }`,
optionally with `#[gts_static(NAME)]` to also emit a `pub static NAME: LazyLock<Type>`, and
`gts_instance_raw!({ "id": gts_id!(..), .. })`. These need a direct `gts` dependency, which mini-chat
already has.

Permissions: base `toolkit_gts::AuthzPermissionV1 { id: GtsInstanceId, resource_type: String, action: String, display_name: String }`,
type `gts.cf.toolkit.authz.permission.v1~`. Instance convention:
`gts.cf.toolkit.authz.permission.v1~<vendor>.<package>.<namespace>.<permission_name>.v1`, for example:

```rust
toolkit_gts::gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.mini_chat._.chat_read.v1"),
        resource_type: gts_id!("cf.core.mini_chat.chat.v1~").to_owned(),
        action: "read".to_owned(),
        display_name: "Read chat".to_owned(),
    }
}
```

(No gear in the repo declares permission instances yet. The shape above follows the
`toolkit-gts-macros` doc example.)

---

## 6. Gotchas checklist

1. `token_scopes == []` means **no capability**, not unrestricted. Use `has_scope()`. With
   `auth_disabled` the injected context has empty scopes.
2. The static PDP never returns `owner_id` and never looks at `resource.id`. Always apply
   `ensure_owner(ctx.subject_id())` for owner-only resources, or keep an explicit `user_id` filter.
3. The static PDP denies an anonymous or nil-tenant subject (`EnforcerError::Denied { deny_reason: None }`).
4. `require_constraints` defaults to `true`. A resource with no supported properties then fails with
   `CompileFailed(ConstraintsRequiredButAbsent)`. Use `.require_constraints(false)` for pure
   permission checks such as Model.
5. `Denied`/`CompileFailed` -> 403 `permission_denied` (with a reason). `EvaluationFailed` -> 503
   `service_unavailable`, adding `.with_retry_after_seconds(n)` if you want `Retry-After`. A PDP
   timeout (30 s by default) is also reported as `EvaluationFailed`.
6. `CanonicalError::internal(msg)`: `msg` never reaches the client. The wire detail is fixed.
7. `permission_denied()` / `failed_precondition()` / `invalid_argument()` details are fixed (the last
   one unless you use `with_format`/`with_constraint`). Put the machine-readable data in
   `reason`/`violations`.
8. `with_precondition_violation(subject, description, type_)` takes its arguments in that order, while
   on the wire the keys appear as `{type, subject, description}`.
9. Use `toolkit::api::rest::extract::{Json, Path, Query}` as extractors. `axum::Json` is for responses only.
10. `authn-resolver-sdk` is a path dependency, not a workspace one. `ClientCredentialsRequest` has no
    `Debug`/`Clone`.
