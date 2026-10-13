# Research: Platform Integrations for the `mini-chat` Gear

**Scope**: what the new gear `gears/mini-chat/mini-chat` (lib `mini_chat`) needs to know to call an
OpenAI-compatible LLM provider through the Outbound API Gateway (OAGW). Also covers the platform pieces
around that call: S2S AuthN, AuthZ/PEP, types-registry plugin discovery, SecurityContext, licence gating
and metrics.

All paths are relative to `/app`. Line numbers refer to the tree at commit `849a09f` (spec-bench seed).
Snippets are copied from the source unless they are marked **(proposed)**.

---

## 0. TL;DR - facts that drive design decisions

| # | Fact | Source |
|---|------|--------|
| 1 | The OAGW client trait is `oagw_sdk::ServiceGatewayClientV1`, obtained with `ctx.client_hub().get::<dyn ServiceGatewayClientV1>()?`. Every method returns `Result<_, toolkit_canonical_errors::CanonicalError>`. | `gears/system/oagw/oagw-sdk/src/api.rs:63-156`, `gears/system/oagw/oagw/src/gear.rs:160-164` |
| 2 | Proxy URI format: `/{alias}{path_suffix}?{query}`. The first path segment is the alias. The rest is matched against the route `HttpMatch.path` by **prefix**. | `oagw/src/infra/proxy/service.rs:367-378`, `infra/storage/route_repo.rs:143-151` |
| 3 | Alias rules. For **hostname** endpoints, OAGW **derives** the alias: `host` on the standard port (80 for http, 443 for https), `host:port` otherwise. An explicit alias that differs from the derived one is **rejected** (400). For **IP** endpoints (`127.0.0.1`), an explicit alias is **required**. | `oagw/src/domain/services/management/alias.rs:53-100,149-196` |
| 4 | Plain `http` upstreams need `oagw.config.allow_http_upstream: true`. Without it, every proxy call fails with 400 `HTTP_UPSTREAM_FORBIDDEN`. Loopback / private IPs need `ssrf_policy.enabled: false`, otherwise upstream **creation** fails with 400. | `oagw/src/config.rs:15-22`, `service.rs:750-758`, `management/validation.rs:96-123` |
| 5 | OAGW upstreams and routes live in an **in-memory** repo. They are lost on restart, so mini-chat must (re)create them on every start. | `oagw/src/gear.rs:80-81` |
| 6 | Duplicate alias in the same tenant gives `CanonicalError::AlreadyExists` (resource `gts.cf.core.oagw.upstream.v1~`). Duplicate route (same path + priority + method) gives `AlreadyExists` (resource `...route.v1~`). A `secret_ref` that is not yet readable gives `FailedPrecondition` with violation subject `auth.config.secret_ref`, type `STATE`; this one is retryable. | `oagw/src/api/rest/error.rs:154-168,232-242`, `management/validation.rs:176-237`, `management/bind.rs:156-196` |
| 7 | An upstream created in tenant T is visible to **descendant** tenants only if at least one of `auth`, `rate_limit`, `plugins`, `cors` has `sharing != Private`. Use `SharingMode::Inherit` on `auth` when the upstream is created under the S2S (root) tenant. | `oagw/src/domain/services/management/merge.rs:14-35`, `resolution.rs:66-83` |
| 8 | Default header passthrough is `None`: only `Content-Type` is forwarded. Inbound `Authorization` / `Cookie` are **always** stripped. The auth plugin then injects the credential header (e.g. `api-key`). Extra provider headers (e.g. `Accept`, `anthropic-version`) need `HeadersConfig.request.passthrough = Allowlist/All` or `set`. | `oagw/src/infra/proxy/headers.rs:13-56`, `service.rs:632-649,671-694` |
| 9 | Query params must be listed in `HttpMatch.query_allowlist`. Otherwise the call fails with 400 `QUERY_NOT_ALLOWED`; this matters for Azure's `api-version`. | `service.rs:594-611` |
| 10 | `proxy_request` returns `Ok(Response)` for **any upstream status**, so a provider 4xx/5xx/429 arrives as `Ok`. The response body is **always** `Body::Stream`. `resp.extensions().get::<ErrorSource>()` tells gateway errors (`Gateway`) apart from provider errors (`Upstream`). Pingora-level failures (connect refused, read timeout, ...) also come back as `Ok(Response)` with status 502/503/504, an `application/problem+json` body and `ErrorSource::Gateway`. Pipeline failures before forwarding (alias not found, authz, timeout waiting for headers, ...) come back as `Err(CanonicalError)`. | `service.rs:1741-1760`, `pingora_proxy.rs:777-880`, `oagw/tests/proxy_integration.rs:302-321,747-815` |
| 11 | Timeouts: `proxy_timeout_secs` (default 30) bounds the wait for **response headers** and gives `Err(CanonicalError::DeadlineExceeded)` (HTTP 504). An SSE body has a separate **idle** timeout, `streaming_idle_timeout_secs` (default 300). When it fires, the stream simply **ends** (`None`) with no error, so mini-chat must treat "stream ended without a terminal event" as a failure. | `service.rs:1029,1115-1138,1173-1179`, `session_bridge.rs:504-549` |
| 12 | S2S: `AuthNResolverClient::exchange_client_credentials(&ClientCredentialsRequest{client_id, client_secret: SecretString, scopes})` returns `AuthenticationResult{security_context}`. The static plugin maps `s2s_credentials[].client_id/secret` to `identity`. The identity defaults to `DEFAULT_SUBJECT_ID` / `DEFAULT_TENANT_ID` (`00000000-df51-5b42-9538-d2b56b7ee953`) with scopes `["*"]`. | `authn-resolver-sdk/src/api.rs:69-72`, `models.rs:28-39`, `static-authn-plugin/src/config.rs:96-112` |
| 13 | PEP: `PolicyEnforcer::access_scope(&ctx, &RESOURCE, action, Some(id))` returns `Result<AccessScope, EnforcerError>`. Map `Denied` / `CompileFailed` to 403 and `EvaluationFailed` to 503 (the credstore precedent). The static-authz plugin allows everything and returns **only** an `owner_tenant_id IN (ctx tenant)` constraint, never an `owner_id` constraint. A nil tenant is denied. | `authz-resolver-sdk/src/pep/enforcer.rs:33-50,517-610`, `static-authz-plugin/src/domain/service.rs:49-172`, `gears/credstore/credstore/src/domain/authz.rs:33-49` |
| 14 | Plugin discovery: a `#[gts_type_schema(base = PluginV1, type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.<x>.plugin.v1~"))]` unit struct. Plugins publish with `PluginV1::<Spec>::build_registration(..)` and `register_scoped(ClientScope::gts_id(&id), ..)`. The main gear resolves lazily with `GtsPluginSelector` + `choose_plugin_instance::<Spec>(vendor, ..)`; the **lowest** priority wins. | `toolkit-gts/src/plugin.rs`, `toolkit/src/plugins/mod.rs`, `authn-resolver/src/domain/service.rs:49-96` |
| 15 | Licence: an operation declares a feature with `.authenticated().require_license_features([&License])`. The api-gateway middleware is a **stub**: it allows only `gts.cf.core.lic.feat.v1~cf.core.global.base.v1` and answers 403 `LICENSE_FEATURE_REQUIRED` for any other feature. | `api-gateway/src/middleware/license_validation.rs:13,48-79` |
| 16 | Metrics: `opentelemetry::global::meter_with_scope(InstrumentationScope::builder("<gear>").build())`, then `meter.u64_counter(name).with_description(..).build()`. When OTel metrics are disabled, the provider is a no-op. | `oagw/src/gear.rs:99-105`, `oagw/src/infra/metrics.rs:62-106` |

---

## 1. OAGW (Outbound API Gateway)

### 1.1 Crates and wiring

- SDK: `gears/system/oagw/oagw-sdk` (package `cf-gears-oagw-sdk`, lib `oagw_sdk`). Workspace dep `oagw-sdk = { workspace = true }`.
- Impl: `gears/system/oagw/oagw` (package `cf-gears-oagw`, lib `oagw`). It has a `test-utils` feature that exposes `oagw::test_support` (`AppHarness`, `MockGuard`, ...).
- Gear declaration (`oagw/src/gear.rs:41-45`):

```rust
#[toolkit::gear(
    name = "oagw",
    deps = [types_registry, authz_resolver, credstore, tenant_resolver],
    capabilities = [system, rest]
)]
pub struct OutboundApiGatewayGear { ... }
```

OAGW is a `system` gear and **requires** the `credstore` and `tenant_resolver` gears to be present.

- Client registration (`oagw/src/gear.rs:159-164`):

```rust
let oagw: Arc<dyn ServiceGatewayClientV1> =
    Arc::new(ServiceGatewayClientV1Facade::new(cp.clone(), dp.clone()));
ctx.client_hub()
    .register::<dyn ServiceGatewayClientV1>(oagw.clone());
```

- Repos are in-memory (`oagw/src/gear.rs:80-81`: `InMemoryUpstreamRepo::new()`, `InMemoryRouteRepo::new()`).
- **Linking caveat.** `apps/cf-gears-example-server` links OAGW only under its `oagw` feature (`registered_gears.rs:4-5`, `use api_egress as _;`). `config/mini-chat.yaml` documents the run command as `--features mini-chat,static-authn,static-authz,single-tenant,static-credstore`, which has **no** `oagw`. `gears/mini-chat/mini-chat/Cargo.toml` already depends on `oagw = { workspace = true }`. Either mini-chat forces the link (for example `use oagw as _;` in `lib.rs`), or the server feature `mini-chat` must also enable `oagw`. Otherwise the OAGW gear is not registered and `client_hub().get::<dyn ServiceGatewayClientV1>()` fails.

### 1.2 The client trait (copy of `oagw-sdk/src/api.rs:63-156`, doc comments trimmed)

```rust
use async_trait::async_trait;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;
use crate::body::Body;

#[async_trait]
pub trait ServiceGatewayClientV1: Send + Sync {
    async fn create_upstream(&self, ctx: SecurityContext, req: CreateUpstreamRequest) -> Result<Upstream, CanonicalError>;
    async fn get_upstream(&self, ctx: SecurityContext, id: Uuid) -> Result<Upstream, CanonicalError>;
    async fn list_upstreams(&self, ctx: SecurityContext, query: &ListQuery) -> Result<Vec<Upstream>, CanonicalError>;
    async fn update_upstream(&self, ctx: SecurityContext, id: Uuid, req: UpdateUpstreamRequest) -> Result<Upstream, CanonicalError>;
    async fn delete_upstream(&self, ctx: SecurityContext, id: Uuid) -> Result<(), CanonicalError>;

    async fn create_route(&self, ctx: SecurityContext, req: CreateRouteRequest) -> Result<Route, CanonicalError>;
    async fn get_route(&self, ctx: SecurityContext, id: Uuid) -> Result<Route, CanonicalError>;
    async fn list_routes(&self, ctx: SecurityContext, upstream_id: Option<Uuid>, query: &ListQuery) -> Result<Vec<Route>, CanonicalError>;
    async fn update_route(&self, ctx: SecurityContext, id: Uuid, req: UpdateRouteRequest) -> Result<Route, CanonicalError>;
    async fn delete_route(&self, ctx: SecurityContext, id: Uuid) -> Result<(), CanonicalError>;

    /// Resolve merged upstream + matched route without proxying (diagnostics).
    async fn resolve_proxy_target(&self, ctx: SecurityContext, alias: &str, method: &str, path: &str)
        -> Result<(Upstream, Route), CanonicalError>;

    /// resolve -> auth -> rate-limit -> forward -> respond.
    /// The request URI must follow `/{alias}/{path_suffix}?query`.
    async fn proxy_request(&self, ctx: SecurityContext, req: http::Request<Body>)
        -> Result<http::Response<Body>, CanonicalError>;
}

/// On proxy responses: resp.extensions().get::<ErrorSource>()
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSource { Gateway, Upstream }   // api.rs:19-23
```

Note that `ctx: SecurityContext` is passed **by value**, so clone it per call.

SDK re-exports (`oagw-sdk/src/lib.rs`): `AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HeadersConfig, HttpMatch, HttpMethod, ListQuery, MatchRules, PassthroughMode, PathSuffixMode, RequestHeaderRules, ResponseHeaderRules, Route, Scheme, Server, SharingMode, Upstream, UpdateUpstreamRequest, UpdateRouteRequest, ServiceGatewayClientV1, Body, Json, ServiceGatewayError, StreamingError, APIKEY_AUTH_PLUGIN_ID, HTTP_PROTOCOL_ID, UPSTREAM_SCHEMA, ROUTE_SCHEMA, PROXY_SCHEMA, ServerEvent, ServerEventsResponse, ServerEventsStream, FromServerEvent`. `ErrorSource` is at `oagw_sdk::api::ErrorSource`.

### 1.3 Model types (`oagw-sdk/src/models.rs`)

The models have no serde derives; they are plain Rust types.

```rust
// models.rs:16-22
pub enum SharingMode { #[default] Private, Inherit, Enforce }

// models.rs:30-37
pub enum Scheme { Http, #[default] Https, Wss, Wt, Grpc }

// models.rs:41-45
pub struct Endpoint { pub scheme: Scheme, pub host: String, pub port: u16 }
// models.rs:66-68
pub struct Server { pub endpoints: Vec<Endpoint> }

// models.rs:76-82
pub struct AuthConfig {
    pub plugin_type: String,                         // GTS id of auth plugin
    pub sharing: SharingMode,
    pub config: Option<HashMap<String, String>>,     // flat k/v, schema per plugin
}

// models.rs:90-125
pub struct HeadersConfig { pub request: Option<RequestHeaderRules>, pub response: Option<ResponseHeaderRules> }
pub struct RequestHeaderRules {
    pub set: HashMap<String, String>, pub add: HashMap<String, String>, pub remove: Vec<String>,
    pub passthrough: PassthroughMode, pub passthrough_allowlist: Vec<String>,
}
pub enum PassthroughMode { #[default] None, Allowlist, All }

// models.rs:264-304
pub enum HttpMethod { Get, Post, Put, Delete, Patch }     // no HEAD/OPTIONS
pub enum PathSuffixMode { Disabled, #[default] Append }
pub struct HttpMatch {
    pub methods: Vec<HttpMethod>,       // >= 1 required
    pub path: String,                   // prefix, must start with '/'
    pub query_allowlist: Vec<String>,   // empty = allow NO query params
    pub path_suffix_mode: PathSuffixMode,
}
pub struct MatchRules { pub http: Option<HttpMatch>, pub grpc: Option<GrpcMatch> }

// models.rs:312-341
pub struct Route { pub id: Uuid, pub tenant_id: Uuid, pub upstream_id: Uuid, pub match_rules: MatchRules,
    pub plugins: Option<PluginsConfig>, pub rate_limit: Option<RateLimitConfig>, pub cors: Option<CorsConfig>,
    pub tags: Vec<String>, pub priority: i32, pub enabled: bool }
pub struct Upstream { pub id: Uuid, pub tenant_id: Uuid, pub alias: String, pub server: Server,
    pub protocol: String, pub enabled: bool, pub auth: Option<AuthConfig>, pub headers: Option<HeadersConfig>,
    pub plugins: Option<PluginsConfig>, pub rate_limit: Option<RateLimitConfig>, pub cors: Option<CorsConfig>,
    pub tags: Vec<String> }

// models.rs:348-360
pub struct ListQuery { pub top: u32, pub skip: u32 }   // Default { top: 50, skip: 0 }
```

Builders. The request types have private fields, so construction goes through the builders:

```rust
// models.rs:383, 443-489
CreateUpstreamRequest::builder(server: Server, protocol: impl Into<String>) -> CreateUpstreamRequestBuilder
  .alias(impl Into<String>) .auth(AuthConfig) .headers(HeadersConfig) .plugins(PluginsConfig)
  .rate_limit(RateLimitConfig) .cors(CorsConfig) .tags(Vec<String>) .enabled(bool)   // enabled defaults to true
  .build() -> CreateUpstreamRequest
// models.rs:510 — UpdateUpstreamRequest::builder(server, protocol) — same setters (PUT semantics)
// models.rs:638, 688-724
CreateRouteRequest::builder(upstream_id: Uuid, match_rules: MatchRules)
  .plugins(..) .rate_limit(..) .cors(..) .tags(Vec<String>) .priority(i32 /*default 0*/) .enabled(bool /*default true*/) .build()
// models.rs:742 — UpdateRouteRequest::builder(match_rules)
```

GTS constants (`oagw-sdk/src/gts.rs:28-53`):

```rust
pub const HTTP_PROTOCOL_ID: &str     = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.http.v1");
pub const APIKEY_AUTH_PLUGIN_ID: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1");
pub const UPSTREAM_SCHEMA: &str = gts_id!("cf.core.oagw.upstream.v1~");
pub const ROUTE_SCHEMA: &str    = gts_id!("cf.core.oagw.route.v1~");
pub const PROXY_SCHEMA: &str    = gts_id!("cf.core.oagw.proxy.v1~");
```

`gts_id!("x")` expands to `"gts.x"`, so `APIKEY_AUTH_PLUGIN_ID == "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"`. This is the same string that `config/mini-chat.yaml` uses as `auth_plugin_type`.

### 1.4 The API-key auth plugin config

`oagw/src/infra/plugin/apikey_auth.rs:10-63`:

```rust
#[derive(Debug, Deserialize)]
struct ApiKeyConfig {
    header: String,            // e.g. "api-key" (Azure) / "authorization" (OpenAI)
    #[serde(default)]
    prefix: String,            // e.g. "" (Azure) / "Bearer " (OpenAI)
    secret_ref: String,        // e.g. "azure-openai-key" or "cred://azure-openai-key"
}
// authenticate(): strip "cred://" prefix, credstore.get(&ctx.security_context, &SecretRef)
//   -> header.to_lowercase() = prefix + secret
```

- The credstore lookup runs with the **proxy caller's** `SecurityContext`, which is the user's context. A static-credstore secret declared without `tenant_id` is **global** and readable by any tenant (`static-credstore-plugin/src/config.rs:38-53`). `config/mini-chat.yaml:129-133` (`key: "azure-openai-key"`) is such a global secret.
- `create_upstream` checks up front that the `secret_ref` is readable with the **creator's** context (`management/mod.rs:127-133` → `bind.rs:156-196`). If it is not readable (not provisioned yet or not shared), OAGW returns `CanonicalError::FailedPrecondition` with `violations[0].subject == "auth.config.secret_ref"` and `type_ == "STATE"`. This is the retryable case that DESIGN's background reconcile handles.

### 1.5 Upstream + route creation: code for mini-chat **(proposed)**

This is modelled on `oagw/tests/proxy_integration.rs:113-211`.

```rust
use std::collections::HashMap;
use oagw_sdk::{
    APIKEY_AUTH_PLUGIN_ID, AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint,
    HTTP_PROTOCOL_ID, HeadersConfig, HttpMatch, HttpMethod, ListQuery, MatchRules, PassthroughMode,
    PathSuffixMode, RequestHeaderRules, Scheme, Server, ServiceGatewayClientV1, SharingMode,
};
use toolkit_canonical_errors::CanonicalError;

async fn ensure_provider_upstream(
    gw: &dyn ServiceGatewayClientV1,
    s2s: &SecurityContext,
    host: &str, port: u16, use_http: bool,
    alias: &str,                       // see alias rules in §1.6
    api_path: &str,                    // "/openai/v1/responses"
    auth_plugin_type: &str,            // APIKEY_AUTH_PLUGIN_ID
    auth_config: HashMap<String, String>, // {"header":"api-key","prefix":"","secret_ref":"azure-openai-key"}
) -> Result<(), CanonicalError> {
    let server = Server { endpoints: vec![Endpoint {
        scheme: if use_http { Scheme::Http } else { Scheme::Https },
        host: host.to_owned(),
        port,
    }]};
    let req = CreateUpstreamRequest::builder(server, HTTP_PROTOCOL_ID)
        .alias(alias)
        .auth(AuthConfig {
            plugin_type: auth_plugin_type.to_owned(),
            // Inherit => visible to descendant tenants (merge.rs:14-35). Private hides it from children.
            sharing: SharingMode::Inherit,
            config: Some(auth_config),
        })
        .headers(HeadersConfig {
            request: Some(RequestHeaderRules {
                passthrough: PassthroughMode::Allowlist,
                passthrough_allowlist: vec!["accept".into(), "x-request-id".into()],
                ..Default::default()
            }),
            response: None,
        })
        .tags(vec!["mini-chat".into()])
        .build();

    let upstream_id = match gw.create_upstream(s2s.clone(), req).await {
        Ok(u) => u.id,                                  // u.alias = alias OAGW actually stored
        Err(CanonicalError::AlreadyExists { .. }) => {
            // Reuse: find by alias (aliases are normalised lowercase, trailing dots stripped).
            let all = gw.list_upstreams(s2s.clone(), &ListQuery { top: 1000, skip: 0 }).await?;
            all.into_iter().find(|u| u.alias == alias.to_ascii_lowercase())
                .map(|u| u.id)
                .ok_or_else(|| CanonicalError::internal("upstream exists but not listed").create())?
        }
        Err(e) => return Err(e),  // FailedPrecondition{auth.config.secret_ref} => defer & retry
    };

    let route = CreateRouteRequest::builder(upstream_id, MatchRules {
        http: Some(HttpMatch {
            methods: vec![HttpMethod::Post],
            path: api_path.to_owned(),
            query_allowlist: vec!["api-version".into()],   // Azure needs this
            path_suffix_mode: PathSuffixMode::Append,
        }),
        grpc: None,
    }).build();
    match gw.create_route(s2s.clone(), route).await {
        Ok(_) | Err(CanonicalError::AlreadyExists { .. }) => Ok(()), // route overlap => already there
        Err(e) => Err(e),
    }
}
```

Notes:

- Route uniqueness: an enabled route that has the same `path`, the same `priority` and any overlapping method conflicts → `AlreadyExists` (`management/validation.rs:176-237`). Route matching uses `path_suffix.starts_with(route.path)`, longest prefix first, then highest priority (`infra/storage/route_repo.rs:143-153`).
- A single route with `path: "/openai"`, `PathSuffixMode::Append` and methods `[Get, Post, Delete]` would cover `/openai/files`, `/openai/vector_stores/...` and `/openai/v1/responses`. `PathSuffixMode::Disabled` rejects any extra suffix with 400 `PATH_SUFFIX_NOT_ALLOWED` (`service.rs:613-630`).
- How the upstream URL is built: `scheme://host[:port] + route.path + remaining_suffix + ?query` (`request_builder.rs:10-58`, called at `service.rs:775-789`). Port 80 (http) and port 443 (https) are omitted.
- No AuthZ check runs on upstream or route **create** unless an ancestor tenant already owns the same alias (bind flow, `bind.rs:40-80`). Creation needs a `SecurityContext` whose tenant the tenant-resolver knows, because `build_tenant_chain` calls `get_ancestors` (`resolution.rs:18-38`). The single-tenant plugin rejects a nil tenant.
- Validation: endpoints must be all-IP or all-hostname, IPv6 is rejected, and a pool shares one scheme and one port (`management/validation.rs:31-89`).

### 1.6 Alias derivation (important for the `upstream_alias defaults to host` design)

`oagw/src/domain/services/management/alias.rs`:

```rust
// 58-66 (doc): Single host, standard port → hostname; non-standard port → hostname:port;
//              multi-host → common registrable suffix (+":port" if non-standard); IP addresses → None
pub(in crate::domain::services) fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String>

// 154-178
pub(in crate::domain::services) fn enforce_alias_create_with(user_alias: &str, endpoints: &[Endpoint]) -> Result<String, DomainError> {
    match compute_derived_alias(endpoints) {
        Some(derived) => {
            let normalized_user = normalize_alias(user_alias);
            if normalized_user != derived {
                return Err(DomainError::validation(format!(
                    "alias is auto-derived for hostname-based endpoints; \
                     remove the 'alias' field (derived: '{derived}')")));
            }
            validate_alias(&derived)?; Ok(derived)
        }
        None => { let normalized = normalize_alias(user_alias); validate_alias(&normalized)?; Ok(normalized) }
    }
}
// 184-196: no alias + IP endpoint => Err("explicit alias is required for IP-based or heterogeneous-host endpoints")
```

- `normalize_alias` lowercases the alias and strips trailing dots. Allowed characters are `[A-Za-z0-9.:_-]`, at most 253 characters, at least one alphanumeric, and not `.` or `..` (`alias.rs:9-46`).
- Standard port depends on the scheme: `Http => 80`, otherwise `443` (`domain/model.rs:49-54`). The SDK helper `Endpoint::alias_contribution` (`oagw-sdk/src/models.rs:51-62`) treats **both** 80 and 443 as standard.
- What this means for mini-chat:
  - Azure host on 443 with alias = host: accepted.
  - Hostname on a non-standard port (for example `localhost:8080`): passing alias = `localhost` is **rejected**. Pass `host:port` or omit the alias.
  - `127.0.0.1:<port>` (IP): an explicit alias is **required**, for example `mock-openai` or `127.0.0.1`.
- The proxy URI uses the alias OAGW stored. `Upstream.alias` from `create_upstream` is authoritative.

### 1.7 Sending a request (`proxy_request`)

Request parsing (`oagw/src/infra/proxy/service.rs:361-378`):

```rust
let (alias, path_suffix) = {
    let path = req.uri().path();
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    let (alias, raw_suffix) = match trimmed.find('/') {
        Some(pos) => (&trimmed[..pos], &trimmed[pos..]),
        None => (trimmed, ""),
    };
    (alias.to_string(), normalize_path(raw_suffix))
};
```

So the request URI is `"/{alias}{api_path}"` (for example `/my-res.openai.azure.com/openai/v1/responses?api-version=2025-03-01-preview`). It is a relative URI with no scheme and no host.

Pipeline order (`service.rs:524-878`):

1. `parse_proxy_request`.
2. **AuthZ** with the PROXY resource and action `invoke` (`service.rs:547-557`):

   ```rust
   self.policy_enforcer.access_scope_with(&ctx, &resources::PROXY, actions::INVOKE, None,
       &AccessRequest::new().require_constraints(false).context_tenant_id(ctx.subject_tenant_id())).await?;
   // resources::PROXY = ResourceType::from_static(PROXY_SCHEMA, &[pep_properties::OWNER_TENANT_ID])  (infra/proxy/mod.rs:30-36)
   ```

3. Resolve upstream and route across the tenant chain.
4. CORS check.
5. Query allowlist check.
6. Path suffix mode check.
7. Header passthrough and strip.
8. Auth plugin.
9. Guard plugins.
10. Header rules.
11. Transform plugins.
12. Endpoint selection.
13. **HTTP scheme check** (`service.rs:750-758`).
14. Rate limit.
15. Build the URL.
16. Bridge into Pingora.
17. Apply the response pipeline.

Minimal call **(proposed)**:

```rust
use oagw_sdk::{Body, ServiceGatewayClientV1};
let uri = format!("/{alias}{api_path}?api-version={ver}");
let req = http::Request::builder()
    .method(http::Method::POST)
    .uri(uri)
    .header(http::header::CONTENT_TYPE, "application/json")
    .header(http::header::ACCEPT, "text/event-stream")   // forwarded only if passthrough allows it
    .body(Body::from(serde_json::to_vec(&payload)?))       // Body::from(Vec<u8>|Bytes|String|&'static str)
    .map_err(...)?;
let resp: http::Response<Body> = gw.proxy_request(user_ctx.clone(), req).await?;
```

`Body` (`oagw-sdk/src/body.rs:27-108`):

```rust
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, BoxError>> + Send>>;
pub enum Body { Empty, Bytes(Bytes), Stream(BodyStream) }
impl Body {
    pub async fn into_bytes(self) -> Result<Bytes, BoxError>;   // buffers streams
    pub fn into_stream(self) -> BodyStream;
    pub fn try_into_bytes(self) -> Result<Bytes, Self>;
    pub fn try_into_stream(self) -> Result<BodyStream, Self>;
}
// From<()>, From<Bytes> (empty -> Empty), From<Vec<u8>>, From<String>, From<&'static str>
```

Use `Body::Stream` for streaming multipart uploads; it is handled by the chunked path at `service.rs:1031-1152`. The body size limit is `max_body_size_bytes`, default 100 MB, and exceeding it gives 413.

### 1.8 Consuming a streaming (SSE) response

OAGW always returns `Body::Stream` (`service.rs:1750-1757`). For `text/event-stream` responses it wraps the stream with the idle-timeout and shutdown lifecycle (`service.rs:243-253`).

SDK helper (`oagw-sdk/src/sse/stream.rs:41-152`, `sse/event.rs`):

```rust
pub struct ServerEvent { pub id: Option<String>, pub event: Option<String>, pub data: String, pub retry: Option<u64> }
impl ServerEvent { pub fn json<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error>; }

pub enum ServerEventsResponse<T: FromServerEvent = ServerEvent> {
    Events(ServerEventsStream<T>),         // content-type starts with text/event-stream
    Response(http::Response<Body>),        // anything else, returned intact (e.g. provider 4xx JSON)
}
impl ServerEventsStream { pub fn from_response<T: FromServerEvent>(resp: impl Into<http::Response<Body>>) -> ServerEventsResponse<T>; }
impl<T> ServerEventsStream<T> { pub fn status(&self) -> StatusCode; pub fn headers(&self) -> &HeaderMap; }
impl<T: FromServerEvent> Stream for ServerEventsStream<T> { type Item = Result<T, StreamingError>; }
// Typed: ServerEventsStream::from_response::<oagw_sdk::Json<MyEvent>>(resp)   (Json<T> derefs to T)
```

Consumer pattern **(proposed)**, based on `oagw-sdk/tests/usage.rs:336-378`:

```rust
use futures_util::StreamExt;
use oagw_sdk::api::ErrorSource;
use oagw_sdk::sse::{ServerEvent, ServerEventsResponse, ServerEventsStream};

let source = resp.extensions().get::<ErrorSource>().copied().unwrap_or(ErrorSource::Upstream);
let status = resp.status();
match ServerEventsStream::from_response::<ServerEvent>(resp) {
    ServerEventsResponse::Events(mut events) => {
        let mut saw_terminal = false;
        while let Some(ev) = events.next().await {
            let ev = ev?;                                     // StreamingError::{ServerEventsParse, Stream(..)}
            match ev.event.as_deref() {                       // Responses API sets `event:` lines
                Some("response.output_text.delta") => { /* relay ev.data immediately */ }
                Some("response.completed") | Some("response.failed") | Some("response.incomplete") => { saw_terminal = true; break; }
                _ => {}
            }
        }
        if !saw_terminal { /* idle timeout / disconnect: stream ended silently -> treat as provider error */ }
    }
    ServerEventsResponse::Response(r) => {
        // Non-SSE: provider error JSON (ErrorSource::Upstream) or OAGW problem+json (ErrorSource::Gateway)
        let retry_after = r.headers().get(http::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let body = r.into_body().into_bytes().await?;
        // map by (source, status): 429 -> rate_limited (honour Retry-After), 5xx -> provider_error, 504 gateway -> timeout ...
    }
}
```

Mid-stream behaviour:

- **Idle timeout**: `session_bridge::streaming_body_with_lifecycle` (`session_bridge.rs:504-549`) returns `None` when the deadline elapses or on gateway shutdown. The consumer sees a **clean end of stream, not an error**. The default is 300 s (`config.rs:43-47`).
- Upstream body read errors surface as `Err(StreamingError::Stream(..))` items.
- Dropping the `ServerEventsStream` (client disconnect) drops the duplex bridge, and Pingora closes the upstream connection.

### 1.9 How errors are represented

**A. `Err(CanonicalError)` from `proxy_request`.** These are raised before or while bridging. The mapping is `oagw/src/api/rest/error.rs:127-348`; the HTTP status comes from `CanonicalError::status_code()`.

| Condition | CanonicalError variant | HTTP | Retry-After (ctx.retry_after_seconds) |
|---|---|---|---|
| Alias / route not found | `NotFound` (resource_type `UPSTREAM_SCHEMA`/`ROUTE_SCHEMA`) | 404 | - |
| Upstream disabled | `ServiceUnavailable` | 503 | 30 |
| Request timeout waiting for upstream headers (`proxy_timeout_secs`) | `DeadlineExceeded` (`OagwProxyError::deadline_exceeded`) | **504** | - |
| Connect timeout / idle timeout | `DeadlineExceeded` | 504 | - |
| Bridge / downstream error | `ServiceUnavailable` + override 502 | 502 | 5 |
| Protocol error (H2) / stream aborted | `ServiceUnavailable` + override 502 | 502 | 5 |
| Link unavailable (no route / connect error) | `ServiceUnavailable` | 503 | 10 |
| Circuit breaker open | `ServiceUnavailable` | 503 | 30 |
| OAGW rate limit | `ResourceExhausted` (quota `RATE_LIMIT`) | 429 | in `violations[0].retry_after_seconds` |
| AuthZ deny on proxy | `PermissionDenied` reason `AUTHZ_DENIED` | 403 | - |
| AuthZ eval failure / compile failure | `Internal` (oagw maps `EnforcerError::EvaluationFailed/CompileFailed` → `DomainError::Internal`, `domain/error.rs:303-328`) | 500 | - |
| Auth plugin failure | `Unauthenticated` reason `AUTH_PLUGIN_*` | 401 | - |
| Secret not found at proxy time | `Internal` | 500 | - |
| HTTP upstream not allowed | `InvalidArgument` field `endpoint.scheme`, reason `HTTP_UPSTREAM_FORBIDDEN` | 400 | - |
| Query param not allowlisted | `InvalidArgument` reason `QUERY_NOT_ALLOWED` | 400 | - |
| Body too large | `OutOfRange` + override 413 | 413 | - |

Constants (`oagw/src/api/rest/error.rs:59-73`): `RETRY_AFTER_TRANSIENT_SECS = 5`, `RETRY_AFTER_LINK_SECS = 10`, `RETRY_AFTER_CIRCUIT_BREAKER_SECS = 30`, `RETRY_AFTER_ADMIN_SECS = 30`, `RETRY_AFTER_GUARD_SECS = 10`.

Typed projection (`oagw-sdk/src/error.rs:86-218, 231-316`):

```rust
let err: oagw_sdk::ServiceGatewayError = canonical.into();
match err {
    ServiceGatewayError::RateLimited { retry_after_secs } => ..,   // ResourceExhausted
    ServiceGatewayError::Timeout => ..,                            // DeadlineExceeded (504)
    ServiceGatewayError::Unavailable { retry_after_secs } => ..,   // ServiceUnavailable (502/503)
    ServiceGatewayError::AuthFailed { reason, detail } => ..,
    ServiceGatewayError::PermissionDenied { reason, detail } => ..,
    ServiceGatewayError::PayloadTooLarge { detail } => ..,
    ServiceGatewayError::Validation { field, reason, detail } => ..,
    ServiceGatewayError::NotFound { resource, name } => ..,
    ServiceGatewayError::AlreadyExists { resource, name } => ..,
    ServiceGatewayError::FailedPrecondition { precondition_type, subject, detail } => ..,
    ServiceGatewayError::Internal { detail } => ..,
    ServiceGatewayError::Other { canonical } => ..,
    _ => ..
}
```

**B. `Ok(Response)` with `ErrorSource::Gateway`.** These are Pingora-level failures after the bridge started. `PingoraProxy::fail_to_proxy` (`pingora_proxy.rs:777-880`) maps Pingora errors to `DomainError`, then to an RFC 9457 `Problem` written as the response:

- `ConnectTimedout`, `ReadTimedout` / `WriteTimedout` → 504 `deadline_exceeded`.
- `ConnectRefused`, `ConnectionClosed`, TLS errors → 502.
- `ConnectNoRoute` / `ConnectError` → 503 with `Retry-After: 10`.

The response carries `content-type: application/problem+json` and the header `x-oagw-error-source: gateway`, which `build_proxy_response` converts into `ErrorSource::Gateway` (`headers.rs:95-110`). The Problem JSON includes `context.retry_after_seconds` for `service_unavailable`.

Because the 30 s bridge `tokio::time::timeout` and the Pingora `read_timeout` (same value, `gear.rs:112-120`) race, a hanging provider can surface **either** as `Err(DeadlineExceeded)` **or** as `Ok(504, Gateway)`. Handle both.

**C. `Ok(Response)` with `ErrorSource::Upstream`.** The provider's own status passes through: 400/401/404/429/500, its JSON body, and its headers including `Retry-After`. Only hop-by-hop and internal headers are stripped (`headers.rs:113-116`). Test: `proxy_integration.rs:302-321` (upstream 500 → `Ok`, status 500, `ErrorSource::Upstream`). OAGW never retries (`cpt-cf-oagw-principle-no-retry`).

### 1.10 OAGW config (`oagw/src/config.rs:8-68`)

```yaml
oagw:
  config:
    proxy_timeout_secs: 30            # default 30; time to response headers; also pingora read timeout
    max_body_size_bytes: 104857600    # default 100MB
    allow_http_upstream: false        # default false; REQUIRED true for http:// mocks (rejected under fips)
    streaming_idle_timeout_secs: 300  # SSE idle; must be > 0
    websocket_idle_timeout_secs: 300
    token_cache_ttl_secs: 300
    protocol_cache_ttl_secs: 3600
    management_api_enabled: true      # REST CRUD only; SDK CRUD always available
    ssrf_policy:                      # domain/ssrf.rs:17-37
      enabled: true                   # false => all SSRF checks skipped (create-time + connect-time)
      extra_deny_cidrs: []
      allow_cidrs: []                 # alternative to disabling: allow_cidrs: ["127.0.0.1/32"]
      extra_deny_hostnames: []
      allow_hostnames: []
    metrics: { prefix: "" }
```

`#[serde(deny_unknown_fields)]` applies. The existing configs differ:

- `config/mini-chat.yaml:430-434` sets `proxy_timeout_secs: 60` and `ssrf_policy.enabled: false`, but **does not set `allow_http_upstream`**. That is fine for https Azure but not for http mocks.
- `config/e2e-local.yaml:536-541` sets `proxy_timeout_secs: 2`, `allow_http_upstream: true` and `ssrf_policy.enabled: false`. Note the **2 s** timeout: a mock that delays its response headers by more than 2 s gives a 504. Pingora's read timeout is also 2 s per read, so a gap of more than 2 s between SSE chunks can abort the stream.

### 1.11 Mock upstream on `127.0.0.1:<port>` (how existing tests do it)

There are **no** Python e2e suites in this tree: `testing/e2e/suites/` contains only `__init__.py`. `testing/e2e/helpers/mock_server.py` is a static-file `HTTPServer` bound to `0.0.0.0` with `/ping` and `/health` endpoints; it has no OpenAI mock. The established pattern lives in the Rust OAGW integration tests (`oagw/tests/proxy_integration.rs`):

```rust
// proxy_integration.rs:28-45 (REST json form) / 135-166 (SDK form)
Endpoint { scheme: Scheme::Http, host: "127.0.0.1".into(), port: h.mock_port() }
CreateUpstreamRequest::builder(Server{endpoints: vec![..]}, HTTP_PROTOCOL_ID)
    .alias("auth-hdr-test")         // explicit alias REQUIRED for IP endpoints
    .auth(oagw_sdk::AuthConfig {
        plugin_type: APIKEY_AUTH_PLUGIN_ID.into(),
        sharing: SharingMode::Private,
        config: Some([("header".into(),"authorization".into()),
                      ("prefix".into(),"Bearer ".into()),
                      ("secret_ref".into(),"cred://openai-key".into())].into_iter().collect()),
    }).build()
// request URI: format!("/auth-hdr-test{}", guard.path("/v1/chat/completions"))
```

`AppHarness` (`oagw/src/test_support/harness.rs`, feature `test-utils`) wires a real CP/DP with an in-process mock upstream (`MockUpstream`, axum on 127.0.0.1). It allows http (`domain/test_support.rs:651` `.with_allow_http_upstream(true)`). APIs:

- `AppHarness::builder().with_credentials(vec![("cred://openai-key".into(), "sk-test123".into())]).with_request_timeout(..).with_authz_client(..).build().await`
- `h.facade() -> &dyn ServiceGatewayClientV1`, `h.security_context()`, `h.mock_port()`
- `MockGuard::new()` gives a unique `/t-<uuid>` path prefix. Then `guard.mock("POST", "/v1/responses", MockResponse{status, headers, body})`, `guard.path(..)`, `guard.mock_sequence(..)`, `guard.mock_gated(..)` (never responds, for timeout tests), and `guard.recorded_requests().await`.
- `MockBody::{Json(Value), Text(String), Sse(Vec<String>), Channel{..}}`. `Sse` writes only `data: {chunk}\n\n` frames, with no `event:` line (`mock.rs:113-130`). To emulate the Responses API's `event: response.output_text.delta` lines, use `MockBody::Text(raw_sse)` with header `content-type: text/event-stream`.

To use this from mini-chat tests, add under `[dev-dependencies]`: `oagw = { workspace = true, features = ["test-utils"] }`.

For a stand-alone server (e2e) with a mock on `127.0.0.1:<port>`, the mini-chat provider entry needs:

- `host: "127.0.0.1"`, `port: <port>`, `use_http: true`
- an explicit `upstream_alias` (e.g. `"mock-openai"`; `"127.0.0.1"` is also a valid alias)

plus OAGW `allow_http_upstream: true` and `ssrf_policy.enabled: false` (or `allow_cidrs: ["127.0.0.1/32"]`). The secret must be readable: declare a global secret in `static-credstore-plugin.config.secrets`.

### 1.12 SecurityContext requirements (CRUD vs proxy)

| Operation | Context used by mini-chat | Requirements |
|---|---|---|
| `create_upstream` / `create_route` / `list_*` | S2S context from `exchange_client_credentials` | Non-nil tenant known to the tenant-resolver. No PDP call unless binding to an ancestor alias. The `secret_ref` must be readable by **this** context. The upstream gets `tenant_id = ctx.subject_tenant_id()`. |
| `proxy_request` | The **user's request context** (DESIGN: "`SecurityContext` (user_id, tenant_id) propagated through all in-process calls"). | PDP check on `gts.cf.core.oagw.proxy.v1~` / `invoke` with `context_tenant_id = subject_tenant_id`. The static-authz plugin denies a nil tenant. Alias resolution walks `[user_tenant, ancestors...]`, so the upstream's tenant must be in that chain, and if it is not the user's own tenant it must be shared (`sharing != Private`). The credstore lookup for the API key also uses this context. |

In the dev configs, the S2S default identity tenant is `DEFAULT_TENANT_ID = 00000000-df51-5b42-9538-d2b56b7ee953`. That equals the auth-disabled default context tenant (`api-gateway/src/gear.rs:467-480`) and the `e2e-root` tenant (`config/e2e-local.yaml:169-174`), so in single-tenant setups the user and S2S tenants coincide.

### 1.13 Alternative: provisioning upstreams via types-registry

OAGW's `post_init` materialises upstreams and routes registered as GTS instances in types-registry (`oagw/src/gear.rs:214-312`, `infra/type_provisioning.rs`). DESIGN chose SDK CRUD at mini-chat `start`, so this path is listed only for completeness.

---

## 2. AuthN resolver: S2S client credentials

### 2.1 SDK API (`gears/system/authn-resolver/authn-resolver-sdk`)

The crate is `cf-gears-authn-resolver-sdk`, lib `authn_resolver_sdk`. mini-chat's Cargo.toml already lists it by path.

```rust
// api.rs:29-73
#[async_trait]
pub trait AuthNResolverClient: Send + Sync {
    async fn authenticate(&self, bearer_token: &str) -> Result<AuthenticationResult, AuthNResolverError>;
    async fn exchange_client_credentials(&self, request: &ClientCredentialsRequest)
        -> Result<AuthenticationResult, AuthNResolverError>;
}
// models.rs:11-39
pub struct AuthenticationResult { pub security_context: SecurityContext }
pub struct ClientCredentialsRequest {
    pub client_id: String,
    pub client_secret: secrecy::SecretString,
    pub scopes: Vec<String>,          // passed to IdP; static plugin ignores
}
// error.rs:6-28
pub enum AuthNResolverError {
    Unauthorized(String), NoPluginAvailable, ServiceUnavailable(String),
    TokenAcquisitionFailed(String), Internal(String),
}
```

Usage **(proposed)**:

```rust
use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use secrecy::SecretString;

let authn = ctx.client_hub().get::<dyn AuthNResolverClient>()?;   // in init()
let s2s: SecurityContext = authn
    .exchange_client_credentials(&ClientCredentialsRequest {
        client_id: cfg.client_credentials.client_id.clone(),
        client_secret: SecretString::from(cfg.client_credentials.client_secret.clone()),
        scopes: vec![],
    })
    .await
    .map_err(|e| anyhow::anyhow!("S2S credential exchange failed: {e}"))?
    .security_context;
```

- The resolver is registered by the `authn-resolver` gear (`authn-resolver/src/gear.rs:26-75`, `hub.register::<dyn AuthNResolverClient>`). That gear is a `system` gear with `deps = [types_registry]`.
- Plugin discovery is lazy, on the first call (`authn-resolver/src/domain/service.rs:49-96`). A call made before the plugin registered its scoped client returns `PluginUnavailable`, which surfaces as `AuthNResolverError::ServiceUnavailable` / `NoPluginAvailable`. Calling it in `start` (after every `init`) is safe.

### 2.2 Static AuthN plugin `s2s_credentials`

`static-authn-plugin/src/config.rs:10-112`:

```rust
#[serde(default, deny_unknown_fields)]
pub struct StaticAuthNPluginConfig {
    pub vendor: String,            // default "constructorfabric"
    pub priority: i16,             // default 100
    pub mode: AuthNMode,           // accept_all (default) | static_tokens
    pub default_identity: IdentityConfig,
    pub tokens: Vec<TokenMapping>,
    pub s2s_credentials: Vec<S2sCredentialMapping>,
}
pub struct IdentityConfig {        // #[serde(default)]
    pub subject_id: Uuid,          // default DEFAULT_SUBJECT_ID  11111111-6a88-4768-9dfc-6bcd5187d9ed
    pub subject_tenant_id: Uuid,   // default DEFAULT_TENANT_ID   00000000-df51-5b42-9538-d2b56b7ee953
    pub token_scopes: Vec<String>, // default ["*"]
    pub subject_type: Option<String>,
}
pub struct S2sCredentialMapping {
    pub client_id: String,
    pub client_secret: SecretString,
    #[serde(default)] pub identity: IdentityConfig,
    #[serde(default)] pub bearer_token: Option<SecretString>,
}
```

The exchange is an exact match on `client_id` plus a secret compare (`domain/service.rs:126-135`). A miss returns `TokenAcquisitionFailed("invalid client credentials")` (`domain/client.rs:23-30`). The resulting context has no `subject_type` and no bearer token unless they are configured.

YAML already present (`config/mini-chat.yaml:91-98`, `config/e2e-local.yaml:349-351`):

```yaml
static-authn-plugin:
  config:
    vendor: "constructorfabric"
    priority: 100
    mode: accept_all
    s2s_credentials:
      - client_id: "mini-chat"
        client_secret: "mini-chat-dev-secret"
mini-chat:
  config:
    client_credentials:
      client_id: "mini-chat"
      client_secret: "mini-chat-dev-secret"
```

---

## 3. AuthZ / PEP

### 3.1 SDK surface (`gears/system/authz-resolver/authz-resolver-sdk`)

Re-exports (`lib.rs:55-71`): `AuthZResolverApi`, `PolicyEnforcer`, `AccessRequest`, `ResourceType`, `EnforcerError`, `IntoPropertyValue`, `EvaluationRequest/Response`, `Capability`, `TenantMode`, `BarrierMode`, `DenyReason`, `Constraint`, `Predicate`, ...

```rust
// api.rs:24-50  (#[toolkit::contract]; in-process impl registered in ClientHub)
pub trait AuthZResolverApi: Send + Sync {
    async fn evaluate(&self, ctx: PlatformSecurityContext, req: EvaluationRequest)
        -> Result<EvaluationResponse, CanonicalError>;   // deny = Ok(decision:false)
}

// pep/enforcer.rs:33-50
pub enum EnforcerError {
    Denied { deny_reason: Option<DenyReason> },          // PDP decision == false
    EvaluationFailed(#[from] CanonicalError),             // transport / PDP failure / 30s deadline
    CompileFailed(#[from] ConstraintCompileError),        // missing/unsupported constraints (fail-closed)
}
// models.rs:92-98
pub struct DenyReason { pub error_code: String, pub details: Option<String> }

// pep/enforcer.rs:176-214
impl ResourceType {
    pub const fn from_static(name: &'static str, supported_properties: &'static [&'static str]) -> Self;
    pub fn new(name: impl Into<Cow<'static, str>>, supported_properties: &'static [&'static str]) -> Self;
}

// pep/enforcer.rs:312-384, 517-610
impl PolicyEnforcer {
    pub fn new(authz: Arc<dyn AuthZResolverApi>) -> Self;     // eager
    pub fn from_hub(hub: Arc<ClientHub>) -> Self;             // lazy (OoP-friendly)
    pub fn with_capabilities(self, caps: Vec<Capability>) -> Self;
    pub fn with_deadline(self, d: Duration) -> Self;          // default DEFAULT_EVAL_DEADLINE = 30s
    pub async fn access_scope(&self, ctx: &SecurityContext, resource: &ResourceType, action: &str,
        resource_id: Option<Uuid>) -> Result<AccessScope, EnforcerError>;          // require_constraints = true
    pub async fn access_scope_with(&self, ctx: &SecurityContext, resource: &ResourceType, action: &str,
        resource_id: Option<Uuid>, request: &AccessRequest) -> Result<AccessScope, EnforcerError>;
}
// AccessRequest (enforcer.rs:80-165): new(), resource_property(k, v), resource_properties(map),
//   context_tenant_id(Uuid), tenant_mode(TenantMode), barrier_mode(BarrierMode),
//   tenant_status(Vec<String>), tenant_context(TenantContext), require_constraints(bool)
```

The evaluation request built by `build_request_with` (`enforcer.rs:411-503`) contains:

- `subject.id = ctx.subject_id()`
- `subject.properties["tenant_id"] = ctx.subject_tenant_id()`
- `subject_type`
- `resource{type: name, id, properties}`
- `context{tenant_context (only if set), token_scopes, require_constraints, capabilities, supported_properties, bearer_token}`

`pep_properties` (`libs/toolkit-security/src/access_scope.rs:94-103`): `OWNER_TENANT_ID = "owner_tenant_id"`, `RESOURCE_ID = "id"`, `OWNER_ID = "owner_id"`.

### 3.2 How constraints compile into `AccessScope`

`pep/compiler.rs:6-35` gives the matrix (assuming `decision = true`):

| require_constraints | constraints | Result |
|---|---|---|
| false | empty | `AccessScope::allow_all()` |
| false | present | compile |
| true | empty | `Err(ConstraintsRequiredButAbsent)` → `EnforcerError::CompileFailed` |
| true | present | compile |

- Each `Constraint` is an AND of predicates and becomes one `ScopeConstraint`. Multiple constraints are OR-ed.
- A predicate on a property **not** listed in `supported_properties` fails that constraint. If every constraint fails, the result is `AllConstraintsFailed`.

The scope is then applied with toolkit-db Secure ORM:

```rust
// entity (gears/credstore/credstore/src/infra/storage/entity/secrets.rs:12-14)
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "credstore_secrets")]
#[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
// mini-chat chats would use: #[secure(tenant_col = "tenant_id", resource_col = "id", owner_col = "user_id", no_type)]
//   tenant_col -> "owner_tenant_id", resource_col -> "id", owner_col -> "owner_id"  (toolkit-db-macros/src/lib.rs:62-73)

// query (gears/credstore/credstore/src/infra/storage/repo_impl/reads.rs:126-129, writes.rs:62-77)
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt, SecureDeleteExt, SecureInsertExt};
let rows = entity::Entity::find().secure().scope_with(&scope).filter(..).all(&conn).await.map_err(map_scope_err)?;
entity::Entity::update_many().col_expr(..).filter(..).secure().scope_with(&scope).exec(&conn).await?;
entity::Entity::insert(am).secure().scope_unchecked(&scope).map_err(..)?.exec(&conn).await?;  // INSERT
```

`ScopeError` variants: `Db`, `Invalid`, `TenantNotInScope` (maps to access denied), `Denied`, and others (`#[non_exhaustive]`). See the `map_scope_err` example at `credstore/src/infra/storage/repo_impl/helpers.rs:81-101`.

### 3.3 Mapping PDP deny vs PDP failure (credstore precedent)

`gears/credstore/credstore/src/domain/authz.rs:23-72`:

```rust
pub fn secret_type_resource(gts_id: &str) -> ResourceType {
    ResourceType::new(gts_id.to_owned(), &[pep_properties::OWNER_TENANT_ID])
}
pub mod actions { pub const READ: &str = "read"; pub const WRITE: &str = "write"; pub const DELETE: &str = "delete"; }

/// `Denied` / `CompileFailed` → `AccessDenied` (403); `EvaluationFailed` → `ServiceUnavailable` (503).
pub fn map_enforcer_err(err: EnforcerError) -> DomainError {
    match err {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => DomainError::AccessDenied { cause: Some(Box::new(err)) },
        EnforcerError::EvaluationFailed(source) => DomainError::ServiceUnavailable {
            detail: "authorization evaluation failed".to_owned(), retry_after: None,
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

OAGW maps differently: `Denied` becomes 403 with reason `deny_reason.error_code` or `AUTHZ_DENIED`, and `CompileFailed` / `EvaluationFailed` become 500 Internal (`oagw/src/domain/error.rs:303-328`). Mini-chat's DESIGN requires **403 `AUTHZ_DENIED`** for deny or compile failure and **503 with `Retry-After: 5`** for evaluation failure. Canonical builders for that **(proposed)**:

```rust
use toolkit_canonical_errors::{CanonicalError, resource_error};
#[resource_error(gts_id!("cf.core.mini_chat.chat.v1~"))]
pub struct ChatResourceError;

fn map_pep(e: authz_resolver_sdk::EnforcerError) -> CanonicalError {
    match e {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) =>
            ChatResourceError::permission_denied().with_reason("AUTHZ_DENIED").create(),        // 403
        EnforcerError::EvaluationFailed(cause) => {
            tracing::warn!(error = %cause, "PDP evaluation failed");
            CanonicalError::service_unavailable().with_retry_after_seconds(5).create()          // 503 + Retry-After: 5
        }
    }
}
```

`Problem::into_response` emits the `Retry-After` header from `service_unavailable` `retry_after_seconds` (`libs/toolkit-canonical-errors/src/problem.rs:734-746`). The builders used here are `CanonicalError::service_unavailable() -> ServiceUnavailableBuilder` (`builder.rs:655`), `.with_retry_after_seconds(u64)` (`builder.rs:779`), and `#[resource_error(...)]`-generated `::permission_denied()...with_reason(..).create()` (as used in oagw `error.rs:290-301`). Also note that the PEP's own 30 s deadline elapsing yields `EvaluationFailed(service_unavailable)` (`enforcer.rs:576-593`).

### 3.4 Full example of a handler / service PEP flow **(proposed)**

Built from the APIs above:

```rust
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer, ResourceType, AccessRequest, EnforcerError};
use toolkit_security::{SecurityContext, AccessScope, pep_properties};
use toolkit_gts::gts_id;

pub const CHAT: ResourceType = ResourceType::from_static(
    gts_id!("cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~"),   // DESIGN.md §authz
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID, pep_properties::RESOURCE_ID],
);
pub const MODEL: ResourceType = ResourceType::from_static(
    gts_id!("cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~"), &[]);

// init():
let authz = ctx.client_hub().get::<dyn AuthZResolverApi>()?;
let enforcer = PolicyEnforcer::new(authz);          // or PolicyEnforcer::from_hub(ctx.client_hub())

// service:
pub async fn get_chat(&self, ctx: &SecurityContext, id: Uuid) -> Result<Chat, CanonicalError> {
    let scope: AccessScope = self.enforcer
        .access_scope(ctx, &CHAT, "read", Some(id))
        .await
        .map_err(map_pep)?;
    // static-authz yields In(owner_tenant_id, [ctx tenant]) ONLY — enforce owner-only explicitly:
    let row = chats::Entity::find_by_id(id).secure().scope_with(&scope)
        .filter(chats::Column::UserId.eq(ctx.subject_id()))
        .one(&conn).await.map_err(map_scope_err)?;
    row.ok_or_else(|| ChatResourceError::not_found("chat not found").with_resource(id.to_string()).create())
}

pub async fn create_chat(&self, ctx: &SecurityContext, ..) -> Result<Chat, CanonicalError> {
    let scope = self.enforcer.access_scope_with(ctx, &CHAT, "create", None,
        &AccessRequest::new()
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .resource_property(pep_properties::OWNER_ID, ctx.subject_id()))
        .await.map_err(map_pep)?;
    // insert with .secure().scope_unchecked(&scope) or scope_with_model(..)
}

pub async fn list_models(&self, ctx: &SecurityContext) -> Result<..> {
    // permission-only: no table, no constraints
    self.enforcer.access_scope_with(ctx, &MODEL, "list", None,
        &AccessRequest::new().require_constraints(false)).await.map_err(map_pep)?;   // allow_all() on static PDP
    ...
}
```

### 3.5 What the static-authz plugin does

`static-authz-plugin/src/domain/service.rs:49-172`:

- Tenant = `context.tenant_context.root_id`, otherwise `subject.properties["tenant_id"]`. No tenant or the **nil UUID** → `decision: false` (Denied).
- `resource.properties` containing `group_ids` / `ancestor_group_ids` → deny.
- Otherwise `decision: true`, with:
  - `In(owner_tenant_id, [tid])` **only if** `owner_tenant_id` is in `supported_properties`;
  - plus `InTenantSubtree(owner_tenant_id|id, tid)` constraints if the PEP advertises `Capability::TenantHierarchy`.
- It never constrains `owner_id`, never checks actions or resource types, and ignores `token_scopes`.
- `supported_properties = []` with `require_constraints = true` gives `CompileFailed` (`ConstraintsRequiredButAbsent`), which should map to 403. Use `require_constraints(false)` for permission-only resources.

Config: `static-authz-plugin: config: { vendor: "constructorfabric", priority: 100 }` (`static-authz-plugin/src/config.rs`).

### 3.6 Test doubles

Implement `AuthZResolverApi` directly; there is an allow-all example at `oagw/src/infra/proxy/service.rs:1890-1909`:

```rust
struct AllowAllAuthZ;
#[async_trait]
impl AuthZResolverApi for AllowAllAuthZ {
    async fn evaluate(&self, _ctx: PlatformSecurityContext, _r: EvaluationRequest) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse { decision: true, context: EvaluationResponseContext { constraints: Vec::new(), deny_reason: None } })
    }
}
let enforcer = PolicyEnforcer::new(Arc::new(AllowAllAuthZ));
```

For deny tests, return `decision: false`. For the 503 path, return `Err(CanonicalError::service_unavailable().create())`. OAGW's `test_support` also exports `CapturingAuthZResolverClient` and `DenyingAuthZResolverClient`.

---

## 4. Types-registry plugin discovery (plugin SPIs)

### 4.1 Building blocks

**Base type** (`libs/toolkit-gts/src/plugin.rs:29-107`, re-exported as `toolkit::gts::PluginV1`):

```rust
#[gts_type_schema(dir_path = "schemas", type_id = gts_id!("cf.toolkit.plugins.plugin.v1~"),
    description = "Base toolkit plugin schema", properties = "id,vendor,priority,properties", base = true)]
pub struct PluginV1<P: gts::GtsSchema> {
    pub id: GtsInstanceId,
    pub vendor: String,
    pub priority: i16,          // lower = higher priority
    pub properties: P,
}
impl<P: gts::GtsSchema + gts::GtsSerialize + Default> PluginV1<P> {
    pub fn build_registration(instance_segment: &str, vendor: impl Into<String>, priority: i16)
        -> serde_json::Result<(GtsInstanceId, serde_json::Value)>;
    // id = GtsInstanceId::new(P::TYPE_ID, instance_segment)
}
```

**Selection** (`libs/toolkit/src/plugins/mod.rs`):

```rust
pub struct GtsPluginSelector { .. }                // single-flight cache of the chosen instance id
impl GtsPluginSelector {
    pub fn new() -> Self;
    pub async fn get_or_init<F, Fut, E>(&self, resolve: F) -> Result<Arc<str>, E>
        where F: FnOnce() -> Fut, Fut: Future<Output = Result<String, E>>;
    pub async fn reset(&self) -> bool;             // drop cached id (e.g. client missing in hub)
}
pub enum ChoosePluginError { InvalidPluginInstance { gts_id, reason }, PluginNotFound { type_id, vendor } }
pub fn choose_plugin_instance<'a, P>(vendor: &str,
    instances: impl IntoIterator<Item = (&'a str, &'a serde_json::Value)>) -> Result<String, ChoosePluginError>
where P: for<'de> gts::GtsDeserialize<'de> + gts::GtsSchema;
// filters content.vendor == vendor, picks LOWEST priority; errors if content.id != gts_id
```

**Scoped clients** (`libs/toolkit/src/client_hub.rs`): `ClientScope::gts_id(&str)`, `hub.register_scoped::<dyn T>(scope, Arc<T>)`, `hub.try_get_scoped::<dyn T>(&scope) -> Option<Arc<T>>`, `hub.get_scoped(..) -> Result<..>`.

**Types-registry client** (`types-registry-sdk`): `hub.get::<dyn TypesRegistryClient>()`, `registry.list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*"))) -> Result<Vec<GtsInstance>, CanonicalError>` (`GtsInstance { id: GtsInstanceId, object: Value, .. }`), `registry.register(Vec<Value>) -> Result<Vec<RegisterResult>, CanonicalError>`, `RegisterResult::ensure_all_ok(&results)?`. For tests, `types_registry_sdk::testing::MockTypesRegistryClient` is behind feature `test-util`.

**Schema registration is automatic.** Each `#[toolkit_gts::gts_type_schema(..)]` submits an `InventoryTypeSchema` through `inventory`, and types-registry loads all of them at boot (`libs/toolkit-gts/src/lib.rs:7-19,63-155`). The authn-resolver gear doc says: "The `AuthNResolverPluginSpecV1` schema itself reaches `types-registry` automatically via the `toolkit-gts` link-time inventory — no per-init registration is needed" (`authn-resolver/src/gear.rs:22-25`).

### 4.2 Reference implementation (authn-resolver)

Spec (`authn-resolver-sdk/src/gts.rs:41-49`):

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

Main-gear selection (`authn-resolver/src/domain/service.rs:49-96`):

```rust
async fn get_plugin(&self) -> Result<Arc<dyn AuthNResolverPluginClient>, DomainError> {
    let instance_id = self.selector.get_or_init(|| self.resolve_plugin()).await?;
    let scope = ClientScope::gts_id(instance_id.as_ref());
    if let Some(client) = self.hub.try_get_scoped::<dyn AuthNResolverPluginClient>(&scope) {
        Ok(client)
    } else {
        Err(DomainError::PluginUnavailable { gts_id: instance_id.to_string(), reason: "client not registered yet".into() })
    }
}
async fn resolve_plugin(&self) -> Result<String, DomainError> {
    let registry = self.hub.get::<dyn TypesRegistryClient>()
        .map_err(|e| DomainError::TypesRegistryUnavailable(e.to_string()))?;
    let plugin_type_id = AuthNResolverPluginSpecV1::gts_type_id().clone();
    let instances = registry
        .list_instances(InstanceQuery::new().with_pattern(format!("{plugin_type_id}*")))
        .await?;
    let gts_id = choose_plugin_instance::<AuthNResolverPluginSpecV1>(
        &self.vendor,
        instances.iter().map(|e| (e.id.as_ref(), &e.object)),
    )?;
    Ok(gts_id)
}
```

The credstore variant (`gears/credstore/credstore/src/infra/plugin_select.rs:19-103`) maps `ChoosePluginError::PluginNotFound` to 503 and `InvalidPluginInstance` to 500.

Plugin gear (`static-authn-plugin/src/gear.rs:25-91`):

```rust
#[toolkit::gear(name = "static-authn-plugin", deps = [types_registry])]
pub struct StaticAuthNPlugin { service: OnceLock<Arc<Service>> }

#[async_trait]
impl Gear for StaticAuthNPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticAuthNPluginConfig = ctx.config_or_default()?;
        let (instance_id, instance_json) =
            PluginV1::<AuthNResolverPluginSpecV1>::build_registration(
                "cf.builtin.static_authn_resolver.plugin.v1", cfg.vendor.clone(), cfg.priority)?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;
        let service = Arc::new(Service::from_config(&cfg));
        let api: Arc<dyn AuthNResolverPluginClient> = service;
        ctx.client_hub().register_scoped::<dyn AuthNResolverPluginClient>(ClientScope::gts_id(&instance_id), api);
        Ok(())
    }
}
```

### 4.3 GTS id naming conventions

- Plugin spec type: `gts.cf.toolkit.plugins.plugin.v1~<vendor>.<package>.<gear>.plugin.v1~`. Examples:
  - `gts.cf.toolkit.plugins.plugin.v1~cf.core.authn_resolver.plugin.v1~`
  - `...~cf.core.credstore.plugin.v1~` (`credstore-sdk/src/gts.rs:30-38`)
  - see also `docs/TOOLKIT_PLUGINS.md:104-125`.
- Plugin instance: `<spec type id><vendor>.<package>.<name>.v1`, e.g. `...~cf.core.authn_resolver.plugin.v1~cf.builtin.static_authn_resolver.plugin.v1`.
- Resource types for AuthZ / errors: `gts.cf.core.<gear>.<resource>.v1~`, optionally chained (`gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~`).
- Permissions as well-known instances: `gts.cf.toolkit.authz.permission.v1~<vendor>.<package>.<namespace>.<perm>.v1`, declared with `toolkit_gts::gts_instance! { AuthzPermissionV1 { id: gts_id!("..."), resource_type: ..., action: ..., display_name: ... } }` (`libs/toolkit-gts/src/permission.rs`, `libs/toolkit-gts-macros/src/lib.rs:491-523`). **No gear in this tree currently uses `gts_instance!`**; it is optional for mini-chat.
- `gts_id!("suffix")` prepends `gts.` at compile time. Use it everywhere instead of hard-coding the prefix.

### 4.4 Minimal code for the mini-chat SPIs **(proposed)**

**Crate location.** The workspace has **no `mini-chat-sdk` member** (`Cargo.toml:60-102` lists only `gears/mini-chat/mini-chat`). `apps/cf-gears-example-server/src/registered_gears.rs:31-38` expects `mini_chat::infra::plugins::static_audit` and `mini_chat::infra::plugins::static_model_policy` to be modules inside the `mini_chat` lib. The current stub `lib.rs` already declares them as empty modules. There are two options:

- (a) Put the SPI traits and specs in `mini_chat` itself, e.g. `mini_chat::plugin_api` / `mini_chat::gts`.
- (b) Add a new crate `gears/mini-chat/mini-chat-sdk` (package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`) to workspace `members` and `[workspace.dependencies]`.

Either way, `#[gts_type_schema]` expands to `schemars::JsonSchema` and `serde` paths (`gts-macros-0.12.1/src/lib.rs:489-499`). The crate holding the specs therefore needs `schemars`, `serde`, `serde_json`, `gts`, `toolkit-gts` and `toolkit` dependencies. `mini-chat/Cargo.toml` currently has **no `schemars`**, so add `schemars = { workspace = true }`.

```rust
// gts.rs
use toolkit::gts::PluginV1;
use toolkit_gts::gts_type_schema;

#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy.plugin.v1~"),
    description = "Mini-Chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit.plugin.v1~"),
    description = "Mini-Chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;

// plugin_api.rs
#[async_trait::async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    async fn get_current_policy_version(&self, tenant_id: Uuid) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError>;
    async fn get_policy_snapshot(&self, tenant_id: Uuid, version: u64) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError>;
    async fn get_user_limits(&self, tenant_id: Uuid, user_id: Uuid) -> Result<UserLimits, MiniChatModelPolicyPluginError>;
    async fn publish_usage(&self, event: UsageEvent) -> Result<(), MiniChatModelPolicyPluginError>;
    async fn check_user_license(&self, _tenant_id: Uuid, _user_id: Uuid) -> Result<UserLicenseStatus, MiniChatModelPolicyPluginError> {
        Ok(UserLicenseStatus { active: false })      // DESIGN §4332: default body, never called
    }
}
#[async_trait::async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    async fn emit(&self, event: AuditEvent) -> Result<(), MiniChatAuditPluginError>;  // Transient | PluginTimeout | Permanent
}
```

The method set above is illustrative; take the exact methods from DESIGN.md §"Model Policy Plugin" and §5512.

Static plugin as a second gear inside the same crate (`mini_chat::infra::plugins::static_audit`):

```rust
#[toolkit::gear(name = "static-mini-chat-audit-plugin", deps = [types_registry])]
pub struct StaticMiniChatAuditPlugin { svc: OnceLock<Arc<Service>> }
impl Default for StaticMiniChatAuditPlugin { fn default() -> Self { Self { svc: OnceLock::new() } } }

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticAuditConfig { pub vendor: String, pub priority: i16, pub enabled: bool }   // yaml: config/mini-chat.yaml:231-235
impl Default for StaticAuditConfig { fn default() -> Self { Self { vendor: "constructorfabric".into(), priority: 100, enabled: true } } }

#[async_trait]
impl Gear for StaticMiniChatAuditPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticAuditConfig = ctx.config_or_default()?;
        if !cfg.enabled { return Ok(()); }   // no instance => main gear sees "no plugin" => drop events
        let (instance_id, payload) = PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
            "cf.builtin.static_mini_chat_audit.plugin.v1", cfg.vendor.clone(), cfg.priority)?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        RegisterResult::ensure_all_ok(&registry.register(vec![payload]).await?)?;
        let svc: Arc<dyn MiniChatAuditPluginClientV1> = Arc::new(Service::default());
        ctx.client_hub().register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(&instance_id), svc);
        Ok(())
    }
}
```

The model-policy plugin gear `static-mini-chat-model-policy-plugin` follows the same pattern. Its config is `vendor`, `priority` and `model_catalog: Vec<ModelCatalogEntry>` (`config/mini-chat.yaml:237-428`).

Main gear (`mini-chat`): `#[toolkit::gear(name = "mini-chat", deps = [types_registry, authn_resolver, authz_resolver, oagw], capabilities = [db, rest, stateful])]`. Read `vendor` from config, then build gateways holding `hub`, `vendor` and a `GtsPluginSelector` per SPI, exactly like §4.2. For the audit gateway's "not cached when absent" rule (DESIGN line 415), do **not** cache `PluginNotFound`. `GtsPluginSelector` caches only `Ok` values, so an `Err(PluginNotFound)` is retried on the next call. Call `selector.reset().await` when the instance resolves but `try_get_scoped` returns `None`.

---

## 5. SecurityContext (`libs/toolkit-security/src/context.rs`)

```rust
// context.rs:22-47
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SecurityContext {
    subject_id: Uuid,
    subject_type: Option<String>,
    subject_tenant_id: Uuid,
    #[serde(default)] token_scopes: Vec<String>,   // ["*"] = unrestricted; EMPTY = no capability
    #[serde(skip)] bearer_token: Option<SecretString>,
}
impl SecurityContext {
    pub fn builder() -> SecurityContextBuilder;
    pub fn anonymous() -> Self;                 // nil ids
    pub fn subject_id(&self) -> Uuid;
    pub fn subject_type(&self) -> Option<&str>;
    pub fn subject_tenant_id(&self) -> Uuid;
    pub fn token_scopes(&self) -> &[String];
    pub fn is_anonymous(&self) -> bool;         // nil subject or nil tenant
    pub fn has_scope(&self, scope: &str) -> bool;
    pub fn bearer_token(&self) -> Option<&SecretString>;
}
// context.rs:140-214 builder: subject_id(Uuid) [required], subject_type(&str), subject_tenant_id(Uuid) [required],
//   token_scopes(Vec<String>), bearer_token(impl Into<SecretString>), build() -> Result<SecurityContext, SecurityContextBuildError>
```

Constants (`constants.rs`): `DEFAULT_TENANT_ID = 00000000-df51-5b42-9538-d2b56b7ee953`, `DEFAULT_SUBJECT_ID = 11111111-6a88-4768-9dfc-6bcd5187d9ed`.

In tests (pattern from `oagw/src/infra/plugin/apikey_auth.rs:88-94`):

```rust
let ctx = SecurityContext::builder()
    .subject_id(Uuid::new_v4())
    .subject_tenant_id(Uuid::new_v4())
    .subject_type("gts.cf.core.security.subject_user.v1~")   // optional
    .token_scopes(vec!["*".to_owned()])
    .build()
    .expect("test security context");
```

In handlers the api-gateway inserts `SecurityContext` into request extensions; extract it with `axum::Extension<SecurityContext>`. With `auth_disabled: true` (as in `config/mini-chat.yaml:54`), every request gets `{DEFAULT_SUBJECT_ID, DEFAULT_TENANT_ID}` with **empty** scopes (`api-gateway/src/gear.rs:465-482`). The static PDP ignores scopes, so this works in dev.

---

## 6. License gating

Declaring a feature on an operation (`libs/toolkit/src/api/operation_builder.rs:65-67, 252-254, 1133-1193`):

```rust
pub const CORE_GLOBAL_BASE_LICENSE_FEATURE: &str = gts_id!("cf.core.lic.feat.v1~cf.core.global.base.v1");
pub trait LicenseFeature: AsRef<str> {}
// after .authenticated():
pub fn require_license_features<F: LicenseFeature>(self, licenses: impl IntoIterator<Item = F>) -> OperationBuilder<.., AuthSet, LicenseSet>;
pub fn no_license_required(self) -> OperationBuilder<.., AuthSet, LicenseSet>;
// .register() only compiles once LicenseSet; .public() routes skip licensing.
```

Pattern used by OAGW (`oagw/src/api/rest/routes/mod.rs:12-22`, `routes/upstream.rs:18-33`):

```rust
use toolkit::api::operation_builder::{LicenseFeature, OperationBuilder};
pub(super) struct License;
impl AsRef<str> for License { fn as_ref(&self) -> &'static str { toolkit::api::operation_builder::CORE_GLOBAL_BASE_LICENSE_FEATURE } }
impl LicenseFeature for License {}

router = OperationBuilder::post("/mini-chat/v1/chats")
    .operation_id("mini_chat.create_chat")
    .authenticated()
    .require_license_features([&License])          // non-empty => enforced
    .json_request::<CreateChatReq>(openapi, "..")
    .handler(handlers::create_chat)
    .json_response_with_schema::<ChatDto>(openapi, http::StatusCode::CREATED, "Created")
    .standard_errors(openapi)
    .register(router, openapi);
```

Caveat: OAGW passes `[]` here (`require_license_features::<License>([])`), which means **no** requirement is recorded. Mini-chat must pass `[&License]`.

Enforcement in api-gateway (`gears/system/api-gateway/src/middleware/license_validation.rs:13-79`):

```rust
const BASE_FEATURE: &str = gts_id!("cf.core.lic.feat.v1~cf.core.global.base.v1");
// keyed by (Method, matched path) from OperationSpec.license_requirement
// TODO: this is a stub implementation
if required.iter().any(|r| r != BASE_FEATURE) {
    return ApiGatewayRouteError::permission_denied().with_reason("LICENSE_FEATURE_REQUIRED").create().into_response();  // 403
}
next.run(req).await
```

What this means in practice:

- The base feature always passes, in every setup (single-tenant, static, auth disabled). The license-resolver gear and plugin are **not** consulted.
- Any other feature (for example a future `...~cf.core.ai_chat...`) gets 403 `LICENSE_FEATURE_REQUIRED` on every request.
- The middleware sits innermost, after auth and throttling (`api-gateway/src/gear.rs:384-424`).
- This matches DESIGN's "interim: require the base feature" (`gears/mini-chat/docs/DESIGN.md:255-261`).

---

## 7. Metrics (OpenTelemetry)

Workspace versions: `opentelemetry = "0.32"` with features `["trace","metrics"]`; `opentelemetry_sdk = "0.32"` (`Cargo.toml:392-393`). mini-chat already depends on `opentelemetry`, and on `opentelemetry_sdk` (feature `testing`) as a dev-dependency.

Pattern (`oagw/src/gear.rs:99-105`, `oagw/src/infra/metrics.rs:62-106`):

```rust
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter, UpDownCounter};

let scope = opentelemetry::InstrumentationScope::builder("mini-chat").build();
let meter: Meter = opentelemetry::global::meter_with_scope(scope);
// (simpler alternative used by credstore: opentelemetry::global::meter("credstore"))

pub struct MiniChatMetrics { audit_emit: Counter<u64>, ttft_ms: Histogram<f64>, active_streams: UpDownCounter<i64> }
impl MiniChatMetrics {
    pub fn new(meter: &Meter) -> Self {
        Self {
            audit_emit: meter.u64_counter("mini_chat_audit_emit").with_description("Audit delivery outcomes").build(),
            ttft_ms: meter.f64_histogram("mini_chat_ttft_ms").with_unit("ms")
                .with_boundaries(vec![50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0]).build(),
            active_streams: meter.i64_up_down_counter("mini_chat_active_streams").build(),
        }
    }
}
metrics.audit_emit.add(1, &[KeyValue::new("result", "ok")]);
```

- The Prometheus exporter appends `_total` to counters, so `mini_chat_audit_emit` is exported as `mini_chat_audit_emit_total`.
- If `opentelemetry.metrics.enabled` is false (it is false in `config/mini-chat.yaml:440-441`), the global provider is a no-op (`libs/toolkit/src/telemetry/init.rs:349-356`). Instruments still build safely.

Test harness (`oagw/src/infra/metrics_tests.rs:12-24`):

```rust
use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
let exporter = InMemoryMetricExporter::default();
let provider = SdkMeterProvider::builder().with_reader(PeriodicReader::builder(exporter.clone()).build()).build();
let m = MiniChatMetrics::new(&provider.meter("mini-chat"));
// ... provider.force_flush(); exporter.get_finished_metrics()
```

---

## 8. Checklist / gotchas for the implementer

1. **Link OAGW.** Make sure the `oagw` gear is linked when the server is built with only `--features mini-chat` (§1.1).
2. **Config for http mocks.** `oagw.allow_http_upstream: true` and `ssrf_policy.enabled: false` are both required (§1.10).
3. **Alias.**
   - IP endpoint: explicit alias required.
   - Hostname on a non-standard port: the alias **must** equal `host:port` or be omitted.
   - Hostname on the standard port: alias = host is accepted.
   - Always proxy to `/{upstream.alias}{api_path}` (§1.6).
4. **Recreate upstreams/routes on every start** (in-memory repo). Treat `AlreadyExists` as reuse, and `FailedPrecondition{subject:"auth.config.secret_ref"}` as defer-and-retry (§1.5).
5. **Upstream visibility.** Set `AuthConfig.sharing = Inherit` so tenants below the S2S tenant can resolve the upstream (§1.12).
6. **Query and headers.**
   - Put `api-version` in the route `query_allowlist` (§1.5).
   - Put headers other than `Content-Type` that the provider needs in `passthrough_allowlist` or `set` (§1.5, §0 #8).
7. **Proxy outcomes.** Handle all three (§1.9):
   - (a) `Err(CanonicalError)`;
   - (b) `Ok` + `ErrorSource::Gateway` with a problem+json body;
   - (c) `Ok` + `ErrorSource::Upstream` with the provider status and `Retry-After`.
   The response body is always a stream: call `into_bytes()` for non-SSE responses.
8. **SSE ending.** The stream can end without an error on idle timeout. Detect a missing terminal event (§1.8).
9. **PEP mapping.** `Denied | CompileFailed` → 403 `AUTHZ_DENIED`; `EvaluationFailed` → 503 + `Retry-After: 5` (§3.3).
10. **Owner-only.** The static PDP never emits `owner_id` constraints, so also filter `user_id = ctx.subject_id()` (§3.5).
11. **Spec crate deps.** Plugin spec structs need `#[derive(Default)]`, and the crate needs a `schemars` dependency (§4.4).
12. **Licence.** Use `.require_license_features([&License])` with the base feature. Any other feature is denied by the stub (§6).
