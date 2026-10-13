# 03 — OAGW, AuthN (S2S), AuthZ (PEP), license gate, credstore: code-level reference for `mini-chat`

Scope: everything a `mini-chat` gear needs to (a) get an S2S `SecurityContext`, (b) provision OAGW
upstreams/routes, (c) proxy HTTP/SSE/multipart through OAGW in-process, (d) act as an AuthZ PEP,
(e) read the request `SecurityContext` and declare the license gate, (f) make `secret_ref` / credstore work.

All paths are relative to `/app`. "VERBATIM" = copied from source; "SKETCH" = suggested code
built only from the verified APIs below.

---

## 0. Gotchas (read first)

1. **Hostname upstreams get their alias derived automatically.** If you pass `.alias(x)` and `x` differs from
   the derived alias (`host`, or `host:port` for a non-standard port), `create_upstream` returns a
   **validation error** (`"alias is auto-derived for hostname-based endpoints; remove the 'alias' field (derived: '…')"`).
   A custom `upstream_alias` only works for IP endpoints. Passing `alias == host` is fine for port 443/80.
   (`gears/system/oagw/oagw/src/domain/services/management/alias.rs`, `enforce_alias_create_with`.)
2. **Upstream alias conflict → `CanonicalError::AlreadyExists`** (`resource_type = UPSTREAM_SCHEMA`,
   `resource_name = alias`). The SDK has **no get-by-alias**. To reuse, page `list_upstreams` and match on `alias`.
   Route conflict (same upstream, same path, same priority, overlapping method) → `AlreadyExists` with
   `resource_type = ROUTE_SCHEMA`, name `"{upstream_id}:{path}:{Method:?}"`.
3. **OAGW storage is in-memory** (`InMemoryUpstreamRepo`/`InMemoryRouteRepo`). Everything is lost on restart,
   so provisioning must run on every start.
4. **Request headers are NOT forwarded by default.** Default `PassthroughMode::None`: only `Content-Type` is
   forwarded. `Authorization`, `Cookie`, `Proxy-Authorization`, `Set-Cookie` are always stripped, as are
   `x-oagw-*` and hop-by-hop headers. Provider headers like `anthropic-version`, `OpenAI-Beta` or `Accept` need
   `HeadersConfig.request.{passthrough=Allowlist, passthrough_allowlist}` or `set` rules on the upstream.
5. **Query params must be in the route's `query_allowlist`**, otherwise the request is rejected
   (`field::QUERY_NOT_ALLOWED`). For example, Azure `api-version` must be allow-listed.
6. **Visibility across tenants:** an upstream owned by tenant T is visible to descendant tenants only if
   one of `auth/rate_limit/plugins/cors` has `sharing != Private` (`is_visible_to_descendant`). Use
   `SharingMode::Inherit` (or `Enforce`) on `AuthConfig` if user tenants differ from the S2S tenant.
7. **Credstore gateway reads metadata from its own DB.** Secrets that exist only in `static-credstore-plugin`
   config are **not readable** through `CredStoreClientV1::get` (issue #5094, see `config/quickstart.yaml`
   ~L446). `create_upstream` with `secret_ref` then fails with `FailedPrecondition` (`subject =
   "auth.config.secret_ref"`, `type = "STATE"`). That is the *retryable / deferred* case. Fix it by POSTing
   or PUTting the secret through the credstore REST API.
8. **Static AuthZ returns only `In(owner_tenant_id, [tenant])`.** It returns no `owner_id` predicate. Add
   the owner filter yourself with `AccessScope::ensure_owner(ctx.subject_id())`.
9. **Two failure channels on `proxy_request`:** `Err(CanonicalError)` (pre-flight, auth, rate limit, timeout
   while waiting for response headers) **and** `Ok(Response)` with `ErrorSource::Gateway` + status 502/503/504 +
   `application/problem+json` body (Pingora connect/read/TLS failures). Upstream statuses come back as
   `Ok(Response)` with `ErrorSource::Upstream`. OAGW never retries (including on 429).
10. **SSE idle timeout ends the body stream silently** (`None`, not `Err`) after
    `streaming_idle_timeout_secs` (default 300 s) with no upstream bytes. Treat "no terminal event" as a failure.
11. **`proxy_timeout_secs` (default 30, `config/mini-chat.yaml` sets 60) also covers streaming uploads.** The
    timer runs from the start of the body upload until response headers arrive.
12. `require_license_features::<L>([])` (empty) means **no license required**. To gate on the base feature,
    pass `[BaseLicense]`.
13. `SecurityContext` has **no `tenant_id()`**. Use `subject_tenant_id()`.

---

## 1. Crates and import paths

`gears/mini-chat/mini-chat/Cargo.toml` already declares:

| Cargo dep key | package | `use` name |
|---|---|---|
| `authn-resolver-sdk` | `cf-gears-authn-resolver-sdk` (path dep) | `authn_resolver_sdk` |
| `authz-resolver-sdk` | `cf-gears-authz-resolver-sdk` | `authz_resolver_sdk` |
| `oagw-sdk` | `cf-gears-oagw-sdk` (feature `axum` is OFF in mini-chat) | `oagw_sdk` |
| `toolkit-security` | `cf-gears-toolkit-security` | `toolkit_security` |
| `toolkit-canonical-errors` (feature `axum`) | `cf-gears-toolkit-canonical-errors` | `toolkit_canonical_errors` |
| `toolkit`, `toolkit-gts`, `secrecy`, `http`, `bytes`, `futures`, `axum`, `multer` | | |

Notes:
- `oagw_sdk::ServerEventsStream::into_response()` (re-emits SSE as an axum response) exists only with
  `oagw-sdk/axum`. It is not needed when you emit your own SSE.
- Get clients from the ClientHub: `ctx.client_hub().get::<dyn Trait>()?` returns `Arc<dyn Trait>`, and
  `ctx.client_hub()` returns `Arc<ClientHub>`.

```rust
// SKETCH — imports used below
use authn_resolver_sdk::{AuthNResolverClient, AuthNResolverError, AuthenticationResult, ClientCredentialsRequest};
use authz_resolver_sdk::{AuthZResolverApi, EnforcerError, PolicyEnforcer, TenantMode, DenyReason};
use authz_resolver_sdk::pep::{AccessRequest, ResourceType};
use oagw_sdk::api::{ErrorSource, ServiceGatewayClientV1};
use oagw_sdk::body::{Body, BodyStream, BoxError};
use oagw_sdk::sse::{ServerEvent, ServerEventsResponse, ServerEventsStream};
use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HeadersConfig, HttpMatch, HttpMethod,
    ListQuery, MatchRules, MultipartBody, Part, PassthroughMode, PathSuffixMode, RequestHeaderRules, Route,
    Scheme, Server, ServiceGatewayError, SharingMode, StreamingError, Upstream,
    APIKEY_AUTH_PLUGIN_ID, HTTP_PROTOCOL_ID,
};
use oagw_sdk::gts::{Resource as OagwResource, ROUTE_SCHEMA, UPSTREAM_SCHEMA};
use toolkit_canonical_errors::{CanonicalError, Problem};
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
```

### Gear declaration (SKETCH, modeled on `gears/credstore/credstore/src/gear.rs`)

```rust
#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],   // dep idents use '_' for '-'
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct MiniChatGear { /* OnceLock<...> fields */ }
```
- `authn-resolver`, `authz-resolver` and `oagw` are **system** gears (`capabilities = [system, …]`). They run
  `init` and register their ClientHub clients before non-system gears run `init`, so
  `hub.get::<dyn ServiceGatewayClientV1>()` works in mini-chat `init`.
- AuthN/AuthZ **plugin discovery is lazy** (first call after types-registry is ready). OAGW also
  materializes types-registry upstreams in `post_init`. So **do the S2S exchange and OAGW provisioning in the
  lifecycle entry (`start`/`serve`), not in `init`**. That matches DESIGN §"start".

---

## 2. (a) S2S `SecurityContext` via client credentials

### SDK — `gears/system/authn-resolver/authn-resolver-sdk/src/` (VERBATIM signatures)

```rust
// api.rs
#[async_trait]
pub trait AuthNResolverClient: Send + Sync {
    async fn authenticate(&self, bearer_token: &str)
        -> Result<AuthenticationResult, AuthNResolverError>;
    async fn exchange_client_credentials(&self, request: &ClientCredentialsRequest)
        -> Result<AuthenticationResult, AuthNResolverError>;
}

// models.rs
#[derive(Debug, Clone)]
pub struct AuthenticationResult { pub security_context: SecurityContext }

pub struct ClientCredentialsRequest {          // NOTE: no Clone/Debug derive
    pub client_id: String,
    pub client_secret: SecretString,           // secrecy::SecretString
    pub scopes: Vec<String>,                   // passed as OAuth2 `scope`; static plugin ignores
}

// error.rs
pub enum AuthNResolverError {
    Unauthorized(String), NoPluginAvailable, ServiceUnavailable(String),
    TokenAcquisitionFailed(String), Internal(String),
}
```
Re-exported at the crate root: `AuthNResolverClient, AuthNResolverError, AuthenticationResult, ClientCredentialsRequest`.

Resolver-side error mapping (`authn-resolver/src/domain/error.rs`): plugin not found → `NoPluginAvailable`;
plugin not ready → `ServiceUnavailable`; types-registry down → `Internal`; bad creds → `TokenAcquisitionFailed`.

### Static plugin behaviour — `gears/system/authn-resolver/plugins/static-authn-plugin/src/`

Config (`config.rs`, `deny_unknown_fields`):
```yaml
static-authn-plugin:
  config:
    vendor: "constructorfabric"
    priority: 100
    mode: accept_all            # or static_tokens
    s2s_credentials:
      - client_id: "mini-chat"
        client_secret: "mini-chat-dev-secret"
        # identity: { subject_id, subject_tenant_id, token_scopes, subject_type }   # optional
        # bearer_token: "..."                                                       # optional
```
Exchange logic (`domain/service.rs`, VERBATIM):
```rust
pub fn exchange_client_credentials(&self, request: &ClientCredentialsRequest) -> Option<AuthenticationResult> {
    let entry = self.s2s_credentials.get(&request.client_id)?;
    if entry.client_secret.expose_secret() != request.client_secret.expose_secret() {
        return None;
    }
    build_result(&entry.identity, entry.bearer_token.clone())
}
```
- `None` → `AuthNResolverError::TokenAcquisitionFailed("invalid client credentials")` (`domain/client.rs`).
- When `identity` is omitted it defaults to `subject_id = DEFAULT_SUBJECT_ID (11111111-6a88-4768-9dfc-6bcd5187d9ed)`,
  `subject_tenant_id = DEFAULT_TENANT_ID (00000000-df51-5b42-9538-d2b56b7ee953)`, `token_scopes = ["*"]`,
  `subject_type = None`, and no bearer token unless `bearer_token` is configured.
- **Nothing else in the repo calls `exchange_client_credentials` yet** (only mocks in api-gateway tests).

### Usage (SKETCH)

```rust
async fn s2s_context(hub: &toolkit::client_hub::ClientHub, cfg: &ClientCredentialsCfg)
    -> anyhow::Result<SecurityContext>
{
    let authn = hub.get::<dyn AuthNResolverClient>()?;
    let req = ClientCredentialsRequest {
        client_id: cfg.client_id.clone(),
        client_secret: secrecy::SecretString::from(cfg.client_secret.clone()),
        scopes: vec![],
    };
    match authn.exchange_client_credentials(&req).await {
        Ok(AuthenticationResult { security_context }) => Ok(security_context),
        // retryable at startup: plugin not registered/ready yet
        Err(AuthNResolverError::NoPluginAvailable | AuthNResolverError::ServiceUnavailable(_)) => { /* retry w/ backoff */ todo!() }
        // deterministic misconfiguration → fail startup
        Err(e @ AuthNResolverError::TokenAcquisitionFailed(_)) => Err(anyhow::anyhow!("S2S exchange failed: {e}")),
        Err(e) => Err(e.into()),
    }
}
```
The mini-chat config block is `client_credentials: { client_id, client_secret }`. DESIGN requires both to be
non-empty, the secret to be redacted in logs, and `${VAR}` expansion to work.

---

## 3. (b) OAGW upstream/route provisioning

### Trait — `gears/system/oagw/oagw-sdk/src/api.rs` (VERBATIM)

```rust
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
    async fn resolve_proxy_target(&self, ctx: SecurityContext, alias: &str, method: &str, path: &str)
        -> Result<(Upstream, Route), CanonicalError>;   // resolve only, no auth/forward — good for startup validation
    async fn proxy_request(&self, ctx: SecurityContext, req: http::Request<Body>)
        -> Result<http::Response<Body>, CanonicalError>;
}
```
`ctx` is passed **by value**, so clone it per call. All CRUD is scoped to `ctx.subject_tenant_id()`.

### Models — `oagw-sdk/src/models.rs` (no serde derives)

```rust
pub enum Scheme { Http, #[default] Https, Wss, Wt, Grpc }
pub struct Endpoint { pub scheme: Scheme, pub host: String, pub port: u16 }
pub struct Server   { pub endpoints: Vec<Endpoint> }
pub enum SharingMode { #[default] Private, Inherit, Enforce }
pub struct AuthConfig {
    pub plugin_type: String,                        // GTS id of auth plugin
    pub sharing: SharingMode,
    pub config: Option<HashMap<String, String>>,    // flat k/v, plugin specific
}
pub struct HeadersConfig { pub request: Option<RequestHeaderRules>, pub response: Option<ResponseHeaderRules> }
pub struct RequestHeaderRules {
    pub set: HashMap<String,String>, pub add: HashMap<String,String>, pub remove: Vec<String>,
    pub passthrough: PassthroughMode,               // None (default) | Allowlist | All
    pub passthrough_allowlist: Vec<String>,
}
pub enum HttpMethod { Get, Post, Put, Delete, Patch }
pub enum PathSuffixMode { Disabled, #[default] Append }
pub struct HttpMatch {
    pub methods: Vec<HttpMethod>,       // ≥1
    pub path: String,                   // prefix, must start with '/'
    pub query_allowlist: Vec<String>,   // empty = allow none
    pub path_suffix_mode: PathSuffixMode,
}
pub struct MatchRules { pub http: Option<HttpMatch>, pub grpc: Option<GrpcMatch> }   // exactly one
pub struct ListQuery { pub top: u32, pub skip: u32 }   // Default { top: 50, skip: 0 }
pub struct Upstream { pub id: Uuid, pub tenant_id: Uuid, pub alias: String, pub server: Server, pub protocol: String,
    pub enabled: bool, pub auth: Option<AuthConfig>, pub headers: Option<HeadersConfig>, pub plugins: Option<PluginsConfig>,
    pub rate_limit: Option<RateLimitConfig>, pub cors: Option<CorsConfig>, pub tags: Vec<String> }
pub struct Route { pub id: Uuid, pub tenant_id: Uuid, pub upstream_id: Uuid, pub match_rules: MatchRules,
    pub plugins: Option<PluginsConfig>, pub rate_limit: Option<RateLimitConfig>, pub cors: Option<CorsConfig>,
    pub tags: Vec<String>, pub priority: i32, pub enabled: bool }

// builders (private fields; use builders)
CreateUpstreamRequest::builder(server: Server, protocol: impl Into<String>)
    .alias(..).auth(AuthConfig).headers(HeadersConfig).plugins(..).rate_limit(..).cors(..)
    .tags(Vec<String>).enabled(bool).build()          // enabled defaults to true
CreateRouteRequest::builder(upstream_id: Uuid, match_rules: MatchRules)
    .plugins(..).rate_limit(..).cors(..).tags(..).priority(i32 /*default 0*/).enabled(bool).build()
UpdateUpstreamRequest::builder(server, protocol) / UpdateRouteRequest::builder(match_rules)   // PUT semantics (full replace)
```

GTS constants (`oagw-sdk/src/gts.rs`, re-exported at the root):
- `HTTP_PROTOCOL_ID = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"`
- `APIKEY_AUTH_PLUGIN_ID = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"` (the value in `config/mini-chat.yaml`)
- Other built-in auth plugins (impl crate, `oagw/src/domain/gts_helpers.rs`): `…auth_plugin.v1~cf.core.oagw.noop.v1`,
  `…oauth2_client_cred.v1`, `…oauth2_client_cred_basic.v1`. The registry (`infra/plugin/registry.rs`) wires
  apikey, noop and the oauth2 variants.
- `UPSTREAM_SCHEMA = "gts.cf.core.oagw.upstream.v1~"`, `ROUTE_SCHEMA = "gts.cf.core.oagw.route.v1~"`, `PROXY_SCHEMA = "gts.cf.core.oagw.proxy.v1~"`.

### API-key auth plugin config — `oagw/src/infra/plugin/apikey_auth.rs` (VERBATIM)

```rust
struct ApiKeyConfig {
    header: String,          // e.g. "Authorization", "api-key", "x-api-key" (lower-cased on insert)
    #[serde(default)] prefix: String,   // e.g. "Bearer " or ""
    secret_ref: String,      // "cred://openai-key" or bare "openai-key"
}
// authenticate(): strip "cred://", SecretRef::new(raw), credstore.get(&ctx.security_context, &key)
//   -> None => PluginError::SecretNotFound ; value must be UTF-8 ; header = prefix + secret
```
The secret is resolved **at proxy time with the request's `SecurityContext`** (user ctx for chat requests).

### Validation performed by `create_upstream` (`oagw/src/domain/services/management/mod.rs`)
Steps in order:
1. `validate_endpoints`: at least one endpoint, no IPv6, no IP/hostname mixing, same scheme for all.
2. SSRF hostname/IP check (`ssrf_policy`).
3. CORS and budget checks.
4. Alias rules (gotcha #1). Valid alias charset: `[A-Za-z0-9.:_-]`, at most 253 chars, normalized to lowercase,
   trailing dots trimmed.
5. Ancestor bind check.
6. **`validate_secret_ref_accessible(ctx, credstore, secret_ref)`** (`bind.rs`):
   - `Ok(Some)` → ok.
   - `Ok(None)` or credstore not-found → `SecretRefNotAccessible` → `CanonicalError::FailedPrecondition`
     with precondition violation `{type:"STATE", subject:"auth.config.secret_ref"}` (HTTP 400). Retry later.
   - credstore `Internal` → `CanonicalError::Internal`.
   - Malformed ref → `InvalidArgument` (validation).
7. Repo insert. Alias already taken for this tenant → `AlreadyExists`.

`create_route`: the upstream must exist in the same tenant (otherwise validation error). Then
`validate_match_rules`, then `check_route_overlap`: same `path` + same `priority` + overlapping method → `AlreadyExists`.

### Route matching and URL building (proxy side)
- Match: `path.starts_with(route.path)` (plain string prefix, not segment-aware). Longest prefix wins, then
  priority. The method must be in `methods`. Only enabled routes match.
- `PathSuffixMode::Disabled`: any extra path after `route.path` → `InvalidArgument` (`PATH_SUFFIX_NOT_ALLOWED`).
- Upstream URL = `{scheme}://{host[:port unless default]}{route.path}{remaining_suffix}?{allowlisted query}`
  (`infra/proxy/request_builder.rs`).

### Idempotent ensure (SKETCH)

```rust
pub enum ProvisionError { Deferred(String), Fatal(String) }

fn classify(e: CanonicalError) -> ProvisionError {
    match ServiceGatewayError::from(e) {
        ServiceGatewayError::FailedPrecondition { subject, .. } if subject == "auth.config.secret_ref"
            => ProvisionError::Deferred("credstore secret not readable yet".into()),
        ServiceGatewayError::Unavailable { .. } | ServiceGatewayError::Timeout
            => ProvisionError::Deferred("oagw unavailable".into()),
        ServiceGatewayError::Internal { detail } => ProvisionError::Deferred(detail), // e.g. credstore down
        other => ProvisionError::Fatal(other.to_string()),                             // validation etc.
    }
}

async fn ensure_upstream(gw: &dyn ServiceGatewayClientV1, s2s: &SecurityContext, p: &ProviderEntry)
    -> Result<Upstream, ProvisionError>
{
    let scheme = if p.use_http { Scheme::Http } else { Scheme::Https };
    let port = p.port.unwrap_or(if p.use_http { 80 } else { 443 });
    let auth = AuthConfig {
        plugin_type: p.auth_plugin_type.clone(),                 // APIKEY_AUTH_PLUGIN_ID
        sharing: SharingMode::Inherit,                           // visible to descendant tenants (gotcha #6)
        config: Some(p.auth_config.clone()),                     // {"header":"api-key","prefix":"","secret_ref":"azure-openai-key"}
    };
    let headers = HeadersConfig {
        request: Some(RequestHeaderRules {
            passthrough: PassthroughMode::Allowlist,
            passthrough_allowlist: vec!["accept".into(), "anthropic-version".into()],   // as needed
            ..Default::default()
        }),
        response: None,
    };
    let req = CreateUpstreamRequest::builder(
            Server { endpoints: vec![Endpoint { scheme, host: p.host.clone(), port }] },
            HTTP_PROTOCOL_ID)
        .alias(p.upstream_alias.clone())        // must equal derived alias for hostnames (host or host:port)
        .auth(auth)
        .headers(headers)
        .tags(vec!["mini-chat".into()])
        .build();

    match gw.create_upstream(s2s.clone(), req).await {
        Ok(u) => Ok(u),
        Err(CanonicalError::AlreadyExists { .. }) => find_upstream_by_alias(gw, s2s, &p.upstream_alias)
            .await?
            .ok_or_else(|| ProvisionError::Fatal("alias exists but not listable".into())),
        Err(e) => Err(classify(e)),
    }
}

async fn find_upstream_by_alias(gw: &dyn ServiceGatewayClientV1, s2s: &SecurityContext, alias: &str)
    -> Result<Option<Upstream>, ProvisionError>
{
    let want = alias.to_ascii_lowercase();
    let mut skip = 0;
    loop {
        let page = gw.list_upstreams(s2s.clone(), &ListQuery { top: 100, skip }).await.map_err(classify)?;
        if let Some(u) = page.iter().find(|u| u.alias == want) { return Ok(Some(u.clone())); }
        if page.len() < 100 { return Ok(None); }
        skip += 100;
    }
}

async fn ensure_route(gw: &dyn ServiceGatewayClientV1, s2s: &SecurityContext, upstream_id: uuid::Uuid,
                      methods: Vec<HttpMethod>, path: &str, query_allowlist: Vec<String>)
    -> Result<Route, ProvisionError>
{
    let rules = MatchRules {
        http: Some(HttpMatch { methods: methods.clone(), path: path.to_owned(), query_allowlist,
                               path_suffix_mode: PathSuffixMode::Append }),
        grpc: None,
    };
    match gw.create_route(s2s.clone(), CreateRouteRequest::builder(upstream_id, rules).build()).await {
        Ok(r) => Ok(r),
        Err(CanonicalError::AlreadyExists { .. }) => {
            let routes = gw.list_routes(s2s.clone(), Some(upstream_id), &ListQuery { top: 1000, skip: 0 })
                .await.map_err(classify)?;
            routes.into_iter()
                .find(|r| r.match_rules.http.as_ref().is_some_and(|h| h.path == path))
                .ok_or_else(|| ProvisionError::Fatal("route conflict but not found".into()))
        }
        Err(e) => Err(classify(e)),
    }
}
```
Example route for Azure Responses: `methods=[Post]`, `path="/openai/v1/responses"`, `query_allowlist=["api-version"]`.
For files/vector stores, add routes with prefix `/openai/v1/files` or `/openai/v1/vector_stores` and
`[Get, Post, Delete]`. A single broad route such as `/openai/v1` with `Append` also works.

Startup validation option: `gw.resolve_proxy_target(ctx, alias, "POST", "/openai/v1/responses")` resolves without proxying.

### Typed error projection — `oagw-sdk/src/error.rs`
`ServiceGatewayError::from(CanonicalError)` (infallible) gives these variants:
- `RateLimited{retry_after_secs}`
- `Timeout`
- `Unavailable{retry_after_secs}`
- `AuthFailed{reason, detail}`
- `PermissionDenied{reason, detail}`
- `PayloadTooLarge{detail}`
- `InvalidTargetHost{code, detail}`
- `Validation{field, reason, detail}`
- `NotFound{resource: gts::Resource, name}`
- `AlreadyExists{resource, name}`
- `FailedPrecondition{precondition_type, subject, detail}`
- `Aborted{reason, detail}`
- `Internal{detail}`
- `Other{canonical}`

`gts::Resource::{Proxy, Upstream, Route, AuthPlugin, GuardPlugin, TransformPlugin, Unknown(String)}`. Raw accessors:
`err.resource_type()`, `err.resource_name()`, `err.detail()`, `err.gts_type()`.

### OAGW gear config — `oagw/src/config.rs` (`deny_unknown_fields`)
| key | default | meaning |
|---|---|---|
| `proxy_timeout_secs` | 30 | request→response-headers timeout **and** Pingora upstream read timeout |
| `max_body_size_bytes` | 100 MiB | buffered and streaming request bodies (413 on overflow) |
| `allow_http_upstream` | false | if false, an `http` endpoint fails **at proxy time** with `Validation{field:"endpoint.scheme", reason: HTTP_UPSTREAM_FORBIDDEN}` (create still succeeds); rejected under `fips` |
| `streaming_idle_timeout_secs` | 300 | SSE: no upstream bytes for N s → body stream ends |
| `websocket_idle_timeout_secs` / `websocket_close_timeout_secs` | 300 / 5 | |
| `token_cache_ttl_secs` / `token_cache_capacity` | 300 / 10000 | OAuth2 plugin cache |
| `protocol_cache_ttl_secs` | 3600 | ALPN cache |
| `management_api_enabled` | true | REST CRUD only; SDK CRUD is always available |
| `ssrf_policy` | `{enabled:true, extra_deny_cidrs:[], allow_cidrs:[], extra_deny_hostnames:[], allow_hostnames:[]}` | built-in deny-list of private/loopback/link-local; allow-lists take precedence; set `enabled:false` for localhost mocks |
| `metrics.prefix` | "" → `oagw` | |

Connect timeout is fixed at 10 s (`gear.rs`). `config/mini-chat.yaml` has:
```yaml
oagw:
  config:
    proxy_timeout_secs: 60
    ssrf_policy:
      enabled: false     # local mocks on 127.0.0.1
```
`config/e2e-local.yaml` additionally sets `allow_http_upstream: true` (needed for `use_http` providers or mocks).

OAGW gear deps: `deps = [types_registry, authz_resolver, credstore, tenant_resolver]`. A tenant resolver
(e.g. `single-tenant-tr-plugin`) and the credstore gateway must be loaded.

---

## 4. (c) In-process proxying: `proxy_request`

```rust
async fn proxy_request(&self, ctx: SecurityContext, req: http::Request<oagw_sdk::Body>)
    -> Result<http::Response<oagw_sdk::Body>, CanonicalError>;
```

### Body type — `oagw-sdk/src/body.rs` (VERBATIM essentials)
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
// From: (), Bytes (empty→Empty), Vec<u8>, String, &'static str, BodyStream, MultipartBody
```

### Request URI convention
The URI must be origin-form `/{alias}{path}?{query}`. The data plane splits on the first `/` after the leading
`/` to get the alias, then normalizes the suffix (`.`/`..`, `//`). Query params are URL-decoded and checked
against the route allowlist. Always build with a leading slash:
`format!("/{alias}{api_path}")`, e.g. `/my-res.openai.azure.com/openai/v1/responses?api-version=2025-03-01-preview`.

### Pipeline order (`oagw/src/infra/proxy/service.rs::proxy_request`)
1. Parse the request. Reject a bad Content-Type and request-smuggling headers. A `Body::Bytes` larger than
   the max → 413.
2. **PEP check**: `access_scope_with(ctx, PROXY{owner_tenant_id}, "invoke", None,
   require_constraints(false).context_tenant_id(ctx.subject_tenant_id()))`.
   - deny → `PermissionDenied` (403).
   - nil-tenant ctx is denied by static-authz.
   - PDP failure → `Internal`.
3. `resolve_proxy_target` (tenant-chain walk):
   - no upstream → `NotFound(upstream)`.
   - disabled upstream → `Unavailable` (503).
   - no route → `NotFound(route)`.
4. CORS (only if `Origin` is present). Query allowlist. `path_suffix_mode`.
5. Outbound headers: passthrough filter (gotcha #4), strip hop-by-hop and `x-oagw-*`.
6. Auth plugin (adds e.g. `api-key: <secret>`):
   - plugin missing / failed → `AuthFailed` (401).
   - secret missing at proxy time → `Internal` (500).
   - bad plugin config → `Validation`.
7. Guard plugins, header rules (`set`/`add`/`remove`), transform plugins.
8. Endpoint selection. `X-OAGW-Target-Host` is optional and validated. `http` + `!allow_http_upstream` → Validation.
9. Rate limits → `RateLimited` (429).
10. Build the URL and bridge into Pingora over an in-memory duplex.
    - Buffered bodies are sent with `Content-Length`.
    - `Body::Stream` is sent with `Transfer-Encoding: chunked`.

### Sending JSON and reading a buffered response (SKETCH)
```rust
let req = http::Request::builder()
    .method(http::Method::POST)
    .uri(format!("/{alias}{api_path}?api-version={api_version}"))
    .header(http::header::CONTENT_TYPE, "application/json")       // always forwarded
    .body(Body::from(serde_json::to_vec(&payload)?))?;
let resp = gw.proxy_request(user_ctx.clone(), req).await?;          // Err = gateway-side failure
let source = resp.extensions().get::<ErrorSource>().copied();       // Gateway | Upstream (always set)
let status = resp.status();
let bytes = resp.into_body().into_bytes().await.map_err(|e| anyhow::anyhow!(e))?;
if !status.is_success() {
    if source == Some(ErrorSource::Gateway) {
        // application/problem+json produced by OAGW (502/503/504); parse with toolkit_canonical_errors::Problem
        let _p: Problem = serde_json::from_slice(&bytes)?;
    } else { /* provider error body: map per provider */ }
}
```

### Reading SSE chunk by chunk — `oagw-sdk/src/sse/stream.rs`
```rust
// VERBATIM signatures
pub enum ServerEventsResponse<T: FromServerEvent = ServerEvent> { Events(ServerEventsStream<T>), Response(http::Response<Body>) }
impl ServerEventsStream { pub fn from_response<T: FromServerEvent>(resp: impl Into<http::Response<Body>>) -> ServerEventsResponse<T>; }
impl<T: FromServerEvent> ServerEventsStream<T> { pub fn status(&self) -> StatusCode; pub fn headers(&self) -> &HeaderMap; }
impl<T: FromServerEvent> Stream for ServerEventsStream<T> { type Item = Result<T, StreamingError>; }
pub struct ServerEvent { pub id: Option<String>, pub event: Option<String>, pub data: String, pub retry: Option<u64> }
impl ServerEvent { pub fn json<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error>; }
// FromServerEvent impls: ServerEvent (raw), oagw_sdk::Json<T> (deserializes `data`)
pub enum StreamingError { ServerEventsParse{detail}, Stream(BoxError), WebSocketConnect{detail}, WebSocketBridge{detail} }
```
Detection is by `Content-Type: text/event-stream` (`sse::is_server_events_response`). Anything else is
returned untouched as `Response(resp)`, which is the normal path for 4xx/5xx JSON errors.

```rust
// SKETCH
use futures::StreamExt;
let resp = gw.proxy_request(ctx.clone(), req).await?;
match ServerEventsStream::from_response::<ServerEvent>(resp) {
    ServerEventsResponse::Events(mut events) => {
        while let Some(item) = events.next().await {
            match item {
                Ok(ev) => {
                    if ev.data == "[DONE]" { break; }          // OpenAI chat-completions terminator
                    // ev.event == Some("response.output_text.delta") etc. for Responses API
                    let v: serde_json::Value = ev.json()?;
                    /* relay delta */
                }
                Err(StreamingError::Stream(e)) => { /* upstream read error / truncated stream */ break; }
                Err(e) => { /* parse error */ break; }
            }
        }
        // stream ended: if no terminal event seen → treat as failure (idle timeout / shutdown end silently)
    }
    ServerEventsResponse::Response(resp) => { /* non-SSE: error JSON or Problem (check ErrorSource) */ }
}
```
The raw response body for SSE is `Body::Stream`. Parsing is opt-in, so you can also consume
`resp.into_body().into_stream()` yourself. Reference tests: `oagw-sdk/tests/usage.rs`,
`oagw/tests/proxy_integration.rs::proxy_sse_streaming`.

### Streaming request body / multipart upload — `oagw-sdk/src/multipart.rs`
```rust
// VERBATIM signatures
impl MultipartBody {
    pub fn new() -> Self;                                   // random UUID boundary
    pub fn with_boundary(b: impl Into<String>) -> Result<Self, MultipartError>;
    pub fn part(self, part: Part) -> Self;
    pub fn text(self, name: &str, value: impl Into<String>) -> Self;
    pub fn bytes(self, name: &str, data: impl Into<Bytes>) -> Self;
    pub fn content_type(&self) -> String;                   // "multipart/form-data; boundary=..."
    pub fn into_body(self) -> Body;                         // Body::Stream if any streaming part
    pub fn into_request<M, U>(self, method: M, uri: U) -> Result<http::Request<Body>, http::Error>; // sets Content-Type
}
impl Part {
    pub fn text(name: &str, value: impl Into<String>) -> Self;
    pub fn bytes(name: &str, data: impl Into<Bytes>) -> Self;
    pub fn stream(name: &str, stream: BodyStream) -> Self;
    pub fn filename(self, f: impl Into<String>) -> Self;
    pub fn content_type(self, ct: impl Into<String>) -> Self;
}
```
```rust
// SKETCH: forward an incoming upload stream (e.g. from multer field / axum body) to OpenAI/Azure Files API
let file_stream: BodyStream = Box::pin(
    field_stream.map(|r: Result<bytes::Bytes, multer::Error>| r.map_err(|e| Box::new(e) as BoxError)));
let req = MultipartBody::new()
    .text("purpose", "assistants")
    .part(Part::stream("file", file_stream).filename(file_name).content_type(mime))
    .into_request(http::Method::POST, format!("/{alias}/openai/v1/files?api-version={v}"))?;
let resp = gw.proxy_request(ctx.clone(), req).await?;
```
Streaming-body semantics (`service.rs::bridge_and_respond`):
- A spawned task forwards chunks as HTTP/1.1 chunked encoding and skips empty chunks.
- Exceeding `max_body_size_bytes` → `Err(PayloadTooLarge)` (413).
- A stream `Err` item, or a write error, **aborts without the terminating `0\r\n\r\n`** →
  `Err(ServiceUnavailable 502 "streaming request body failed mid-stream")`. A clean end-of-stream writes the
  terminator.
- The response-header timeout (`proxy_timeout_secs`) runs **concurrently with the upload** (gotcha #11).

### Timeouts and error channels (summary)
| Situation | What the caller sees |
|---|---|
| No response headers within `proxy_timeout_secs` (tokio timeout in bridge) | `Err(CanonicalError::DeadlineExceeded)` → `ServiceGatewayError::Timeout` (504). Test `proxy_upstream_timeout_returns_504` asserts this for in-process calls |
| Pingora connect timeout (10 s) / upstream read/write timeout | `Ok(resp)`: status 504, `content-type: application/problem+json`, `ErrorSource::Gateway` (`pingora_proxy.rs::fail_to_proxy`). REST e2e asserts `x-oagw-error-source: gateway`. The two timers race, so handle both |
| Connect refused / TLS failure / peer closed / read error / H2 error | `Ok(resp)` 502 (or 503 for no route / link unavailable), Problem body, `ErrorSource::Gateway` |
| Upstream 4xx/5xx (incl. 429) | `Ok(resp)` with the upstream status and body, `ErrorSource::Upstream`. **No retry** |
| SSE: no data for `streaming_idle_timeout_secs` or gateway shutdown | body stream ends (`None`) silently |
| SSE: upstream stalls longer than Pingora read timeout mid-body | body truncated / stream error (inferred from Pingora per-read timeout) |
| Rate limit | `Err(ResourceExhausted)` → `RateLimited{retry_after_secs}` |
| Proxy authz deny | `Err(PermissionDenied)` (reason from deny_reason or `AUTHZ_DENIED`) |

Canonical → HTTP status (impl `oagw/src/api/rest/error.rs`):
- `DownstreamError`, `ProtocolError`, `StreamAborted` → 503-category with status override **502**, Retry-After.
- `LinkUnavailable`, `UpstreamDisabled`, `CircuitBreakerOpen` → 503.
- `ConnectionTimeout`, `RequestTimeout`, `IdleTimeout` → `DeadlineExceeded` (504).
- `SecretNotFound` → `Internal` (500).
- `SecretRefNotAccessible` → `FailedPrecondition` (400).

### Cancellation by dropping (from code; not covered by a dedicated test)
- The response `Body::Stream` **owns the duplex read half** (`session_bridge::parse_response_stream` moves
  `io` into the body stream). Dropping the `ServerEventsStream`/`Body` closes the in-memory pipe, Pingora's
  downstream writes fail, and it tears down the upstream connection. This is how a client disconnect cancels
  generation.
- Dropping the `proxy_request` future before headers arrive drops the client side too. The
  `RequestMetricsGuard` records outcome `"Cancelled"`.
- For **streaming uploads** the body-forwarding task is spawned and holds the write half. Cancelling the caller
  does not stop it by itself. To abort an upload, make your `BodyStream` yield an `Err` (e.g. on a
  `CancellationToken`): that aborts **without** the chunked terminator, so the upstream sees an incomplete
  body. Ending the stream cleanly would write the terminator and complete the upload.
- OAGW does not cancel anything provider-side (e.g. OpenAI `/responses/{id}/cancel`). That is the adapter's job.

---

## 5. (d) AuthZ as a PEP

### SDK surface — `gears/system/authz-resolver/authz-resolver-sdk/src/`
Root re-exports: `AuthZResolverApi`, `PolicyEnforcer`, `AccessRequest`, `ResourceType`, `EnforcerError`,
`IntoPropertyValue`, models (`EvaluationRequest`, `EvaluationResponse`, `Subject`, `Action`, `Resource`,
`TenantContext`, `TenantMode{RootOnly, Subtree(default)}`, `BarrierMode{Respect(default), Ignore}`,
`Capability{TenantHierarchy, GroupMembership, GroupHierarchy}`, `DenyReason{error_code, details}`), and
constraints (`Constraint{predicates}`, `Predicate::{Eq,In,InGroup,InGroupSubtree,InTenantSubtree}`).

```rust
// api.rs — the PDP contract (toolkit::contract). Deny is Ok(decision=false), not an Err.
async fn evaluate(&self, ctx: PlatformSecurityContext, req: EvaluationRequest) -> Result<EvaluationResponse, CanonicalError>;

// pep/enforcer.rs (VERBATIM signatures)
pub enum EnforcerError {
    Denied { deny_reason: Option<DenyReason> },          // PDP decision=false
    EvaluationFailed(#[from] CanonicalError),             // transport/PDP failure or 30s deadline (service_unavailable)
    CompileFailed(#[from] ConstraintCompileError),        // missing/unsupported constraints (fail-closed)
}
impl AccessRequest {
    pub fn new() -> Self;
    pub fn resource_property(self, key: impl Into<String>, value: impl IntoPropertyValue) -> Self;
    pub fn resource_properties(self, props: HashMap<String, serde_json::Value>) -> Self;
    pub fn context_tenant_id(self, id: Uuid) -> Self;      // TenantContext.root_id
    pub fn tenant_mode(self, mode: TenantMode) -> Self;
    pub fn barrier_mode(self, mode: BarrierMode) -> Self;
    pub fn tenant_status(self, statuses: Vec<String>) -> Self;
    pub fn tenant_context(self, tc: TenantContext) -> Self;
    pub fn require_constraints(self, require: bool) -> Self;   // default true
}
impl ResourceType {
    pub const fn from_static(name: &'static str, supported_properties: &'static [&'static str]) -> Self;
    pub fn new(name: impl Into<Cow<'static, str>>, supported_properties: &'static [&'static str]) -> Self;
}
impl PolicyEnforcer {
    pub fn new(authz: Arc<dyn AuthZResolverApi>) -> Self;          // eager (in-process)
    pub fn from_hub(hub: Arc<ClientHub>) -> Self;                   // lazy, resolves client per call (works in/out of process)
    pub fn with_capabilities(self, caps: Vec<Capability>) -> Self;
    pub fn with_deadline(self, d: Duration) -> Self;                // default DEFAULT_EVAL_DEADLINE = 30s
    pub async fn access_scope(&self, ctx: &SecurityContext, resource: &ResourceType, action: &str,
                              resource_id: Option<Uuid>) -> Result<AccessScope, EnforcerError>;   // require_constraints = true
    pub async fn access_scope_with(&self, ctx: &SecurityContext, resource: &ResourceType, action: &str,
                              resource_id: Option<Uuid>, request: &AccessRequest) -> Result<AccessScope, EnforcerError>;
    pub fn build_request_with(..) -> EvaluationRequest;             // low-level
}
```
What the enforcer sends:
- `subject = { id: ctx.subject_id(), type: ctx.subject_type(), properties: {"tenant_id": ctx.subject_tenant_id()} }`
- `action.name`
- `resource = {type, id, properties}`
- `context = { tenant_context (only if you set one), token_scopes, require_constraints, capabilities, supported_properties, bearer_token }`

Compilation matrix (`pep/compiler.rs`), with `decision=true`:
| `require_constraints` | constraints | result |
|---|---|---|
| false | empty | `AccessScope::allow_all()` |
| false or true | present | compiled |
| true | empty | `ConstraintsRequiredButAbsent` |

Predicates on properties not listed in `supported_properties` fail that constraint. If all constraints fail →
`AllConstraintsFailed`. Each constraint becomes an AND of `ScopeFilter`s, and constraints are OR-ed.

### Static AuthZ plugin — `gears/system/authz-resolver/plugins/static-authz-plugin/src/domain/service.rs`
1. Tenant = `context.tenant_context.root_id` if set, else `subject.properties["tenant_id"]`. If none or nil →
   `decision:false` (→ `EnforcerError::Denied{deny_reason:None}`).
2. `resource.properties` contains `group_ids` or `ancestor_group_ids` → deny.
3. If `supported_properties` contains `owner_tenant_id` → constraint `[In(owner_tenant_id, [tid])]`.
4. If the PEP advertised `Capability::TenantHierarchy` → also `InTenantSubtree(owner_tenant_id|id, tid)` for each supported one.
5. Otherwise `decision:true` with **empty constraints**. With `require_constraints=true` the compiler turns
   that into `CompileFailed` (403). With `false` it becomes `allow_all`.
6. **It never emits `owner_id` (user) predicates**, ignores action and resource id, and has no config besides
   `vendor`/`priority`.

### toolkit-security `AccessScope` — `libs/toolkit-security/src/access_scope.rs`
```rust
pub mod pep_properties { pub const OWNER_TENANT_ID: &str = "owner_tenant_id"; pub const RESOURCE_ID: &str = "id"; pub const OWNER_ID: &str = "owner_id"; }
impl AccessScope {
    pub fn allow_all() -> Self; pub fn deny_all() -> Self; pub fn for_tenant(id: Uuid) -> Self; pub fn for_tenants(ids: Vec<Uuid>) -> Self;
    pub fn for_resource(id: Uuid) -> Self; pub fn from_constraints(v: Vec<ScopeConstraint>) -> Self; pub fn single(c: ScopeConstraint) -> Self;
    pub fn constraints(&self) -> &[ScopeConstraint]; pub fn is_unconstrained(&self) -> bool; pub fn is_deny_all(&self) -> bool;
    pub fn all_uuid_values_for(&self, property: &str) -> Vec<Uuid>; pub fn contains_uuid(&self, property: &str, id: Uuid) -> bool;
    pub fn has_property(&self, property: &str) -> bool;
    pub fn tenant_only(&self) -> Self;            // keep only owner_tenant_id filters (WIDENS — for child entities w/o owner col)
    pub fn tenant_and_owner(&self) -> Self;       // keep owner_tenant_id + owner_id filters (WIDENS)
    pub fn ensure_owner(&self, owner_id: Uuid) -> Self;  // inject/intersect Eq(owner_id); unconstrained → owner-only; deny stays deny
}
```
Secure ORM entity: `#[derive(DeriveEntityModel, Scopable)]` with
`#[secure(tenant_col = "tenant_id", owner_col = "user_id", resource_col = "id", no_type)]`. Use `no_owner`,
`no_resource` or `no_tenant` where a column is absent. Example entity:
`gears/credstore/credstore/src/infra/storage/entity/secrets.rs`.

Query and insert patterns (credstore, `infra/storage/repo_impl/{reads,writes}.rs`):
```rust
use toolkit_db::secure::{SecureEntityExt, SecureInsertExt, SecureUpdateExt, SecureDeleteExt};
Entity::find().secure().scope_with(&scope).filter(..)        // SELECT with WHERE from scope
Entity::update_many().col_expr(..).filter(..).secure().scope_with(&scope).exec(&conn)
Entity::insert(am).secure().scope_unchecked(&scope)?.exec(&conn)   // INSERT (no row to clamp yet)
```

### End-to-end example from another gear: credstore (`gears/credstore/credstore/src/domain/authz.rs`, VERBATIM)
```rust
use authz_resolver_sdk::pep::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use toolkit_security::{AccessScope, SecurityContext, pep_properties};

pub fn secret_type_resource(gts_id: &str) -> ResourceType {
    ResourceType::new(gts_id.to_owned(), &[pep_properties::OWNER_TENANT_ID])
}

pub fn map_enforcer_err(err: EnforcerError) -> DomainError {
    match err {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => {
            DomainError::AccessDenied { cause: Some(Box::new(err)) }
        }
        EnforcerError::EvaluationFailed(source) => DomainError::ServiceUnavailable {
            detail: "authorization evaluation failed".to_owned(),
            retry_after: None,
            cause: Some(Box::new(EnforcerError::EvaluationFailed(source))),
        },
    }
}

pub async fn scope_for(enforcer: &PolicyEnforcer, ctx: &SecurityContext, resource: &ResourceType, action: &str)
    -> Result<AccessScope, DomainError>
{
    let tenant = ctx.subject_tenant_id();
    let request = AccessRequest::new()
        .resource_property(pep_properties::OWNER_TENANT_ID, tenant)
        .require_constraints(true);
    enforcer.access_scope_with(ctx, resource, action, None, &request).await.map_err(map_enforcer_err)
}
```
Wiring (`gears/credstore/credstore/src/gear.rs::init`):
```rust
let authz_client = ctx.client_hub().get::<dyn AuthZResolverApi>()
    .map_err(|e| anyhow::anyhow!("failed to get AuthZResolverApi: {e}"))?;
let enforcer = PolicyEnforcer::new(authz_client);   // cloneable; one per service, all resource types
```
Usage (`domain/secret/service.rs::get`): `scope_for(...)`, then `repo.scope_includes_tenant(&scope, tenant)`.
`AccessDenied` on read is folded into `Ok(None)` (anti-enumeration).

Other PEPs to study:
- OAGW proxy: `oagw/src/infra/proxy/service.rs`. `require_constraints(false)` permission-only check on
  resource `PROXY = ResourceType::from_static(PROXY_SCHEMA, &[pep_properties::OWNER_TENANT_ID])`,
  action `"invoke"`.
- OAGW management bind checks: `oagw/src/domain/services/management/bind.rs`.
- OAGW maps `EnforcerError` to errors in `oagw/src/domain/error.rs`: Denied → `Forbidden{reason: deny_reason.error_code | AUTHZ_DENIED}`, otherwise `Internal`.

### mini-chat PEP (SKETCH, per DESIGN §3.8 "Authorization")
```rust
pub const CHAT: ResourceType = ResourceType::from_static(
    "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~",       // or toolkit_gts::gts_id!("cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~")
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID, pep_properties::RESOURCE_ID],
);
pub const MODEL: ResourceType = ResourceType::from_static(
    "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~", &[]);            // permission-only
pub const USER_QUOTA: ResourceType = ResourceType::from_static(
    "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID]);

pub enum AuthzError { Forbidden, Unavailable }   // → 403 / 503 + Retry-After (fail closed)

fn map(e: EnforcerError) -> AuthzError {
    match e {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => AuthzError::Forbidden,
        EnforcerError::EvaluationFailed(_) => AuthzError::Unavailable,
    }
}

impl ChatPep {
    pub async fn chat_scope(&self, ctx: &SecurityContext, action: &str, chat_id: Option<Uuid>)
        -> Result<AccessScope, AuthzError>
    {
        let req = AccessRequest::new()
            .context_tenant_id(ctx.subject_tenant_id())
            .tenant_mode(TenantMode::RootOnly);                       // DESIGN: no subtree for content
        let scope = self.enforcer.access_scope_with(ctx, &CHAT, action, chat_id, &req).await.map_err(map)?;
        Ok(scope.ensure_owner(ctx.subject_id()))                      // defence in depth (static PDP gives tenant only)
    }
    pub async fn create_scope(&self, ctx: &SecurityContext) -> Result<AccessScope, AuthzError> {
        let req = AccessRequest::new()
            .context_tenant_id(ctx.subject_tenant_id())
            .tenant_mode(TenantMode::RootOnly)
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .resource_property(pep_properties::OWNER_ID, ctx.subject_id());
        Ok(self.enforcer.access_scope_with(ctx, &CHAT, "create", None, &req).await.map_err(map)?
            .ensure_owner(ctx.subject_id()))
    }
    pub async fn model_permission(&self, ctx: &SecurityContext, action: &str) -> Result<(), AuthzError> {
        self.enforcer.access_scope_with(ctx, &MODEL, action, None,
            &AccessRequest::new().require_constraints(false)).await.map_err(map)?;
        Ok(())
    }
}
```
With static-authz plus the CHAT descriptor above: the PDP returns `[In(owner_tenant_id,[tid])]`.
`ensure_owner` turns that into `[In(owner_tenant_id,[tid]) AND Eq(owner_id, subject)]`. The Secure ORM then
maps `owner_tenant_id → tenant_id` and `owner_id → user_id` through `#[secure(...)]`.

Actions from DESIGN: `create`, `list`, `read`, `delete`, `update`, `list_messages`, `send_message`,
`upload_attachment`, `read_attachment`, `delete_attachment`, `read_turn`, `retry_turn`, `edit_turn`,
`delete_turn`, `set_reaction`, `delete_reaction`. The model resource uses `list`/`read` (no constraints).
UserQuota uses `read`.

---

## 6. (e) Request `SecurityContext` in handlers and the license gate

### How the ctx reaches the handler
`gears/system/api-gateway/src/middleware/auth.rs::authn_middleware`:
- Required routes: validate the bearer token through `AuthNResolverClient::authenticate`, then
  `req.extensions_mut().insert(result.security_context)`.
- Public routes and CORS preflight: insert `SecurityContext::anonymous()` (nil ids).
- With `auth_disabled: true` (as in `config/mini-chat.yaml`, `gears/system/api-gateway/src/gear.rs` ~L463):
  every request gets `SecurityContext{subject_id: DEFAULT_SUBJECT_ID, subject_tenant_id: DEFAULT_TENANT_ID}`.
  It has **no token scopes** and no bearer token.

Errors produced by the middleware:
- Missing bearer → 401 `MISSING_BEARER`.
- Rejected token → 401 `AUTHN_FAILED`.
- Plugin unavailable → 503 with retry-after 5 s.

Extractor is plain axum `Extension` (pattern from `oagw/src/api/rest/handlers/proxy.rs`):
```rust
use axum::Extension;
use toolkit_security::SecurityContext;

pub async fn create_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<std::sync::Arc<AppServices>>,
    axum::Json(body): axum::Json<CreateChatReq>,
) -> axum::response::Response {
    let user_id   = ctx.subject_id();          // Uuid
    let tenant_id = ctx.subject_tenant_id();   // Uuid  (there is no ctx.tenant_id())
    if ctx.is_anonymous() { /* nil subject/tenant → reject */ }
    // ctx.subject_type(): Option<&str>, ctx.token_scopes(): &[String], ctx.has_scope("x"), ctx.bearer_token(): Option<&SecretString>
    todo!()
}
```
`SecurityContext` (`libs/toolkit-security/src/context.rs`):
- `Clone + Serialize`; `bearer_token` is skipped during serialization.
- Builder: `SecurityContext::builder().subject_id(..).subject_tenant_id(..).subject_type(&str).token_scopes(Vec<String>).bearer_token(..).build()?`.
  Both ids are required.

### License gate on routes
`libs/toolkit/src/api/operation_builder.rs`:
```rust
pub const CORE_GLOBAL_BASE_LICENSE_FEATURE: &str = gts_id!("cf.core.lic.feat.v1~cf.core.global.base.v1");
pub trait LicenseFeature: AsRef<str> {}
impl<T: LicenseFeature + ?Sized> LicenseFeature for &T {}
// only after .authenticated(); mandatory before .register() for authenticated routes:
pub fn require_license_features<F: LicenseFeature>(self, licenses: impl IntoIterator<Item = F>) -> OperationBuilder<.., AuthSet, LicenseSet>;
pub fn no_license_required(self) -> OperationBuilder<.., AuthSet, LicenseSet>;
// visibility axis (independent of auth): .exposed() marks x-toolkit-visibility: exposed for the GatewayProvider
```
An empty iterator stores `license_requirement = None`, which means **not gated**. The types-registry routes use
`require_license_features::<License>([])` and are therefore not gated.

Middleware `gears/system/api-gateway/src/middleware/license_validation.rs` (stub):
- Builds a `(Method, path) → features` map from `OperationSpec.license_requirement`.
- If any required feature ≠ `gts.cf.core.lic.feat.v1~cf.core.global.base.v1` →
  `permission_denied().with_reason("LICENSE_FEATURE_REQUIRED")` (403).
- The base feature always passes. There is no real tenant license lookup yet.
- Test: `gears/system/api-gateway/tests/license_middleware.rs`.

Declaration for mini-chat routes (SKETCH, pattern from `types-registry/src/api/rest/routes.rs`):
```rust
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::{CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder};

struct BaseLicense;
impl AsRef<str> for BaseLicense { fn as_ref(&self) -> &'static str { CORE_GLOBAL_BASE_LICENSE_FEATURE } }
impl LicenseFeature for BaseLicense {}

router = OperationBuilder::post("/mini-chat/v1/chats")
    .operation_id("mini_chat.create_chat")
    .tag("Mini Chat")
    .authenticated()
    .require_license_features([BaseLicense])          // NOT `[]`
    .json_request::<CreateChatReq>(openapi, "Create chat")
    .handler(create_chat)
    .json_response_with_schema::<ChatDto>(openapi, StatusCode::CREATED, "Created")
    .standard_errors(openapi)
    .register(router, openapi);
```
(`config/mini-chat.yaml` also defines the api-gateway throttling zones `rl_mini_chat_chat` and
`ifl_mini_chat_chat` that the create-chat route references. Gateway startup fails validation if a route
references an undefined zone.)

---

## 7. (f) `secret_ref` / `cred://` resolution and credstore config

Resolution chain at proxy time:
1. OAGW apikey plugin strips `cred://` and calls `credstore_sdk::SecretRef::new(bare)`. A valid ref is
   `[A-Za-z0-9_-]{1,255}` (`gears/credstore/credstore-sdk/src/models.rs`).
2. It calls `CredStoreClientV1::get(&request_ctx, &key) -> Result<Option<GetSecretResponse>, CredStoreError>`.
   The credstore gateway (`gears/credstore/credstore/src/domain/secret/service.rs::get`) then:
   1. Builds the ancestor chain of `ctx.subject_tenant_id()`.
   2. `repo.resolve_for_get`: a **DB metadata row** must exist (Active, not expired). Sharing visibility:
      private = same tenant + owner; tenant = same tenant; shared = ancestors.
   3. Runs a PDP check (resource = the secret's concrete GTS type, action `read`, `OWNER_TENANT_ID` constraint).
   4. Reads the value from the backend plugin selected by `vendor` (static-credstore-plugin).
   5. Verifies the value fingerprint.
   6. Denial or a miss returns `Ok(None)` (anti-enumeration).
3. The same check runs at `create_upstream` time with the **S2S ctx** (`validate_secret_ref_accessible`).

### Config needed (`config/mini-chat.yaml`)
```yaml
credstore:                         # the gateway: needs a DB
  database: { server: "sqlite_mini_chat", file: "credstore.db" }
  config:
    vendor: "constructorfabric"    # must match the plugin vendor (default "constructorfabric")
static-credstore-plugin:
  config:
    # vendor/priority default to "constructorfabric"/100
    secrets:
      - key: "azure-openai-key"            # == auth_config.secret_ref (with or without cred://)
        value: "${AZURE_OPENAI_API_KEY}"   # ${VAR} expanded (config_expanded_or_default)
        # tenant_id: <uuid>   omitted → GLOBAL (Shared) secret; with tenant_id only → Tenant; +owner_id → Private
        # sharing: private|tenant|shared   (optional, inferred)
```
`StaticCredStorePluginConfig { vendor, priority, secrets: Vec<SecretConfig{ tenant_id: Option<Uuid>, owner_id: Option<Uuid>, key, value, sharing: Option<SharingMode> }> }`
(`gears/credstore/plugins/static-credstore-plugin/src/config.rs`). Validation rules:
- nil uuids are rejected;
- `owner_id` requires `tenant_id`;
- `owner_id` is only valid with `private`;
- `private` requires `owner_id`;
- duplicate keys per class are rejected.

**Known limitation (#5094):** these config values are only a *backend* store. The credstore gateway resolves
the metadata row from its DB first, so a config-only secret reads as `None`. Mini-chat must treat
`FailedPrecondition(auth.config.secret_ref)` as **deferred** and retry (DESIGN: first retry after 2 s,
doubling to a 60 s cap, one warning after 2 min, retry until stop). The operator creates the row once through REST, e.g.:
```bash
# first run (no metadata row yet): POST creates it
read -s -p 'key: ' KEY && printf '{"reference":"azure-openai-key","value":"%s","sharing":"tenant"}' "$KEY" \
  | curl -X POST http://127.0.0.1:8087/cf/credstore/v1/secrets \
      -H 'Authorization: Bearer dev' -H 'content-type: application/json' -d @-
# later runs: PUT with If-Match: *
read -s -p 'key: ' KEY && printf '{"value":"%s","sharing":"tenant"}' "$KEY" \
  | curl -X PUT http://127.0.0.1:8087/cf/credstore/v1/secrets/azure-openai-key \
      -H 'Authorization: Bearer dev' -H 'content-type: application/json' -H 'If-Match: *' -d @-
```
(The `/cf` prefix comes from `api-gateway.config.prefix_path`. `config/mini-chat.yaml` sets no prefix and binds
`127.0.0.1:8087`, so adjust the path.)
(See `config/quickstart.yaml` ~L446–455 and `gears/credstore/QUICKSTART.md`.) Use `"sharing":"shared"` if
user requests come from descendant tenants of the tenant that owns the secret. `"tenant"` only covers the
same tenant. With `auth_disabled: true`, both the S2S ctx and user ctx are in `DEFAULT_TENANT_ID`.

---

## 8. Startup flow checklist for mini-chat (SKETCH)

1. `init`:
   - Parse config. Fill `upstream_alias = host` when unset (keep the port suffix if non-standard).
   - Get `Arc<dyn AuthNResolverClient>`, `Arc<dyn AuthZResolverApi>`, `Arc<dyn ServiceGatewayClientV1>` from the hub.
   - Build `PolicyEnforcer::new(authz)` (or `from_hub`).
   - Build the provider resolver keyed by alias.
2. `start` / lifecycle entry:
   - `exchange_client_credentials` → `s2s_ctx`.
   - For each provider and tenant override: `ensure_upstream`, then `ensure_route`(s).
     - `Fatal` → fail startup.
     - `Deferred` → background reconcile task.
3. Requests:
   - Handler extracts `Extension<SecurityContext>`, then PEP `access_scope_with` (+ `ensure_owner`), then scoped DB ops.
   - Quota checks happen before any outbound call.
   - `gw.proxy_request(user_ctx.clone(), req)` → SSE relay. Drop the stream on client disconnect.
4. Errors to the client:
   - PEP Denied/CompileFailed → 403.
   - EvaluationFailed → 503 with Retry-After.
   - OAGW `Err` → map via `ServiceGatewayError` (Timeout → 504, Unavailable/RateLimited → 503/429, …).
   - `Ok` with `ErrorSource::Gateway` + 5xx → provider-unavailable class.

---

## 9. File index

| Topic | File |
|---|---|
| AuthN SDK | `gears/system/authn-resolver/authn-resolver-sdk/src/{api,models,error,lib}.rs` |
| Static AuthN (s2s) | `gears/system/authn-resolver/plugins/static-authn-plugin/src/{config.rs,domain/service.rs,domain/client.rs}` |
| OAGW SDK trait/models/errors | `gears/system/oagw/oagw-sdk/src/{api,models,error,gts,body,multipart}.rs`, `src/sse/*.rs` |
| OAGW SDK examples | `gears/system/oagw/oagw-sdk/tests/{usage,third_party_in_process}.rs`, `README.md` |
| OAGW config / gear wiring | `gears/system/oagw/oagw/src/{config.rs,gear.rs}` |
| OAGW proxy pipeline | `gears/system/oagw/oagw/src/infra/proxy/{service,headers,request_builder,session_bridge,pingora_proxy,mod}.rs` |
| OAGW CP (create/alias/route/secret check) | `gears/system/oagw/oagw/src/domain/services/management/{mod,alias,bind,validation,resolution,merge}.rs` |
| OAGW apikey plugin | `gears/system/oagw/oagw/src/infra/plugin/apikey_auth.rs` |
| OAGW Domain→Canonical mapping | `gears/system/oagw/oagw/src/api/rest/error.rs`, `src/domain/error.rs` |
| OAGW integration tests | `gears/system/oagw/oagw/tests/proxy_integration.rs` (SSE L215, 504 L747, streaming body L2347) |
| AuthZ SDK PEP | `gears/system/authz-resolver/authz-resolver-sdk/src/pep/{enforcer,compiler,mod}.rs`, `models.rs`, `constraints.rs` |
| Static AuthZ | `gears/system/authz-resolver/plugins/static-authz-plugin/src/domain/service.rs`, `README.md` |
| AccessScope / SecurityContext | `libs/toolkit-security/src/{access_scope,context,constants}.rs` |
| PEP example gear | `gears/credstore/credstore/src/domain/authz.rs`, `src/gear.rs`, `src/domain/secret/service.rs`, `src/infra/storage/{entity/secrets.rs,repo_impl/*.rs}` |
| Auth middleware / auth_disabled ctx | `gears/system/api-gateway/src/middleware/auth.rs`, `src/gear.rs` (~L463) |
| License gate | `libs/toolkit/src/api/operation_builder.rs` (L66, L1150), `gears/system/api-gateway/src/middleware/license_validation.rs`, `gears/system/types-registry/types-registry/src/api/rest/routes.rs` |
| Credstore SDK / static plugin | `gears/credstore/credstore-sdk/src/{api,models}.rs`, `gears/credstore/plugins/static-credstore-plugin/src/{config.rs,domain/service.rs}` |
| Configs | `config/mini-chat.yaml`, `config/quickstart.yaml` (#5094 note ~L446), `config/e2e-local.yaml` (oagw L536) |
| mini-chat design | `gears/mini-chat/docs/DESIGN.md` (OAGW provisioning L411, start L604, authz §L2508+, config L6815+) |
