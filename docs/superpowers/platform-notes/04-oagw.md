# 04 — OAGW (Outbound API Gateway): reference for the `mini-chat` gear

Scope: everything `mini-chat` needs to (a) provision OAGW upstreams and routes at startup with an
S2S `SecurityContext` (create, or reuse if they already exist), and (b) send provider traffic through
OAGW's in-process client: SSE `POST {alias}/v1/responses`, non-streaming JSON POST, multipart
`POST {alias}/v1/files`, `DELETE {alias}/v1/files/{id}`, `POST/GET/DELETE {alias}/v1/vector_stores...`,
and the Azure variants that add `?api-version=`.

Every claim below comes from the code. Paths are relative to `/app`. Crates:

| Crate (package) | lib name | Path |
|---|---|---|
| `cf-gears-oagw-sdk` | `oagw_sdk` | `gears/system/oagw/oagw-sdk` (traits, models, `Body`, SSE, multipart, errors) |
| `cf-gears-oagw` | `oagw` | `gears/system/oagw/oagw` (gear, control plane, data plane, Pingora proxy) |
| `cf-gears-credstore-sdk` / static plugin | `credstore_sdk` | `gears/credstore/...` |

`gears/mini-chat/mini-chat/Cargo.toml` already depends on `oagw-sdk`, `oagw`, `authn-resolver-sdk`,
`http`, `bytes`, `futures`, `toolkit-security`, `toolkit-canonical-errors`.

---

## 0. TL;DR: the rules that cause bugs

1. **Proxy URI shape is `/{alias}/{path}?{query}`**, with no `/oagw/v1/proxy` prefix. The data plane takes
   the first path segment as the alias (`infra/proxy/service.rs::parse_proxy_request`). The REST handler
   removes `/oagw/v1/proxy` before calling the same code (`api/rest/handlers/proxy.rs:181`). The SDK mocks in
   `oagw-sdk/tests/usage.rs` use `/api/oagw/v1/proxy/...`. That URI only works against a mock, so do not copy it.
2. **The response body is always `Body::Stream`**, including JSON and errors (`build_proxy_response`,
   `service.rs:1741`). Read it with `.into_body().into_bytes().await` or consume it as a stream. Never call
   `try_into_bytes()`.
3. **Upstream non-2xx responses come back as `Ok(Response)`** with the provider's status, headers and body,
   and `ErrorSource::Upstream` in `resp.extensions()`. Gateway failures fall into three groups:
   * pipeline errors before forwarding (no alias, no route, query not allowed, auth plugin, http-forbidden,
     time to response headers exceeded) return `Err(CanonicalError)`;
   * transport failures inside Pingora (connect refused, read timeout, TLS) return
     `Ok(Response)` with status 502/503/504, a `application/problem+json` body and `ErrorSource::Gateway`;
   * on some failure paths the same failure can surface either way (see §4.6).
4. **OAGW never retries** (no connect retries, no 429 retries; only an idempotent request on a stale
   pooled connection is reconnected). See `infra/proxy/pingora_proxy.rs:741-775`.
5. **By default only `Content-Type` is forwarded from your request headers.** With `PassthroughMode::None`,
   `Accept`, `OpenAI-Beta`, `X-Request-Id` and so on are dropped. `Authorization`/`Cookie` are always stripped.
   The auth plugin injects the credential (`infra/proxy/headers.rs::apply_passthrough`).
6. **Every query parameter must be in the matched route's `query_allowlist`.** If not, the call fails
   with `400 invalid_argument` and reason `QUERY_NOT_ALLOWED`. Azure needs `"api-version"` in the list.
7. **Alias rules.** For hostname endpoints the alias is derived (`host`, or `host:port` for a non-standard
   port). An explicit `alias` that differs from the derived one is **rejected**. IP endpoints such as
   `127.0.0.1` **require** an explicit alias (`domain/services/management/alias.rs`).
8. **A mock at `http://127.0.0.1:PORT` needs two config switches.** Set `oagw.config.allow_http_upstream: true`
   (checked at proxy time). Also set `ssrf_policy.enabled: false` or `allow_cidrs: ["127.0.0.1/32"]` (checked at
   create time and again at connect time).
9. **Repositories are in memory.** Upstreams and routes disappear on restart, so you must provision on
   every start. They are tenant-scoped by `ctx.subject_tenant_id()`.
10. **`secret_ref` is validated at `create_upstream`** against credstore under the caller's ctx. If the
    secret is unreadable, you get `CanonicalError::FailedPrecondition`, which is retryable. This check runs
    **before** the alias conflict check.

---

## 1. Getting the client and an S2S context

### 1.1 `ServiceGatewayClientV1` from ClientHub

OAGW registers its facade in `Gear::init` (`oagw/src/gear.rs`):

```rust
// gears/system/oagw/oagw/src/gear.rs
#[toolkit::gear(
    name = "oagw",
    deps = [types_registry, authz_resolver, credstore, tenant_resolver],
    capabilities = [system, rest]
)]
pub struct OutboundApiGatewayGear { ... }
// in init():
let oagw: Arc<dyn ServiceGatewayClientV1> =
    Arc::new(ServiceGatewayClientV1Facade::new(cp.clone(), dp.clone()));
ctx.client_hub().register::<dyn ServiceGatewayClientV1>(oagw.clone());
```

The consumer declares `oagw` in its own `deps = [...]` so that OAGW's `init` runs first. Then:

```rust
use std::sync::Arc;
use oagw_sdk::ServiceGatewayClientV1;          // re-export of oagw_sdk::api::ServiceGatewayClientV1

let gw: Arc<dyn ServiceGatewayClientV1> = ctx.client_hub().get::<dyn ServiceGatewayClientV1>()?;
// libs/toolkit/src/client_hub.rs:182  pub fn get<T>(&self) -> Result<Arc<T>, ClientHubError>
```

Every method takes the `SecurityContext` **by value** (`ctx.clone()` per call).

### 1.2 S2S `SecurityContext` (authn-resolver)

```rust
// gears/system/authn-resolver/authn-resolver-sdk/src/api.rs:69
async fn exchange_client_credentials(&self, request: &ClientCredentialsRequest)
    -> Result<AuthenticationResult, AuthNResolverError>;
// models.rs: pub struct ClientCredentialsRequest { pub client_id: String,
//            pub client_secret: SecretString, pub scopes: Vec<String> }
// AuthenticationResult { pub security_context: SecurityContext, .. }
```

With `static-authn-plugin`, `s2s_credentials[].identity` defaults to `DEFAULT_SUBJECT_ID` /
`DEFAULT_TENANT_ID` (`static-authn-plugin/src/config.rs:99`):
`libs/toolkit-security/src/constants.rs`: `DEFAULT_TENANT_ID = 00000000-df51-5b42-9538-d2b56b7ee953`,
`DEFAULT_SUBJECT_ID = 11111111-6a88-4768-9dfc-6bcd5187d9ed`. The upstreams you create land in
**that tenant** (§2.6). `config/mini-chat.yaml` and `config/e2e-local.yaml` both configure
`client_id: "mini-chat"`, `client_secret: "mini-chat-dev-secret"` on the authn plugin and in `mini-chat.config.client_credentials`.

---

## 2. Upstreams

### 2.1 SDK trait (full surface) — `oagw-sdk/src/api.rs`

```rust
#[async_trait]
pub trait ServiceGatewayClientV1: Send + Sync {
    async fn create_upstream(&self, ctx: SecurityContext, req: CreateUpstreamRequest) -> Result<Upstream, CanonicalError>;
    async fn get_upstream(&self, ctx: SecurityContext, id: Uuid) -> Result<Upstream, CanonicalError>;
    async fn list_upstreams(&self, ctx: SecurityContext, query: &ListQuery) -> Result<Vec<Upstream>, CanonicalError>;
    async fn update_upstream(&self, ctx: SecurityContext, id: Uuid, req: UpdateUpstreamRequest) -> Result<Upstream, CanonicalError>;
    async fn delete_upstream(&self, ctx: SecurityContext, id: Uuid) -> Result<(), CanonicalError>;  // cascades routes
    async fn create_route(&self, ctx: SecurityContext, req: CreateRouteRequest) -> Result<Route, CanonicalError>;
    async fn get_route(&self, ctx: SecurityContext, id: Uuid) -> Result<Route, CanonicalError>;
    async fn list_routes(&self, ctx: SecurityContext, upstream_id: Option<Uuid>, query: &ListQuery) -> Result<Vec<Route>, CanonicalError>;
    async fn update_route(&self, ctx: SecurityContext, id: Uuid, req: UpdateRouteRequest) -> Result<Route, CanonicalError>;
    async fn delete_route(&self, ctx: SecurityContext, id: Uuid) -> Result<(), CanonicalError>;
    /// alias + method + path -> (effective merged Upstream, matched Route); no auth/forwarding.
    async fn resolve_proxy_target(&self, ctx: SecurityContext, alias: &str, method: &str, path: &str)
        -> Result<(Upstream, Route), CanonicalError>;
    async fn proxy_request(&self, ctx: SecurityContext, req: http::Request<Body>)
        -> Result<http::Response<Body>, CanonicalError>;
}
```

There is **no get-by-alias**. To find an existing alias, page through `list_upstreams`. `ListQuery { top, skip }`
defaults to `{ top: 50, skip: 0 }`. The in-memory repo sorts by `id` and does not cap `top`
(`infra/storage/upstream_repo.rs::list`). `resolve_proxy_target(ctx, alias, "POST", "/v1/responses")` is a
cheap startup self-check that the alias and route resolve for a given ctx.

### 2.2 Models — `oagw-sdk/src/models.rs` (no serde; plain structs)

```rust
pub enum Scheme { Http, #[default] Https, Wss, Wt, Grpc }
pub struct Endpoint { pub scheme: Scheme, pub host: String, pub port: u16 }
pub struct Server   { pub endpoints: Vec<Endpoint> }
pub enum SharingMode { #[default] Private, Inherit, Enforce }
pub struct AuthConfig {
    pub plugin_type: String,                         // GTS id of auth plugin
    pub sharing: SharingMode,
    pub config: Option<HashMap<String, String>>,     // flat k/v, plugin specific
}
pub struct HeadersConfig { pub request: Option<RequestHeaderRules>, pub response: Option<ResponseHeaderRules> }
pub struct RequestHeaderRules {
    pub set: HashMap<String,String>, pub add: HashMap<String,String>, pub remove: Vec<String>,
    pub passthrough: PassthroughMode,                // None (default) | Allowlist | All
    pub passthrough_allowlist: Vec<String>,
}
pub struct Upstream {
    pub id: Uuid, pub tenant_id: Uuid, pub alias: String, pub server: Server,
    pub protocol: String, pub enabled: bool, pub auth: Option<AuthConfig>,
    pub headers: Option<HeadersConfig>, pub plugins: Option<PluginsConfig>,
    pub rate_limit: Option<RateLimitConfig>, pub cors: Option<CorsConfig>, pub tags: Vec<String>,
}
// Builder (fields private; getters exist):
CreateUpstreamRequest::builder(server: Server, protocol: impl Into<String>)
    .alias(..).auth(AuthConfig).headers(HeadersConfig).plugins(..).rate_limit(..).cors(..)
    .tags(Vec<String>).enabled(bool /* default true */).build()
UpdateUpstreamRequest::builder(server, protocol)...build()   // PUT semantics: full replacement
```

Constants (`oagw-sdk/src/gts.rs`, re-exported at the crate root). `gts_id!` prepends `gts.`:

```rust
oagw_sdk::HTTP_PROTOCOL_ID      // "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
oagw_sdk::APIKEY_AUTH_PLUGIN_ID // "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
oagw_sdk::UPSTREAM_SCHEMA       // "gts.cf.core.oagw.upstream.v1~"   (resource_type in errors)
oagw_sdk::ROUTE_SCHEMA          // "gts.cf.core.oagw.route.v1~"
oagw_sdk::PROXY_SCHEMA          // "gts.cf.core.oagw.proxy.v1~"
```

The built-in auth plugins are registered under exact GTS ids (`infra/plugin/registry.rs::with_builtins`): apikey,
`...~cf.core.oagw.noop.v1`, `...~cf.core.oagw.oauth2_client_cred*.v1`. `resolve()` is an exact
`HashMap` lookup. An unknown id is **not** rejected at create time. It fails at proxy time with
`401 unauthenticated`, reason `AUTH_PLUGIN_NOT_FOUND`.

### 2.3 Endpoint / scheme / port / `use_http`

* OAGW has **no `use_http` field**. `use_http` is a mini-chat config knob (DESIGN.md B.1: "port default
  443, 80 when `use_http = true`; effective only when OAGW `allow_http_upstream` is on"). Map it to
  `Endpoint { scheme: Scheme::Http, port: cfg.port.unwrap_or(80), .. }`, otherwise `Scheme::Https` with port 443.
* Validation at create (`management/validation.rs::validate_endpoints`):
  * at least one endpoint; IPv6 rejected;
  * all-IP or all-hostname (no mixing);
  * hostnames must be RFC 1123 (labels `[A-Za-z0-9-]`, so **no `:port` inside `host`**);
  * multi-endpoint pools must share scheme and port.
* SSRF check at create (`validate_endpoints_ssrf`): blocked hostnames (`localhost`, `localhost.localdomain`
  plus `extra_deny_hostnames`) and blocked literal IPs return a 400 validation error unless allow-listed or SSRF is disabled.
* **`allow_http_upstream` is NOT checked at create.** An `http` upstream is created fine, and
  proxying then fails with `400 invalid_argument`, field `endpoint.scheme`, reason `HTTP_UPSTREAM_FORBIDDEN`
  (`service.rs` step 6b).
* Upstream URL scheme/port: `request_builder::build_upstream_url` omits the port for https:443 / http:80.
  The Host header is `host`, or `host:port` when the port is not 80/443 (`headers.rs::set_host_header`).
* Connect timeout is fixed at 10 s (`gear.rs`). TLS verification is always on
  (`skip_upstream_tls_verify` is only reachable through the test harness).

### 2.4 Alias rules — `management/alias.rs`

```text
compute_derived_alias(endpoints):
  IP endpoints / empty            -> None   (explicit alias REQUIRED)
  single host (or identical hosts)-> normalized_host            if standard port for scheme
                                     normalized_host:port       otherwise
  multi-host                      -> common registrable suffix (>=2 labels, not a public suffix) [+ :port]
create, alias given:  derived=Some(d): normalize(user)==d ? d : ERR "alias is auto-derived for hostname-based
                                       endpoints; remove the 'alias' field (derived: 'd')"
                      derived=None   : normalize(user) validated and accepted
create, no alias:     derived=Some(d) -> d ; None -> ERR "explicit alias is required for IP-based ..."
normalize_alias = to_ascii_lowercase + trim trailing '.'
validate_alias: 1..=253 chars of [A-Za-z0-9.:_-], not "." / "..", >=1 alphanumeric
```

"Standard port" here is scheme-aware: `Http` uses 80, everything else uses 443
(`domain/model.rs::Endpoint::is_standard_port`). The SDK's `Endpoint::alias_contribution()` treats 80 and 443
as standard for any scheme, so **don't use the SDK helper to predict the alias**. Read `Upstream.alias` from
the create response instead.

Consequences for mini-chat (its DESIGN says `upstream_alias` defaults to `host` and is "always passed"):
* `host = api.openai.com`, https:443, alias `api.openai.com`: OK (equals the derived alias).
* `host = 127.0.0.1`, http:PORT, alias `127.0.0.1` (or any custom alias): OK (IP, explicit alias).
* `host = my.mock`, http:8080, alias `my.mock`: **rejected**. The derived alias is `my.mock:8080`.
  Either omit `alias` or pass the derived value.
* A custom `upstream_alias` that differs from the host on a hostname endpoint: **rejected**.
* Alias lookup at proxy time is case-insensitive (the request alias is normalized).

The alias is immutable. `update_upstream` rejects alias changes, including endpoint changes that would
change the derived alias.

### 2.5 Auth plugin `apikey` — `infra/plugin/apikey_auth.rs`

```rust
#[derive(Deserialize)]
struct ApiKeyConfig {
    header: String,          // e.g. "authorization", "api-key" (inserted lower-cased)
    #[serde(default)] prefix: String,   // e.g. "Bearer " ; "" for Azure api-key
    secret_ref: String,      // "cred://openai-key" or "openai-key"
}
// authenticate():
let raw_ref = config.secret_ref.strip_prefix("cred://").unwrap_or(&config.secret_ref);
let key = SecretRef::new(raw_ref)?;                        // [A-Za-z0-9_-]{1,255}
let response = self.credstore.get(&ctx.security_context, &key).await  // ctx = proxy caller's ctx
    .map_err(|e| PluginError::Internal(..))?
    .ok_or_else(|| PluginError::SecretNotFound(config.secret_ref.clone()))?;
ctx.headers.insert(config.header.to_lowercase(), format!("{}{}", config.prefix, secret_str));
```

* **Both `cred://openai-key` and `openai-key` are accepted**. The prefix is stripped in the plugin and
  also in create-time validation (`management/bind.rs::validate_secret_ref_accessible`). Anything else
  with `/`, `:` or `.` fails `SecretRef::new`.
* Secret lookup always goes through `CredStoreClientV1::get(ctx, key)` with the ctx passed to
  `proxy_request` (the `AuthContext.security_context` is `ctx.clone()`, `service.rs::execute_auth_plugin`).
  At create time the ctx is the one passed to `create_upstream`.
* Proxy-time failure mapping (`execute_auth_plugin`):
  * `SecretNotFound` becomes `DomainError::SecretNotFound`, i.e. **500 internal**;
  * a credstore error becomes `PluginError::Internal`, i.e. `401 unauthenticated` with reason `AUTH_PLUGIN_INTERNAL`;
  * an unknown plugin id becomes 401 with reason `AUTH_PLUGIN_NOT_FOUND`.

Create-time `secret_ref` check (`bind.rs::validate_secret_ref_accessible`). It runs only when
`auth.config["secret_ref"]` is present:

| credstore result | `create_upstream` error |
|---|---|
| `Ok(Some(_))` | passes |
| `Ok(None)` or `Err(e) if e.is_not_found()` | `CanonicalError::FailedPrecondition`, resource_type `UPSTREAM_SCHEMA`, violation subject `auth.config.secret_ref`, type `STATE` (HTTP 400). **Retryable**: "not provisioned yet, or not shared" |
| `Err(CredStoreError::Internal)` | `CanonicalError::Internal` (500) |
| invalid ref / other | `InvalidArgument` (400, Format) |

`mini-chat` DESIGN §"OAGW provisioning" turns the `FailedPrecondition` case into "deferred provider,
retried by background reconcile: 2 s, then doubling up to 60 s, with one warning after 2 min".

### 2.6 Tenant scoping and "already exists"

* `create_upstream` stores `tenant_id = ctx.subject_tenant_id()` (`management/mod.rs`).
  `get/list/update/delete` are filtered by the same tenant. `get_upstream` for another tenant's id returns NotFound.
* Alias uniqueness is per `(tenant_id, alias)` (`InMemoryUpstreamRepo::create`, atomic `DashMap` entry).
  A duplicate returns `RepositoryError::Conflict{entity:"upstream", resource: alias}`, which becomes
  `CanonicalError::AlreadyExists` with `resource_type() == Some(UPSTREAM_SCHEMA)`, `resource_name() == Some(alias)` and
  detail `"alias '<a>' already exists for tenant"`. That is HTTP 409 (`api/rest/error.rs`, `Conflict` arm).
* Order inside `create_upstream`:
  1. validate endpoints
  2. SSRF
  3. cors/budget
  4. alias rules
  5. tenant chain (`tenant_resolver.get_ancestors`)
  6. ancestor bind check
  7. **`secret_ref` check**
  8. budget allocation
  9. repo insert (**conflict**)

  So an unreadable secret masks AlreadyExists, and a bad alias or endpoint fails before either.
* If an **ancestor** tenant already owns the alias, the create is a "bind". It needs authz
  `bind` / `override_auth` / ... permissions on the ancestor upstream (`bind.rs::validate_bind_constraints`).
  An ancestor with `Enforce` sharing blocks auth or rate overrides.
* Proxy-time resolution walks `[caller_tenant, parent, ..., root]` (`management/resolution.rs::resolve_alias`).
  The caller tenant's own upstream always wins. An **ancestor's upstream is visible to a descendant only
  if at least one of `auth / rate_limit / plugins / cors` has `sharing != Private`**
  (`merge.rs::is_visible_to_descendant`). A root upstream with `AuthConfig{sharing: Private}` is invisible to
  child tenants, which then get 404. A disabled upstream returns 503 (`UpstreamDisabled`).

**Which ctx to proxy with.** If you proxy with the same S2S ctx you provisioned with, resolution and secret
lookup are simple, because everything is in the S2S tenant. If you proxy with the end-user ctx:
* the provider upstream (in the S2S/root tenant) must be visible to descendants, e.g. `auth.sharing = Inherit` or `Enforce`;
* the credstore secret must be readable by the **user's** tenant and subject (credstore `Shared` from
  an ancestor, or a `Tenant` secret in the user's own tenant; see §7).

In single-tenant dev (`single-tenant-tr-plugin`) every user is in `DEFAULT_TENANT_ID`. A `Private`
credstore secret is still bound to one `owner_id` subject in that case.

### 2.7 Create-or-reuse snippet

```rust
use std::collections::HashMap;
use oagw_sdk::{AuthConfig, CreateUpstreamRequest, Endpoint, ListQuery, Scheme, Server, SharingMode,
               Upstream, ServiceGatewayClientV1, HTTP_PROTOCOL_ID, UPSTREAM_SCHEMA};
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;

pub enum ProvisionError { Deferred(CanonicalError) /* secret not readable yet */, Fatal(CanonicalError) }

pub async fn ensure_upstream(
    gw: &dyn ServiceGatewayClientV1, ctx: &SecurityContext,
    host: &str, port: u16, use_http: bool, alias: Option<&str>,
    auth_plugin_type: &str, auth_config: HashMap<String, String>,
) -> Result<Upstream, ProvisionError> {
    let server = Server { endpoints: vec![Endpoint {
        scheme: if use_http { Scheme::Http } else { Scheme::Https }, host: host.to_owned(), port }] };
    let mut b = CreateUpstreamRequest::builder(server, HTTP_PROTOCOL_ID)
        .auth(AuthConfig { plugin_type: auth_plugin_type.to_owned(),
                           sharing: SharingMode::Inherit, // visible to descendants; Private = own tenant only
                           config: Some(auth_config) })
        .tags(vec!["mini-chat".into()]);
    if let Some(a) = alias { b = b.alias(a); }
    match gw.create_upstream(ctx.clone(), b.build()).await {
        Ok(u) => Ok(u),                                    // u.alias is the authoritative alias
        Err(e @ CanonicalError::AlreadyExists { .. })
            if e.resource_type() == Some(UPSTREAM_SCHEMA) => {
            let wanted = e.resource_name().unwrap_or_default().to_owned(); // normalized alias
            let mut skip = 0;
            loop {
                let page = gw.list_upstreams(ctx.clone(), &ListQuery { top: 100, skip })
                    .await.map_err(ProvisionError::Fatal)?;
                if let Some(u) = page.iter().find(|u| u.alias == wanted) { return Ok(u.clone()); }
                if page.len() < 100 { return Err(ProvisionError::Fatal(e)); }
                skip += 100;
            }
            // NOTE: reused upstream keeps its old endpoints/auth; call update_upstream (full
            // replacement, same alias) if config drift matters.
        }
        Err(e @ CanonicalError::FailedPrecondition { .. }) => Err(ProvisionError::Deferred(e)),
        Err(e) => Err(ProvisionError::Fatal(e)),           // InvalidArgument (alias/endpoint/SSRF), ...
    }
}
```

You can also project errors to `oagw_sdk::ServiceGatewayError::from(err)`. That gives
`AlreadyExists { resource: oagw_sdk::gts::Resource::Upstream, name }`, `FailedPrecondition {..}`,
`Validation { field, reason, detail }`, `NotFound { resource, name }`, `Timeout`, `Unavailable {..}`, and so on
(`oagw-sdk/src/error.rs`).

---

## 3. Routes

### 3.1 Model

```rust
pub enum HttpMethod { Get, Post, Put, Delete, Patch }         // no HEAD/OPTIONS
pub enum PathSuffixMode { Disabled, #[default] Append }
pub struct HttpMatch {
    pub methods: Vec<HttpMethod>,
    pub path: String,                  // prefix; plain string, NOT validated (keep it "/..." with no '?')
    pub query_allowlist: Vec<String>,  // empty = no query params allowed
    pub path_suffix_mode: PathSuffixMode,
}
pub struct MatchRules { pub http: Option<HttpMatch>, pub grpc: Option<GrpcMatch> } // exactly one
pub struct Route { pub id, pub tenant_id, pub upstream_id, pub match_rules, pub plugins, pub rate_limit,
                   pub cors, pub tags, pub priority: i32, pub enabled: bool }
CreateRouteRequest::builder(upstream_id: Uuid, match_rules: MatchRules)
    .priority(i32 /*0*/).enabled(bool /*true*/).tags(..).plugins(..).rate_limit(..).cors(..).build()
```

`create_route` checks (`management/mod.rs`, `validation.rs`):
* the upstream must exist **in the caller's tenant**, otherwise a 400 validation error
  (`"upstream '<id>' not found for this tenant"`);
* exactly one of http/grpc;
* **overlap**: another enabled route on the same upstream with the identical `path`, the same `priority`
  and any shared method returns `CanonicalError::AlreadyExists` with `resource_type() == Some(ROUTE_SCHEMA)`,
  resource `"{upstream_id}:{path}:{Method:?}"` (409). Reuse it: `list_routes(ctx, Some(upstream_id), ..)`
  and match on `path`, or treat this error as success.

### 3.2 Matching — `infra/storage/route_repo.rs::find_matching` + `resolution.rs`

Proxy URI `/{alias}{path_suffix}?q`:
1. `alias` = first segment of the raw path. `path_suffix` = the rest, normalized (`//` collapsed, `.`/`..`
   resolved, `service.rs::normalize_path`).
2. Resolve the upstream by alias across the tenant chain (§2.6).
3. Route candidates are routes attached to the selected upstream id, searched across the tenant chain
   (closest tenant first). If none match, OAGW falls back to routes on ancestor upstreams in the merge chain.
   A candidate must be `enabled`, have an `http` match, include the method, and satisfy
   **`path_suffix.starts_with(route.path)`**. This is a raw string prefix with no segment boundary:
   `/v1/files` also matches `/v1/files_x`.
4. Winner: longest `path`, then highest `priority`.
5. No route: `CanonicalError::NotFound`, resource_type `ROUTE_SCHEMA`, resource_name the nil UUID (404).
   No alias: `NotFound` with `UPSTREAM_SCHEMA` (404). Neither case contacts the upstream.

After the match (`service.rs` steps 3/3b):
* every query key must be in `query_allowlist`, otherwise `400`, field `query`, reason `QUERY_NOT_ALLOWED`;
* `Disabled` suffix mode with extra path beyond `route.path` gives `400`, reason `PATH_SUFFIX_NOT_ALLOWED`.

Outbound URL (`request_builder::build_upstream_url`):
`{scheme}://{host}[:{port}]{route.path}{path_suffix minus route.path}?{query re-encoded via form_urlencoded}`.
The upstream path therefore equals the proxied path. The route's `path` is a filter, not a rewrite.

### 3.3 Suggested route set per provider (matches DESIGN.md §3.5 table)

| Purpose | `path` | methods | `query_allowlist` |
|---|---|---|---|
| OpenAI chat | `/v1/responses` | `[Post]` | `[]` |
| Azure chat (`api_path` `/openai/v1/responses?api-version=...`) | `/openai/v1/responses` (path part only) | `[Post]` | `["api-version"]` |
| OpenAI files | `/v1/files` | `[Post, Delete]` (DELETE `/v1/files/{id}` matches via Append) | `[]` |
| OpenAI vector stores | `/v1/vector_stores` | `[Post, Get, Delete]` | plus any list params you send (`limit`, `after`, `order`, `filter`) |
| Azure files / vector stores | `/openai/files`, `/openai/vector_stores` | same | `["api-version", ...]` |

If `api_path` contains `{model}`, use the prefix before the placeholder as `path` with `Append` mode.

```rust
use oagw_sdk::{CreateRouteRequest, HttpMatch, HttpMethod, MatchRules, PathSuffixMode, Route, ROUTE_SCHEMA};

async fn ensure_route(gw: &dyn ServiceGatewayClientV1, ctx: &SecurityContext, upstream_id: uuid::Uuid,
                      path: &str, methods: Vec<HttpMethod>, query: &[&str]) -> Result<(), CanonicalError> {
    let rules = MatchRules { grpc: None, http: Some(HttpMatch {
        methods, path: path.to_owned(),
        query_allowlist: query.iter().map(|s| (*s).to_owned()).collect(),
        path_suffix_mode: PathSuffixMode::Append }) };
    match gw.create_route(ctx.clone(), CreateRouteRequest::builder(upstream_id, rules).build()).await {
        Ok(_) => Ok(()),
        Err(e @ CanonicalError::AlreadyExists { .. }) if e.resource_type() == Some(ROUTE_SCHEMA) => Ok(()),
        Err(e) => Err(e),
    }
}
```

---

## 4. `proxy_request`

### 4.1 Pipeline (`infra/proxy/service.rs::DataPlaneServiceImpl::proxy_request`)

1. `parse_proxy_request` extracts alias, suffix and query, then:
   * validates `Content-Type` (single header, must parse as MIME, otherwise 400 `DUPLICATE_HEADER` / `INVALID_MIME_TYPE`);
   * runs anti-smuggling checks on CL/TE;
   * checks size: `Body::Bytes` larger than `max_body_size_bytes` (default 100 MiB) gives 413. A `Body::Stream`
     is checked while it is forwarded.
2. AuthZ: `policy_enforcer.access_scope_with(ctx, PROXY resource, "invoke", ..)`. A denial is `PermissionDenied` (403).
3. `cp.resolve_proxy_target(ctx, alias, method, path_suffix)` (§3.2).
4. CORS (only if `Origin` is sent and the upstream has cors). Then the query allowlist and suffix-mode checks.
5. Outbound headers: `apply_passthrough` (mode from `upstream.headers.request.passthrough`, default **None**,
   so only `Content-Type` is kept) → strip hop-by-hop → strip `x-oagw-*`. `authorization`, `cookie`,
   `proxy-authorization` and `set-cookie` are always removed.
6. Auth plugin (§2.5), guard plugins, header rules `set/add/remove`, transform plugins.
7. Endpoint selection:
   * `X-OAGW-Target-Host` header if present;
   * the only endpoint for single-endpoint upstreams;
   * otherwise round robin.

   **HTTPS-only check:** if `!allow_http_upstream && scheme == Http`, the call fails with
   400 `HTTP_UPSTREAM_FORBIDDEN`. Then the Host header is set.
8. Rate limits (only if configured). Build the URL.
9. Bridge into Pingora over an in-memory duplex:
   * `Body::Bytes`/`Empty` are written with `Content-Length`;
   * `Body::Stream` is written with `Transfer-Encoding: chunked` (`session_bridge.rs::serialize_request_wire`), always `Connection: close`.
10. `tokio::time::timeout(request_timeout, parse_response_stream(..))` waits for the **response status line
    and headers**. On expiry: `DomainError::RequestTimeout`, i.e. **`Err(CanonicalError::DeadlineExceeded)`, HTTP 504**.
11. Response guards/transforms/CORS/header rules. For `text/event-stream` responses, the body is wrapped
    with idle timeout and shutdown handling (`session_bridge::streaming_body_with_lifecycle`).
12. `build_proxy_response`:
    * reads `x-oagw-error-source` into `resp.extensions()` as an `oagw_sdk::api::ErrorSource`;
    * strips hop-by-hop and `x-oagw-*` headers (including `transfer-encoding`; the body is already de-chunked);
    * body = `Body::Stream`.

### 4.2 Request types

* `http::Request<oagw_sdk::Body>`. `Body` is `{ Empty, Bytes(Bytes), Stream(BodyStream) }`, where
  `BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, BoxError>> + Send>>` (`oagw-sdk/src/body.rs`).
  `From` is implemented for `()`, `Bytes` (empty becomes `Empty`), `Vec<u8>`, `String`, `&'static str` and `BodyStream`.
* The URI is relative: `format!("/{alias}{path}")`, plus `?api-version=...` for Azure. The method is a normal `http::Method`.
* Streaming *request* bodies are supported (`Body::Stream`). They are sent chunked. If a stream item errors,
  OAGW aborts and returns `Err(ServiceUnavailable)` with an HTTP 502 override. If the total exceeds the max, it returns 413.

### 4.3 Response handling

```rust
use oagw_sdk::{api::ErrorSource, Body};
let resp = gw.proxy_request(ctx.clone(), req).await?;          // Err = gateway pipeline error
let status = resp.status();
let source = resp.extensions().get::<ErrorSource>().copied();  // Gateway | Upstream
let bytes  = resp.into_body().into_bytes().await?;             // buffers the stream
// streaming: resp.into_body().into_stream()  -> BodyStream
```

SSE helpers (`oagw-sdk/src/sse/*`, default features):

```rust
use futures::StreamExt;
use oagw_sdk::sse::{ServerEvent, ServerEventsResponse, ServerEventsStream};

match ServerEventsStream::from_response::<ServerEvent>(resp) {      // checks Content-Type: text/event-stream*
    ServerEventsResponse::Events(mut events) => {
        // events.status(), events.headers() available
        while let Some(ev) = events.next().await {                 // Result<ServerEvent, StreamingError>
            let ev = ev?;                                            // ev.event: Option<String>, ev.data: String
            if ev.data == "[DONE]" { break; }                        // use ServerEvent (not Json<T>) if sentinels occur
            let v: serde_json::Value = ev.json()?;
        }
        // Stream end without a terminal provider event (e.g. response.completed) = truncated:
        // OAGW ends the stream silently (None) on streaming_idle_timeout_secs or shutdown.
    }
    ServerEventsResponse::Response(resp) => { /* non-SSE: usually a provider 4xx/5xx JSON or problem+json */ }
}
```

`Json<T>` (`oagw_sdk::Json`) implements `FromServerEvent` for any `DeserializeOwned` type. It fails on
non-JSON data such as `[DONE]`. Also check the status before parsing: a 200 with SSE is the happy path.

### 4.4 Snippets for each mini-chat call

```rust
use http::{Method, Request, header};
use oagw_sdk::{Body, MultipartBody, Part};

// (1) streaming Responses API (OpenAI); Azure: format!("/{alias}/openai/v1/responses?api-version={ver}")
let req = Request::builder().method(Method::POST)
    .uri(format!("/{alias}/v1/responses"))
    .header(header::CONTENT_TYPE, "application/json")
    .header(header::ACCEPT, "text/event-stream")   // dropped unless passthrough allowlists "accept"
    .body(Body::from(serde_json::to_vec(&json!({"model": m, "input": i, "stream": true}))?))?;

// (2) non-streaming JSON (thread summary): same URI, "stream": false, then into_bytes() + serde_json.

// (3) multipart upload (Files API)
let req = MultipartBody::new()                    // random UUID boundary; with_boundary(..) validates RFC 2046
    .text("purpose", "assistants")
    .part(Part::bytes("file", file_bytes).filename(name).content_type(mime))
    // or Part::stream("file", body_stream) -> whole body becomes Body::Stream (chunked upload)
    .into_request(Method::POST, format!("/{alias}/v1/files"))?;   // sets Content-Type: multipart/form-data; boundary=..
// Azure: format!("/{alias}/openai/files?api-version={ver}")

// (4) delete file
let req = Request::builder().method(Method::DELETE)
    .uri(format!("/{alias}/v1/files/{file_id}")).body(Body::Empty)?;

// (5) vector stores
Request::builder().method(Method::POST).uri(format!("/{alias}/v1/vector_stores"))
    .header(header::CONTENT_TYPE, "application/json").body(Body::from(json_bytes))?;
Request::builder().method(Method::POST).uri(format!("/{alias}/v1/vector_stores/{vs}/files")) ...;
Request::builder().method(Method::GET).uri(format!("/{alias}/v1/vector_stores/{vs}/files/{fid}")).body(Body::Empty)?;
Request::builder().method(Method::DELETE).uri(format!("/{alias}/v1/vector_stores/{vs}")).body(Body::Empty)?;
```

Multipart notes:
* All-buffered parts produce `Body::Bytes` with `Content-Length`. Any streaming part produces `Body::Stream`, sent upstream as
  **chunked**. Some providers reject chunked multipart; buffering also gives deterministic retries.
* OAGW never rewrites the multipart body. `Content-Type` is always forwarded.

Headers you need upstream (`OpenAI-Beta`, `Accept`, `x-ms-client-request-id`, ...) have two routes:
* put them on the upstream: `HeadersConfig { request: Some(RequestHeaderRules { passthrough: PassthroughMode::Allowlist,
  passthrough_allowlist: vec!["accept".into(), ...], ..Default::default() }), response: None }`;
* or set constants: `RequestHeaderRules.set`.

### 4.5 Timeouts (config → code)

| Config (`oagw.config`) | Default | Where | Effect |
|---|---|---|---|
| `proxy_timeout_secs` | 30 | `gear.rs`: `DataPlaneServiceImpl::with_request_timeout` **and** Pingora `peer.options.read_timeout` | (a) max wait for response headers → `Err(DeadlineExceeded)` 504; (b) **per-read** upstream socket timeout, which also applies *between SSE chunks*: a gap longer than this aborts the stream (Pingora `ReadTimedout`) |
| (fixed) connect | 10 s | `gear.rs` `connect_timeout` | `ConnectTimedout` gives a gateway 504 response |
| `streaming_idle_timeout_secs` | 300 | SSE responses only | silent end of the body stream after no data for N s |
| `max_body_size_bytes` | 100 MiB | request body | 413 |
| `allow_http_upstream` | false | step 6b | 400 `HTTP_UPSTREAM_FORBIDDEN` |
| `management_api_enabled` | true | REST only | does **not** affect the in-process client |

`config/mini-chat.yaml` sets `proxy_timeout_secs: 60`. `config/e2e-local.yaml` sets **`proxy_timeout_secs: 2`**,
so mock LLM upstreams in e2e must send headers within 2 s and keep inter-chunk gaps under 2 s.

### 4.6 Error matrix (what the caller sees)

| Situation | Result | Canonical / HTTP |
|---|---|---|
| alias not found (or invisible) | `Err` | `NotFound` (UPSTREAM_SCHEMA) 404 |
| upstream disabled | `Err` | `ServiceUnavailable` 503 (retry-after 30) |
| no route for method+path | `Err` | `NotFound` (ROUTE_SCHEMA) 404 |
| query not allowlisted / suffix disabled / bad content-type | `Err` | `InvalidArgument` 400 (`QUERY_NOT_ALLOWED`, `PATH_SUFFIX_NOT_ALLOWED`, `INVALID_MIME_TYPE`) |
| http endpoint without `allow_http_upstream` | `Err` | `InvalidArgument` 400 `HTTP_UPSTREAM_FORBIDDEN` |
| secret missing at proxy time | `Err` | `Internal` 500 |
| auth plugin unknown / credstore error | `Err` | `Unauthenticated` 401 (`AUTH_PLUGIN_NOT_FOUND` / `AUTH_PLUGIN_INTERNAL`) |
| authz denies `invoke` | `Err` | `PermissionDenied` 403 |
| OAGW rate limit (if configured) | `Err` | `ResourceExhausted` 429 |
| body too large | `Err` | `OutOfRange` with 413 override |
| no response headers within `proxy_timeout_secs` | `Err` | `DeadlineExceeded` 504 (`proxy_upstream_timeout_returns_504` test) |
| bridge/IO failure, streaming body abort | `Err` | `ServiceUnavailable` with 502 override |
| connect refused / closed / TLS failure | usually `Ok(resp)` | 502, `application/problem+json`, `ErrorSource::Gateway` |
| no route to host / DNS / SSRF-filtered IP (`ConnectError`) | usually `Ok(resp)` | 503 `ErrorSource::Gateway` |
| connect timeout / upstream read timeout | usually `Ok(resp)` | 504 `ErrorSource::Gateway` |
| upstream returns 4xx/5xx (incl. 429 + `Retry-After`) | `Ok(resp)` | provider status/body/headers verbatim, `ErrorSource::Upstream` |

The "usually" rows come from the Pingora `fail_to_proxy` mapping (`pingora_proxy.rs:777-880`). The OAGW tests
accept both `Err(ServiceUnavailable | DeadlineExceeded)` and `Ok(502|503|504)` for an unreachable backend
(`tests/proxy_integration.rs::proxy_all_backends_unreachable`, `proxy_unreachable_backend_returns_rfc9457_problem_body`).
Robust handling looks like this:

```rust
match gw.proxy_request(ctx, req).await {
    Err(CanonicalError::DeadlineExceeded { .. }) => ProviderError::Timeout,
    Err(e) => ProviderError::Gateway(e),                             // e.status_code()/Problem::from(e)
    Ok(r) if r.extensions().get::<ErrorSource>() == Some(&ErrorSource::Gateway) && !r.status().is_success()
        => ProviderError::GatewayHttp(r.status()),                     // 502/503/504 problem+json body
    Ok(r) if !r.status().is_success() => ProviderError::Upstream(r.status(), r.into_body().into_bytes().await?),
    Ok(r) => /* success */,
}
```

The gateway problem body is RFC 9457: `type` (GTS URI), `title`, `status`, `detail`, `instance`.

---

## 5. SSRF policy and HTTP upstreams (mock at `http://127.0.0.1:PORT`)

`OagwConfig.ssrf_policy: SsrfPolicy` (`domain/ssrf.rs`, `#[serde(deny_unknown_fields)]`):

```yaml
ssrf_policy:
  enabled: true              # default true; false disables all checks
  extra_deny_cidrs: []       # e.g. ["10.96.0.0/12"]
  allow_cidrs: []            # exemptions, evaluated FIRST (e.g. ["127.0.0.1/32"])
  extra_deny_hostnames: []   # ".suffix" = suffix match, else exact
  allow_hostnames: []
```

The built-in deny list includes `127.0.0.0/8`, RFC 1918, `169.254/16`, `0/8`, `100.64/10`, TEST-NETs, multicast, `240/4`,
IPv6 loopback/ULA/link-local and others. Built-in denied hostnames are `localhost` and `localhost.localdomain`.
The checks run in three places:
* create/update: hostname and literal-IP check (`validate_endpoints_ssrf`), 400 validation error;
* connect time: DNS-resolved addresses are filtered (`pingora_proxy.rs::upstream_peer`, and
  `DnsDiscovery` for multi-endpoint upstreams). Everything filtered gives a gateway 503 `LinkUnavailable` response.

`OagwConfig` (`oagw/src/config.rs`, `deny_unknown_fields`) contains `proxy_timeout_secs`, `max_body_size_bytes`,
`allow_http_upstream`, `token_cache_ttl_secs`, `token_cache_capacity`, `websocket_idle_timeout_secs`,
`websocket_close_timeout_secs`, `streaming_idle_timeout_secs`, `protocol_cache_ttl_secs`,
`management_api_enabled`, `ssrf_policy` and `metrics.prefix`. Under `--features fips`,
`allow_http_upstream: true` fails `validate()` at startup.

Working local/e2e config (mirrors `config/e2e-local.yaml:536`):

```yaml
gears:
  oagw:
    config:
      proxy_timeout_secs: 30
      allow_http_upstream: true        # needed for scheme http (checked at proxy time)
      ssrf_policy:
        enabled: false                 # or: allow_cidrs: ["127.0.0.1/32"]
```

Note: `config/mini-chat.yaml` disables SSRF but does **not** set `allow_http_upstream`. A
`use_http: true` provider then fails every proxy call with `HTTP_UPSTREAM_FORBIDDEN`, although create succeeds.
The mock upstream must be created with `host: "127.0.0.1"` (an IP), an explicit alias, and `Scheme::Http`
on the mock port. `localhost` is blocked by name whenever SSRF is enabled.

---

## 6. Existing usage and test shapes

### 6.1 In-repo consumers

`grep ServiceGatewayClientV1|oagw_sdk` outside `gears/system/oagw` finds only `Cargo.toml` and
`gears/mini-chat/mini-chat/Cargo.toml`. The mini-chat source is a stub (`src/lib.rs`). **No other gear calls
OAGW yet**, so the OAGW crate's own tests are the reference:

* `oagw/tests/proxy_integration.rs`: real facade via `oagw::test_support::AppHarness` (`h.facade()` is a
  `&dyn ServiceGatewayClientV1`, `h.security_context()`, `h.mock_port()`; feature `test-utils`).
* `oagw/tests/e2e_smoke_test.rs`: REST management + proxy round trips (JSON, SSE).
* `oagw-sdk/tests/usage.rs`, `third_party_in_process.rs`: mock `ServiceGatewayClientV1` implementations, plus SSE,
  multipart (`multipart_proxy_buffered`, `multipart_proxy_streaming`) and WebSocket client patterns. The URIs there are fake.

SDK-typed example (`proxy_integration.rs::proxy_upstream_timeout_returns_504`, abridged):

```rust
let upstream = h.facade().create_upstream(ctx.clone(),
    CreateUpstreamRequest::builder(
        Server { endpoints: vec![Endpoint { scheme: Scheme::Http, host: "127.0.0.1".into(), port: h.mock_port() }] },
        HTTP_PROTOCOL_ID).alias("timeout-upstream").build()).await.unwrap();
h.facade().create_route(ctx.clone(), CreateRouteRequest::builder(upstream.id, MatchRules {
    http: Some(HttpMatch { methods: vec![HttpMethod::Get], path: guard.path("/timeout"),
                           query_allowlist: vec![], path_suffix_mode: PathSuffixMode::Disabled }),
    grpc: None }).build()).await.unwrap();
let req = http::Request::builder().method(Method::GET)
    .uri(format!("/timeout-upstream{}", guard.path("/timeout"))).body(Body::Empty).unwrap();
match h.facade().proxy_request(ctx.clone(), req).await {
    Err(err) => assert!(matches!(err, CanonicalError::DeadlineExceeded { .. })), Ok(_) => panic!() }
```

Round trip with read-back (`proxy_chat_completion_round_trip`):
`.uri("/mock-upstream/v1/chat/completions")`, then `response.into_body().into_bytes().await` and `serde_json::from_slice`.
Upstream 500 passthrough (`proxy_upstream_500_passthrough`): `Ok`, status 500, `ErrorSource::Upstream`.

### 6.2 REST JSON shapes (`api/rest/dto.rs`; useful for e2e via `/cf/oagw/v1/...`)

`POST /oagw/v1/upstreams` (from `proxy_integration.rs::setup_openai_mock`):

```json
{
  "server": { "endpoints": [ { "host": "127.0.0.1", "port": 18080, "scheme": "http" } ] },
  "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
  "alias": "mock-upstream",
  "enabled": true,
  "tags": [],
  "auth": {
    "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
    "sharing": "private",
    "config": { "header": "authorization", "prefix": "Bearer ", "secret_ref": "cred://openai-key" }
  }
}
```

The auth key is `"type"` (`#[serde(rename = "type")] plugin_type`). Endpoint defaults are `scheme: "https"`, `port: 443`.
Enums are snake_case. Response 201 has `id`, `alias`, ...

`POST /oagw/v1/routes`. The match key is **`"match"`**. Methods are UPPERCASE:

```json
{ "upstream_id": "<uuid>",
  "match": { "http": { "methods": ["POST", "GET"], "path": "/v1/chat/completions",
                        "query_allowlist": ["api-version"], "path_suffix_mode": "append" } },
  "enabled": true, "tags": [], "priority": 0 }
```

REST proxy: `ANY /oagw/v1/proxy/{alias}/{path}` (behind api-gateway, e.g. `/cf/oagw/v1/proxy/...`).

`testing/e2e/suites/` is empty in this checkout. `testing/e2e/README.md` describes an `oagw` suite with features
`[oagw, credstore, static-tenants, static-authn, static-authz]`. `testing/e2e/helpers/mock_server.py` is a GET-only
static file server, so mini-chat e2e needs its own SSE/multipart-capable mock.

### 6.3 Alternative: declarative provisioning

OAGW `post_init` also materializes upstream/route **GTS instances registered in types-registry** by other gears
(`infra/type_provisioning.rs`, `gear.rs::post_init`). It does so under each instance's `tenant_id` or the root tenant, with
`DEFAULT_SUBJECT_ID`, and a provisioning error is fatal. mini-chat's DESIGN instead calls `create_upstream`/`create_route` at gear `start`,
which allows reuse and deferral.

---

## 7. Credstore: static plugin config and resolution

### 7.1 `static-credstore-plugin` config (`gears/credstore/plugins/static-credstore-plugin/src/config.rs`)

```rust
pub struct StaticCredStorePluginConfig { pub vendor: String /*"constructorfabric"*/, pub priority: i16 /*100*/,
                                         #[expand_vars] pub secrets: Vec<SecretConfig> }
pub struct SecretConfig { pub tenant_id: Option<Uuid>, pub owner_id: Option<Uuid>, pub key: String,
                          #[expand_vars] pub value: String /* ${ENV} expanded */, pub sharing: Option<SharingMode> }
// resolve_sharing(): explicit, else (None,_)->Shared, (Some,None)->Tenant, (Some,Some)->Private
```

`Service::from_config` rejects:
* nil UUIDs;
* `owner_id` without `tenant_id`;
* `owner_id` with non-Private sharing, or Private without `owner_id`;
* duplicate keys within a class;
* `key` failing `SecretRef` (`[A-Za-z0-9_-]{1,255}`, so **no `cred://` in `key`**).

Storage classes:
* private `(tenant, owner, key)`;
* tenant `(tenant, key)`;
* shared `(tenant, key)`;
* global `key` (`tenant_id` omitted).

Examples:

```yaml
static-credstore-plugin:
  config:
    secrets:
      - tenant_id: "00000000-df51-5b42-9538-d2b56b7ee953"   # Tenant class
        key: "openai-key"
        value: "${OPENAI_API_KEY}"
      - tenant_id: "00000000-df51-5b42-9538-d2b56b7ee953"   # Private class (e2e-local.yaml style)
        owner_id:  "11111111-6a88-4768-9dfc-6bcd5187d9ed"
        key: "openai-key"
        value: "sk-test-e2e-fake-key"
```

The credstore gear selects its backend plugin by `credstore.config.vendor`, which must match the plugin's vendor.

### 7.2 How `CredStoreClientV1::get(ctx, key)` resolves (`credstore/src/domain/secret/service.rs:567`)

1. `req = ctx.subject_tenant_id()`, `subject = ctx.subject_id()`, `chain = [req, parent, ..., root]`.
2. **Metadata row lookup in the credstore gear's own DB** (`repo_impl/reads.rs::resolve_for_get`).
   The row must be `Active`, not expired, and have `tenant_id ∈ chain`. Visibility:
   * Private: `tenant_id == req && owner_id == subject`;
   * Tenant: `tenant_id == req`;
   * Shared: any tenant in the chain (the only inheritance path).

   No row means `Ok(None)`.
3. A PDP `read` check on the secret type, and the scope must include `req`. A denial looks like a miss (`Ok(None)`).
4. Plugin `get(ctx, row.tenant_id, key, owner)` (owner only for Private) → value. A fingerprint mismatch also returns `Ok(None)`.

**Caveat (issue #5094, `config/quickstart.yaml:446`):** values seeded only in `static-credstore-plugin.config`
have **no metadata row**, so `get` returns `None`. In that state:
* OAGW `create_upstream` returns `FailedPrecondition`;
* proxy-time auth returns 500 `SecretNotFound`.

Workarounds:
* create the secret through credstore: REST `POST /cf/credstore/v1/secrets` with
  `{"reference":"openai-key","value":"...","sharing":"tenant"}`, then `PUT .../secrets/{ref}` with `If-Match: *`;
* or call `CredStoreClientV1::create(ctx, &SecretRef::new("openai-key")?, value, SharingMode::Tenant)` from a test or bootstrap step.

mini-chat must treat `FailedPrecondition` from `create_upstream` as "deferred, retry later"
(DESIGN.md §OAGW provisioning).

Pick credstore sharing to match the ctx used for proxying:
* S2S ctx only: `Tenant` (or `Private` with the S2S subject as `owner_id`) in the S2S tenant;
* end-user ctx in child tenants: `Shared` in an ancestor;
* end-user ctx in the same single tenant: `Tenant`. `Private` would be readable only by that one owner subject.

---

## 8. Startup checklist for mini-chat

1. `deps` includes `oagw` and `authn_resolver`. Get `Arc<dyn ServiceGatewayClientV1>` from ClientHub in `init`.
2. In `start`: get the S2S ctx via `exchange_client_credentials`. For each provider entry and tenant override:
   * call `ensure_upstream` (alias: `127.0.0.1`-style IP hosts need an explicit alias; for hostnames pass none
     or the exact derived value);
   * call `ensure_route` for the chat path and the RAG prefixes, with `query_allowlist` (`api-version` for Azure);
   * on `FailedPrecondition`, retry later; on `InvalidArgument`, fail startup.
3. Store `upstream.alias` (from OAGW) as the routing key and build URIs as `/{alias}{path}`.
4. Optional self-check: `resolve_proxy_target(ctx, alias, "POST", chat_path)`.
5. Proxy with a ctx whose tenant can see the upstream and read the secret (§2.6, §7.2).
6. Handle the `Err` / `Ok(gateway 5xx)` / `Ok(upstream non-2xx)` split, and expect no retries from OAGW.
