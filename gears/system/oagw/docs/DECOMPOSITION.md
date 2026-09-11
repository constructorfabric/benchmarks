# Decomposition: Outbound API Gateway (OAGW)


<!-- toc -->

- [1. Overview](#1-overview)
- [2. Entries](#2-entries)
  - [2.1 Gear Foundation, Configuration and Error Model - HIGH](#21-gear-foundation-configuration-and-error-model---high)
  - [2.2 Upstream Management API - HIGH](#22-upstream-management-api---high)
  - [2.3 Route Management API - HIGH](#23-route-management-api---high)
  - [2.4 Plugin Management API - MEDIUM](#24-plugin-management-api---medium)
  - [2.5 Alias Resolution, Hierarchical Config Merge and Route Matching - HIGH](#25-alias-resolution-hierarchical-config-merge-and-route-matching---high)
  - [2.6 HTTP Proxy Data Plane - HIGH](#26-http-proxy-data-plane---high)
  - [2.7 Streaming and WebSocket Proxy - HIGH](#27-streaming-and-websocket-proxy---high)
  - [2.8 Plugin Runtime and Rate Limiting - MEDIUM](#28-plugin-runtime-and-rate-limiting---medium)
- [3. Feature Dependencies](#3-feature-dependencies)

<!-- /toc -->

**Overall implementation status:**
- [ ] `p1` - **ID**: `cpt-cf-oagw-status-oagw-implementation`
## 1. Overview

This decomposition breaks the OAGW technical design into eight ordered features. It moves from gear scaffolding and the error model, through control-plane CRUD for upstreams, routes, and plugins, into the data-plane concerns of configuration resolution, HTTP proxying, streaming, and plugin/rate-limit execution. Each feature builds only on the feature(s) it depends on, so the dependency chain also reflects a buildable delivery order: static configuration surfaces are established before the request-time behaviors that consume them.

The following controller-supplied overrides take precedence over PRD.md and DESIGN.md wherever the two disagree, and are reflected in the affected entries below:

1. **Gear-relative routing.** All management and proxy endpoints are registered gear-relative — `/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`, `/oagw/v1/proxy/{alias}/...` — not `/api/oagw/v1/...` as tabulated in PRD.md and DESIGN.md. The `/api` prefix belongs to an operator gateway sitting in front of this gear, and the api-gateway gear nests each gear's router under a `prefix_path` that is empty in the graded configuration, so this gear must not repeat any prefix itself. Every API list in this document uses the gear-relative form.
2. **`http` and `ws` are accepted upstream schemes, as a documented extension beyond the checked-in schema.** `upstream.v1.schema.json`'s `server.endpoints[].scheme` enum currently accepts only `https`, `wss`, `wt`, and `grpc` — that is the schema's enum as checked in, stated here as fact. This override additionally requires the management API to accept `http` and, as the symmetric plaintext counterpart of `wss`, `ws`, beyond that checked-in enum; both are explicitly labelled an extension, not a silent schema change. The default port is 80 for `http`/`ws` and 443 for the TLS family (`https`, `wss`, `wt`, `grpc`). `config/e2e-local.yaml` sets `oagw.config.allow_http_upstream: true`. DESIGN.md's `cpt-cf-oagw-constraint-https-only` describes the default posture, and this flag lifts it for the graded deployment. Two questions stay separate: which schemes `server.endpoints[].scheme` accepts is a validation-time question, decided by the upstream management surface as this documented superset of the schema enum; whether a plaintext connection is actually opened is a data-plane question, governed by `allow_http_upstream`.
3. **Proxying spans three transport modes.** "Proxying" in this decomposition means plain HTTP request/response, server-sent-event streams, and WebSocket upgrades together, not HTTP request/response alone.
4. **Control-plane persistence is in-process for this deployment.** The graded configuration declares no database for OAGW. Database-schema design IDs (`cpt-cf-oagw-db-schema`) are cited below only as informing the shape of the upstream/route/plugin entities and their invariants — per-tenant uniqueness, cascade delete, contiguous plugin binding positions — not because a SQL database is actually configured or queried in this deployment.
5. **Shared out-of-scope boundary.** Every entry below excludes: gRPC proxying at runtime (the upstream/route schemas accept gRPC protocol and match values, but no gRPC proxy code path is exercised); Starlark custom-plugin execution; the Redis L2 control-plane cache; and distributed, cross-node rate-limit counter synchronization.

## 2. Entries

Five cross-entry conventions apply throughout this section. The `pN` tag on a covered-requirement line is the priority of the feature entry that covers it, not necessarily the requirement's own priority recorded in PRD.md; PRD.md remains the canonical source for a requirement's own priority. A checked covered-requirement box mirrors that requirement's own `[x]` state in PRD.md — `cfs validate`'s `def-done-ref-not-done` check requires this — and does not assert that this feature itself has been implemented. Reference lines to upstream definitions that carry no checkbox in their defining document — design principles, constraints, components, interfaces, sequences, database-schema entries, and ADR IDs — are written without a checkbox here too, matching the source and satisfying `ref-task-def-no-task`. Each of the eight entries below is sized as a single implementation increment with no internal sub-phasing. Reference-count density differs across entries because control-plane CRUD features and data-plane execution features cite design material differently; this reflects feature nature, not a sizing imbalance, and scope-item counts across all entries stay within a 1.75x band.

### 2.1 [Gear Foundation, Configuration and Error Model](feature-gear-foundation/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-gear-foundation`

- **Purpose**: Establishes OAGW as a loadable gear with typed configuration and a uniform error contract, so every later feature has a crate, a mounted router, and a consistent failure format to build on. It fixes how the gear starts, how operators tune proxy timeout and plaintext/SSRF posture, and how every error response is shaped and attributed to gateway or upstream.

- **Depends On**: None

- **Scope**:
  - Crate skeleton and gear registration, including GTS type registration for the gear's schemas, so the gear can be loaded, started, and stopped by the platform runtime.
  - The gear's configuration section, covering proxy timeout, plaintext-upstream policy, and SSRF-guard toggling, resolved once at startup; the graded deployment sets the `proxy_timeout_secs`, `allow_http_upstream`, and `ssrf_policy.enabled` configuration keys.
  - REST capability wiring that mounts the gear's router so downstream features can register their own routes beneath it, gear-relative (override 1).
  - In-process control-plane state holding upstream, route, and plugin configuration for the life of the process; no database is configured for OAGW in this deployment (override 4).
  - The RFC 9457 `application/problem+json` error body shape, the error-type/HTTP-status table, and the `X-OAGW-Error-Source: gateway|upstream` response header.

- **Out of scope**:
  - Concrete management or proxy resource endpoints; this feature only mounts the router that later features attach to.
  - Prometheus metrics emission and structured audit logging (DESIGN.md §4.2–§4.3); this feature covers only the error-response contract, not request/response instrumentation.
  - Multi-SQL-backend persistence portability; no database is configured for OAGW in this deployment (override 4), so the multi-SQL constraint is cited only as informing why control-plane state is a single in-process structure rather than a SQL-backend abstraction.
  - gRPC proxying at runtime, Starlark custom-plugin execution, the Redis L2 cache, and distributed rate-limit synchronization (shared exclusion, Overview override 5).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-error-codes`
  - [ ] `p1` - `cpt-cf-oagw-contract-types-registry`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-rfc9457`
  - `p1` - `cpt-cf-oagw-principle-error-source`
  - `p1` - `cpt-cf-oagw-principle-tenant-scope`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-toolkit-deploy`
  - `p1` - `cpt-cf-oagw-constraint-multi-sql`
  - `p1` - `cpt-cf-oagw-constraint-https-only`

- **Domain Model Entities**:
  - Gear configuration section (proxy timeout, plaintext-upstream policy, SSRF-guard toggling)
  - Problem Details error response

- **Design Components**:

  - `p1` - `cpt-cf-oagw-component-model`
  - `p1` - `cpt-cf-oagw-design-overview`
  - `p1` - `cpt-cf-oagw-design-drivers`
  - `p1` - `cpt-cf-oagw-design-layers`
  - `p1` - `cpt-cf-oagw-design-dependencies`
  - `p1` - `cpt-cf-oagw-tech-dependencies`
  - `p1` - `cpt-cf-oagw-interface-api`
  - `p1` - `cpt-cf-oagw-adr-error-source-distinction`
  - `p1` - `cpt-cf-oagw-adr-request-routing`

- **API**:
  - None (this feature mounts the gear's router; concrete resource endpoints are added by the features that follow)

- **Sequences**:

  - None

- **Data**:

  - `p1` - `cpt-cf-oagw-db-schema`

### 2.2 [Upstream Management API](feature-upstream-management/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-upstream-management`

- **Purpose**: Gives platform operators and tenant administrators CRUD control over upstream definitions, the root configuration object every proxy request ultimately resolves to. It fixes alias derivation and per-tenant uniqueness so aliases can be trusted as stable routing keys in later features.

- **Depends On**: `cpt-cf-oagw-feature-gear-foundation`

- **Scope**:
  - CRUD operations at `/oagw/v1/upstreams` (gear-relative, override 1): create, list, get by ID, replace, delete.
  - Request validation against `upstream.v1.schema.json` (whose checked-in `scheme` enum is `https`, `wss`, `wt`, `grpc`), covering server endpoints, protocol, auth reference, headers, plugins, rate limit, and CORS fields, AND acceptance of `http` and `ws` as an extension beyond that checked-in enum (override 2) — a documented, deliberate superset of the schema enum, with default ports 80 for `http`/`ws` and 443 for the TLS family.
  - Alias derivation from hostname endpoints, ASCII-lowercase normalization with trailing-dot stripping, and rejection of a user-supplied alias that diverges from the derived value.
  - Per-tenant alias uniqueness (`tenant_id` + `alias`) and alias immutability once set, including the endpoint-change transitions that force delete-and-recreate instead of an in-place update.
  - Enable/disable semantics: a disabled upstream causes proxy requests to be rejected, and a descendant cannot re-enable an ancestor-disabled upstream.

- **Out of scope**:
  - Alias resolution against the tenant hierarchy and alias shadowing at request time (covered by config-resolution).
  - Whether a plaintext connection is actually opened to an `http` upstream; this feature only validates that the scheme is acceptable (override 2), the connection decision belongs to http-proxy.
  - Persistence is in-process for this deployment (override 4); the database-schema design is cited only as informing the upstream entity's shape and its per-tenant uniqueness invariant, not as an actual SQL table.
  - gRPC proxying at runtime, Starlark custom-plugin execution, the Redis L2 cache, and distributed rate-limit synchronization (shared exclusion, Overview override 5).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-upstream-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`
  - [x] `p1` - `cpt-cf-oagw-fr-alias-resolution`
  - [ ] `p1` - `cpt-cf-oagw-nfr-multi-tenancy`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
  - [ ] `p1` - `cpt-cf-oagw-usecase-configure-upstream`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-tenant-scope`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-https-only`

- **Domain Model Entities**:
  - Upstream
  - ServerConfig
  - Endpoint
  - AuthConfig
  - HeadersConfig
  - PluginsConfig
  - RateLimitConfig
  - CorsConfig

- **Design Components**:

  - `p1` - `cpt-cf-oagw-design-domain-model`
  - `p1` - `cpt-cf-oagw-component-model`
  - `p1` - `cpt-cf-oagw-interface-management-api`
  - `p1` - `cpt-cf-oagw-adr-request-routing`

- **API**:
  - POST /oagw/v1/upstreams
  - GET /oagw/v1/upstreams
  - GET /oagw/v1/upstreams/{id}
  - PUT /oagw/v1/upstreams/{id}
  - DELETE /oagw/v1/upstreams/{id}

- **Sequences**:

  - None

- **Data**:

  - `p1` - `cpt-cf-oagw-db-schema`

### 2.3 [Route Management API](feature-route-management/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-route-management`

- **Purpose**: Gives platform operators and tenant administrators CRUD control over routes, the objects that map an inbound method and path — or, at the schema level only, a gRPC service/method pair — to upstream-relative matching behavior. Each route belongs to exactly one upstream, fixed at creation.

- **Depends On**: `cpt-cf-oagw-feature-upstream-management`

- **Scope**:
  - CRUD operations at `/oagw/v1/routes` (gear-relative, override 1): create, list, get by ID, replace, delete.
  - Request validation against `route.v1.schema.json`, enforcing that `match` contains exactly one of `http` or `grpc`. The schema's top-level properties are `id, tags, upstream_id, match, plugins, rate_limit` only.
  - `upstream_id` reference checking on create, requiring the referenced upstream to belong to the calling tenant, and `upstream_id` immutability on replace.
  - Duplicate-match conflict detection: no two enabled routes under the same upstream may share the same method, path prefix, and priority. `enabled` and `priority` are route fields defined by DESIGN.md's Route domain model and required by `cpt-cf-oagw-fr-enable-disable`; neither appears in `route.v1.schema.json`'s properties, so this feature accepts and validates both as application-level fields beyond the checked-in schema, not as schema-validated fields.
  - Enable/disable semantics for routes: a disabled route is excluded entirely from route matching.

- **Out of scope**:
  - Route matching against inbound proxy requests at request time (covered by config-resolution and http-proxy).
  - gRPC match validation is accepted at the schema level (`service`/`method`), but no gRPC proxy code path is implemented or reachable; this feature only stores the configuration.
  - Persistence is in-process for this deployment (override 4); the database-schema design is cited only as informing the route entity's shape, its cascade-delete relationship to its upstream, and the match-key uniqueness invariant.
  - gRPC proxying at runtime, Starlark custom-plugin execution, the Redis L2 cache, and distributed rate-limit synchronization (shared exclusion, Overview override 5).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-route-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`
  - [ ] `p1` - `cpt-cf-oagw-nfr-multi-tenancy`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
  - [ ] `p1` - `cpt-cf-oagw-usecase-configure-route`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-tenant-scope`

- **Design Constraints Covered**:

  - None

- **Domain Model Entities**:
  - Route
  - MatchConfig (HTTP method allowlist, path, query allowlist, path-suffix mode; gRPC service/method at schema level only)

- **Design Components**:

  - `p1` - `cpt-cf-oagw-design-domain-model`
  - `p1` - `cpt-cf-oagw-component-model`
  - `p1` - `cpt-cf-oagw-interface-management-api`
  - `p1` - `cpt-cf-oagw-adr-request-routing`

- **API**:
  - POST /oagw/v1/routes
  - GET /oagw/v1/routes
  - GET /oagw/v1/routes/{id}
  - PUT /oagw/v1/routes/{id}
  - DELETE /oagw/v1/routes/{id}

- **Sequences**:

  - None

- **Data**:

  - `p1` - `cpt-cf-oagw-db-schema`

### 2.4 [Plugin Management API](feature-plugin-management/) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-oagw-feature-plugin-management`

- **Purpose**: Gives tenants a management surface for custom plugin definitions — creating, listing, inspecting, and deleting them, and retrieving their stored source — while keeping every plugin definition immutable once created. Built-in named plugins are not managed here; they are resolved from an in-process registry instead.

- **Depends On**: `cpt-cf-oagw-feature-gear-foundation`

- **Scope**:
  - Create, list, and get-by-ID operations at `/oagw/v1/plugins` (gear-relative, override 1); no replace operation, because plugin definitions are immutable after creation.
  - `GET /oagw/v1/plugins/{id}/source` for retrieving the stored source text of a custom plugin definition.
  - Delete at `/oagw/v1/plugins/{id}`, rejected with a plugin-in-use conflict when the plugin is still referenced by any upstream or route binding.
  - Storage of custom, UUID-backed plugin definitions, kept distinct from named built-in plugins, which are never stored here.

- **Out of scope**:
  - Executing any plugin (auth, guard, or transform) against a live request; this feature only manages plugin definitions and the referential integrity of their bindings.
  - Running the stored plugin source: Starlark custom-plugin execution is out of scope entirely for this decomposition (Overview override 5); this feature stores source text but never executes it.
  - Garbage collection of unlinked plugins after their TTL elapses is a lifecycle detail PRD.md marks out of scope and is not covered here.
  - Persistence is in-process for this deployment (override 4); the database-schema design is cited only as informing the plugin entity's shape and the plugin-in-use check across upstream/route bindings.
  - gRPC proxying at runtime, the Redis L2 cache, and distributed rate-limit synchronization (remaining shared exclusions, Overview override 5).

- **Requirements Covered**:

  - [ ] `p2` - `cpt-cf-oagw-fr-plugin-system`
  - [ ] `p2` - `cpt-cf-oagw-nfr-multi-tenancy`

- **Design Principles Covered**:

  - `p2` - `cpt-cf-oagw-principle-plugin-immutable`

- **Design Constraints Covered**:

  - None

- **Domain Model Entities**:
  - Plugin (plugin type, name, config schema, source code, last-used timestamp, GC-eligible timestamp)

- **Design Components**:

  - `p2` - `cpt-cf-oagw-design-domain-model`
  - `p2` - `cpt-cf-oagw-component-model`
  - `p2` - `cpt-cf-oagw-adr-plugin-system`

- **API**:
  - POST /oagw/v1/plugins
  - GET /oagw/v1/plugins
  - GET /oagw/v1/plugins/{id}
  - DELETE /oagw/v1/plugins/{id}
  - GET /oagw/v1/plugins/{id}/source

- **Sequences**:

  - None

- **Data**:

  - `p2` - `cpt-cf-oagw-db-schema`

### 2.5 [Alias Resolution, Hierarchical Config Merge and Route Matching](feature-config-resolution/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-config-resolution`

- **Purpose**: Turns a proxy request's alias and path into a fully merged, effective configuration ready for execution. It walks the tenant hierarchy to resolve the alias, merges upstream, route, and tenant settings by sharing mode, and matches the HTTP route, producing a plan the data plane executes without recomputing it on every request.

- **Depends On**: `cpt-cf-oagw-feature-route-management`

- **Scope**:
  - Alias lookup with descendant-to-root tenant hierarchy shadowing; the walk skips upstreams whose `enabled` field is false, so the closest enabled match wins, and enforced ancestor limits still apply across shadowing. When only disabled upstreams carry the alias, resolution reports an upstream-disabled outcome that http-proxy (2.6) renders as a `503` link-unavailable response.
  - Sharing modes `private`, `inherit`, and `enforce`, and the three-tier merge order Upstream < Route < Tenant — Tenant is the highest-priority layer — applied per configuration field (auth, rate limit, plugins, CORS).
  - Emitting the merged outputs once: the single merged rate limit and the plugin bindings concatenated as `upstream + route + tenant`, consumed as given by downstream features.
  - Tag union across the hierarchy: descendants may add tags but may never remove an ancestor's tags.
  - HTTP route matching: the inbound method plus the longest path prefix SELECT the route, and `path_suffix_mode` (`disabled` versus `append`) is carried into the resolved plan; a method or path matching no candidate route yields route-not-found, not a guard rejection.
  - Header transformation planning: computing the effective set/add/remove/passthrough rules from upstream and route configuration ahead of execution.
  - A resolved-config cache that avoids recomputing the alias resolution, merge, and match on every proxy request.

- **Out of scope**:
  - gRPC `(service, method)` route matching is a schema-accepted match type only; no gRPC proxy code path resolves or executes it.
  - Executing the resolved plan against the upstream service — building the outbound request, calling it, and mapping the response — belongs to http-proxy.
  - The Redis L2 shared control-plane cache layer is not implemented; only an in-process resolved-config cache is covered here.
  - Rejecting a request that fails a post-match guard — the query-parameter allowlist or the path-suffix mode, enforced as a guard rather than as a route selector — is covered by http-proxy (2.6), not here.
  - Starlark custom-plugin execution and distributed rate-limit synchronization (remaining shared exclusions, Overview override 5).

- **Requirements Covered**:

  - [x] `p1` - `cpt-cf-oagw-fr-alias-resolution`
  - [x] `p1` - `cpt-cf-oagw-fr-config-layering`
  - [x] `p1` - `cpt-cf-oagw-fr-hierarchical-config`
  - [ ] `p1` - `cpt-cf-oagw-fr-header-transform`
  - [ ] `p1` - `cpt-cf-oagw-fr-request-proxy`
  - [ ] `p1` - `cpt-cf-oagw-nfr-low-latency`

- **Design Principles Covered**:

  - None

- **Design Constraints Covered**:

  - None

- **Domain Model Entities**:
  - Effective (merged) upstream configuration (ADR-0006: `EffectiveUpstream`), including merged AuthConfig, HeadersConfig, RateLimitConfig, and CorsConfig
  - Effective (merged) route configuration (ADR-0006: `MatchedRoute`)
  - Resolved-config cache key (ADR-0006: `CacheKey`)

- **Design Components**:

  - `p1` - `cpt-cf-oagw-component-model`
  - `p1` - `cpt-cf-oagw-adr-request-routing`
  - `p1` - `cpt-cf-oagw-adr-data-plane-caching`
  - `p1` - `cpt-cf-oagw-adr-state-management`

- **API**:
  - None (internal resolution invoked by the proxy request path; not a directly addressable endpoint)

- **Sequences**:

  - `p1` - `cpt-cf-oagw-seq-proxy-flow`

- **Data**:

  - None (resolved configuration is held in an in-memory cache, not a persisted table)

### 2.6 [HTTP Proxy Data Plane](feature-http-proxy/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-http-proxy`

- **Purpose**: Executes plain-HTTP proxy requests end to end — validating and transforming the inbound request, forwarding it to the resolved upstream endpoint, and mapping the response or failure back to the caller with a clear gateway-versus-upstream error distinction. It is the data-plane feature every downstream proxying feature builds on.

- **Depends On**: `cpt-cf-oagw-feature-config-resolution`

- **Scope**:
  - `{METHOD} /oagw/v1/proxy/{alias}/{path}` for plain HTTP requests (gear-relative, override 1), including support for `http` as an accepted extension upstream scheme beyond the checked-in schema enum, and conditionally connectable per `allow_http_upstream` (override 2).
  - Guard rules: query-parameter allowlist and path-suffix mode enforcement, rejecting non-conforming requests before forwarding; method conformance is guaranteed by route selection in 2.5.
  - Rendering the upstream-disabled outcome reported by resolution as a `503` link-unavailable problem response, before any guard runs and without opening a connection.
  - The plaintext-connection policy governed by `allow_http_upstream`, covering both the `http` and `ws` endpoint schemes, and reused unchanged by the streaming entry (2.7) for `ws` upgrades.
  - Body validation, including the 100MB hard size limit rejected before buffering, and `Transfer-Encoding`/`Content-Length` consistency checks.
  - Hop-by-hop header stripping (`Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`) and `Host` (or `:authority`) rewriting to the upstream host.
  - `X-OAGW-Target-Host` handling and multi-endpoint pool selection, including the required-header case for common-suffix-alias upstreams.
  - Proxy timeouts and upstream-versus-gateway error mapping onto the RFC 9457 error table and `X-OAGW-Error-Source` header established by gear-foundation.
  - The built-in CORS handler: permissive-204 OPTIONS preflight handling, and origin/method enforcement on actual cross-origin requests.

- **Out of scope**:
  - Circuit-breaker enforcement: the `CircuitBreakerOpen` error type is mapped when applicable, but the breaker logic itself is future work (DESIGN.md §4.7) and is not implemented by this feature.
  - DNS resolution and IP-pinning implementation details are explicitly out of scope per PRD.md §4.2 and DESIGN.md §4.5; this feature covers only guard-rule-level request validation.
  - Server-sent-event and WebSocket handling (covered by streaming-proxy) and plugin chain execution beyond invoking it (covered by plugin-runtime).
  - gRPC proxying at runtime, Starlark custom-plugin execution, the Redis L2 cache, and distributed rate-limit synchronization (shared exclusion, Overview override 5).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-request-proxy`
  - [ ] `p1` - `cpt-cf-oagw-fr-header-transform`
  - [ ] `p1` - `cpt-cf-oagw-fr-error-codes`
  - [ ] `p1` - `cpt-cf-oagw-nfr-ssrf-protection`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
  - [ ] `p1` - `cpt-cf-oagw-nfr-high-availability`
  - [ ] `p1` - `cpt-cf-oagw-nfr-observability`
  - [ ] `p1` - `cpt-cf-oagw-usecase-proxy-request`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-no-retry`
  - `p1` - `cpt-cf-oagw-principle-no-cache`
  - `p1` - `cpt-cf-oagw-principle-rfc9457`
  - `p1` - `cpt-cf-oagw-principle-error-source`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-body-limit`
  - `p1` - `cpt-cf-oagw-constraint-https-only`
  - `p1` - `cpt-cf-oagw-constraint-no-direct-internet`

- **Domain Model Entities**:
  - Proxy request/response context
  - Guard rule set
  - Body validation outcome
  - CorsConfig (enforced allowed origins, methods, and headers)

- **Design Components**:

  - `p1` - `cpt-cf-oagw-component-model`
  - `p1` - `cpt-cf-oagw-interface-api`
  - `p1` - `cpt-cf-oagw-interface-proxy-api`
  - `p1` - `cpt-cf-oagw-adr-cors`
  - `p1` - `cpt-cf-oagw-adr-error-source-distinction`
  - `p1` - `cpt-cf-oagw-adr-request-routing`

- **API**:
  - {METHOD} /oagw/v1/proxy/{alias}/{path}
  - OPTIONS /oagw/v1/proxy/{alias}/{path} (CORS preflight)

- **Sequences**:

  - `p1` - `cpt-cf-oagw-seq-proxy-flow`

- **Data**:

  - None (data-plane execution produces no persisted table)

### 2.7 [Streaming and WebSocket Proxy](feature-streaming-proxy/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-streaming-proxy`

- **Purpose**: Extends the HTTP proxy data plane to long-lived connections, forwarding server-sent-event streams and relaying WebSocket frames bidirectionally, with connection lifecycle handling in both directions so neither side is left waiting on a half-closed link.

- **Depends On**: `cpt-cf-oagw-feature-http-proxy`

- **Scope**:
  - Server-sent-event passthrough: forwarding events as received, with open/close/error lifecycle handling that mirrors the upstream connection state.
  - Client-disconnect and upstream-close propagation for SSE: either side closing ends the other side's connection.
  - WebSocket upgrade proxying: negotiating the upgrade with the upstream and relaying frames bidirectionally for the life of the connection.
  - Close-frame propagation: a close initiated by either the client or the upstream ends the paired connection.

- **Out of scope**:
  - WebTransport session flows, which PRD.md's streaming requirement also names, are not implemented by any feature in this decomposition; the controller-supplied feature set and override 3 scope proxying to plain HTTP, SSE, and WebSocket only.
  - gRPC streaming is not covered; gRPC proxying is schema-accepted configuration only, with no runtime code path (Overview override 5).
  - Plugin execution during a streamed request (auth/guard/transform) is covered by plugin-runtime, not here; the plugin-invocation point is established once in http-proxy and reused unchanged by both streaming paths, so no direct streaming-proxy-to-plugin-runtime dependency edge is needed.
  - Starlark custom-plugin execution, the Redis L2 cache, and distributed rate-limit synchronization (remaining shared exclusions, Overview override 5).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-streaming`
  - [ ] `p1` - `cpt-cf-oagw-usecase-sse-streaming`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-no-retry`

- **Design Constraints Covered**:

  - None

- **Domain Model Entities**:
  - SSE stream session
  - WebSocket proxy session

- **Design Components**:

  - `p1` - `cpt-cf-oagw-component-model`
  - `p1` - `cpt-cf-oagw-interface-api`
  - `p1` - `cpt-cf-oagw-interface-proxy-api`
  - `p1` - `cpt-cf-oagw-adr-error-source-distinction`
  - `p1` - `cpt-cf-oagw-adr-request-routing`

- **API**:
  - GET /oagw/v1/proxy/{alias}/{path} (`Accept: text/event-stream`)
  - GET /oagw/v1/proxy/{alias}/{path} (WebSocket upgrade)

- **Sequences**:

  - `p1` - `cpt-cf-oagw-seq-proxy-flow`

- **Data**:

  - None

### 2.8 [Plugin Runtime and Rate Limiting](feature-plugin-runtime/) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-oagw-feature-plugin-runtime`

- **Purpose**: Executes the Auth → Guard → Transform plugin chain against each proxy request, injects credentials by reference, enforces the built-in required-header guard, propagates request IDs, and applies token-bucket rate limiting with standard response headers.

- **Depends On**: `cpt-cf-oagw-feature-http-proxy`, `cpt-cf-oagw-feature-plugin-management`

- **Scope**:
  - Plugin execution order: Auth, then Guards, then Transform(request), then the upstream call, then Transform(response/error); upstream-bound plugins execute before route-bound plugins.
  - Built-in auth plugins `noop`, `apikey`, and `oauth2_client_cred` (`Form` and `Basic` client-auth variants), all resolving credentials by `cred_store` reference only, never by value.
  - The built-in `required_headers` guard plugin, checking configured request and/or response header names for presence, independently per phase.
  - The built-in `request_id` transform plugin, propagating `X-Request-ID` on request and response.
  - Token-bucket rate limiting with configurable scope (global/tenant/user/IP/route), strategy, and per-request cost, rejecting with `429` plus `Retry-After` and `X-RateLimit-*` headers.

- **Out of scope**:
  - The sandbox for custom Starlark plugins is out of scope entirely for this decomposition (Overview override 5); only built-in Auth/Guard/Transform execution is covered here.
  - Retrying the upstream call after a 401, which the OAuth2 client-credentials ADR explicitly defers to a future iteration, is not implemented by this feature.
  - Distributed, cross-node rate-limit counter synchronization is out of scope; only per-instance token-bucket enforcement is covered (Overview override 5).
  - gRPC proxying at runtime and the Redis L2 control-plane cache (remaining shared exclusions, Overview override 5).

- **Requirements Covered**:

  - [ ] `p2` - `cpt-cf-oagw-fr-auth-injection`
  - [ ] `p2` - `cpt-cf-oagw-fr-rate-limiting`
  - [ ] `p2` - `cpt-cf-oagw-fr-plugin-system`
  - [ ] `p2` - `cpt-cf-oagw-fr-builtin-plugins`
  - [ ] `p2` - `cpt-cf-oagw-fr-header-transform`
  - [ ] `p2` - `cpt-cf-oagw-nfr-credential-isolation`
  - [ ] `p2` - `cpt-cf-oagw-nfr-starlark-sandbox`
  - [ ] `p2` - `cpt-cf-oagw-usecase-rate-limit-exceeded`
  - [ ] `p2` - `cpt-cf-oagw-contract-cred-store`

- **Design Principles Covered**:

  - `p2` - `cpt-cf-oagw-principle-cred-isolation`
  - `p2` - `cpt-cf-oagw-principle-no-retry`
  - `p2` - `cpt-cf-oagw-principle-plugin-immutable`
  - `p2` - `cpt-cf-oagw-principle-rfc9457`
  - `p2` - `cpt-cf-oagw-principle-error-source`

- **Design Constraints Covered**:

  - None

- **Domain Model Entities**:
  - Auth/Guard/Transform plugin bindings (PluginsConfig)
  - Rate limit configuration (RateLimitConfig: sustained rate, burst capacity, scope, strategy, cost)
  - Token bucket

- **Design Components**:

  - `p2` - `cpt-cf-oagw-component-model`
  - `p2` - `cpt-cf-oagw-adr-plugin-system`
  - `p2` - `cpt-cf-oagw-adr-rate-limiting`
  - `p2` - `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`
  - `p2` - `cpt-cf-oagw-adr-required-headers-guard-plugin`
  - `p2` - `cpt-cf-oagw-adr-state-management`
  - `p2` - `cpt-cf-oagw-design-domain-model`

- **API**:
  - None (plugin execution and rate limiting occur within the proxy request lifecycle established by http-proxy; no separate endpoint)

- **Sequences**:

  - `p2` - `cpt-cf-oagw-seq-proxy-flow`

- **Data**:

  - None

---

## 3. Feature Dependencies

```text
cpt-cf-oagw-feature-gear-foundation
    ↓
    ├─→ cpt-cf-oagw-feature-upstream-management
    │       ↓
    │       cpt-cf-oagw-feature-route-management
    │               ↓
    │               cpt-cf-oagw-feature-config-resolution
    │                       ↓
    │                       cpt-cf-oagw-feature-http-proxy
    │                               ├─→ cpt-cf-oagw-feature-streaming-proxy
    │                               └─→ cpt-cf-oagw-feature-plugin-runtime
    │                                       ↑
    └─→ cpt-cf-oagw-feature-plugin-management ────────┘
```

**Dependency Rationale**:

- `cpt-cf-oagw-feature-upstream-management` requires `cpt-cf-oagw-feature-gear-foundation`: it needs the mounted router, error model, and gear configuration section before it can expose CRUD endpoints.
- `cpt-cf-oagw-feature-route-management` requires `cpt-cf-oagw-feature-upstream-management`: routes validate and reference an existing `upstream_id`, so the upstream surface must exist first.
- `cpt-cf-oagw-feature-plugin-management` requires `cpt-cf-oagw-feature-gear-foundation`: it needs the mounted router and error model. The create/list/get/source endpoints build independently of the upstream/route CRUD surfaces, but the delete plugin-in-use check reads upstream and route plugin bindings and cannot be exercised end-to-end until those surfaces exist.
- `cpt-cf-oagw-feature-config-resolution` requires `cpt-cf-oagw-feature-route-management`: it merges and matches against upstream and route data that must already be creatable and readable.
- `cpt-cf-oagw-feature-http-proxy` requires `cpt-cf-oagw-feature-config-resolution`: it executes against the resolved, merged configuration and matched route that config-resolution produces.
- `cpt-cf-oagw-feature-streaming-proxy` requires `cpt-cf-oagw-feature-http-proxy`: it extends the same request lifecycle — guards, headers, error mapping — to long-lived connections.
- `cpt-cf-oagw-feature-plugin-runtime` requires `cpt-cf-oagw-feature-http-proxy`: it hooks into the proxy request lifecycle established there.
- `cpt-cf-oagw-feature-plugin-runtime` requires `cpt-cf-oagw-feature-plugin-management`: it resolves and executes plugin bindings that reference plugin definitions created there.
- `cpt-cf-oagw-feature-plugin-management` and the `cpt-cf-oagw-feature-upstream-management` / `cpt-cf-oagw-feature-route-management` / `cpt-cf-oagw-feature-config-resolution` chain are independent of each other and can be developed in parallel, since both trace back only to `cpt-cf-oagw-feature-gear-foundation`.
