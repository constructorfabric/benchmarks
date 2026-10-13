# OAGW / AuthN / AuthZ / Security / License research for `mini-chat`

Read-only research of `/app`. Line numbers are from the current tree. Unless a path is absolute, it is relative to `/app`.

Crates and imports (from `/app/Cargo.toml` and `gears/mini-chat/mini-chat/Cargo.toml`, which already depends on all of these):

| crate (package)                          | lib name              | Cargo key in mini-chat |
|------------------------------------------|-----------------------|------------------------|
| cf-gears-oagw-sdk                        | `oagw_sdk`            | `oagw-sdk = { workspace = true }` (no `axum` feature) |
| cf-gears-authn-resolver-sdk              | `authn_resolver_sdk`  | path dep, already present |
| cf-gears-authz-resolver-sdk              | `authz_resolver_sdk`  | `authz-resolver-sdk = { workspace = true }` |
| cf-gears-toolkit-security                | `toolkit_security`    | `toolkit-security = { workspace = true }` |
| cf-gears-toolkit-canonical-errors        | `toolkit_canonical_errors` | present (with `axum`) |
| `secrecy`                                 | `secrecy`             | present (needed for `SecretString`) |

`gears/mini-chat/mini-chat/src/lib.rs` is currently only a stub with empty modules.

---------------------------------------------------------------------------------------------------

## 1. OAGW SDK (`gears/system/oagw/oagw-sdk`)

### 1.1 Trait `ServiceGatewayClientV1` (`oagw-sdk/src/api.rs:62-157`)

```rust
use oagw_sdk::api::ServiceGatewayClientV1;   // also re-exported as oagw_sdk::ServiceGatewayClientV1
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;

#[async_trait]
pub trait ServiceGatewayClientV1: Send + Sync {
    async fn create_upstream(&self, ctx: SecurityContext, req: CreateUpstreamRequest) -> Result<Upstream, CanonicalError>;
    async fn get_upstream(&self, ctx: SecurityContext, id: Uuid) -> Result<Upstream, CanonicalError>;
    async fn list_upstreams(&self, ctx: SecurityContext, query: &ListQuery) -> Result<Vec<Upstream>, CanonicalError>;
    async fn update_upstream(&self, ctx: SecurityContext, id: Uuid, req: UpdateUpstreamRequest) -> Result<Upstream, CanonicalError>;
    async fn delete_upstream(&self, ctx: SecurityContext, id: Uuid) -> Result<(), CanonicalError>;   // cascades route deletion

    async fn create_route(&self, ctx: SecurityContext, req: CreateRouteRequest) -> Result<Route, CanonicalError>;
    async fn get_route(&self, ctx: SecurityContext, id: Uuid) -> Result<Route, CanonicalError>;
    async fn list_routes(&self, ctx: SecurityContext, upstream_id: Option<Uuid>, query: &ListQuery) -> Result<Vec<Route>, CanonicalError>;
    async fn update_route(&self, ctx: SecurityContext, id: Uuid, req: UpdateRouteRequest) -> Result<Route, CanonicalError>;
    async fn delete_route(&self, ctx: SecurityContext, id: Uuid) -> Result<(), CanonicalError>;

    /// resolve alias+method+path -> (effective Upstream, Route); no auth/forwarding. Useful for startup validation.
    async fn resolve_proxy_target(&self, ctx: SecurityContext, alias: &str, method: &str, path: &str) -> Result<(Upstream, Route), CanonicalError>;

    /// Full pipeline: resolve -> authz -> auth plugin -> rate-limit -> forward -> respond.
    /// URI MUST be `/{alias}/{path_suffix}?query`.
    async fn proxy_request(&self, ctx: SecurityContext, req: http::Request<Body>) -> Result<http::Response<Body>, CanonicalError>;
}
```

- The context is passed **by value** (`SecurityContext`, which is `Clone`).
- There is no get-by-alias call. To find an existing upstream, call `list_upstreams` and filter on `.alias`. List is scoped to `ctx.subject_tenant_id()`. Default `ListQuery` is `{top: 50, skip: 0}` (`models.rs:349-360`).
- Implementation: `ServiceGatewayClientV1Facade` (`oagw/src/domain/services/client.rs:28-180`). OAGW `init()` registers it in ClientHub (`oagw/src/gear.rs:159-164`):
  `ctx.client_hub().register::<dyn ServiceGatewayClientV1>(oagw.clone());`
  The consumer gets it with `ctx.client_hub().get::<dyn oagw_sdk::ServiceGatewayClientV1>()?` and must declare `deps = [..., oagw]` in `#[toolkit::gear(...)]`.
- **Storage is in-memory** (`InMemoryUpstreamRepo`/`InMemoryRouteRepo`, `oagw/src/gear.rs:80-81`). Every process start begins empty, so the gear must re-provision at each start.

### 1.2 Model types (`oagw-sdk/src/models.rs`; all re-exported at the crate root, `lib.rs:15-23`)

No serde derives are present; these are plain structs.

```rust
pub enum Scheme { Http, #[default] Https, Wss, Wt, Grpc }                 // :29-37
pub struct Endpoint { pub scheme: Scheme, pub host: String, pub port: u16 } // :40-45
pub struct Server { pub endpoints: Vec<Endpoint> }                        // :65-68
pub enum SharingMode { #[default] Private, Inherit, Enforce }              // :16-22
pub struct AuthConfig {                                                   // :75-82
    pub plugin_type: String,                 // GTS id of the auth plugin
    pub sharing: SharingMode,
    pub config: Option<HashMap<String, String>>,   // flat k/v
}
pub struct HeadersConfig { pub request: Option<RequestHeaderRules>, pub response: Option<ResponseHeaderRules> } // :89-93
pub struct RequestHeaderRules {                                           // :96-108
    pub set: HashMap<String,String>, pub add: HashMap<String,String>, pub remove: Vec<String>,
    pub passthrough: PassthroughMode,          // None (default) | Allowlist | All
    pub passthrough_allowlist: Vec<String>,
}
pub enum HttpMethod { Get, Post, Put, Delete, Patch }                     // :264-270 (no HEAD/OPTIONS)
pub enum PathSuffixMode { Disabled, #[default] Append }                    // :274-278
pub struct HttpMatch {                                                     // :282-290
    pub methods: Vec<HttpMethod>,      // at least one
    pub path: String,                  // prefix, must start with '/'
    pub query_allowlist: Vec<String>,  // EMPTY = allow NO query params
    pub path_suffix_mode: PathSuffixMode,
}
pub struct MatchRules { pub http: Option<HttpMatch>, pub grpc: Option<GrpcMatch> } // exactly one must be Some
pub struct Upstream { pub id, pub tenant_id, pub alias: String, pub server, pub protocol: String, pub enabled: bool,
                      pub auth: Option<AuthConfig>, pub headers: Option<HeadersConfig>, pub plugins, pub rate_limit, pub cors, pub tags: Vec<String> } // :327-341
pub struct Route { pub id, pub tenant_id, pub upstream_id, pub match_rules, pub plugins, pub rate_limit, pub cors, pub tags, pub priority: i32, pub enabled: bool } // :312-323
```

Builders have private fields, so construct requests through them:
- `CreateUpstreamRequest::builder(server: Server, protocol: impl Into<String>)` → `.alias() .auth() .headers() .plugins() .rate_limit() .cors() .tags(Vec<String>) .enabled(bool)` → `.build()` (`models.rs:381-490`). The default for `enabled` is `true`.
- `UpdateUpstreamRequest::builder(server, protocol)`: same setters, with PUT/full-replacement semantics, so `None` clears a field (`:508-617`).
- `CreateRouteRequest::builder(upstream_id: Uuid, match_rules: MatchRules)` → `.plugins() .rate_limit() .cors() .tags() .priority(i32) .enabled(bool)` → `.build()` (`:636-725`).
- `UpdateRouteRequest::builder(match_rules)` (`:740-823`).

GTS constants (`oagw-sdk/src/gts.rs:27-53`, re-exported at the root):
```rust
oagw_sdk::HTTP_PROTOCOL_ID      // "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
oagw_sdk::APIKEY_AUTH_PLUGIN_ID // "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
oagw_sdk::UPSTREAM_SCHEMA / ROUTE_SCHEMA / PROXY_SCHEMA   // resource_type strings on NotFound/AlreadyExists
```

API key auth plugin config (`oagw/src/infra/plugin/apikey_auth.rs:9-19, 34-64`) uses three keys:
- `header`: the header name. It is lower-cased before insert.
- `prefix`: optional, default `""`.
- `secret_ref`: the credstore key. A `cred://` prefix is optional and is stripped.

At **proxy time** the plugin calls `credstore.get(&ctx.security_context, &SecretRef::new(raw))` with the **ctx passed to proxy_request** and sets `header: prefix+secret`. If the secret is missing, the result is `PluginError::SecretNotFound` → `DomainError::SecretNotFound` → canonical `Internal` (500).

At **create/update time** the control plane also validates that `secret_ref` is readable with the caller ctx (`management/mod.rs:127-133` → `bind.rs:156-200`):
- If the secret does not resolve, or is not shared, you get `DomainError::SecretRefNotAccessible` → canonical **`FailedPrecondition`** (HTTP 400, precondition subject `auth.config.secret_ref`, type `"STATE"`). This case is retryable (`api/rest/error.rs:238-242`).
- If credstore itself is down, you get `Internal`.
- A malformed ref gives `InvalidArgument`.

`static-credstore-plugin` secrets with no `tenant_id` are global and readable by any tenant (`gears/credstore/plugins/static-credstore-plugin/src/config.rs:36-80`). That is the shape used in `config/mini-chat.yaml:128-132` (`key: "azure-openai-key"`).

### 1.3 Copy-paste: provisioning an upstream + route (pattern from `oagw/tests/proxy_integration.rs:128-185` and `tests/proxy_request_rejections.rs:142-178`)

```rust
use std::collections::HashMap;
use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HttpMatch, HttpMethod, ListQuery,
    MatchRules, PathSuffixMode, Scheme, Server, ServiceGatewayClientV1, ServiceGatewayError, SharingMode,
    APIKEY_AUTH_PLUGIN_ID, HTTP_PROTOCOL_ID,
};
use oagw_sdk::gts::Resource;
use toolkit_canonical_errors::CanonicalError;

async fn ensure_upstream(
    gw: &dyn ServiceGatewayClientV1,
    s2s: &toolkit_security::SecurityContext,
    scheme: Scheme, host: &str, port: u16, alias: &str,
    header: &str, prefix: &str, secret_ref: &str,
) -> Result<oagw_sdk::Upstream, CanonicalError> {
    let req = CreateUpstreamRequest::builder(
        Server { endpoints: vec![Endpoint { scheme, host: host.to_owned(), port }] },
        HTTP_PROTOCOL_ID,
    )
    .alias(alias)                       // see alias rules in 1.5!
    .auth(AuthConfig {
        plugin_type: APIKEY_AUTH_PLUGIN_ID.to_owned(), // == "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
        sharing: SharingMode::Private,   // Inherit => visible to descendant tenants (see 2.3)
        config: Some(HashMap::from([
            ("header".to_owned(), header.to_owned()),      // e.g. "api-key" or "authorization"
            ("prefix".to_owned(), prefix.to_owned()),      // e.g. "" or "Bearer "
            ("secret_ref".to_owned(), secret_ref.to_owned()), // e.g. "azure-openai-key" or "cred://openai-key"
        ])),
    })
    .tags(vec!["mini-chat".to_owned()])
    .build();

    match gw.create_upstream(s2s.clone(), req).await {
        Ok(u) => Ok(u),
        Err(e @ CanonicalError::AlreadyExists { .. }) => {
            // alias taken in this tenant -> reuse
            let all = gw.list_upstreams(s2s.clone(), &ListQuery { top: 1000, skip: 0 }).await?;
            all.into_iter().find(|u| u.alias == alias.to_ascii_lowercase()).ok_or(e)
        }
        Err(e) => Err(e),
    }
}

async fn ensure_route(gw: &dyn ServiceGatewayClientV1, s2s: &toolkit_security::SecurityContext,
                      upstream_id: uuid::Uuid, api_path: &str) -> Result<(), CanonicalError> {
    let rules = MatchRules {
        http: Some(HttpMatch {
            methods: vec![HttpMethod::Post],
            path: api_path.to_owned(),                  // e.g. "/openai/v1/responses"
            query_allowlist: vec!["api-version".to_owned()], // REQUIRED if you send ?api-version=
            path_suffix_mode: PathSuffixMode::Append,
        }),
        grpc: None,
    };
    match gw.create_route(s2s.clone(), CreateRouteRequest::builder(upstream_id, rules).build()).await {
        Ok(_) | Err(CanonicalError::AlreadyExists { .. }) => Ok(()),   // route overlap => already provisioned
        Err(e) => Err(e),
    }
}

// Typed dispatch alternative:
// match ServiceGatewayError::from(err) {
//     ServiceGatewayError::AlreadyExists { resource: Resource::Upstream, name } => ..., // name = alias
//     ServiceGatewayError::AlreadyExists { resource: Resource::Route, .. } => ...,
//     ServiceGatewayError::FailedPrecondition { .. } => /* secret_ref not readable yet -> retry later */,
//     ServiceGatewayError::Validation { field, reason, detail } => /* deterministic misconfig -> fail startup */,
//     _ => ...
// }
```

### 1.4 What `create_upstream` checks (`oagw/src/domain/services/management/mod.rs:84-157`), in order

1. `validate_endpoints` (`validation.rs:31+`): at least one endpoint, no IPv6, no mixing IPs and hostnames, all endpoints on one scheme.
2. `validate_endpoints_ssrf`: skipped when `ssrf_policy.enabled=false` (`validation.rs:96-122`).
3. CORS and budget checks.
4. Alias enforcement (see 1.5).
5. Tenant chain lookup via tenant-resolver (`get_ancestors`). This fails for a nil tenant.
6. Ancestor bind validation.
7. `secret_ref` accessibility check through credstore, using the ctx.
8. Budget allocation.
9. `repo.create`.

**Duplicate alias**: `InMemoryUpstreamRepo::create` returns `RepositoryError::Conflict{entity:"upstream", resource: alias, detail:"alias '<a>' already exists for tenant"}` (`infra/storage/upstream_repo.rs:35-50`). The path to the caller is then:
- `DomainError::Conflict` → `OagwUpstreamError::already_exists(detail).with_resource(alias)` (`api/rest/error.rs:154-168`).
- The caller sees **`CanonicalError::AlreadyExists`** (HTTP 409) with `resource_type == oagw_sdk::UPSTREAM_SCHEMA` and `resource_name == alias`.
- Through `ServiceGatewayError::from` this becomes `AlreadyExists { resource: Resource::Upstream, name: alias }` (`oagw-sdk/src/error.rs:273-276`).

Uniqueness is per `(tenant_id, alias)`.

**Duplicate route**: `check_route_overlap` (`validation.rs:176-237`) rejects an enabled route on the same upstream with the same `path` + `priority` + an overlapping method. The result is `DomainError::conflict("route", ...)` → `CanonicalError::AlreadyExists` with `ROUTE_SCHEMA`. If `create_route` names an upstream that is not found for the tenant, you get a Validation (InvalidArgument) error, not NotFound (`mod.rs:339-349`).

### 1.5 Alias rules (important) (`oagw/src/domain/services/management/alias.rs:8-196`)

- **Hostname endpoints**: the alias is AUTO-DERIVED. With the standard port (80/443) it is `host`; with any other port it is `host:port`. If you pass `.alias(x)` and `x` differs from the derived value, you get Validation `"alias is auto-derived for hostname-based endpoints; remove the 'alias' field (derived: '...')"` (`:154-171`).
  - So a custom `upstream_alias` that differs from the host is rejected for hostname upstreams.
  - `alias = host` works only when the port is 443/80. For example, `localhost:8080` derives `localhost:8080`, not `localhost`.
- **IP endpoints** (e.g. mock at 127.0.0.1): an explicit alias is REQUIRED (`:184-196`, `"explicit alias is required for IP-based ..."`). Any valid alias is accepted.
- The alias is normalized to lower case with trailing dots stripped. The charset is `[A-Za-z0-9.:_-]`, max 253 characters, and it must contain at least one alphanumeric.
- `Upstream.alias` returned by OAGW is the effective (normalized or derived) alias. Proxy lookups are case-insensitive (`resolution.rs:59-60`).

### 1.6 `proxy_request`: building requests

The URI is `/{alias}/{rest}`. The first path segment is the alias; the rest is normalized (`//`, `.`, `..` collapsed). See `oagw/src/infra/proxy/service.rs:361-385`. In-process callers do **not** use `/oagw/v1/proxy`; that prefix belongs only to the REST surface (`api/rest/routes/mod.rs:79`).

```rust
use bytes::Bytes;
use oagw_sdk::Body;

let uri = format!("/{alias}{api_path}?api-version={api_version}");   // e.g. "/my.openai.azure.com/openai/v1/responses?api-version=2025-03-01-preview"
let req = http::Request::builder()
    .method(http::Method::POST)
    .uri(uri)
    .header(http::header::CONTENT_TYPE, "application/json")
    .header(http::header::ACCEPT, "text/event-stream")   // NOTE: dropped unless passthrough allows it (see headers)
    .body(Body::from(serde_json::to_vec(&payload)?))     // Body::Bytes; Body::Empty for GET; Body::Stream(BodyStream) for streaming upload
    .expect("valid request");
let resp: http::Response<Body> = gw.proxy_request(ctx.clone(), req).await?;   // Err = gateway-side failure
```

`Body` (`oagw-sdk/src/body.rs:27-148`):
- Variants: `enum Body { Empty, Bytes(Bytes), Stream(BodyStream) }`, where `type BodyStream = Pin<Box<dyn Stream<Item=Result<Bytes, BoxError>> + Send>>`.
- `From` impls exist for `()`, `Bytes`, `Vec<u8>`, `String`, `&'static str` and `BodyStream`. Empty bytes become `Empty`.
- Methods: `into_bytes().await`, `into_stream()`, `try_into_bytes()`, `try_into_stream()`.

**Multipart** (`oagw-sdk/src/multipart.rs`):
```rust
use oagw_sdk::{MultipartBody, Part};
let req = MultipartBody::new()
    .text("purpose", "assistants")
    .part(Part::bytes("file", file_bytes).filename("a.pdf").content_type("application/pdf"))  // or Part::stream(name, BodyStream)
    .into_request(http::Method::POST, format!("/{alias}/openai/v1/files"))?;   // sets Content-Type multipart/form-data; boundary=...
```
If every part is buffered you get `Body::Bytes`; if any part streams, the result is `Body::Stream`, sent with chunked encoding (`service.rs:1018-1145`).

**Header passthrough** (`infra/proxy/headers.rs:13-56`, `service.rs:625-639`):
- The default `PassthroughMode::None` forwards NO inbound headers except `Content-Type`.
- `authorization`, `cookie`, `proxy-authorization` and `set-cookie` are ALWAYS stripped, even in `All` mode.
- Hop-by-hop and internal `x-oagw-*` headers are stripped.
- To forward e.g. `accept`, set the upstream `headers: HeadersConfig{ request: Some(RequestHeaderRules{ passthrough: PassthroughMode::Allowlist, passthrough_allowlist: vec!["accept".into()], ..Default::default() }), response: None }`, or use `set` for fixed headers.
- `Host` is set to the endpoint.
- The auth plugin header is injected after passthrough.

### 1.7 Response handling, SSE streaming

- A successful pipeline returns `Ok(http::Response<Body>)` with **`Body::Stream`** (for every response, not only SSE) (`service.rs:1741-1761`).
- **The upstream HTTP status is passed through**: 4xx/5xx from the LLM arrive as `Ok(resp)` with that status. `resp.extensions().get::<oagw_sdk::api::ErrorSource>() == Some(&ErrorSource::Upstream)` (see test `tests/proxy_integration.rs:301-321`).
- Gateway-generated responses from the Pingora layer also arrive as `Ok(resp)`, carrying `ErrorSource::Gateway` and an `application/problem+json` body. Mapping (`infra/proxy/pingora_proxy.rs:777-878`):

  | Pingora error | Status | Canonical type |
  |---|---|---|
  | connect refused / closed, TLS failure | 502 | service_unavailable |
  | connect error / no route | 503 | service_unavailable |
  | connect timeout or upstream read/write timeout | 504 | deadline_exceeded |

  So check `ErrorSource` **and** the status.
- SSE: if the response is `text/event-stream`, the body is wrapped with an idle timeout (`streaming_idle_timeout_secs`, default 300s) plus graceful shutdown (`service.rs:240-249`).

```rust
use futures::StreamExt;
use oagw_sdk::sse::{ServerEvent, ServerEventsResponse, ServerEventsStream};
use oagw_sdk::api::ErrorSource;

let source = resp.extensions().get::<ErrorSource>().copied().unwrap_or(ErrorSource::Upstream);
if !resp.status().is_success() {
    let status = resp.status();
    let body = resp.into_body().into_bytes().await.map_err(|e| anyhow::anyhow!(e))?;  // problem+json if Gateway
    // map (status, source, body) to your provider error
}
match ServerEventsStream::from_response::<ServerEvent>(resp) {
    ServerEventsResponse::Events(mut events) => {
        while let Some(ev) = events.next().await {
            let ev = ev?;                          // Err = oagw_sdk::StreamingError (ServerEventsParse / Stream(BoxError))
            if ev.data == "[DONE]" { break; }      // OpenAI chat sentinel
            // ev.event: Option<String> (Responses API uses `event: response.output_text.delta` etc.)
            // let v: serde_json::Value = ev.json()?;
        }
    }
    ServerEventsResponse::Response(resp) => { /* not SSE: buffered JSON */ }
}
// Raw bytes alternative: resp.into_body().into_stream()  -> BodyStream of Bytes chunks
```

Notes:
- `ServerEvent { id, event, data, retry }` (`sse/event.rs`).
- `ServerEventsStream::<T>::from_response::<Json<T>>` gives typed JSON (`codec.rs`).
- `into_response()` needs the SDK `axum` feature, which mini-chat does NOT enable.
- See the examples in `oagw-sdk/tests/usage.rs:336-378` (OpenAI `[DONE]`) and `:638-670`.

### 1.8 Errors from `proxy_request` (`Err(CanonicalError)`) (`api/rest/error.rs:127-330`, `domain/error.rs:303-329`)

| Situation | CanonicalError (HTTP) | `ServiceGatewayError::from` |
|---|---|---|
| alias not found / no matching route | `NotFound` (404), resource_type UPSTREAM/ROUTE, name = nil uuid | `NotFound` |
| upstream disabled | `ServiceUnavailable` (503, Retry-After) | `Unavailable` |
| query param not in allowlist | `InvalidArgument` field `query`, reason `QUERY_NOT_ALLOWED` (400) | `Validation{field:"query",reason:"QUERY_NOT_ALLOWED"}` |
| path suffix while mode Disabled | `InvalidArgument` reason `PATH_SUFFIX_NOT_ALLOWED` | `Validation` |
| HTTP endpoint and `allow_http_upstream=false` | `InvalidArgument` field `endpoint.scheme`, reason `HTTP_UPSTREAM_FORBIDDEN` (400) | `Validation` |
| auth plugin secret missing at proxy time | `Internal` (500) | `Internal` |
| auth plugin internal error | `Unauthenticated` reason `AUTH_PLUGIN_INTERNAL` (401) | `AuthFailed` |
| PDP deny (`proxy` resource, action `invoke`) | `PermissionDenied` reason `AUTHZ_DENIED` (403) | `PermissionDenied` |
| PDP failure / compile failure | `Internal` | `Internal` |
| rate limited | `ResourceExhausted` (429) | `RateLimited{retry_after_secs}` |
| request body > max_body_size (100MB default) | `OutOfRange` (413) | `PayloadTooLarge` |
| **timeout waiting for upstream response headers** (`proxy_timeout_secs`; mini-chat.yaml uses 60) | `DeadlineExceeded` (**504**, gts `gts.cf.core.errors.err.v1~cf.core.err.deadline_exceeded.v1~`) | `Timeout` |
| proxy bridge broken / streaming request body failed | `ServiceUnavailable` with 502 override | `Unavailable` |

The timeout comes from `tokio::time::timeout(self.request_timeout, parse_response_stream(..))` around the header read (`service.rs:1123-1129, 1157-1166`). Pingora's `read_timeout` (also `proxy_timeout_secs`) yields an `Ok(resp)` 504 with `ErrorSource::Gateway` if it fires inside Pingora. Test: `tests/proxy_integration.rs:743-814` asserts `CanonicalError::DeadlineExceeded`.

### 1.9 `allow_http_upstream` and SSRF (`oagw/src/config.rs:8-68`, `gear.rs:72-157`)

- **`allow_http_upstream`** (default `false`): `Scheme::Http` endpoints are **created fine** but rejected at **proxy time** (`service.rs:750-758`) with `HTTP_UPSTREAM_FORBIDDEN`.
  - `config/mini-chat.yaml:430-434` sets only `proxy_timeout_secs: 60` and `ssrf_policy.enabled: false`. It does **not** set `allow_http_upstream`, so plain-HTTP mock providers will fail there.
  - `config/e2e-local.yaml:537-541` sets `allow_http_upstream: true`, `proxy_timeout_secs: 2` and SSRF disabled.
  - The flag is rejected under the `fips` feature.
- **`ssrf_policy`** (`domain/ssrf.rs:20-37`): fields are `enabled` (default true), `extra_deny_cidrs`, `allow_cidrs`, `extra_deny_hostnames`, `allow_hostnames`.
  - With `enabled:false`, the create/update validation (`validation.rs:96-102`) and the data-plane/endpoint-selector IP checks are all skipped, so 127.0.0.1 and localhost upstreams are allowed.
  - With SSRF enabled, loopback and private IPs are rejected at create time with a Validation error.
- Other config keys: `max_body_size_bytes` (100MB), `streaming_idle_timeout_secs` (300), `management_api_enabled` (true).

### 1.10 Security context for OAGW calls

- **Control plane** (create/list/get/update/delete): everything is scoped to `ctx.subject_tenant_id()`.
  - The upstream's `tenant_id` is the ctx tenant.
  - There is NO PDP check on create (`management/mod.rs`); only the ancestor-bind path uses the enforcer.
  - The tenant-resolver must know the tenant: `get_ancestors(ctx, tenant)`. With `single-tenant-tr-plugin`, any non-nil ctx tenant is treated as a root with no ancestors (`single-tenant-tr-plugin/src/domain/client.rs:113-134`). A nil tenant fails.
- **Data plane**:
  - Runs `policy_enforcer.access_scope_with(&ctx, &PROXY(resource gts.cf.core.oagw.proxy.v1~, props [owner_tenant_id]), "invoke", None, AccessRequest::new().require_constraints(false).context_tenant_id(ctx.subject_tenant_id()))` (`service.rs:546-556`). static-authz denies only the nil tenant.
  - Then resolves the alias walking `[ctx tenant, ancestors...]`.
  - Then credstore is read with the same ctx in the API key plugin.
- **S2S ctx vs user ctx**: OAGW does not distinguish them; it only uses `subject_tenant_id` (and the subject for rate-limit `User` scope).
  - **An upstream created under the S2S ctx (tenant = DEFAULT_TENANT_ID `00000000-df51-5b42-9538-d2b56b7ee953`) is visible only to proxy calls whose ctx tenant is the same, or a descendant when the upstream is shared.**
  - "Shared" means at least one of `auth/rate_limit/plugins/cors` has `sharing != Private` (`management/merge.rs:14-35`, `resolution.rs:66-83`).
  - With `single-tenant-tr-plugin` there are no ancestors, so a user in another tenant gets `NotFound`.
  - Inference: proxying with the **same S2S ctx** used for provisioning is the robust choice. The mini-chat DESIGN (`gears/mini-chat/docs/DESIGN.md:411, 604, 1404, 1429`) says provisioning runs under the S2S ctx, with "created, or reused when OAGW reports it already exists".
- **OAGW's own provisioning-from-config precedent** (`oagw/src/gear.rs:241-299`, `post_init`) builds contexts like this:
  ```rust
  let ctx = SecurityContext::builder()
      .subject_tenant_id(tenant_id)
      .subject_id(toolkit_security::constants::DEFAULT_SUBJECT_ID)
      .build()?;
  ```

### 1.11 Existing code that provisions upstreams programmatically

No other gear calls `create_upstream` outside OAGW (grep over gears/libs/apps). The examples are:
- `oagw/src/gear.rs:258-299`: OAGW `post_init` materializes upstreams/routes from types-registry via the internal `cp.create_upstream`.
- `oagw/tests/proxy_integration.rs`:
  - `:128-199`: apikey auth with `header=authorization`, `prefix="Bearer "`, `secret_ref="cred://openai-key"`. Proxy via `h.facade().proxy_request(ctx, req)` with URI `"/auth-hdr-test/v1/chat/completions"`.
  - `:209-299`: SSE streaming with a `[DONE]` body.
  - `:347-378`: disabled upstream → `ServiceUnavailable`.
  - `:743-814`: timeout → `DeadlineExceeded`.
- `oagw/tests/proxy_request_rejections.rs:142-178`: Http upstream at `127.0.0.1:mock_port` with alias `"ws-no-upgrade"`.
- Test harness: `oagw/src/test_support/harness.rs` (`AppHarness::builder().with_credentials(vec![("cred://openai-key".into(), "sk-test123".into())]).build()`, `.facade()`, `.security_context()`, `.mock_port()`). These are `oagw::test_support`, a dev dependency on the `oagw` crate.

---------------------------------------------------------------------------------------------------

## 2. Alias resolution, path and query forwarding

Proxy pipeline order (`oagw/src/infra/proxy/service.rs:524-887`):
1. Parse.
2. PDP `invoke`.
3. `cp.resolve_proxy_target(ctx, alias, method, path_suffix)`.
4. CORS.
5. **Query allowlist check**.
6. `path_suffix_mode` check.
7. Header passthrough.
8. Auth plugin.
9. Guards.
10. Header rules.
11. Transforms.
12. Endpoint selection.
13. HTTP scheme check.
14. Rate limit.
15. Build URL.
16. Pingora.

Steps 1–3 in detail:
1. **Alias extraction**: the first segment of `req.uri().path()`; `path_suffix` is the rest, normalized (`service.rs:369-381`).
2. **Upstream resolution** (`management/resolution.rs:51-166`): walk the tenant chain `[ctx tenant, parent, ..., root]`. The first enabled upstream with that alias wins. Ancestor upstreams count only if shared (non-Private sharing on some section). If only a disabled one exists you get 503, otherwise 404.
3. **Route matching** (`infra/storage/route_repo.rs:98-160`): same tenant, enabled, method in `methods`, and `path.starts_with(route.path)` (a raw string prefix, so `/v1` also matches `/v1x`). The longest prefix wins, then priority. If no route exists for the selected upstream, ancestor upstream routes are searched (`resolution.rs:102-142`).

The upstream URL is built by `request_builder::build_upstream_url(endpoint, route_path, remaining_suffix, query_params)` (`infra/proxy/request_builder.rs:10-58`):
- The URL is `{scheme}://{host[:port]}{route.path}{suffix after route.path}`. The port is omitted for 443/80.
- `Append` mode forwards the remainder of the path. `Disabled` mode errors if there is anything beyond `route.path`.
- Query params are re-encoded with `form_urlencoded`.
- **Query params must be allowlisted**: any inbound query key not in `HttpMatch.query_allowlist` gives 400 `QUERY_NOT_ALLOWED` (`service.rs:591-610`). An empty allowlist means no query params are allowed. **So `?api-version=...` (Azure) requires `query_allowlist: vec!["api-version".into()]` on the route.**

Example: an upstream with alias `my.openai.azure.com` (https:443) and a route `POST /openai/v1/responses` (Append, allowlist `["api-version"]`). The request `POST /my.openai.azure.com/openai/v1/responses?api-version=2025-03-01-preview` goes to `https://my.openai.azure.com/openai/v1/responses?api-version=2025-03-01-preview`. A route with path `/openai/v1` would also cover `/openai/v1/files/...` through Append.

---------------------------------------------------------------------------------------------------

## 3. AuthN resolver (`gears/system/authn-resolver/authn-resolver-sdk`)

The trait is in `authn-resolver-sdk/src/api.rs:29-73`, models in `models.rs:12-50`, errors in `error.rs:6-28`:
```rust
use authn_resolver_sdk::{AuthNResolverClient, AuthNResolverError, AuthenticationResult, ClientCredentialsRequest};

#[async_trait]
pub trait AuthNResolverClient: Send + Sync {
    async fn authenticate(&self, bearer_token: &str) -> Result<AuthenticationResult, AuthNResolverError>;
    async fn exchange_client_credentials(&self, request: &ClientCredentialsRequest) -> Result<AuthenticationResult, AuthNResolverError>;
}
pub struct AuthenticationResult { pub security_context: SecurityContext }
pub struct ClientCredentialsRequest { pub client_id: String, pub client_secret: secrecy::SecretString, pub scopes: Vec<String> }
pub enum AuthNResolverError { Unauthorized(String), NoPluginAvailable, ServiceUnavailable(String), TokenAcquisitionFailed(String), Internal(String) }
```

Copy-paste S2S exchange:
```rust
let authn = ctx.client_hub().get::<dyn authn_resolver_sdk::AuthNResolverClient>()?;   // gear deps: authn_resolver
let s2s: toolkit_security::SecurityContext = authn
    .exchange_client_credentials(&authn_resolver_sdk::ClientCredentialsRequest {
        client_id: cfg.client_credentials.client_id.clone(),
        client_secret: secrecy::SecretString::from(cfg.client_credentials.client_secret.clone()),
        scopes: vec![],
    })
    .await
    .map_err(|e| anyhow::anyhow!("S2S client_credentials exchange failed: {e}"))?
    .security_context;
```

- The resolver finds its plugin lazily through types-registry (`authn-resolver/src/domain/service.rs`, "resolved on first API call after types-registry is ready"). If the plugin client is not registered yet you get `PluginUnavailable` → `ServiceUnavailable`.
- So call it from the gear's start/serve lifecycle or `post_init`, not from `init()`, or retry. The DESIGN puts it in `start`.

**static-authn-plugin** (`plugins/static-authn-plugin/src/config.rs:10-126`, `domain/service.rs:102-162`). Config keys: `vendor`, `priority`, `mode` (`accept_all` (default) | `static_tokens`), `default_identity`, `tokens`, `s2s_credentials`.
- `IdentityConfig { subject_id, subject_tenant_id, token_scopes, subject_type: Option<String> }`. The default is `DEFAULT_SUBJECT_ID` / `DEFAULT_TENANT_ID` / `["*"]` / `None`.
- `accept_all`: ANY non-empty bearer token maps to `default_identity`. Every token gives the same subject and tenant, with `bearer_token` set to the token. An empty token gives `Unauthorized`.
- `static_tokens`: `tokens: [{token, identity:{subject_id, subject_tenant_id, token_scopes, subject_type}}]`. An unknown token gives `Unauthorized("invalid token")`.
- `s2s_credentials: [{client_id, client_secret, identity?, bearer_token?}]`. The secret must match exactly. When `identity` is omitted you get the default identity: **subject `11111111-6a88-4768-9dfc-6bcd5187d9ed`, tenant `00000000-df51-5b42-9538-d2b56b7ee953`, scopes `["*"]`, no subject_type, no bearer_token**. A mismatch gives `TokenAcquisitionFailed("invalid client credentials")`.
- `config/mini-chat.yaml:94-101` and `config/e2e-local.yaml:349-351` both have `client_id: "mini-chat"`, `client_secret: "mini-chat-dev-secret"` with no identity, so the default identity applies.

**api-gateway with `auth_disabled: true`** (`gears/system/api-gateway/src/gear.rs:464-484`) builds one context for every request:
```rust
SecurityContext::builder().subject_id(DEFAULT_SUBJECT_ID).subject_tenant_id(DEFAULT_TENANT_ID).build()?
```
It is inserted as a request extension; the `Authorization` header is ignored entirely. **The context has `token_scopes` EMPTY** (so `has_scope(..)` is false), no `subject_type`, no bearer. Constants are in `libs/toolkit-security/src/constants.rs:12,21`:
- `DEFAULT_TENANT_ID = 00000000-df51-5b42-9538-d2b56b7ee953`
- `DEFAULT_SUBJECT_ID = 11111111-6a88-4768-9dfc-6bcd5187d9ed`

**Acting as different users/tenants in tests/e2e** requires `auth_disabled: false` plus static-authn `mode: static_tokens`:
- `config/e2e-local.yaml:283-348` has:
  - `e2e-token-tenant-a`: subject 1111..., tenant `00000000-df51-...`.
  - `e2e-token-tenant-a-reviewer`: subject `44444444-...`, same tenant.
  - `e2e-token-tenant-b`: subject `22222222-6a88-...`, tenant `bbbbbbbb-bbbb-...`.
  - hierarchy tokens.
  - `e2e-token-nil-tenant`: nil tenant, denied by static-authz.
- The e2e default header is `Authorization: Bearer ${E2E_AUTH_TOKEN:-e2e-token-tenant-a}` (`testing/e2e/conftest.py:17-29`).
- In `accept_all` mode every bearer token maps to the same default identity (subject 1111.../tenant 00000000-df51...). You cannot impersonate different users that way.
- In Rust unit tests, just build contexts directly with `SecurityContext::builder()`.

---------------------------------------------------------------------------------------------------

## 4. AuthZ resolver / PolicyEnforcer (`gears/system/authz-resolver/authz-resolver-sdk`)

The enforcer is in `authz-resolver-sdk/src/pep/enforcer.rs`. Re-exports are in `lib.rs:66-67`: `use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer, AccessRequest, EnforcerError, ResourceType, IntoPropertyValue};`, also available under `authz_resolver_sdk::pep::*`.

Construction:
- `PolicyEnforcer::new(authz: Arc<dyn AuthZResolverApi>)` (`:314`) takes `ctx.client_hub().get::<dyn AuthZResolverApi>()?` and needs deps `authz_resolver`.
- `PolicyEnforcer::from_hub(ctx.client_hub())` (`:331`) resolves lazily per call.
- Optional: `.with_capabilities(vec![Capability::TenantHierarchy])`, `.with_deadline(Duration)` (default 30s).
- The enforcer is cheap to clone.

ResourceType (`:176-227`):
```rust
use toolkit_security::pep_properties;   // OWNER_TENANT_ID="owner_tenant_id", RESOURCE_ID="id", OWNER_ID="owner_id"  (libs/toolkit-security/src/access_scope.rs:94-103)
pub const CHAT: ResourceType = ResourceType::from_static(
    toolkit_gts::gts_id!("cf.core.mini_chat.chat.v1~"),        // any dotted name
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID, pep_properties::RESOURCE_ID],
);
// runtime name: ResourceType::new(String, &'static [&'static str])
```

There is **no `enforce()` method**. The API is (`:517-602`):
```rust
pub async fn access_scope(&self, ctx: &SecurityContext, resource: &ResourceType, action: &str, resource_id: Option<Uuid>)
    -> Result<AccessScope, EnforcerError>;                // require_constraints = true
pub async fn access_scope_with(&self, ctx: &SecurityContext, resource: &ResourceType, action: &str, resource_id: Option<Uuid>, request: &AccessRequest)
    -> Result<AccessScope, EnforcerError>;
// AccessRequest::new().resource_property(k, v: impl IntoPropertyValue /*Uuid,&Uuid,String,&str,i64,bool,Value*/)
//   .resource_properties(HashMap) .context_tenant_id(Uuid) .tenant_mode(TenantMode) .barrier_mode(BarrierMode)
//   .tenant_status(Vec<String>) .tenant_context(TenantContext) .require_constraints(bool /*default true*/)
// low level: build_request / build_request_with -> EvaluationRequest
```

What the request carries:
- Subject: `{id: ctx.subject_id(), subject_type, properties: {"tenant_id": ctx.subject_tenant_id()}}`.
- Context: `token_scopes`, the `supported_properties` of the ResourceType, and the bearer token.
- Tenant context: only if set through `AccessRequest`.

Errors (`:34-50`, compiler errors in `pep/compiler.rs:50-80`):
```rust
pub enum EnforcerError {
    Denied { deny_reason: Option<DenyReason /* {error_code: String, details: Option<String>} */> },  // PDP decision=false
    EvaluationFailed(CanonicalError),        // PDP transport failure / timeout (service_unavailable) / lazy client missing
    CompileFailed(ConstraintCompileError),   // ConstraintsRequiredButAbsent | AllConstraintsFailed{reason} | UnadvertisedCapabilities{..} | ...
}
```
Mapping conventions:
- credstore maps `Denied` and `CompileFailed` to 403, and `EvaluationFailed` to 503 (`gears/credstore/credstore/src/domain/authz.rs:33-49`).
- The mini-chat DESIGN (`DESIGN.md:1414`) wants deny/compile → 403 and evaluation failure → retryable 503 with Retry-After.
- OAGW maps Denied → 403 and the others → 500 (`oagw/src/domain/error.rs:303-329`).

**static-authz-plugin** behaviour (`plugins/static-authz-plugin/src/domain/service.rs:49-172`):
- The tenant is `tenant_context.root_id`, falling back to `subject.properties["tenant_id"]`. **Deny** (`decision:false`, no deny_reason) if it is missing or nil, or if resource properties contain `group_ids`/`ancestor_group_ids`.
- Otherwise `decision:true`:
  - If the ResourceType declares `owner_tenant_id`: constraints `[In(owner_tenant_id, [tid])]`.
  - With `Capability::TenantHierarchy` advertised, it adds `InTenantSubtree(owner_tenant_id|id, tid)`.
  - **It never emits `owner_id` (user) predicates.**
- If the ResourceType declares no `owner_tenant_id`, the constraints are empty. With `require_constraints=true` that becomes `CompileFailed(ConstraintsRequiredButAbsent)` (deny); with `false` it becomes `AccessScope::allow_all()`.
- So for owner-only chat data, add owner scoping yourself: `scope.ensure_owner(ctx.subject_id())` (`libs/toolkit-security/src/access_scope.rs:1073-1120`). It intersects or injects `owner_id = subject` into every constraint; unconstrained scope becomes `[owner_id=subject]`; deny-all stays deny-all.

`AccessScope` API (`libs/toolkit-security/src/access_scope.rs:826-1120`):
- Constructors: `allow_all()`, `deny_all()`, `for_tenant(id)`, `for_tenants(ids)`, `for_resource(id)`, `for_resources(ids)`, `single(constraint)`, `from_constraints(..)`.
- Queries: `is_unconstrained()`, `is_deny_all()`, `constraints()`, `all_values_for(prop)`, `all_uuid_values_for(prop)`, `contains_uuid` (not an authz decision), `has_property`.
- Narrowing: `tenant_only()`, `tenant_and_owner()`, `ensure_owner(owner_id)`.
- toolkit-db SecureORM consumes the scope as WHERE clauses for `#[derive(Scopable)]` entities.

Real gear usage (credstore):
```rust
// gears/credstore/credstore/src/gear.rs:110-117
let authz_client = ctx.client_hub().get::<dyn AuthZResolverApi>()
    .map_err(|e| anyhow::anyhow!("failed to get AuthZResolverApi: {e}"))?;
let enforcer = PolicyEnforcer::new(authz_client);

// gears/credstore/credstore/src/domain/authz.rs:23-72
pub fn secret_type_resource(gts_id: &str) -> ResourceType {
    ResourceType::new(gts_id.to_owned(), &[pep_properties::OWNER_TENANT_ID])
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
Other real call sites:
- OAGW DP: `oagw/src/infra/proxy/service.rs:546-556` (action `"invoke"`, `require_constraints(false)`).
- OAGW resource consts: `oagw/src/infra/proxy/mod.rs:24-37`.
- types-registry entities (`gears/system/types-registry/types-registry/src/infra/storage/entity/*.rs`).

Suggested mini-chat pattern:
```rust
let scope = enforcer
    .access_scope_with(&ctx, &CHAT, "read", Some(chat_id),
        &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()))
    .await
    .map_err(|e| match e {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => MyErr::Forbidden,
        EnforcerError::EvaluationFailed(_) => MyErr::Unavailable { retry_after: 1 },
    })?
    .ensure_owner(ctx.subject_id());   // static PDP gives tenant-only constraints
```

---------------------------------------------------------------------------------------------------

## 5. `SecurityContext` (`libs/toolkit-security/src/context.rs`)

```rust
use toolkit_security::SecurityContext;                       // also toolkit_security::prelude
use toolkit_security::constants::{DEFAULT_SUBJECT_ID, DEFAULT_TENANT_ID};

// getters (:73-128)
ctx.subject_id() -> Uuid; ctx.subject_type() -> Option<&str>; ctx.subject_tenant_id() -> Uuid;
ctx.token_scopes() -> &[String]; ctx.has_scope(&str) -> bool /* "*" satisfies all; empty = none */;
ctx.is_anonymous() -> bool /* nil subject or tenant */; ctx.bearer_token() -> Option<&SecretString>;
SecurityContext::anonymous()   // nil ids, no scopes (OAGW/static-authz/tenant-resolver will reject it)

// builder (:141-218): subject_id + subject_tenant_id REQUIRED, else SecurityContextBuildError::{MissingSubjectId, MissingSubjectTenantId}
let system_ctx = SecurityContext::builder()
    .subject_id(DEFAULT_SUBJECT_ID)
    .subject_tenant_id(DEFAULT_TENANT_ID)          // or the tenant the task acts for
    .token_scopes(vec!["*".to_owned()])            // optional; empty = no capability
    .subject_type("gts.cf.core.security.subject_user.v1~") // optional
    .build()?;                                     // Result<SecurityContext, SecurityContextBuildError>
```

- Serde: `Serialize`/`Deserialize`; `bearer_token` is skipped.
- For system/background tasks, the precedent is to build with the DEFAULT ids, as OAGW `post_init` does (`oagw/src/gear.rs:242-245, 261-264`). Alternatively, use the S2S context from `exchange_client_credentials`, which produces the same ids with static-authn defaults plus `token_scopes ["*"]`.
- For per-tenant background work (e.g. thread summary of a chat), build a ctx with `subject_tenant_id = chat.tenant_id` and `subject_id = chat.owner/user id` so PEP/SecureORM scoping matches.

---------------------------------------------------------------------------------------------------

## 6. License feature gating

Routes declare license features through `OperationBuilder` (`libs/toolkit/src/api/operation_builder.rs:252-254, 1135-1199`):
- The type-state requires `.authenticated()` then exactly one of:
  - `.require_license_features(iter of F: LicenseFeature)`
  - `.no_license_required()`

  Only after that is `.register()` available. Public routes cannot call either method.
- `LicenseFeature` is a marker trait: `pub trait LicenseFeature: AsRef<str> {}`.
- The base feature constant is `toolkit::api::operation_builder::CORE_GLOBAL_BASE_LICENSE_FEATURE = gts_id!("cf.core.lic.feat.v1~cf.core.global.base.v1")` (`:66-67`).
- **Passing an empty iterator (`::<License>([])`) records NO requirement**, because `license_requirement = None` (`:1158-1163`). That is what types-registry and oagw do (`types-registry/src/api/rest/routes.rs:26-34, 112-113`; `oagw/src/api/rest/routes/mod.rs:12-22`).

To actually gate on the base feature:
```rust
use toolkit::api::operation_builder::{LicenseFeature, CORE_GLOBAL_BASE_LICENSE_FEATURE};
struct License;
impl AsRef<str> for License { fn as_ref(&self) -> &'static str { CORE_GLOBAL_BASE_LICENSE_FEATURE } }
impl LicenseFeature for License {}

OperationBuilder::post("/mini-chat/v1/chats")
    .operation_id("mini_chat.create_chat")
    .authenticated()
    .require_license_features([License])        // non-empty => requirement recorded
    .json_request::<CreateChatReq>(openapi, "...")
    .handler(handlers::create_chat)
    .json_response_with_schema::<ChatDto>(openapi, StatusCode::CREATED, "Created")
    .standard_errors(openapi)
    .register(router, openapi);
```
(Test usage: `gears/system/api-gateway/tests/license_middleware.rs:107,117` uses `.require_license_features([&feature])`.)

Middleware: `gears/system/api-gateway/src/middleware/license_validation.rs:13-79`, layered unconditionally at `gears/system/api-gateway/src/gear.rs:416-424` (it also runs with `auth_disabled`).
- It builds a map `(Method, path template) -> Vec<feature>` from the OperationSpecs.
- It is a **stub**: if any required feature is not the base feature `gts.cf.core.lic.feat.v1~cf.core.global.base.v1`, it returns 403 `permission_denied` with reason `LICENSE_FEATURE_REQUIRED`. Otherwise it passes.
- It does not consult the license-resolver / static-license-plugin.
- Consequence: the base feature always passes, and any other feature (e.g. a future `ai_chat`) always gets 403. That matches the DESIGN interim gate (`DESIGN.md:255-261, 1240`).

---------------------------------------------------------------------------------------------------

## Gotchas checklist for mini-chat

1. A hostname upstream alias is auto-derived (`host`, or `host:port` for a non-standard port). A custom `upstream_alias` different from that gives Validation 400. An IP host needs an explicit alias.
2. `?api-version=` must be in the route's `query_allowlist`, or the request fails with 400 `QUERY_NOT_ALLOWED`.
3. `config/mini-chat.yaml` lacks `oagw.config.allow_http_upstream: true`, so `http://` mock providers fail at proxy time with `HTTP_UPSTREAM_FORBIDDEN`. SSRF is already disabled there.
4. OAGW repos are in-memory: provision on every start. `AlreadyExists` happens only within a process lifetime (e.g. two providers sharing a host, or re-running the reconcile). On upstream `AlreadyExists`, reuse it by `list_upstreams` + alias match.
5. The `secret_ref` must be readable by the provisioning ctx at create time; otherwise `FailedPrecondition`, which should be retried.
6. Upstreams belong to the provisioning ctx tenant. Proxy with that ctx (S2S), or make the auth section `sharing: Inherit` and rely on the tenant hierarchy. The single-tenant TR plugin has no hierarchy.
7. Upstream 4xx/5xx come back as `Ok(resp)` with `ErrorSource::Upstream`. Pingora-level failures come back as `Ok(resp)` 502/503/504 with `ErrorSource::Gateway` and a problem+json body. Pipeline failures and the response-header timeout come back as `Err(CanonicalError)` (504 `DeadlineExceeded` for the timeout).
8. Only `Content-Type` is forwarded by default. `Authorization` is always stripped (the auth plugin injects credentials).
9. `auth_disabled: true` means every request runs as subject 1111.../tenant 00000000-df51... with EMPTY scopes. To test multiple users, use `static_tokens` mode with `auth_disabled: false`.
10. static-authz gives tenant constraints only. Apply `ensure_owner(ctx.subject_id())` for owner-only data. `require_constraints=true` with no `owner_tenant_id` property on the ResourceType becomes a deny.
11. `.require_license_features::<License>([])` means NO gate. Pass `[License]` to gate on the base feature.
