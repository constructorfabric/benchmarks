# Decomposition: Outbound API Gateway (OAGW)


<!-- toc -->

- [1. Overview](#1-overview)
- [2. Entries](#2-entries)
  - [2.1 Gear Foundation - HIGH](#21-gear-foundation---high)
  - [2.2 Upstream and Route Management - HIGH](#22-upstream-and-route-management---high)
  - [2.3 Plugin Management - MEDIUM](#23-plugin-management---medium)
  - [2.4 Proxy Engine - HIGH](#24-proxy-engine---high)
  - [2.5 Auth Plugins and Rate Limiting - HIGH](#25-auth-plugins-and-rate-limiting---high)
  - [2.6 Streaming — SSE and WebSocket - HIGH](#26-streaming--sse-and-websocket---high)
  - [2.7 Observability - LOW](#27-observability---low)
- [3. Feature Dependencies](#3-feature-dependencies)

<!-- /toc -->

**Overall implementation status:**
- [ ] `p1` - **ID**: `cpt-cf-oagw-status-oagw`
## 1. Overview

The DESIGN decomposes into seven features that follow its Control Plane / Data Plane split and its
DDD-Light layering (`api/rest`, `domain`, `infra`). The gear skeleton comes first because every other
feature registers its handlers, errors and configuration through it. The control plane follows
(upstream/route management, then plugin management), the data plane follows the control plane because
it resolves configuration from it, and the request pipeline concerns (auth plugins and rate limiting,
streaming, observability) are last since they all hang off the proxy path. Entries are ordered by
dependency: each entry depends only on entries above it.

`cpt-cf-oagw-feature-auth-plugins-and-rate-limiting` depends on both `cpt-cf-oagw-feature-plugin-management`
(for plugin resolution) and `cpt-cf-oagw-feature-proxy-engine` (for its place in the proxy pipeline), so it
is the only feature with two dependencies. `cpt-cf-oagw-feature-streaming-sse-websocket` hangs off the
request pipeline and `cpt-cf-oagw-feature-observability` hangs off the data plane; the two are independent
of each other and can be built in parallel.

**Decomposition assumptions** (task-level corrections and environment constraints that override
PRD/DESIGN wording where they differ):

1. **Gear-relative paths.** Routes are registered as `/oagw/v1/...` without the `/api` prefix. The
   api-gateway nests a gear's router under its own `prefix_path`, and that prefix is empty in the
   graded configuration. The PRD/DESIGN tables show `/api/oagw/v1/...`, which is the absolute path
   behind an operator gateway with prefix `/api` — it is not what this gear registers.
2. **`http` is a legal upstream endpoint scheme.** `config/e2e-local.yaml` sets
   `oagw.config.allow_http_upstream: true`, so the endpoint `scheme` enum admits `http` alongside
   `https|wss|wt|grpc`, and the flag governs whether a plaintext upstream connection is actually
   attempted. This corrects the default posture of `cpt-cf-oagw-constraint-https-only` for the graded
   configuration only; the default configuration stays HTTPS-only.
3. **In-memory persistence.** The store is in-memory (`dashmap`/`parking_lot`/`arc-swap` over the
   crate's existing dependency set). No SeaORM/`toolkit-db` dependency exists in the crate's
   `Cargo.toml` and the e2e config provides no `oagw` database block. The DESIGN table model
   (`cpt-cf-oagw-db-schema`) is kept as the logical data model, and its invariants — unique
   `(tenant_id, alias)`, route match uniqueness, plugin binding positions contiguous from 0 — are
   enforced by the store. `cpt-cf-oagw-constraint-multi-sql` is therefore satisfied at the logical
   model level, not by SQL backends.
4. **Custom Starlark plugins are stored, not executed.** Definitions are persisted and served
   (including `GET .../source`), but no Starlark interpreter dependency exists in the crate's
   dependency set, so `cpt-cf-oagw-nfr-starlark-sandbox` is satisfied at the storage/contract level
   only.
5. **Testing.** Every feature is covered by in-crate Rust tests (`#[cfg(test)]` modules and `tests/`
   under the gear crate). The `testing/e2e/gears/oagw/` directory is reserved for the component's
   acceptance suite and is not used here.
6. **WebTransport descope.** The `wt` scheme is accepted at upstream create time, but no WebTransport
   session flow is implemented because no QUIC transport dependency exists in the crate. `cpt-cf-oagw-fr-streaming`
   is therefore covered for HTTP request/response, SSE and WebSocket only (entry 2.6).
7. **`ssrf_policy.enabled` is an operator escape hatch.** The key is present in the runtime
   configuration (`config/e2e-local.yaml` sets it `false`) but absent from DESIGN's `OagwConfig`
   surface. Disabling it relaxes upstream host validation; the default `true` keeps DESIGN's SSRF
   posture (host validation, IP pinning rules, no internal-header injection) unconditional.
8. **Gear dependency substitution.** The gear resolves `types-registry`, `tenant-resolver`,
   `credstore` and `authz-resolver`; `api_ingress` REST hosting is supplied by the host api-gateway;
   the reverse-proxy engine arrives through the crate's existing `pingora-*`/`toolkit-http`
   dependencies rather than a direct `pingora` gear dependency; and `toolkit-db` is dropped per
   assumption 3.
9. **`/metrics` is host-provided.** PRD requires scraping at `/metrics`, which the toolkit/host
   metrics surface already serves. OAGW registers its metric families (`oagw_requests_total`,
   rate-limit and circuit-breaker series, `oagw_routing_endpoint_selected`,
   `oagw_circuit_breaker_transitions_total`) on that surface rather than exposing its own endpoint.

**Coverage and granularity notes**:

- Requirement rows marked `[x]` mirror requirements the PRD already marks satisfied; the
  decomposition's own feature checkboxes and the overall implementation status stay unchecked until
  implementation completes.
- Shared-requirement scope: IDs cited by more than one feature are split along the
  control-plane/data-plane boundary — the control-plane feature owns the management behaviour, the
  data-plane feature owns the request-path behaviour. Intended splits: `fr-alias-resolution` (2.2
  derivation/uniqueness rules, 2.4 request-time resolution), `fr-enable-disable` (2.2 CRUD semantics,
  2.4 enforcement), `fr-error-codes` (2.4 mapping, 2.6 stream errors), `fr-plugin-system` /
  `fr-builtin-plugins` (2.3 CRUD/registry, 2.5 execution), `nfr-low-latency` (2.4 proxy path, 2.7
  non-blocking logging), `nfr-input-validation` (2.2 request validation, 2.4 body/header validation),
  `interface-management-api` (2.1 wiring, 2.2 endpoints).
- Each entry is a single-phase work package with no subordinate decomposition; milestones, TDD slices
  and acceptance criteria are authored in the corresponding FEATURE artifact.
- Granularity rationale: the Proxy Engine is deliberately the largest package because the data-plane
  request path is one cohesive pipeline, and Observability is deliberately narrow because it only
  attaches logging and metrics to that path; both stay independently implementable and testable.

Design-artifact IDs (`cpt-cf-oagw-component-model`, `cpt-cf-oagw-design-layers`,
`cpt-cf-oagw-design-domain-model`, `cpt-cf-oagw-interface-api`, `cpt-cf-oagw-seq-proxy-flow`) carry
priority `p1`: they are structural prerequisites of the gear rather than PRD requirements.

## 2. Entries

### 2.1 [Gear Foundation](features/gear-foundation.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-gear-foundation`

- **Purpose**: Establish the `oagw` gear crate skeleton for the outbound API gateway: toolkit gear
  declaration and registration, the gear config struct, the module layout, and the REST capability
  wiring that makes the host mount the gear's router. It also delivers the cross-cutting response
  contract every other feature relies on — canonical RFC 9457 `application/problem+json` error
  plumbing and the `X-OAGW-Error-Source` response header on every response.

- **Depends On**: None

- **ADRs**:

  - `cpt-cf-oagw-adr-error-source-distinction`

- **Scope**:
  - `#[toolkit::gear]` declaration for gear name `oagw` with `rest` and `stateful` capabilities and
    dependencies on types-registry, tenant-resolver, credstore and authz-resolver declared as
    resolvable.
  - `OagwConfig` bound to the `gears.oagw.config` keys `proxy_timeout_secs`, `allow_http_upstream`,
    `ssrf_policy.enabled`, `token_cache_ttl_secs`, `token_cache_capacity` — key set follows the
    runtime configuration (`config/e2e-local.yaml`), token-cache keys per ADR 0008; serde attributes
    and defaults are fixed in the FEATURE artifact.
  - `init` storing the loaded config on the gear struct.
  - `register_rest` mounting the gear's routes so the host router exposes them under the gear mount
    root.
  - Module layout following DDD-Light layering: `api/rest` (transport), `domain` (services and
    models), `infra` (proxy engine, storage, plugin registries, type provisioning).
  - Error mapping layer converting domain errors into CanonicalError problem+json responses with GTS
    `type` identifiers.
  - `X-OAGW-Error-Source: gateway|upstream` response-header layer applied to all gear responses.

- **Out of scope**:
  - Route handler bodies themselves (Features 2-6).
  - Persistence.

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-interface-management-api`

- **Design Principles Covered**:

  - `cpt-cf-oagw-principle-error-source`
  - `cpt-cf-oagw-principle-rfc9457`

- **Design Constraints Covered**:

  - `cpt-cf-oagw-constraint-https-only`
  - `cpt-cf-oagw-constraint-toolkit-deploy`

  Recorded here with the task-level correction from assumption 2: `http` is a legal endpoint scheme in
  this deployment, and the `allow_http_upstream` config flag governs whether a plaintext upstream
  connection is actually attempted. The default posture stays HTTPS-only.

- **Domain Model Entities**:
  - Upstream (declaration only)
  - Route (declaration only)
  - Plugin (declaration only)

- **Design Components**:

  - `cpt-cf-oagw-component-model`
  - `cpt-cf-oagw-design-layers`

- **API**:
  - Gear route mount root `/oagw/v1/...` (gear-relative).

  Registered paths carry no `/api` prefix: the api-gateway applies its own `prefix_path`, which is
  empty in the graded configuration. The documented `/api/oagw/v1/...` paths are the absolute form
  behind an operator gateway and are NOT what is registered (assumption 1).

- **Sequences**: None

- **Data**: None

### 2.2 [Upstream and Route Management](features/upstream-route-management.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-upstream-route-management`

- **Purpose**: Tenant-scoped management API for upstreams and routes: the in-memory store, CRUD REST
  endpoints, validation, alias derivation and enforcement, enabled/disabled semantics with
  hierarchical inheritance, and config sharing modes with layering. This is the control plane the
  data plane resolves configuration from.

- **Depends On**: `cpt-cf-oagw-feature-gear-foundation`

- **ADRs**:

  - `cpt-cf-oagw-adr-request-routing`
  - `cpt-cf-oagw-adr-state-management`

- **Scope**:
  - Upstream and route models matching `docs/schemas/upstream.v1.schema.json` and
    `docs/schemas/route.v1.schema.json`: `server.endpoints` with `scheme` enum `https|wss|wt|grpc|http`
    (`http` is legal per assumption 2), `host`, `port` (default 443), `protocol` (GTS id), `auth`,
    `headers`, `plugins`, `rate_limit`, `cors`, `tags`, `enabled` (default true), `alias`.
  - Alias derivation from hostname endpoints: longest common domain suffix of at least two labels,
    validated against the public suffix list via the `psl` crate; standard ports 80/443 omitted;
    lowercase normalization with trailing dots stripped; explicit alias required for IP-based or
    non-derivable endpoints; a user-provided alias that differs from the derived value is rejected,
    while the exact derived value is tolerated for idempotency.
  - Immutable `alias`, `id` and `tenant_id` on all resources; route `upstream_id` immutable.
  - Route match rules: HTTP methods `GET|POST|PUT|DELETE|PATCH`, `path`, `query_allowlist`,
    `path_suffix_mode` `disabled|append`. `grpc` match is declared per schema but not served — gRPC
    proxying is out of scope.
  - REST endpoints for both resources: POST create, GET list (OData `$filter`/`$select`/`$orderby`/`$top`/`$skip`), GET by id,
    PUT replace, DELETE.
  - Uniqueness and conflict rules: `(tenant_id, alias)` collision returns 409; duplicate route match
    (path + priority + method) returns 409; creating a route for a non-existent upstream returns 400.
  - Tenant scoping: ancestor resources are invisible to descendants (404) on the management API.
  - Ancestor-permission gates (DESIGN §3.2): `oagw:upstream:bind`, `oagw:upstream:override_auth`,
    `oagw:upstream:override_rate` and `oagw:upstream:add_plugins` — sharing mode `enforce` blocks
    descendant overrides and `private` blocks visibility.
  - Config sharing modes `private|inherit|enforce`, add-only union semantics for tags, and rate-limit
    inheritance as `min(ancestor.enforced, descendant)`.
  - Status codes: 201 create, 200 get/list/put, 204 delete.

- **Out of scope**:
  - Proxy execution (Feature 4).
  - Plugin CRUD (Feature 3).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-upstream-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-fr-route-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`
  - [x] `p2` - `cpt-cf-oagw-fr-config-layering`
  - [x] `p2` - `cpt-cf-oagw-fr-hierarchical-config`
  - [x] `p2` - `cpt-cf-oagw-fr-alias-resolution`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
  - [ ] `p1` - `cpt-cf-oagw-nfr-multi-tenancy`
  - [ ] `p1` - `cpt-cf-oagw-usecase-configure-upstream`
  - [ ] `p1` - `cpt-cf-oagw-usecase-configure-route`
  - [ ] `p1` - `cpt-cf-oagw-interface-management-api`

- **Design Principles Covered**:

  - `cpt-cf-oagw-principle-tenant-scope`

  Satisfied at the logical model level: tenant scoping is enforced by the in-memory store's
  tenant-keyed lookups (assumption 3); the DESIGN's secure-ORM wording describes the SQL backend this
  deployment does not use.

- **Design Constraints Covered**:

  - `cpt-cf-oagw-constraint-multi-sql`

  Satisfied at the logical data model level only: the store is in-memory (assumption 3), so the
  multi-SQL portability requirement is honoured by keeping the logical model backend-neutral and
  enforcing the DESIGN's relational invariants in the store.

- **Domain Model Entities**:
  - Upstream
  - Route
  - ServerConfig / Endpoint
  - MatchConfig
  - RateLimitConfig
  - CorsConfig
  - HeadersConfig
  - PluginsConfig
  - Tag

- **Design Components**:

  - `cpt-cf-oagw-design-domain-model`
  - `cpt-cf-oagw-interface-api`

- **API**:
  - POST /oagw/v1/upstreams
  - GET /oagw/v1/upstreams
  - GET /oagw/v1/upstreams/{id}
  - PUT /oagw/v1/upstreams/{id}
  - DELETE /oagw/v1/upstreams/{id}
  - POST /oagw/v1/routes
  - GET /oagw/v1/routes
  - GET /oagw/v1/routes/{id}
  - PUT /oagw/v1/routes/{id}
  - DELETE /oagw/v1/routes/{id}

- **Sequences**: None

- **Data**:

  - `cpt-cf-oagw-db-schema`

  Logical model only — the store is in-memory (assumption 3).

### 2.3 [Plugin Management](features/plugin-management.md) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-oagw-feature-plugin-management`

- **Purpose**: Plugin registry and plugin management API: registration of the builtin plugin catalog
  in the types-registry-compatible GTS id space, custom plugin definition CRUD (create, list, get,
  delete, source), and plugin-in-use conflict detection on delete.

- **Depends On**: `cpt-cf-oagw-feature-upstream-route-management`

- **ADRs**:

  - `cpt-cf-oagw-adr-plugin-system`

- **Scope**:
  - Plugin types `auth|guard|transform` with GTS ids `gts.cf.core.oagw.auth_plugin.v1~*`,
    `gts.cf.core.oagw.guard_plugin.v1~*`, `gts.cf.core.oagw.transform_plugin.v1~*`.
  - Builtin plugin registries (`AuthPluginRegistry::with_builtins`, `GuardPluginRegistry::with_builtins`)
    covering ApiKeyAuthPlugin, NoopAuthPlugin, OAuth2ClientCredAuthPlugin (Form and Basic variants),
    RequiredHeadersGuardPlugin and RequestIdTransformPlugin.
  - Catalog-only identifiers registered but not resolvable: `basic` and `bearer` (auth), `timeout` and
    `cors` (guards), `logging` and `metrics` (transforms) — using `basic` or `bearer` as
    `auth.plugin_type` fails with `unknown auth plugin`.
  - Custom plugin endpoints: POST create, GET list, GET by id, DELETE (204; 409 PluginInUse
    problem+json with `type` `gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1` and extension keys
    `plugin_id` and `referenced_by`), and GET `{id}/source`.
  - Plugin definitions are immutable: no PUT; changes mean creating a new definition.
  - Plugin binding validation: positions contiguous from 0; named plugins carry `plugin_ref` with
    `plugin_uuid` NULL; custom plugins carry both set and matching.

- **Out of scope**:
  - Starlark execution sandbox — no interpreter dependency exists in the crate's dependency set
    (assumption 4).
  - Plugin garbage-collection job.

- **Requirements Covered**:

  - [ ] `p2` - `cpt-cf-oagw-fr-plugin-system`
  - [ ] `p2` - `cpt-cf-oagw-fr-builtin-plugins`
  - [ ] `p1` - `cpt-cf-oagw-contract-types-registry`
  - [ ] `p3` - `cpt-cf-oagw-nfr-starlark-sandbox`

  Storage and source contract only (assumption 4).

- **Design Principles Covered**:

  - `cpt-cf-oagw-principle-plugin-immutable`

- **Design Constraints Covered**:

  - `cpt-cf-oagw-constraint-multi-sql`

  The plugin tables are part of the same logical data model, satisfied at the logical level only
  (assumption 3).

- **Domain Model Entities**:
  - Plugin
  - PluginBinding

- **Design Components**:

  - `cpt-cf-oagw-component-model`

- **API**:
  - POST /oagw/v1/plugins
  - GET /oagw/v1/plugins
  - GET /oagw/v1/plugins/{id}
  - DELETE /oagw/v1/plugins/{id}
  - GET /oagw/v1/plugins/{id}/source

- **Sequences**: None

- **Data**:

  - `cpt-cf-oagw-db-schema`

  Plugin tables logical model (assumption 3).

### 2.4 [Proxy Engine](features/proxy-engine.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-proxy-engine`

- **Purpose**: The data plane: resolve an alias to an upstream, match a route, merge the effective
  configuration, apply header transforms, validate the request and forward it to the upstream over
  HTTP, mapping every failure to its specified status code and error body with `X-OAGW-Error-Source`
  on every response.

- **Depends On**: `cpt-cf-oagw-feature-upstream-route-management`

- **ADRs**:

  - `cpt-cf-oagw-adr-request-routing`
  - `cpt-cf-oagw-adr-cors`
  - `cpt-cf-oagw-adr-error-source-distinction`

- **Scope**:
  - Proxy endpoint `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?query]`.
  - Tenant-hierarchy alias walk from descendant to root; the closest enabled upstream wins, and
    enforced ancestor limits still apply across shadowing.
  - Route matching: method allowlist, longest path prefix, `path_suffix_mode`, query allowlist.
  - Effective config merge with precedence upstream < route < tenant.
  - Multi-endpoint pools: round-robin distribution, `X-OAGW-Target-Host` bypasses load balancing, and
    a common-suffix alias with multiple endpoints and a missing header returns 400 MissingTargetHost;
    an invalid or unknown header value returns 400, with error bodies carrying the ADR 0007
    extensions `alias`, `valid_hosts` and `invalid_value`.
  - Header transformation: request set/add/remove, passthrough `none|allowlist|all`, response
    set/add/remove, and hop-by-hop stripping of Connection, Keep-Alive, Proxy-Authenticate,
    Proxy-Authorization, TE, Trailer, Transfer-Encoding and Upgrade. `Host`/`:authority` is rewritten
    to the upstream host; `X-OAGW-Target-Host` is stripped after routing.
  - Body validation: `Content-Length` must be a valid integer and must match the actual size (400);
    max 100MB rejected as 413 before buffering; `Transfer-Encoding` other than `chunked` returns 400.
  - Upstream call with the `proxy_timeout_secs` timeout.
  - Adaptive per-host HTTP version negotiation: attempt HTTP/2 via ALPN during the TLS handshake and
    cache the negotiated version per host with a 1h TTL.
  - HTTP smuggling defence: strict header parsing, rejection of CR/LF in header values, and validation
    of `Content-Length` / `Transfer-Encoding` combinations.
  - Error mapping: 400 Validation, 401 AuthenticationFailed, 404 RouteNotFound, 413 PayloadTooLarge,
    429 RateLimitExceeded, 500 SecretNotFound, 502 DownstreamError, 503 CircuitBreakerOpen /
    LinkUnavailable / PluginNotFound, 504 Connection/Request/Idle Timeout, plus the ADR 0004 CORS 403s
    and the ADR 0001/0007 target-host errors.
  - Error source distinction: gateway errors are problem+json with `X-OAGW-Error-Source: gateway`;
    upstream responses, including errors, are passed through with `X-OAGW-Error-Source: upstream`.
  - SSRF posture: validate the target host, do not inject internal headers into outbound requests, and
    honour the `ssrf_policy.enabled` flag.
  - No automatic retries of the client request.
  - Circuit breaker: trips within 5 failed requests in a 30s window per upstream, states
    CLOSED → OPEN → HALF_OPEN, and returns 503 CircuitBreakerOpen while open.

- **Out of scope**:
  - Plugin chain execution (Feature 5).
  - SSE and WebSocket streaming (Feature 6).
  - Metrics and audit logging (Feature 7).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-request-proxy`
  - [x] `p2` - `cpt-cf-oagw-fr-alias-resolution`
  - [ ] `p1` - `cpt-cf-oagw-fr-header-transform`
  - [ ] `p1` - `cpt-cf-oagw-fr-error-codes`
  - [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`
  - [ ] `p1` - `cpt-cf-oagw-nfr-low-latency`
  - [ ] `p1` - `cpt-cf-oagw-nfr-ssrf-protection`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
  - [ ] `p1` - `cpt-cf-oagw-usecase-proxy-request`
  - [ ] `p1` - `cpt-cf-oagw-interface-proxy-api`

- **Design Principles Covered**:

  - `cpt-cf-oagw-principle-error-source`
  - `cpt-cf-oagw-principle-rfc9457`
  - `cpt-cf-oagw-principle-no-retry`
  - `cpt-cf-oagw-principle-no-cache`

- **Design Constraints Covered**:

  - `cpt-cf-oagw-constraint-body-limit`
  - `cpt-cf-oagw-constraint-https-only`
  - `cpt-cf-oagw-constraint-no-direct-internet`

  Enforced with the `allow_http_upstream` flag correction from assumption 2: the flag governs whether
  a plaintext upstream connection is attempted; the default posture stays HTTPS-only.

- **Domain Model Entities**:
  - EffectiveUpstream
  - MatchedRoute
  - RequestContext
  - ResponseContext
  - ErrorContext
  - CircuitBreaker

- **Design Components**:

  - `cpt-cf-oagw-component-model`

- **API**:
  - {METHOD} /oagw/v1/proxy/{alias}
  - {METHOD} /oagw/v1/proxy/{alias}/{path_suffix}

- **Sequences**:

  - `cpt-cf-oagw-seq-proxy-flow`

- **Data**: None

### 2.5 [Auth Plugins and Rate Limiting](features/auth-plugins-and-rate-limiting.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-auth-plugins-and-rate-limiting`

- **Purpose**: Execute the request plugin chain (auth, then guards, then request transforms) with
  credential injection from the cred store, enforce rate limits with token buckets, and run the
  response-phase plugins, with the 429 and 401 semantics the spec defines.

- **Depends On**: `cpt-cf-oagw-feature-plugin-management`, `cpt-cf-oagw-feature-proxy-engine`

- **ADRs**:

  - `cpt-cf-oagw-adr-rate-limiting`
  - `cpt-cf-oagw-adr-cors`
  - `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`
  - `cpt-cf-oagw-adr-plugin-system`

- **Scope**:
  - Plugin execution order: Auth, then Guards, then Transform (request), then the upstream call, then
    Transform (response/error). Upstream plugins execute before route plugins.
  - Auth plugin credential injection for the resolvable set — API Key (header or query), the no-op
    auth plugin, and OAuth2 client credentials, where the Form variant injects
    `Authorization: Bearer` and the Basic variant injects `Authorization: Basic` with base64-encoded
    client credentials — with a token cache of capacity `token_cache_capacity`, TTL
    `min(token_cache_ttl_secs, expires_in - 30s)`, no caching of tokens whose `expires_in` is at most
    30s, no caching of failed fetches, and cache key `tenant:subject:method:config-hash`.
  - Credentials resolved from the credstore by `cred://` UUID reference at request time, and never
    logged or returned.
  - RequiredHeaders guard: case-insensitive presence-only checking; a missing request header returns
    400 REQUIRED_HEADER_MISSING, a missing response header returns 502, only the first missing header
    is reported, and a blank configuration is a no-op.
  - RequestId transform.
  - Rate limiting per ADR 0003: token bucket with sustained rate/window and burst capacity defaulting
    to `sustained.rate`; scope `global|tenant|user|ip|route`; strategy `reject|queue|degrade`; per
    request cost; sharing inheritance `private|inherit|enforce` with
    `min(ancestor.enforced, descendant)`.
  - 429 responses with `Retry-After` and `X-RateLimit-Limit` / `X-RateLimit-Remaining` /
    `X-RateLimit-Reset` headers.
  - CORS handling per ADR 0004: a preflight `OPTIONS` with `Origin` and
    `Access-Control-Request-Method` returns 204 with the exact header set and `Max-Age` 86400; actual
    requests rejected for a disallowed origin or method return 403 with problem+json types
    `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` and
    `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`; `Vary: Origin` is always
    included; combining `allow_credentials` with `*` is rejected at validation time.
  - Auth failure returns 401 AuthenticationFailed.

- **Out of scope**:
  - Starlark sandbox execution (assumption 4).
  - Distributed rate-limit synchronization (Redis).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-auth-injection`
  - [ ] `p1` - `cpt-cf-oagw-fr-rate-limiting`
  - [ ] `p2` - `cpt-cf-oagw-fr-plugin-system`
  - [ ] `p2` - `cpt-cf-oagw-fr-builtin-plugins`
  - [ ] `p1` - `cpt-cf-oagw-nfr-high-availability`
  - [ ] `p1` - `cpt-cf-oagw-nfr-credential-isolation`
  - [ ] `p2` - `cpt-cf-oagw-usecase-rate-limit-exceeded`
  - [ ] `p1` - `cpt-cf-oagw-contract-cred-store`

- **Design Principles Covered**:

  - `cpt-cf-oagw-principle-cred-isolation`
  - `cpt-cf-oagw-principle-plugin-immutable`

- **Design Constraints Covered**: None

- **Domain Model Entities**:
  - AuthConfig
  - PluginsConfig
  - RateLimitConfig
  - TokenBucket

- **Design Components**:

  - `cpt-cf-oagw-component-model`

- **API**: None — no new endpoints. This feature implements the behaviour of
  `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` registered by Feature 4.

- **Sequences**:

  - `cpt-cf-oagw-seq-proxy-flow`

- **Data**: None

### 2.6 [Streaming — SSE and WebSocket](features/streaming-sse-websocket.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-streaming-sse-websocket`

- **Purpose**: Proxy streaming responses: server-sent events forwarded incrementally with a correct
  open, close and error lifecycle, and WebSocket upgrades proxied bidirectionally.

- **Depends On**: `cpt-cf-oagw-feature-auth-plugins-and-rate-limiting`

- **Scope**:
  - SSE detection (`text/event-stream` responses and requests) forwarded as received, without
    buffering the whole body.
  - Upstream closes the stream: close the client connection and log it.
  - Client disconnects: close the upstream connection.
  - `X-OAGW-Error-Source` present on stream responses — gateway errors as problem+json, upstream
    errors passed through.
  - WebSocket upgrade via `scheme: wss` upstream endpoints (`http` scheme with a ws upgrade is
    permitted per `allow_http_upstream`), proxied with Upgrade, Connection and `Sec-WebSocket-*`
    handling, and bidirectional frame relay until either side closes.
  - The idle timeout applies to streams.

- **Out of scope**:
  - WebTransport: the `wt` scheme is accepted at create time but the session flow is not implemented —
    no QUIC transport is available.
  - gRPC proxying (phase 4).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-streaming`
  - [ ] `p1` - `cpt-cf-oagw-usecase-sse-streaming`
  - [ ] `p1` - `cpt-cf-oagw-fr-error-codes`

  StreamAborted 502 on aborted streams.

- **Design Principles Covered**:

  - `cpt-cf-oagw-principle-error-source`
  - `cpt-cf-oagw-principle-rfc9457`

- **Design Constraints Covered**:

  - `cpt-cf-oagw-constraint-https-only`

  Recorded with the assumption-2 correction: a `ws` upgrade over an `http` upstream endpoint is
  permitted when `allow_http_upstream` is enabled; the default posture stays HTTPS-only.

- **Domain Model Entities**:
  - Endpoint (wss upstream endpoints)
  - RequestContext
  - ResponseContext

- **Design Components**:

  - `cpt-cf-oagw-component-model`

- **API**:
  - {METHOD} /oagw/v1/proxy/{alias}/{path} (SSE responses and requests, WebSocket upgrade)

- **Sequences**: None

- **Data**: None

### 2.7 [Observability](features/observability.md) - LOW

- [ ] `p3` - **ID**: `cpt-cf-oagw-feature-observability`

- **Purpose**: Audit logging of every proxy request and Prometheus metrics for routing, rate limiting
  and circuit-breaker state, so operators have full visibility into outbound traffic, errors and
  performance.

- **Depends On**: `cpt-cf-oagw-feature-proxy-engine`

- **ADRs**:

  - `cpt-cf-oagw-adr-request-routing`

- **Scope**:
  - Audit log JSON per ADR 0001 with the exact keys `timestamp`, `level`, `event` (`proxy_request`),
    `request_id`, `tenant_id`, `principal_id`, `host`, `path`, `method`, `status`, `duration_ms`,
    `request_size`, `response_size` and `error_type` (null on success).
  - Correlation IDs propagated end to end.
  - Prometheus metric names per DESIGN: request counts, latencies, error rates, rate-limit state, and
    routing `oagw_routing_endpoint_selected` with `selection_method`
    `explicit_header|round_robin|default`, plus circuit-breaker state and
    `oagw_circuit_breaker_transitions_total` labelled by `host`, `from_state` and `to_state`.

- **Out of scope**:
  - Exposing a `/metrics` endpoint of its own — OAGW registers its metric families on the
    host-provided metrics surface (assumption 9).
  - Distributed tracing backends.

- **Requirements Covered**:

  - [ ] `p2` - `cpt-cf-oagw-nfr-observability`
  - [ ] `p1` - `cpt-cf-oagw-nfr-low-latency`

  Logging must stay non-blocking on the proxy hot path.

- **Design Principles Covered**: None

- **Design Constraints Covered**: None

- **Domain Model Entities**: None

- **Design Components**:

  - `cpt-cf-oagw-component-model`

- **API**:
  - GET /metrics (host-provided)

- **Sequences**: None

- **Data**: None

---

## 3. Feature Dependencies

```text
cpt-cf-oagw-feature-gear-foundation
    |
    +--> cpt-cf-oagw-feature-upstream-route-management
             |
             +--> cpt-cf-oagw-feature-plugin-management
             |        |
             |        +--> cpt-cf-oagw-feature-auth-plugins-and-rate-limiting
             |
             +--> cpt-cf-oagw-feature-proxy-engine
                      |
                      +--> cpt-cf-oagw-feature-auth-plugins-and-rate-limiting
                      |
                      +--> cpt-cf-oagw-feature-observability

cpt-cf-oagw-feature-auth-plugins-and-rate-limiting
    |
    +--> cpt-cf-oagw-feature-streaming-sse-websocket
```

**Dependency Rationale**:

- `cpt-cf-oagw-feature-upstream-route-management` requires `cpt-cf-oagw-feature-gear-foundation`: its handlers
  are registered through the gear's REST wiring, return CanonicalError problem+json bodies carrying
  `X-OAGW-Error-Source`, and read the gear config the foundation feature loads.
- `cpt-cf-oagw-feature-plugin-management` requires `cpt-cf-oagw-feature-upstream-route-management`: plugin-in-use
  conflict detection has to scan upstream and route plugin bindings and auth-plugin references, so the
  store and its invariants must already exist.
- `cpt-cf-oagw-feature-proxy-engine` requires `cpt-cf-oagw-feature-upstream-route-management`: the data plane
  resolves alias to upstream and matches routes against the control plane store, including enabled
  state and inherited limits.
- `cpt-cf-oagw-feature-auth-plugins-and-rate-limiting` requires `cpt-cf-oagw-feature-plugin-management`: the
  plugin chain resolves named and custom plugin definitions through the registries that feature builds.
- `cpt-cf-oagw-feature-auth-plugins-and-rate-limiting` requires `cpt-cf-oagw-feature-proxy-engine`: auth, guard,
  transform and rate-limit execution happen inside the proxy pipeline between route match and the
  upstream call, and their failures must map onto that feature's error mapping.
- `cpt-cf-oagw-feature-streaming-sse-websocket` requires `cpt-cf-oagw-feature-auth-plugins-and-rate-limiting`:
  a streamed response flows through the same plugin, auth and rate-limit pipeline before the body
  starts streaming.
- `cpt-cf-oagw-feature-observability` requires `cpt-cf-oagw-feature-proxy-engine`: audit records and metrics are
  emitted from the proxy request path and its circuit-breaker transitions.
- `cpt-cf-oagw-feature-streaming-sse-websocket` and `cpt-cf-oagw-feature-observability` are independent of each
  other and can be developed in parallel.
