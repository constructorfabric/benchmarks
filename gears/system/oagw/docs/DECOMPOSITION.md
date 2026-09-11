# Decomposition: Outbound API Gateway (OAGW)


<!-- toc -->

- [1. Overview](#1-overview)
  - [Decomposition Strategy](#decomposition-strategy)
  - [Parallelization Opportunities](#parallelization-opportunities)
  - [Mandatory Overrides for the Graded Configuration](#mandatory-overrides-for-the-graded-configuration)
  - [Scope Reality for the Graded Configuration](#scope-reality-for-the-graded-configuration)
- [2. Entries](#2-entries)
  - [2.1 Gear Foundation - HIGH](#21-gear-foundation---high)
  - [2.2 Resource Model and Store - HIGH](#22-resource-model-and-store---high)
  - [2.3 Upstream Management API - HIGH](#23-upstream-management-api---high)
  - [2.4 Route Management API - HIGH](#24-route-management-api---high)
  - [2.5 Plugin Management API - MEDIUM](#25-plugin-management-api---medium)
  - [2.6 Proxy Data Plane — HTTP - HIGH](#26-proxy-data-plane--http---high)
  - [2.7 Proxy Streaming - HIGH](#27-proxy-streaming---high)
  - [2.8 Policy and Plugins - HIGH](#28-policy-and-plugins---high)
- [3. Feature Dependencies](#3-feature-dependencies)

<!-- /toc -->

**Overall implementation status:**
- [ ] `p1` - **ID**: `cpt-cf-oagw-status-oagw`

## 1. Overview

`gears/system/oagw/oagw/` is the single Rust gear implementing OAGW, the outbound API
gateway. Its `src/lib.rs` is currently empty, so this DECOMPOSITION treats the build as a
from-scratch reconstruction against the supplied `PRD.md`, `DESIGN.md`, the nine ADRs, and
the two JSON Schemas (`upstream.v1.schema.json`, `route.v1.schema.json`). The plan below is
the ordered list that the downstream FEATURE-authoring and implementation work must follow.

### Decomposition Strategy

The eight features follow the natural build-up of the gear: first the gear shell and shared
error model that every later handler depends on, then the domain model and its tenant-scoped
store, then the three management REST surfaces (upstream, route, plugin) in the order their
foreign-key-like references require, then the data-plane proxy path for plain HTTP, then
streaming upgrades on top of that path, and finally the cross-cutting policy layer (auth
injection, rate limiting, built-in plugins, CORS, hierarchical configuration) that decorates
proxied requests. Each feature is scoped so it can be implemented and tested on its own once
its dependencies exist, and each carries the full set of PRD/DESIGN/ADR identifiers it is
responsible for.

Feature 2.8 (Policy and Plugins) is deliberately larger than its seven siblings. It is the
cross-cutting policy layer applied on top of an already-working proxy path, and it spans ADR
0003 (rate limiting), ADR 0004 (CORS), ADR 0008 (OAuth2 client-credentials auth plugin), and
ADR 0009 (required-headers guard plugin). It is expected to be implemented as several
independently testable slices inside one feature, not as a single atomic change.

Three conventions apply across this document. Per-feature phase and milestone breakdown is
intentionally out of scope at this artifact level. That detail belongs to the FEATURE artifacts
written for each entry below. Feature headings use the template's plain priority suffix, for
example `- HIGH`, rather than the kit example's emoji convention. A reference line below carries
a checkbox only when the identifier it cites is a tracked task in its own source document. A
reference to a plain design element, one with no task checkbox in its definition, carries no
checkbox here either.

### Parallelization Opportunities

- `cpt-cf-oagw-feature-plugin-management-api` and `cpt-cf-oagw-feature-proxy-data-plane-http`
  both depend only on `cpt-cf-oagw-feature-route-management-api` and do not depend on each
  other; they can be built in parallel once route management lands.
- `cpt-cf-oagw-feature-policy-and-plugins` is the join point for that parallel work — it needs
  both the plugin catalog and the working data-plane proxy, so it cannot start until both are
  done.
- `cpt-cf-oagw-feature-proxy-streaming` only extends the proxy path built in
  `cpt-cf-oagw-feature-proxy-data-plane-http`; it does not need the plugin or policy work and
  could be pulled forward in parallel with `cpt-cf-oagw-feature-plugin-management-api` if
  staffing allows, though the dependency list below keeps it strictly after the HTTP proxy for
  clarity.

### Mandatory Overrides for the Graded Configuration

The graded deployment differs from what `PRD.md` and `DESIGN.md` describe in four ways. Where
the supplied documents and this section disagree, this section governs. The supplied documents
are left unmodified because they describe how the component's original authors deployed it,
not how this build is graded.

**Override 1 — route paths are gear-relative, with no `/api` prefix.** `PRD.md` and
`DESIGN.md` tabulate every management and proxy path as `/api/oagw/v1/...`. That form is the
absolute path behind an operator gateway whose `prefix_path` (the path segment the
`api-gateway` gear puts in front of every route it hosts) is `/api`. In this platform, each
gear registers its own paths, and the `api-gateway` gear nests one global `prefix_path` over
the whole assembled router. This nesting happens via `apply_prefix` in `api-gateway`'s
`gear.rs`. The graded configuration leaves `prefix_path` empty, so the real paths this build
must serve are `/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`, and
`/oagw/v1/proxy/{alias}` — never `/api/oagw/v1/...`. Every feature's API bullets below use the
`/oagw/v1/...` form.

**Override 2 — `http` and `ws` are additional legal endpoint schemes, kept separate from
whether a plaintext call is actually made.** The supplied schema
(`upstream.v1.schema.json`) defines the `scheme` enum as exactly four values: `https`, `wss`,
`wt`, and `grpc`. `config/e2e-local.yaml` sets `oagw.config.allow_http_upstream: true`.
`DESIGN.md`'s `cpt-cf-oagw-constraint-https-only` describes the gateway's default posture
(HTTPS-only, plaintext blocked); it does not describe the graded configuration. Because the
graded flag lifts that default posture, this build adds `http` as a fifth accepted `scheme`
value — a deliberate, named extension beyond the supplied schema, never a restatement of what
the supplied schema already contains. The implementation additionally tolerates `ws` as the
plaintext counterpart of `wss`, so a plaintext WebSocket upstream can be declared under the
same flag; this is likewise a deliberate extension, not part of the supplied enum. Two layers
must stay distinct: (a) which schemes the upstream `scheme` field accepts when an upstream is
created — `http` and `ws` must be accepted alongside the four supplied values, so a create
request naming either is not rejected by schema validation; and (b) whether OAGW actually opens
a plaintext connection to an upstream — that is governed solely by `allow_http_upstream`, which
is `true` in the graded configuration. When the flag is unset or `false`, the default
HTTPS-only posture from `cpt-cf-oagw-constraint-https-only` applies and plaintext connection
attempts are rejected even if the stored scheme is `http` or `ws`.

**Override 3 — proxying covers plain HTTP, SSE, and WebSocket alike.** The proxy path is not
"HTTP requests, and separately, streaming." A single proxy endpoint must serve ordinary
request/response calls, Server-Sent-Events (SSE, a one-way streaming response format) and
WebSocket upgrade negotiation, all through the same alias- and route-resolution logic.
`cpt-cf-oagw-feature-proxy-data-plane-http` and `cpt-cf-oagw-feature-proxy-streaming` split the
work but must compose into one coherent endpoint family.

**Override 4 — automated tests are part of the definition of done, and live in the crate, not
in the component's acceptance suite.** `testing/e2e/gears/oagw/` is reserved for the
component's own end-to-end acceptance tests and must not receive gear-level tests from this
build. Every feature below is expected to ship with inline `#[cfg(test)]` modules and/or tests
under `gears/system/oagw/oagw/tests/` covering its behavior; this applies uniformly across all
eight features and is not repeated per feature.

### Scope Reality for the Graded Configuration

The graded server runs `config/e2e-local.yaml` with `config/e2e-features.txt`. Under that
configuration:

- No database is configured for `oagw`. Persistence is a tenant-scoped in-memory store, not
  SQL migrations. `cpt-cf-oagw-db-schema` and `cpt-cf-oagw-constraint-multi-sql` are covered as
  an in-memory store that honors the documented invariants — `(tenant_id, alias)` uniqueness,
  tenant scoping on every read/write, and the anonymous GTS (Global Type System)
  resource-identifier pattern (`gts.cf.core.oagw.{type}.v1~{uuid}`) — rather than as
  PostgreSQL/MySQL/SQLite migrations.
- gRPC upstreams, Starlark custom-plugin execution, WebTransport session flows, and the
  optional Redis L2 config-cache have no enabled runtime dependency in this deployment. Each is
  declared out of scope under the features that actually own it: gRPC dispatch under
  `cpt-cf-oagw-feature-resource-model-and-store`, `cpt-cf-oagw-feature-route-management-api`,
  and `cpt-cf-oagw-feature-proxy-data-plane-http`; Starlark execution under
  `cpt-cf-oagw-feature-plugin-management-api` and `cpt-cf-oagw-feature-policy-and-plugins`;
  WebTransport under `cpt-cf-oagw-feature-proxy-streaming`; and the Redis L2 cache under
  `cpt-cf-oagw-feature-policy-and-plugins`. The documented API surface for each still validates
  and rejects unsupported requests coherently (e.g., a 501/400-class response) rather than
  panicking or silently mis-routing.
- The `oagw` config block present in `config/e2e-local.yaml` is exactly: `proxy_timeout_secs:
  2`, `allow_http_upstream: true`, `ssrf_policy.enabled: false`. The last setting means SSRF
  (Server-Side Request Forgery) protection logic must exist per `cpt-cf-oagw-nfr-ssrf-protection`,
  but its runtime enforcement is toggled off in this deployment; the feature that owns it
  states this explicitly rather than assuming the policy is always active.
- `cpt-cf-oagw-fr-alias-resolution`, `cpt-cf-oagw-fr-config-layering`, and
  `cpt-cf-oagw-fr-hierarchical-config` are checked (`[x]`) below only because `PRD.md` marks
  their source definitions done, and the validator rule `def-done-ref-not-done` forces a
  reference to match its definition's checkbox state. No code implements any of the three in
  this build. Implementers must treat all three as fully unbuilt; the checkbox state is a
  validator artifact, not a claim of working code.

## 2. Entries

### 2.1 [Gear Foundation](feature-gear-foundation/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-gear-foundation`

- **Purpose**: Registers the `oagw` gear with the host runtime, deserializes `OagwConfig` from
  the `oagw.config` YAML block (`proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy`),
  mounts the base router at `/oagw/v1` (Override 1), and establishes the shared error model
  every later handler uses: RFC 9457 (`application/problem+json`) Problem Details with GTS
  error-type identifiers, plus an `X-OAGW-Error-Source: gateway|upstream`
  header on every response. It also performs GTS schema registration for `cpt-cf-oagw-actor-types-registry`
  at startup so later features can register their own type schemas.

- **Depends On**: None

- **Scope**:
  - Gear registration, lifecycle wiring, and `OagwConfig` deserialization from the `oagw.config` block.
  - Module skeleton for the Control Plane / Data Plane split described in `cpt-cf-oagw-design-layers`.
  - Base router mounted at `/oagw/v1` per Override 1; no resource endpoints live here yet.
  - Shared RFC 9457 Problem Details error type carrying `type`, `title`, `status`, `detail`,
    `instance`, and the OAGW extension fields (`upstream_id`, `host`, `path`,
    `retry_after_seconds`, `trace_id`).
  - `X-OAGW-Error-Source: gateway|upstream` header attached to every response, gateway or
    passthrough, per `cpt-cf-oagw-adr-error-source-distinction`.
  - Registration of the gear's external dependency handles (`types_registry`, `cred_store`,
    `api_ingress`, `toolkit-db`, `toolkit-auth`) even where a given dependency's data path is
    unused in this deployment (e.g., no database configured).
  - Inbound Bearer token authentication via `toolkit-auth`, wired as the shared permission-check
    mechanism that every route registered by this gear, in every later feature, reuses. The
    graded configuration's static auth stack resolves most checks to an always-pass state, but
    the gate itself must still exist and run on every request.

- **Out of scope**:
  - Any concrete resource CRUD or proxy handler logic (later features).
  - `toolkit-db` migrations — no database is configured in the graded deployment (Scope Reality).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-error-codes`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-rfc9457`
  - `p1` - `cpt-cf-oagw-principle-error-source`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-toolkit-deploy`

- **Domain Model Entities**:
  - None

- **Design Components**:

  - `p1` - `cpt-cf-oagw-design-layers`
  - `p1` - `cpt-cf-oagw-tech-dependencies`
  - `p1` - `cpt-cf-oagw-design-drivers`
  - `p1` - `cpt-cf-oagw-design-dependencies`
  - `p1` - `cpt-cf-oagw-interface-api`
  - `p1` - `cpt-cf-oagw-adr-error-source-distinction`

- **API**:
  - Base router mounted at `/oagw/v1` (Override 1). No resource endpoints; subsequent
    features register their paths under this mount.

- **Sequences**:

  - None

- **Data**:

  - None

### 2.2 [Resource Model and Store](feature-resource-model-and-store/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-resource-model-and-store`

- **Purpose**: Defines the Upstream, Route, and Plugin domain entities matching
  `upstream.v1.schema.json` and `route.v1.schema.json` field for field, the request/response
  DTOs and schema-level validation for them, alias derivation and the alias pattern, and the
  tenant-scoped in-memory store that later CRUD and proxy features read and write through.

- **Depends On**: `cpt-cf-oagw-feature-gear-foundation`

- **Scope**:
  - `Upstream`, `Route`, `Plugin`, `ServerConfig`, `Endpoint`, `AuthConfig`, `HeadersConfig`,
    `RateLimitConfig`, `CorsConfig`, and `PluginsConfig` domain types matching the JSON Schemas.
  - Schema-level validation: required fields, enums (including `scheme` accepting the supplied
    `https`, `wss`, `wt`, `grpc` values plus the `http` and `ws` extensions added by Override 2),
    the alias pattern `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`, and the tag pattern
    `^[a-z0-9_-]+$`.
  - Alias derivation rules: hostname-based endpoints auto-derive (single hostname, or common
    registrable suffix across multiple hostnames), IP-based or non-derivable endpoints require
    an explicit alias, normalization to ASCII lowercase with trailing dots stripped, and
    alias immutability once set.
  - Tenant-scoped in-memory store (Scope Reality: no database is configured) enforcing
    `(tenant_id, alias)` uniqueness for upstreams, match-rule uniqueness for routes, and the
    anonymous GTS resource-identifier pattern (`gts.cf.core.oagw.{type}.v1~{uuid}`) for all
    three entity types.
  - Tenant-hierarchy walk primitive (descendant-to-root) used later by alias resolution and
    route matching.

- **Out of scope**:
  - gRPC-specific request dispatch — the `protocol` and `match.grpc` schema fields are
    accepted and validated, but no gRPC proxy code path exists in this build (Scope Reality).
  - `toolkit-db` / SeaORM migrations — persistence here is the in-memory store described above,
    not SQL (Scope Reality, `cpt-cf-oagw-constraint-multi-sql`).

- **Requirements Covered**:

  - [x] `p2` - `cpt-cf-oagw-fr-alias-resolution`
  - [ ] `p1` - `cpt-cf-oagw-nfr-multi-tenancy`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-tenant-scope`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-multi-sql`

- **Domain Model Entities**:
  - Upstream
  - Route
  - Plugin
  - ServerConfig
  - Endpoint

- **Design Components**:

  - `p1` - `cpt-cf-oagw-design-domain-model`

- **API**:
  - None. This feature is the domain/store layer consumed by the REST features that follow.

- **Sequences**:

  - None

- **Data**:

  - `p1` - `cpt-cf-oagw-db-schema`

### 2.3 [Upstream Management API](feature-upstream-management-api/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-upstream-management-api`

- **Purpose**: Exposes CRUD (create/read/update/delete) over upstreams for
  `cpt-cf-oagw-actor-platform-operator` and `cpt-cf-oagw-actor-tenant-admin`, enforcing
  create/replace/delete semantics, alias uniqueness, immutable fields, and tenant scoping so
  every proxy request in later features has a configured target to resolve against.

- **Depends On**: `cpt-cf-oagw-feature-resource-model-and-store`

- **Scope**:
  - `POST /oagw/v1/upstreams`, `GET /oagw/v1/upstreams`, `GET /oagw/v1/upstreams/{id}`,
    `PUT /oagw/v1/upstreams/{id}`, `DELETE /oagw/v1/upstreams/{id}` (Override 1 paths).
  - Create: server-generated UUID, alias auto-derivation or explicit-alias validation, alias
    conflict within tenant returns `409 Conflict`; a create request whose endpoints use scheme
    `http` or `ws` is accepted at validation time regardless of `allow_http_upstream` (Override 2).
  - Replace: full-document replacement, `id`/`tenant_id` immutable, alias recomputed only when
    derivable and unchanged, otherwise the request is rejected (`400 Validation`).
  - Delete and enable/disable (`enabled` boolean, default `true`): a disabled upstream causes
    proxy requests to be rejected with `503 Service Unavailable`; ancestor-disabled upstreams
    cannot be re-enabled by a descendant.
  - List query parameters: OData `$filter`, `$select`, `$orderby`, `$top` (default 50, max
    100), `$skip`.
  - Tenant scoping: ancestor upstreams are invisible (`404`) through this management surface
    even though they remain reachable at proxy time via the tenant-hierarchy walk.
  - Per-endpoint permission gates on every CRUD route:
    `gts.cf.core.oagw.upstream.v1~:{create;override;read;delete}`. The ancestor-alias "bind"
    path — creating an upstream whose alias matches an ancestor's — additionally requires the
    `oagw:upstream:bind` permission on top of `create`.

- **Out of scope**:
  - Route and plugin CRUD (separate features below).
  - Enforcing whether a plaintext connection is actually opened to an `http`- or `ws`-scheme
    upstream — that runtime behavior belongs to the data-plane proxy feature, gated by
    `allow_http_upstream` (Override 2).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-upstream-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`
  - [ ] `p1` - `cpt-cf-oagw-usecase-configure-upstream`

- **Design Principles Covered**:

  - None

- **Design Constraints Covered**:

  - None

- **Domain Model Entities**:
  - Upstream

- **Design Components**:

  - [ ] `p1` - `cpt-cf-oagw-interface-management-api`

- **API**:
  - `POST /oagw/v1/upstreams`
  - `GET /oagw/v1/upstreams`
  - `GET /oagw/v1/upstreams/{id}`
  - `PUT /oagw/v1/upstreams/{id}`
  - `DELETE /oagw/v1/upstreams/{id}`

- **Sequences**:

  - None

- **Data**:

  - None

### 2.4 [Route Management API](feature-route-management-api/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-route-management-api`

- **Purpose**: Exposes CRUD over routes, the matching rules that map an inbound proxy request
  to a specific upstream behavior, so `cpt-cf-oagw-actor-platform-operator` and
  `cpt-cf-oagw-actor-tenant-admin` can define which methods, paths, and query parameters are
  reachable on each upstream before proxying goes live.

- **Depends On**: `cpt-cf-oagw-feature-upstream-management-api`

- **Scope**:
  - `POST /oagw/v1/routes`, `GET /oagw/v1/routes`, `GET /oagw/v1/routes/{id}`,
    `PUT /oagw/v1/routes/{id}`, `DELETE /oagw/v1/routes/{id}` (Override 1 paths).
  - Create: `upstream_id` must exist and belong to the calling tenant (ancestor upstreams are
    not directly addressable here), `match` must contain exactly one of `http` or `grpc`,
    match-rule uniqueness within the upstream (same path + priority + method → `409 Conflict`).
  - Replace: `upstream_id` is immutable and not present in the update DTO; match-rule
    uniqueness is re-validated.
  - List query parameters mirroring the upstream surface (`$filter`, `$select`, `$orderby`,
    `$top`, `$skip`).
  - Per-endpoint permission gates on every CRUD route:
    `gts.cf.core.oagw.route.v1~:{create;override;read;delete}`.

- **Out of scope**:
  - gRPC match dispatch at proxy time — the `match.grpc` shape is validated and stored, but no
    gRPC request is ever routed against it in this build (Scope Reality).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-route-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-usecase-configure-route`

- **Design Principles Covered**:

  - None

- **Design Constraints Covered**:

  - None

- **Domain Model Entities**:
  - Route

- **Design Components**:

  - None

- **API**:
  - `POST /oagw/v1/routes`
  - `GET /oagw/v1/routes`
  - `GET /oagw/v1/routes/{id}`
  - `PUT /oagw/v1/routes/{id}`
  - `DELETE /oagw/v1/routes/{id}`

- **Sequences**:

  - None

- **Data**:

  - None

### 2.5 [Plugin Management API](feature-plugin-management-api/) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-oagw-feature-plugin-management-api`

- **Purpose**: Exposes the create/read/delete/source-fetch surface for custom (Starlark-authored)
  plugin definitions, respecting plugin immutability — there is no update endpoint — and
  guarding deletion so a plugin still referenced by an upstream or route cannot be removed out
  from under them.

- **Depends On**: `cpt-cf-oagw-feature-route-management-api`

- **Scope**:
  - `POST /oagw/v1/plugins`, `GET /oagw/v1/plugins`, `GET /oagw/v1/plugins/{id}`,
    `DELETE /oagw/v1/plugins/{id}`, `GET /oagw/v1/plugins/{id}/source` (Override 1 paths).
  - No `PUT` — plugins are immutable after creation; changes are made by creating a new plugin
    and re-binding upstream/route references to it.
  - `DELETE` returns `204 No Content` when the plugin is unreferenced, and `409 Conflict`
    (`PluginInUse`) when referenced, with the reference list (`referenced_by.upstreams`,
    `referenced_by.routes`) in the RFC 9457 problem body.
  - Plugin identification and storage per the GTS plugin-identification model: named
    (built-in) plugins resolved via in-process registry and never stored; custom plugins
    stored with `id = {uuid}` and both `plugin_ref` and `plugin_uuid` recorded.
  - Reference tracking across `oagw_upstream_plugin`, `oagw_route_plugin` bindings, and the
    scalar `auth_plugin_ref`/`auth_plugin_uuid` columns on upstreams, so the in-use check does
    not require scanning arbitrary JSON.

- **Out of scope**:
  - Starlark sandboxed execution of custom plugin source (no network I/O, no file I/O, no
    imports, timeout/memory limits) — this build stores and serves plugin definitions but has
    no enabled runtime to execute them (Scope Reality). The catalog of built-in, non-Starlark
    plugin identifiers is covered instead by `cpt-cf-oagw-feature-policy-and-plugins`.
  - Time-based garbage collection of unlinked plugins (`gc_eligible_at` sweep) — deferred; the
    reference-tracking check above is still enforced synchronously on delete.

- **Requirements Covered**:

  - [ ] `p2` - `cpt-cf-oagw-fr-plugin-system`

- **Design Principles Covered**:

  - `p2` - `cpt-cf-oagw-principle-plugin-immutable`

- **Design Constraints Covered**:

  - None

- **Domain Model Entities**:
  - Plugin

- **Design Components**:

  - `p2` - `cpt-cf-oagw-adr-plugin-system`

- **API**:
  - `POST /oagw/v1/plugins`
  - `GET /oagw/v1/plugins`
  - `GET /oagw/v1/plugins/{id}`
  - `DELETE /oagw/v1/plugins/{id}`
  - `GET /oagw/v1/plugins/{id}/source`

- **Sequences**:

  - None

- **Data**:

  - None

### 2.6 [Proxy Data Plane — HTTP](feature-proxy-data-plane-http/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-proxy-data-plane-http`

- **Purpose**: Implements the core proxy path for plain HTTP requests from
  `cpt-cf-oagw-actor-app-developer` to an external `cpt-cf-oagw-actor-upstream-service`: alias
  resolution, route matching, guard checks, header transformation, and forwarding, with no
  automatic retry of the client's request.

- **Depends On**: `cpt-cf-oagw-feature-route-management-api`

- **Scope**:
  - `{METHOD} /oagw/v1/proxy/{alias}[/{path}][?{query}]` (Override 1 path form).
  - Authorization as step 1 of every proxy request: the `gts.cf.core.oagw.proxy.v1~:invoke`
    permission check runs before the tenant-chain alias walk and route match described below. In
    the graded configuration's static auth stack this check is trivially satisfied, but it still
    executes on every request.
  - Alias resolution walking the tenant hierarchy descendant-to-root, closest match wins
    (shadowing); disabled upstream → `503 Service Unavailable`.
  - Route matching by method and longest path prefix for HTTP upstreams; guard rules (method
    allowlist, query allowlist, `path_suffix_mode: disabled|append`).
  - Body validation: `Content-Length` consistency, the 100MB hard limit (`413 PayloadTooLarge`
    before buffering), and rejection of unsupported `Transfer-Encoding` values.
  - Header handling per the three categories in DESIGN.md §3.2 "Headers Transformation"
    (routing, hop-by-hop, passthrough): this feature implements only the non-configurable
    behaviour — routing-header consumption and stripping (`X-OAGW-Target-Host`) and hop-by-hop
    header stripping (see DESIGN.md §3.2 for the full hop-by-hop header list) — plus `Host`
    (HTTP/1.1) and `:authority` (HTTP/2) rewriting to the upstream's endpoint.
  - `X-OAGW-Target-Host` multi-endpoint behavior: optional for single-endpoint or
    explicit-alias pools (round-robin if absent), required when the alias was derived from a
    common hostname suffix.
  - Per-request timeout from `OagwConfig.proxy_timeout_secs` (`2` in the graded configuration);
    no automatic re-issuing of the client's request on failure, per
    `cpt-cf-oagw-principle-no-retry`.
  - Upstream error passthrough: the upstream's response body and status are forwarded
    unchanged, tagged `X-OAGW-Error-Source: upstream`, distinct from gateway-originated errors
    tagged `gateway`.
  - Scheme policy split (Override 2): the stored `scheme` may be `http` or `ws`; whether OAGW
    opens a plaintext connection is controlled solely by `allow_http_upstream` (`true` in the
    graded configuration). SSRF (Server-Side Request Forgery) protections per
    `cpt-cf-oagw-nfr-ssrf-protection` — DNS/IP validation, well-known header stripping, path
    and query validation against route configuration — must exist, but their runtime
    enforcement is disabled in this deployment (`ssrf_policy.enabled: false`); the code must
    still evaluate coherently with the policy off, not skip its own guard checks.

- **Out of scope**:
  - gRPC request classification and forwarding (Scope Reality — no gRPC proxy code path).
  - SSE and WebSocket upgrade handling (`cpt-cf-oagw-feature-proxy-streaming`).
  - Auth-plugin credential injection, rate limiting, and built-in CORS
    (`cpt-cf-oagw-feature-policy-and-plugins`) — this feature's guard/transform stages exist
    but the plugin chain itself is populated by that later feature.
  - Configurable header rules driven by the upstream/route `headers` configuration (`set`,
    `add`, `remove`, and passthrough mode) — implemented by
    `cpt-cf-oagw-feature-policy-and-plugins`; this feature only covers the non-configurable
    routing/hop-by-hop/rewrite behaviour above.
  - Circuit breaker enforcement — documented as core resilience functionality but listed as
    future work in `DESIGN.md` §4.7; not implemented in this build.

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-request-proxy`
  - [ ] `p1` - `cpt-cf-oagw-fr-header-transform` (this feature covers the non-configurable
    routing/hop-by-hop/rewrite half; the configurable `set`/`add`/`remove`/passthrough rules are
    covered by `cpt-cf-oagw-feature-policy-and-plugins`)
  - [ ] `p1` - `cpt-cf-oagw-usecase-proxy-request`
  - [ ] `p1` - `cpt-cf-oagw-nfr-ssrf-protection`
  - [ ] `p1` - `cpt-cf-oagw-nfr-low-latency`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-no-retry`
  - `p1` - `cpt-cf-oagw-principle-no-cache`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-body-limit`
  - `p1` - `cpt-cf-oagw-constraint-https-only`
  - `p1` - `cpt-cf-oagw-constraint-no-direct-internet`

- **Domain Model Entities**:
  - ProxyContext (resolved upstream + route + in-flight request state)
  - ProxyResponse

- **Design Components**:

  - [ ] `p1` - `cpt-cf-oagw-interface-proxy-api`
  - `p1` - `cpt-cf-oagw-component-model`
  - `p1` - `cpt-cf-oagw-design-overview`
  - `p1` - `cpt-cf-oagw-adr-request-routing`
  - `p1` - `cpt-cf-oagw-adr-data-plane-caching`
  - `p1` - `cpt-cf-oagw-adr-state-management`

- **API**:
  - `{METHOD} /oagw/v1/proxy/{alias}[/{path}][?{query}]`

- **Sequences**:

  - `p1` - `cpt-cf-oagw-seq-proxy-flow`

- **Data**:

  - None

### 2.7 [Proxy Streaming](feature-proxy-streaming/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-proxy-streaming`

- **Purpose**: Extends the same proxy endpoint built in
  `cpt-cf-oagw-feature-proxy-data-plane-http` to cover Server-Sent-Events (SSE) streams and
  WebSocket upgrades, so `cpt-cf-oagw-actor-app-developer` can consume streaming external APIs
  (e.g., chat-completion SSE) through the identical alias/route/header path.

- **Depends On**: `cpt-cf-oagw-feature-proxy-data-plane-http`

- **Scope**:
  - SSE responses: connection established to the upstream, events forwarded to the client as
    received, with open/close/error lifecycle handling (Override 3).
  - WebSocket upgrade negotiation (`Upgrade: websocket`) through
    `{METHOD} /oagw/v1/proxy/{alias}[/{path}]`, followed by bidirectional frame relay between
    client and upstream.
  - Connection lifecycle on upstream close (client connection closed, event logged) and on
    client disconnect (upstream connection closed).
  - `X-OAGW-Error-Source` semantics applied to streaming connections and upgrade failures,
    consistent with the plain-HTTP error model from `cpt-cf-oagw-feature-gear-foundation`.

- **Out of scope**:
  - WebTransport session flows — `PRD.md` and `DESIGN.md` list WebTransport alongside
    WebSocket, but it has no enabled runtime dependency in this deployment (Scope Reality); the
    `wt` scheme value still validates at the schema layer without a working WebTransport data path.
  - gRPC streaming (bidirectional or server-streaming) — out of scope with gRPC generally in
    this build.

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-streaming`
  - [ ] `p1` - `cpt-cf-oagw-usecase-sse-streaming`

- **Design Principles Covered**:

  - None

- **Design Constraints Covered**:

  - None

- **Domain Model Entities**:
  - None

- **Design Components**:

  - `p1` - `cpt-cf-oagw-interface-api`

- **API**:
  - `{METHOD} /oagw/v1/proxy/{alias}[/{path}]` with `Upgrade: websocket`
  - `GET /oagw/v1/proxy/{alias}[/{path}]` with `Accept: text/event-stream` (SSE)

- **Sequences**:

  - None

- **Data**:

  - None

### 2.8 [Policy and Plugins](feature-policy-and-plugins/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-policy-and-plugins`

- **Purpose**: Applies the built-in policy layer to proxied requests: credential injection by
  auth plugins against `cpt-cf-oagw-actor-cred-store`, the required-headers guard plugin, rate
  limiting, built-in CORS handling, header transformation rules, and hierarchical
  configuration layering/sharing across the tenant hierarchy. This is the layer that turns the
  bare data-plane proxy into the full gateway described in `PRD.md` §1.1.

- **Depends On**: `cpt-cf-oagw-feature-proxy-data-plane-http`, `cpt-cf-oagw-feature-plugin-management-api`

- **Scope**:
  - Auth plugins resolved by GTS identifier and executed once per request before guards:
    `noop`, `apikey` (header/query), `oauth2_client_cred` (Form) and
    `oauth2_client_cred_basic` (Basic) with an internal token cache keyed on
    `(tenant, subject, config)` and TTL `min(config_ttl, expires_in - 30s)`. `basic`/`bearer`
    are catalog-only GTS identifiers with no backing implementation; using either as
    `auth.plugin_type` fails with `unknown auth plugin`. Credentials are resolved from
    `cred_store` by `cred://` reference at request time and never logged.
  - `RequiredHeadersGuardPlugin`: `required_request_headers` checked in the request phase
    (missing header → `400`), `required_response_headers` checked in the response phase
    (missing header → `502`); fail-open when unconfigured; case-insensitive presence check,
    first missing header reported.
  - Rate limiting: token-bucket (default) or sliding-window algorithm, dual-rate
    (`sustained`/`burst`) configuration, `scope` (global/tenant/user/ip/route), `strategy`
    (reject/queue/degrade), `cost` per request, `X-RateLimit-*` and `Retry-After` response
    headers, `429 Too Many Requests` on reject.
  - Built-in CORS: preflight `OPTIONS` returns a permissive `204` at the handler level (no
    upstream resolution, no tenant context); actual cross-origin requests are validated against
    `allowed_origins`/`allowed_methods` after upstream resolution and before forwarding,
    `403 Forbidden` on rejection; `allow_credentials` cannot combine with a wildcard origin.
  - Header set/add/remove and passthrough (`none`/`allowlist`/`all`) transformation rules from
    the upstream/route `headers` configuration — the configurable half of header transformation;
    the non-configurable routing/hop-by-hop/rewrite half is implemented by
    `cpt-cf-oagw-feature-proxy-data-plane-http`.
  - Hierarchical configuration layering and merge order (Upstream < Route < Tenant) and the
    three sharing modes (`private`/`inherit`/`enforce`) for auth, rate limits (`min` of
    ancestor/descendant), plugins (ancestor plugins execute before descendant plugins,
    enforced plugins cannot be removed), and CORS (union under `inherit`, fixed under
    `enforce`); tags use add-only union semantics regardless of sharing mode.
  - Baseline availability behavior expected of every request path (no unhandled panics,
    consistent error responses under upstream failure) as the achievable portion of
    `cpt-cf-oagw-nfr-high-availability` in this build.
  - Metric instrumentation for the core proxy series from `DESIGN.md` §4.2:
    `oagw_requests_total`, `oagw_request_duration_seconds`, `oagw_requests_in_flight`, and
    `oagw_errors_total`, carrying the documented OTel (OpenTelemetry) label keys (`host`,
    `http.request.method`, `http.route`, `http.response.status_code`, `phase`, `error_type`).
  - Structured audit-log fields per `DESIGN.md` §4.3: `request_id`, `tenant_id`, `method`,
    `path`, `status`, and `duration_ms`, emitted at `INFO`/`WARN`/`ERROR` levels. Logs never
    carry request/response bodies or credential material.

- **Out of scope**:
  - Circuit breaker state machine and trip/reset behavior — `DESIGN.md` §4.7 lists it as future
    work and it is not a plugin; `cpt-cf-oagw-nfr-high-availability`'s circuit-breaker clause is
    therefore not implemented, only the baseline availability behavior above is.
  - Starlark custom-plugin sandboxing (no network/file I/O, timeout/memory limits) —
    `cpt-cf-oagw-nfr-starlark-sandbox` has no runtime dependency in this deployment (Scope
    Reality); custom plugin definitions are stored and served by
    `cpt-cf-oagw-feature-plugin-management-api` but never executed here.
  - Redis-backed distributed rate-limit sync and Redis L2 config cache — no Redis dependency is
    enabled in the graded configuration; rate limiting runs as per-instance local state.
  - ADR 0003 proposes a `budget` / `overcommit_ratio` hierarchical-allocation extension for rate
    limits, but that extension was never carried into `upstream.v1.schema.json` or
    `route.v1.schema.json`; those schemas define only `sharing`, `algorithm`, `sustained`,
    `burst`, `scope`, `strategy`, and `cost`. There is no schema field to implement, so this
    feature does not attempt the `budget` mechanism. The effective-limit rule actually built is
    the simple `min(ancestor, descendant)` enforce rule from `DESIGN.md`, covered above.
  - A dedicated `/metrics` scrape route mounted by this gear: `DESIGN.md` marks `/metrics`
    admin-only, and on this platform aggregating admin-facing scrape surfaces is a host-runtime
    concern rather than a per-gear one. This feature emits the metric series above through the
    shared instrumentation hooks and does not assume it also owns that route.

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-auth-injection`
  - [ ] `p1` - `cpt-cf-oagw-fr-rate-limiting`
  - [ ] `p2` - `cpt-cf-oagw-fr-builtin-plugins`
  - [ ] `p1` - `cpt-cf-oagw-fr-header-transform` (this feature covers the configurable
    `set`/`add`/`remove`/passthrough rules; the non-configurable routing/hop-by-hop/rewrite half
    is covered by `cpt-cf-oagw-feature-proxy-data-plane-http`)
  - [x] `p2` - `cpt-cf-oagw-fr-config-layering`
  - [x] `p2` - `cpt-cf-oagw-fr-hierarchical-config`
  - [ ] `p2` - `cpt-cf-oagw-usecase-rate-limit-exceeded`
  - [ ] `p1` - `cpt-cf-oagw-nfr-credential-isolation`
  - [ ] `p2` - `cpt-cf-oagw-nfr-observability`
  - [ ] `p1` - `cpt-cf-oagw-nfr-high-availability`
  - [ ] `p3` - `cpt-cf-oagw-nfr-starlark-sandbox`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-cred-isolation`

- **Design Constraints Covered**:

  - None

- **Domain Model Entities**:
  - Rate limiter state (token bucket)
  - Credential reference (`secret_ref`)
  - CORS policy
  - Header transformation rule

- **Design Components**:

  - [ ] `p1` - `cpt-cf-oagw-contract-cred-store`
  - [ ] `p1` - `cpt-cf-oagw-contract-types-registry`
  - `p1` - `cpt-cf-oagw-interface-api`
  - `p2` - `cpt-cf-oagw-adr-rate-limiting`
  - `p2` - `cpt-cf-oagw-adr-cors`
  - `p1` - `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`
  - `p2` - `cpt-cf-oagw-adr-required-headers-guard-plugin`

- **API**:
  - No new endpoints. Policy runs inside the existing `{METHOD} /oagw/v1/proxy/{alias}...`
    request/response cycle, adding `X-RateLimit-*`/`Retry-After` headers, CORS response
    headers, and permissive `204` handling for preflight `OPTIONS`.

- **Sequences**:

  - None

- **Data**:

  - None

---

## 3. Feature Dependencies

```text
cpt-cf-oagw-feature-gear-foundation
    ↓
cpt-cf-oagw-feature-resource-model-and-store
    ↓
cpt-cf-oagw-feature-upstream-management-api
    ↓
cpt-cf-oagw-feature-route-management-api
    ↓
    ├─→ cpt-cf-oagw-feature-plugin-management-api
    │
    └─→ cpt-cf-oagw-feature-proxy-data-plane-http
            ↓
            cpt-cf-oagw-feature-proxy-streaming

cpt-cf-oagw-feature-plugin-management-api ─────┐
cpt-cf-oagw-feature-proxy-data-plane-http ─────┴─→ cpt-cf-oagw-feature-policy-and-plugins
```

**Dependency Rationale**:

- `cpt-cf-oagw-feature-resource-model-and-store` requires `cpt-cf-oagw-feature-gear-foundation`:
  the domain model and store live inside the gear module skeleton and reuse its error types.
- `cpt-cf-oagw-feature-upstream-management-api` requires
  `cpt-cf-oagw-feature-resource-model-and-store`: the management handlers validate and persist
  through the domain model and store defined there.
- `cpt-cf-oagw-feature-route-management-api` requires
  `cpt-cf-oagw-feature-upstream-management-api`: every route references an `upstream_id` that
  must already exist and be visible through upstream CRUD.
- `cpt-cf-oagw-feature-plugin-management-api` requires
  `cpt-cf-oagw-feature-route-management-api`: plugin-in-use tracking scans upstream and route
  plugin bindings, which requires both resource types to already be manageable.
- `cpt-cf-oagw-feature-proxy-data-plane-http` requires
  `cpt-cf-oagw-feature-route-management-api`: the proxy path resolves an upstream, then matches
  a route, so both CRUD surfaces must exist and be populated first.
- `cpt-cf-oagw-feature-plugin-management-api` and `cpt-cf-oagw-feature-proxy-data-plane-http`
  are independent of each other and can be developed in parallel once route management is done.
- `cpt-cf-oagw-feature-proxy-streaming` requires `cpt-cf-oagw-feature-proxy-data-plane-http`:
  streaming upgrades reuse the same alias resolution, route matching, and header handling built
  there; it only adds upgrade negotiation and bidirectional relay on top.
- `cpt-cf-oagw-feature-policy-and-plugins` requires both
  `cpt-cf-oagw-feature-proxy-data-plane-http` and `cpt-cf-oagw-feature-plugin-management-api`:
  auth/guard/transform execution needs a working proxy request to attach to, and the built-in
  plugin catalog needs the plugin identification and storage model from plugin management.
