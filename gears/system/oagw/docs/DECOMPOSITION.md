# Decomposition: Outbound API Gateway (OAGW)

**Overall implementation status:**
- [ ] `p1` - **ID**: `cpt-cf-oagw-status-decomposition`

<!-- toc -->

- [1. Overview](#1-overview)
- [2. Entries](#2-entries)
  - [2.1 Gear Foundation and Configuration - HIGH](#21-gear-foundation-and-configuration---high)
  - [2.2 Upstream Management - HIGH](#22-upstream-management---high)
  - [2.3 Route Management - HIGH](#23-route-management---high)
  - [2.4 Plugin Catalog and Bindings - MEDIUM](#24-plugin-catalog-and-bindings---medium)
  - [2.5 HTTP Request Proxying - HIGH](#25-http-request-proxying---high)
  - [2.6 Streaming and Upgrade Proxying - HIGH](#26-streaming-and-upgrade-proxying---high)
  - [2.7 Traffic Policy Enforcement - MEDIUM](#27-traffic-policy-enforcement---medium)
- [3. Feature Dependencies](#3-feature-dependencies)

<!-- /toc -->

## 1. Overview

This decomposition splits the OAGW gear into seven features, ordered by dependency: Gear Foundation and Configuration, Upstream Management, Route Management, Plugin Catalog and Bindings, HTTP Request Proxying, Streaming and Upgrade Proxying, and Traffic Policy Enforcement. The ordering follows the natural build-up of the gear: gear wiring and configuration first, then the control-plane resources that a request needs to resolve (upstreams, then routes, then plugins), then the data-plane path that consumes those resources (plain HTTP proxying, then streaming/upgrade proxying built on top of it), and finally the cross-cutting policy layer (rate limiting, CORS, guard plugins, auth injection) that wraps the proxy path. Every functional and non-functional requirement in PRD.md and every design principle and constraint in DESIGN.md is allocated to at least one entry below; entries that own a requirement only partially (because part of it is deliberately deferred) name the deferred part explicitly in their Out of scope list rather than dropping the ID. HTTP Request Proxying is intentionally the largest entry by scope-bullet count: it is the sole owner of the entire data-plane request path (alias resolution, route matching, endpoint selection, configuration merge, header transformation, body/timeout enforcement, outbound connection handling, and error-source distinction), and both Streaming and Upgrade Proxying and Traffic Policy Enforcement build directly on top of that path rather than duplicating any part of it.

Three task-level overrides apply to every entry in this document and take precedence over the literal text of PRD.md and DESIGN.md, which are frozen inputs and are not edited to match:

1. **Gear-relative routes.** PRD.md and DESIGN.md tabulate management and proxy paths as `/api/oagw/v1/...`. That absolute form only holds behind an operator gateway whose `prefix_path` is `/api`; in this workspace the api-gateway nests the assembled router once under its own `prefix_path`, and the graded `config/e2e-local.yaml` leaves `prefix_path` empty. Every API bullet in this document is therefore written gear-relative, as `/oagw/v1/...` (for example `POST /oagw/v1/upstreams`, `{METHOD} /oagw/v1/proxy/{alias}/{path}`), with no `/api` prefix.
2. **`http` and `ws` are legal endpoint schemes at the validation layer.** `config/e2e-local.yaml` sets `oagw.config.allow_http_upstream: true`. DESIGN.md's `cpt-cf-oagw-constraint-https-only` describes the default posture (HTTPS-only, plaintext upstreams blocked); the flag lifts that default for this configuration. This document treats scheme acceptance and connection enforcement as two separate questions: which schemes the upstream `scheme` field accepts (owned by Upstream Management) is independent of whether a plaintext connection is actually made (owned by HTTP Request Proxying and gated by `allow_http_upstream`). A validation layer that only accepted the TLS family would reject a legal request at create time under this configuration. The frozen `upstream.v1.schema.json` declares `scheme` as `enum: [https, wss, wt, grpc]`; under this configuration the accepted set is widened to include the plaintext counterparts `http` and `ws`. The schema is not edited to match — the widening is a deliberate task-level override recorded here.
3. **No database in the graded configuration.** The `gears.oagw` block of `config/e2e-local.yaml` has a `config:` section and no `database:` section, so the gear runs without a database. `cpt-cf-oagw-db-schema` describes the persistent-deployment table plan (`oagw_upstream`, `oagw_route`, `oagw_plugin`, and their binding/tag tables); in the graded build the same entities, invariants, and uniqueness constraints (for example `UNIQUE(tenant_id, alias)`) are realized as in-process memory state instead of database rows. Every entry's Data bullet either cites `cpt-cf-oagw-db-schema` with this in-memory note, or states `None` where the entry genuinely carries no persisted or in-memory domain state of its own.

Additionally, the graded configuration serves gear-level keys `proxy_timeout_secs` (2 seconds), `allow_http_upstream` (true), and `ssrf_policy.enabled` (false), plus `token_cache_ttl_secs` and `token_cache_capacity` from ADR-0008. `proxy_timeout_secs` and `ssrf_policy.enabled` are supplied by the graded deployment configuration `config/e2e-local.yaml` itself and have no upstream PRD/DESIGN/ADR identifier of their own; `token_cache_ttl_secs` and `token_cache_capacity` trace to ADR-0008. All five are surfaced as part of the typed configuration owned by Gear Foundation and Configuration and consumed by the entries that act on them (HTTP Request Proxying for `proxy_timeout_secs`, `allow_http_upstream`, and `ssrf_policy.enabled`; Traffic Policy Enforcement for the OAuth2 token cache settings).

## 2. Entries

### 2.1 [Gear Foundation and Configuration](feature-gear-foundation/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-gear-foundation`

- **Purpose**: Establishes OAGW as a registered ToolKit gear with typed configuration, shared gear state, canonical mapping of domain errors to RFC 9457 problem+json responses, and a mounted router. Every other feature in this decomposition executes inside the wiring, configuration surface, and error-mapping conventions this entry establishes.

- **Implementation Approach**: single phase

- **Depends On**: None

- **Scope**:
  - Gear registration and single-executable deployment wiring
  - The gear's typed configuration surface, including the keys actually served in the graded configuration (`proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy.enabled`) and the OAuth2 token cache settings from ADR-0008 (`token_cache_ttl_secs`, `token_cache_capacity`)
  - Shared gear state accessible to the Control Plane and Data Plane internals
  - Canonical mapping of domain errors to RFC 9457 `application/problem+json` responses, including the full error-code taxonomy that every other feature's error paths rely on
  - Mounting the gear's router under the gear-relative `/oagw/v1` path (see Overview override 1); no `/api` prefix is applied by this gear itself

- **Out of scope**:
  - Persistent database schema and multi-SQL backend behavior beyond the constraint definition — the graded configuration runs without a database (see Overview override 3); persisted-deployment schema ownership is deferred to the entries that own the persisted entities
  - Feature-specific business logic (upstream/route/plugin CRUD, proxy execution, policy enforcement) — covered by the other six entries

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-error-codes`

- **Design Principles Covered**:

  - `cpt-cf-oagw-principle-rfc9457`

- **Design Constraints Covered**:

  - `cpt-cf-oagw-constraint-toolkit-deploy`
  - `cpt-cf-oagw-constraint-multi-sql`

- **Domain Model Entities**:
  - None (this entry establishes gear wiring and configuration; Upstream, Route, and Plugin are introduced by the entries that own them)

- **Design Components**:

  - `cpt-cf-oagw-component-model`

- **API**:
  - None (internal gear registration and router mounting; no dedicated management or proxy endpoint of its own)

- **Sequences**:
  - None

- **Data**:
  - None (no persisted or in-memory domain state is owned at this layer)

### 2.2 [Upstream Management](feature-upstream-management/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-upstream-management`

- **Purpose**: Provides CRUD management of upstream configurations — the fundamental tenant-scoped unit that every proxy request targets — including schema validation, alias derivation and resolution rules, enable/disable semantics, and hierarchical (bind/override) tenant scoping.

- **Implementation Approach**: single phase

- **Depends On**: `cpt-cf-oagw-feature-gear-foundation`

- **Scope**:
  - CRUD operations for upstream configuration (server endpoints, protocol, auth config reference, headers, rate limits, CORS, plugin bindings, tags)
  - Schema validation of upstream payloads, including endpoint `scheme` acceptance: per Overview override 2, the `scheme` field validation accepts `http` and `ws` in addition to the TLS-family schemes, independent of whether a plaintext connection is actually made at proxy time — a deliberate widening beyond the frozen `upstream.v1.schema.json` enum (`[https, wss, wt, grpc]`)
  - Alias derivation rules (hostname auto-derivation, common-suffix derivation with public-suffix validation, explicit-alias requirement for IP-based or non-derivable endpoints), alias normalization, and alias immutability-on-update rules
  - Enable/disable (`enabled`) semantics for upstreams, including ancestor-disables-descendant propagation
  - Tenant scoping of upstream resources, including bind-style creation against an ancestor's alias and the sharing-mode permission checks (`enforce`/`private`/`inherit`) that govern it

- **Out of scope**:
  - Alias resolution performed at proxy request time (tenant-hierarchy shadowing search from descendant to root) — covered by HTTP Request Proxying, which consumes the alias and derivation rules defined here
  - Actual plaintext (`http`/`ws`) connection establishment — gated by `allow_http_upstream` and covered by HTTP Request Proxying
  - Enable/disable semantics for routes - covered by Route Management
  - Persisted database storage — the graded configuration keeps upstream state in process memory (see Overview override 3); the entities, invariants, and uniqueness constraints match `cpt-cf-oagw-db-schema`'s persisted-deployment plan

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-upstream-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`
  - [x] `p2` - `cpt-cf-oagw-fr-alias-resolution`
  - [x] `p2` - `cpt-cf-oagw-fr-hierarchical-config`
  - [ ] `p1` - `cpt-cf-oagw-nfr-multi-tenancy`

- **Design Principles Covered**:

  - `cpt-cf-oagw-principle-tenant-scope`

- **Design Constraints Covered**:

  - `cpt-cf-oagw-constraint-https-only`

- **Domain Model Entities**:
  - Upstream
  - ServerConfig
  - Endpoint
  - AuthConfig
  - HeadersConfig
  - RateLimitConfig
  - CorsConfig
  - PluginsConfig

- **Design Components**:

  - `cpt-cf-oagw-component-model`

- **API**:
  - POST /oagw/v1/upstreams
  - GET /oagw/v1/upstreams
  - GET /oagw/v1/upstreams/{id}
  - PUT /oagw/v1/upstreams/{id}
  - DELETE /oagw/v1/upstreams/{id}

- **Sequences**:
  - None (no dedicated sequence diagram covers management CRUD flow; only the proxy request flow is diagrammed in DESIGN.md)

- **Data**:

  - `cpt-cf-oagw-db-schema`

  Realized as in-process memory state in the graded configuration (Overview override 3): the `oagw_upstream` and `oagw_upstream_tag` entities and the `UNIQUE(tenant_id, alias)` invariant are held in memory rather than in a database table.

### 2.3 [Route Management](feature-route-management/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-route-management`

- **Purpose**: Provides CRUD management of routes, which define the HTTP match rules (method, path, query allowlist) that map inbound proxy requests to specific upstream behaviors, and enforces the match-uniqueness invariant that keeps route matching deterministic.

- **Implementation Approach**: single phase

- **Depends On**: `cpt-cf-oagw-feature-gear-foundation`, `cpt-cf-oagw-feature-upstream-management`

- **Scope**:
  - CRUD operations for route configuration (upstream linkage, HTTP match rules, priority, route-level rate limit/CORS/plugin overrides, tags)
  - HTTP match rule validation: method allowlist, path pattern, query parameter allowlist, path-suffix mode
  - Match-uniqueness invariant: no two enabled routes under the same upstream may share `(path_prefix, priority)` for the same method
  - Upstream linkage validation: `upstream_id` must reference an upstream owned by the calling tenant; `upstream_id` is immutable after creation
  - Enable/disable (`enabled`) semantics for routes, including exclusion of disabled routes from route matching

- **Out of scope**:
  - gRPC match rules (`service`/`method` matching) — the gRPC match schema exists for future use, but PRD.md places gRPC proxying in a later phase and DESIGN.md states no gRPC proxy code path is implemented or reachable; this entry covers HTTP match rule CRUD and validation only, and gRPC match persistence is deferred alongside gRPC proxying itself
  - Route matching performed at proxy request time (longest-prefix match against an inbound request) — covered by HTTP Request Proxying, which consumes the match rules defined here
  - Enable/disable semantics for upstreams - covered by Upstream Management
  - Persisted database storage — the graded configuration keeps route state in process memory (see Overview override 3)

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-route-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`

- **Design Principles Covered**:

  - `cpt-cf-oagw-principle-tenant-scope`

- **Design Constraints Covered**:
  - None (no design constraint uniquely governs route management beyond those already covered by Gear Foundation and Configuration and Upstream Management)

- **Domain Model Entities**:
  - Route
  - MatchConfig
  - RateLimitConfig
  - CorsConfig
  - PluginsConfig

- **Design Components**:

  - `cpt-cf-oagw-component-model`

- **API**:
  - POST /oagw/v1/routes
  - GET /oagw/v1/routes
  - GET /oagw/v1/routes/{id}
  - PUT /oagw/v1/routes/{id}
  - DELETE /oagw/v1/routes/{id}

- **Sequences**:
  - None (no dedicated sequence diagram covers management CRUD flow; only the proxy request flow is diagrammed in DESIGN.md)

- **Data**:

  - `cpt-cf-oagw-db-schema`

  Realized as in-process memory state in the graded configuration (Overview override 3): the `oagw_route`, `oagw_route_http_match`, `oagw_route_method`, and `oagw_route_tag` entities are held in memory rather than in database tables; `oagw_route_grpc_match` is deferred with gRPC support.

### 2.4 [Plugin Catalog and Bindings](feature-plugin-management/) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-oagw-feature-plugin-management`

- **Purpose**: Maintains the catalog of built-in Auth, Guard, and Transform plugins, provides CRUD management of immutable custom plugin definitions, and validates plugin bindings on upstreams and routes, including the in-use conflict rule on delete.

- **Implementation Approach**: single phase

- **Depends On**: `cpt-cf-oagw-feature-gear-foundation`, `cpt-cf-oagw-feature-upstream-management`, `cpt-cf-oagw-feature-route-management`

- **Scope**:
  - Built-in plugin catalog: Auth (`noop`, `apikey`, `oauth2_client_cred`, `oauth2_client_cred_basic`, plus the catalog-only `basic`/`bearer` identifiers with no backing implementation), Guard (`required_headers`, plus the catalog-only `timeout`/`cors` identifiers that are core Data Plane logic, not plugin-bindable), and Transform (`request_id`, plus the catalog-only `logging`/`metrics` identifiers that are core instrumentation, not plugin-bindable)
  - Custom (tenant-defined) plugin definition CRUD: creation, listing, retrieval (including source retrieval), and deletion; plugin definitions are immutable after creation — updates are performed by creating a new plugin and re-binding references
  - Plugin binding validation on upstream and route `plugins.items`: identifier resolution (named vs. UUID-backed), schema-type matching, and ordered-position bookkeeping
  - In-use conflict handling: deletion of a plugin that is still referenced by an upstream or route binding is rejected

- **Out of scope**:
  - Sandboxed runtime execution of custom Starlark plugins (network/file I/O denial, timeout and memory enforcement during execution) — this entry covers catalog CRUD, definition storage, and binding validation only; sandboxed execution is deferred to the data-plane plugin-chain execution work that consumes these bindings, which is not yet part of any of this decomposition's entries
  - Garbage collection of unlinked plugins after TTL — described in DESIGN.md as a periodic job, deferred as an operational concern beyond this entry's CRUD and binding-validation scope
  - Execution of the plugin chain against a live proxy request (Auth -> Guards -> Transform ordering) — covered by Traffic Policy Enforcement, which invokes the plugins this entry catalogs and validates

- **Requirements Covered**:

  - [ ] `p2` - `cpt-cf-oagw-fr-plugin-system`
  - [ ] `p2` - `cpt-cf-oagw-fr-builtin-plugins`
  - [ ] `p3` - `cpt-cf-oagw-nfr-starlark-sandbox`

- **Design Principles Covered**:

  - `cpt-cf-oagw-principle-plugin-immutable`

- **Design Constraints Covered**:
  - None (no design constraint uniquely governs plugin cataloging and binding beyond those already covered elsewhere)

- **Domain Model Entities**:
  - Plugin

- **Design Components**:

  - `cpt-cf-oagw-component-model`

- **API**:
  - POST /oagw/v1/plugins
  - GET /oagw/v1/plugins
  - GET /oagw/v1/plugins/{id}
  - DELETE /oagw/v1/plugins/{id}
  - GET /oagw/v1/plugins/{id}/source

- **Sequences**:
  - None (no dedicated sequence diagram covers management CRUD flow; only the proxy request flow is diagrammed in DESIGN.md)

- **Data**:

  - `cpt-cf-oagw-db-schema`

  Realized as in-process memory state in the graded configuration (Overview override 3): the `oagw_plugin`, `oagw_upstream_plugin`, and `oagw_route_plugin` entities are held in memory rather than in database tables.

### 2.5 [HTTP Request Proxying](feature-proxy-http/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-proxy-http`

- **Purpose**: Executes the data-plane path for plain HTTP proxy requests: resolves the upstream by alias, matches the route, selects an endpoint, merges effective configuration, transforms headers, enforces body and timeout limits, forwards the request, and distinguishes gateway from upstream errors on the way back.

- **Implementation Approach**: two milestones, reflecting this entry's larger scope (see Overview): milestone 1 covers alias resolution, route matching, endpoint selection, and effective configuration merge (request routing and resolution); milestone 2 covers header transformation, body/timeout enforcement, outbound connection handling, and error-source distinction (request execution and error handling)

- **Depends On**: `cpt-cf-oagw-feature-upstream-management`, `cpt-cf-oagw-feature-route-management`

- **Scope**:
  - Alias resolution at request time: tenant-hierarchy search from descendant to root, closest-match shadowing, with enforced ancestor limits still applying across shadowing
  - Route matching against the inbound request (method allowlist, longest path-prefix match, query allowlist validation)
  - Endpoint selection within a multi-endpoint pool and `X-OAGW-Target-Host` handling (reading the header for routing, then stripping it) across HTTP/1.1 `Host` and HTTP/2 `:authority`
  - Effective configuration merge in priority order Upstream (base) < Route < Tenant, and hierarchical sharing-mode application (`private`/`inherit`/`enforce`) at request time
  - Header transformation: routing headers (consumed and stripped), hop-by-hop headers (stripped per HTTP spec), passthrough headers (forwarded per configuration), and simple set/add/remove rules from `upstream.headers`
  - Body validation and the 100MB hard body-size limit, enforced before buffering
  - Request/connection timeout enforcement using `proxy_timeout_secs`
  - Honoring `allow_http_upstream` and `ssrf_policy.enabled` when establishing the outbound connection (Overview override 2: scheme acceptance is Upstream Management's concern; whether a plaintext connection is actually made is this entry's concern)
  - Error-source distinction (`X-OAGW-Error-Source: gateway|upstream`) and RFC 9457 problem+json formatting for gateway-originated errors
  - Request logging with correlation IDs and metrics emission for the proxy request path

- **Out of scope**:
  - DNS resolution and IP-pinning rule implementation details for SSRF protection — PRD.md places these out of scope of the gear as a whole; this entry covers header stripping, request path/query validation against route configuration, and honoring the `ssrf_policy.enabled` gate, but not network-layer DNS/IP enforcement
  - Circuit breaker behavior — DESIGN.md lists circuit breaker config and fallback strategies under Future Developments as core (not yet designed) functionality; this entry surfaces the `CircuitBreakerOpen` error code but does not implement trip/reset logic
  - Automatic request retries — explicitly excluded by design principle; connector-level endpoint failover within a pool is permitted but is not a retry of the client's request
  - SSE, WebSocket, and other upgrade/streaming request handling — covered by Streaming and Upgrade Proxying
  - Rate limiting, CORS, required-headers guard enforcement, and auth credential injection — covered by Traffic Policy Enforcement, which wraps this entry's request path. This entry's contribution to `cpt-cf-oagw-nfr-input-validation` is path, query, and body-size validation; header-presence validation via the required-headers guard is Traffic Policy Enforcement's concern
  - gRPC request classification and proxying — deferred per PRD.md scope (planned for a later phase); no gRPC proxy code path is implemented or reachable

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-request-proxy`
  - [ ] `p1` - `cpt-cf-oagw-fr-header-transform`
  - [x] `p2` - `cpt-cf-oagw-fr-config-layering`
  - [x] `p2` - `cpt-cf-oagw-fr-alias-resolution`
  - [ ] `p1` - `cpt-cf-oagw-nfr-low-latency`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
  - [ ] `p1` - `cpt-cf-oagw-nfr-ssrf-protection`
  - [ ] `p1` - `cpt-cf-oagw-nfr-high-availability`
  - [ ] `p2` - `cpt-cf-oagw-nfr-observability`

- **Design Principles Covered**:

  - `cpt-cf-oagw-principle-no-retry`
  - `cpt-cf-oagw-principle-no-cache`
  - `cpt-cf-oagw-principle-error-source`

- **Design Constraints Covered**:

  - `cpt-cf-oagw-constraint-https-only`
  - `cpt-cf-oagw-constraint-body-limit`
  - `cpt-cf-oagw-constraint-no-direct-internet`

- **Domain Model Entities**:
  - Upstream (read-only resolution)
  - Route (read-only resolution)
  - ServerConfig
  - Endpoint

- **Design Components**:

  - `cpt-cf-oagw-component-model`

- **API**:
  - {METHOD} /oagw/v1/proxy/{alias}
  - {METHOD} /oagw/v1/proxy/{alias}/{path}

- **Sequences**:

  - `cpt-cf-oagw-seq-proxy-flow`

- **Data**:

  - `cpt-cf-oagw-db-schema`

  Read-only resolution against the in-memory upstream/route state maintained by Upstream Management and Route Management under the graded configuration's no-database posture (Overview override 3); no additional state is owned by this entry.

### 2.6 [Streaming and Upgrade Proxying](feature-proxy-streaming/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-proxy-streaming`

- **Purpose**: Extends the HTTP proxy path to server-sent-event stream proxying and WebSocket upgrade proxying, managing connection lifecycle (open/close/error) for long-lived and bidirectional connections.

- **Implementation Approach**: single phase

- **Depends On**: `cpt-cf-oagw-feature-proxy-http`

- **Scope**:
  - SSE (Server-Sent Events) response proxying: establishing the upstream connection, forwarding events as received, and managing open/close/error lifecycle
  - WebSocket upgrade proxying: forwarding the upgrade handshake and subsequent bidirectional frames, and managing connection lifecycle on both client and upstream disconnect
  - Reuse of alias resolution, route matching, header transformation, and error-source distinction from HTTP Request Proxying for the initial request/upgrade handshake

- **Out of scope**:
  - WebTransport session flows — named in `cpt-cf-oagw-fr-streaming`'s requirement text, but DESIGN.md contains no WebTransport design detail beyond the requirement statement and the `wt` scheme enum value; this entry covers SSE and WebSocket only, and WebTransport is deferred pending further design
  - gRPC streaming — deferred per PRD.md scope (planned for a later phase); no gRPC proxy code path is implemented or reachable
  - Rate limiting, CORS, and auth injection applied to streaming/upgrade connections — covered by Traffic Policy Enforcement

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-streaming`

- **Design Principles Covered**:

  - `cpt-cf-oagw-principle-error-source`

- **Design Constraints Covered**:
  - None (no design constraint uniquely governs streaming/upgrade proxying beyond those already covered by HTTP Request Proxying)

- **Domain Model Entities**:
  - None (reuses the Upstream, Route, and Endpoint resolution performed by HTTP Request Proxying; no new domain entities are introduced)

- **Design Components**:

  - `cpt-cf-oagw-component-model`

- **API**:
  - GET /oagw/v1/proxy/{alias}/{path} (SSE stream upgrade)
  - GET /oagw/v1/proxy/{alias}/{path} (WebSocket upgrade)

- **Sequences**:

  - `cpt-cf-oagw-seq-proxy-flow`

- **Data**:
  - None (connection lifecycle state is transient; no persisted or in-memory domain state beyond what HTTP Request Proxying already resolves)

### 2.7 [Traffic Policy Enforcement](feature-traffic-policy/) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-oagw-feature-traffic-policy`

- **Purpose**: Wraps the proxy request path with cross-cutting traffic policy: token-bucket rate limiting with configurable scopes and strategies, CORS preflight and actual-request handling, the required-headers guard plugin, and auth-plugin credential injection.

- **Implementation Approach**: single phase

- **Depends On**: `cpt-cf-oagw-feature-proxy-http`, `cpt-cf-oagw-feature-plugin-management`

- **Scope**:
  - Rate limiting: token-bucket (and sliding-window) evaluation against configured rate, window, capacity, and cost; scopes (global/tenant/user/IP/route); strategies (reject with 429 + `Retry-After`, queue, degrade); `X-RateLimit-*` response headers; hierarchical stricter-wins merge (`effective = min(ancestor.enforced, descendant)`) across the tenant chain
  - CORS: preflight `OPTIONS` handling (permissive response at the handler level, before upstream resolution) and actual-request origin/method validation against `upstream.cors`/`route.cors` after upstream resolution
  - Required-headers guard plugin: binding and enforcement of `required_headers` on upstream/route requests and responses
  - Auth credential injection: executing the bound Auth plugin (API Key, OAuth2 Client Credentials, OAuth2 Client Credentials with Basic auth, No-op) ahead of Guards and Transform in the plugin chain, retrieving credentials from the credential store by reference at request time, and applying the OAuth2 token cache (`token_cache_ttl_secs`, `token_cache_capacity` from ADR-0008)

- **Out of scope**:
  - HTTP Basic and Bearer auth plugin execution — `basic.v1` and `bearer.v1` are catalog-only GTS identifiers with no backing auth-plugin implementation in DESIGN.md's plugin catalog; binding either as `auth.plugin_type` is rejected, so this entry enforces that rejection rather than implementing the plugins
  - Distributed (e.g., Redis-backed) rate-limit counter synchronization across gear instances — DESIGN.md's ADR-0006 State Management scopes rate-limit state to in-memory per-instance counters for this design; cross-instance synchronization is not part of this entry
  - Auth plugin token refresh retrying the original failed upstream request — auth plugins may refresh tokens on 401, but the gateway does not re-issue the original client request
  - Path, query, and body-size input validation — covered by HTTP Request Proxying; this entry's contribution to `cpt-cf-oagw-nfr-input-validation` is limited to header-presence validation via the required-headers guard plugin

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-rate-limiting`
  - [ ] `p1` - `cpt-cf-oagw-fr-auth-injection`
  - [ ] `p1` - `cpt-cf-oagw-nfr-credential-isolation`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`

- **Design Principles Covered**:

  - `cpt-cf-oagw-principle-cred-isolation`

- **Design Constraints Covered**:
  - None (no design constraint uniquely governs traffic policy enforcement beyond those already covered by HTTP Request Proxying and Upstream Management)

- **Domain Model Entities**:
  - RateLimitConfig
  - CorsConfig
  - PluginsConfig

- **Design Components**:

  - `cpt-cf-oagw-component-model`

- **API**:
  - {METHOD} /oagw/v1/proxy/{alias}/{path} (rate limiting, CORS, guard, and auth-injection enforcement applied)
  - OPTIONS /oagw/v1/proxy/{alias}/{path} (CORS preflight)

- **Sequences**:

  - `cpt-cf-oagw-seq-proxy-flow`

- **Data**:
  - None (rate-limit counters and token cache entries are held in in-memory, per-instance state that is not part of the persisted-deployment schema in `cpt-cf-oagw-db-schema`)

---

## 3. Feature Dependencies

```text
cpt-cf-oagw-feature-gear-foundation
    ↓
    ├─→ cpt-cf-oagw-feature-upstream-management
    │       ↓
    │       ├─→ cpt-cf-oagw-feature-route-management
    │       │       ↓
    │       │       └─→ cpt-cf-oagw-feature-plugin-management
    │       │
    │       └─→ cpt-cf-oagw-feature-proxy-http (also depends on route-management)
    │               ↓
    │               ├─→ cpt-cf-oagw-feature-proxy-streaming
    │               │
    │               └─→ cpt-cf-oagw-feature-traffic-policy (also depends on plugin-management)
    │
    └─→ cpt-cf-oagw-feature-route-management (also depends on upstream-management)
```

**Dependency Rationale**:

- `cpt-cf-oagw-feature-upstream-management` requires `cpt-cf-oagw-feature-gear-foundation`: upstream CRUD executes inside the gear's configuration surface, shared state, and error-mapping conventions established by gear foundation.
- `cpt-cf-oagw-feature-route-management` requires `cpt-cf-oagw-feature-gear-foundation`: same gear-wiring dependency as upstream management.
- `cpt-cf-oagw-feature-route-management` requires `cpt-cf-oagw-feature-upstream-management`: every route validates and links to an `upstream_id` that must already exist and belong to the calling tenant.
- `cpt-cf-oagw-feature-plugin-management` requires `cpt-cf-oagw-feature-gear-foundation`: plugin CRUD executes inside the gear's wiring and error-mapping conventions.
- `cpt-cf-oagw-feature-plugin-management` requires `cpt-cf-oagw-feature-upstream-management` and `cpt-cf-oagw-feature-route-management`: plugin binding validation checks bindings against existing upstream and route `plugins.items`, and the in-use conflict check on delete scans both.
- `cpt-cf-oagw-feature-proxy-http` requires `cpt-cf-oagw-feature-upstream-management` and `cpt-cf-oagw-feature-route-management`: the data-plane request path resolves an upstream by alias and matches a route before it can forward anything.
- `cpt-cf-oagw-feature-proxy-streaming` requires `cpt-cf-oagw-feature-proxy-http`: SSE and WebSocket proxying reuse the alias resolution, route matching, and header transformation established for plain HTTP proxying, extending only the connection-lifecycle handling.
- `cpt-cf-oagw-feature-traffic-policy` requires `cpt-cf-oagw-feature-proxy-http`: rate limiting, CORS, and auth injection wrap the proxy request path and cannot be evaluated before a route and upstream are resolved.
- `cpt-cf-oagw-feature-traffic-policy` requires `cpt-cf-oagw-feature-plugin-management`: the required-headers guard plugin and the auth plugins it injects must already be cataloged and bindable before traffic policy can enforce them.
- `cpt-cf-oagw-feature-route-management` and `cpt-cf-oagw-feature-plugin-management` share `cpt-cf-oagw-feature-upstream-management` as a common prerequisite but do not depend on each other directly except through the ordering above; `cpt-cf-oagw-feature-proxy-streaming` and `cpt-cf-oagw-feature-traffic-policy` are independent of each other (one extends the connection type, the other adds policy enforcement) and can be developed in parallel once `cpt-cf-oagw-feature-proxy-http` is complete.
