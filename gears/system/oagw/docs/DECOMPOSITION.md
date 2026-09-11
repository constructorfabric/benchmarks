# Decomposition: Outbound API Gateway (OAGW)


<!-- toc -->

- [1. Overview](#1-overview)
- [2. Entries](#2-entries)
  - [2.1 Gear Foundation and Configuration - HIGH](#21-gear-foundation-and-configuration---high)
  - [2.2 Upstream Management API - HIGH](#22-upstream-management-api---high)
  - [2.3 Route Management API - HIGH](#23-route-management-api---high)
  - [2.4 Plugin Management API - MEDIUM](#24-plugin-management-api---medium)
  - [2.5 Proxy Request Resolution and Forwarding - HIGH](#25-proxy-request-resolution-and-forwarding---high)
  - [2.6 Streaming and Protocol Upgrades - HIGH](#26-streaming-and-protocol-upgrades---high)
  - [2.7 CORS Handling - MEDIUM](#27-cors-handling---medium)
  - [2.8 Rate Limiting - MEDIUM](#28-rate-limiting---medium)
  - [2.9 Plugin Execution Chain and Built-in Plugins - MEDIUM](#29-plugin-execution-chain-and-built-in-plugins---medium)
- [3. Feature Dependencies](#3-feature-dependencies)

<!-- /toc -->

**Overall implementation status:**
- [ ] `p1` - **ID**: `cpt-cf-oagw-status-not-started`
## 1. Overview

`gears/system/oagw/oagw/src/lib.rs` is currently empty — no part of the design is implemented. This document decomposes the existing `DESIGN.md` (itself built on `PRD.md` and the accepted `ADR/0001`–`0009` decisions) into nine ordered, independently implementable and testable FEATURE units. The decomposition strategy follows the design's own Control-Plane/Data-Plane split:

1. A foundation feature (2.1) that stands up the gear, its configuration surface, and the cross-cutting RFC 9457 error contract every other feature relies on.
2. Three Control-Plane CRUD features (2.2–2.4) that manage the persistent configuration resources (Upstream, Route, Plugin), ordered by their dependency chain (a Route references an Upstream; a Plugin binding referenced from Upstream/Route CRUD only needs plugin *identification*, not execution).
3. A Data-Plane core (2.5) that performs alias/route resolution and plain HTTP forwarding, followed by three features that layer additional proxy-time behavior on top of it in parallel (2.6 streaming, 2.7 CORS, 2.8 rate limiting), and a plugin-execution feature (2.9) that wires the Auth/Guard/Transform chain into the same path.

Each entry lists only the behavior documented in `DESIGN.md`, `PRD.md`, the ADRs, the two JSON schemas, and the graded server configuration `config/e2e-local.yaml` (the source of the concrete `oagw.config` keys referenced in 2.1's Scope) — no new requirements or architecture decisions are introduced, and no Rust types, function names, crate names, or in-crate file paths appear below.

**Two corrections carried forward from the task specification, superseding what `PRD.md`/`DESIGN.md` tabulate:**

1. **Path prefix**: `PRD.md` and `DESIGN.md` tabulate `/api/oagw/v1/...`. That is the path as seen behind an operator-facing gateway whose `prefix_path` is `/api`. The gear itself registers gear-relative routes; in the graded configuration (`config/e2e-local.yaml`), the `api-gateway` host's `prefix_path` is not set, so the management API is reached at `/oagw/v1/upstreams` (etc.) and the proxy API at `/oagw/v1/proxy/{alias}`. Every **API** section below uses the `/oagw/v1/...` form.
2. **`http` scheme legality**: `config/e2e-local.yaml` sets `oagw.config.allow_http_upstream: true`, so creating an upstream with `{"scheme": "http", "port": 80}` must succeed. `cpt-cf-oagw-constraint-https-only` in `DESIGN.md` states the **default** (HTTPS-only) posture; the `allow_http_upstream` flag lifts it. These are two distinct questions handled by two different features: *which schemes an upstream record may declare* (feature 2.2, upstream-management) versus *whether OAGW actually opens a plaintext connection* (feature 2.5, proxy-core, gated by the `allow_http_upstream` gear-config flag owned by feature 2.1).

**Deliberate deviation from the kit template — reference checkbox form**: the kit's `DECOMPOSITION` template shows `Design Principles Covered`, `Design Constraints Covered`, `Design Components`, `Sequences`, and `Data` as `- [ ] \`pN\` - \`cpt-...\`` checkbox bullets. This document intentionally uses bare `` - `cpt-...` `` bullets for those five fields instead, because `cfs validate --artifact` enforces that a reference's checkbox-vs-plain-bullet form must match its *definition's* form, and every ID surfaced through those five fields is defined in `DESIGN.md` or an `ADR/*.md` as a bare `**ID**: ...` line with no task-tracking checkbox. Giving such a reference a checkbox produces a `ref-task-def-no-task` validation error. Only `Requirements Covered` uses checkboxes below, because PRD requirement IDs are themselves defined with `- [ ] \`pN\` - ...` checkboxes. This is not an oversight; do not re-flag it.

**Sizing rationale for 2.5 `proxy-core`**: this entry's Scope list is deliberately the longest of the nine (roughly 3-4x the size of the smallest entries, e.g. 2.6 `proxy-streaming`). `proxy-core` is the single data-plane request path — alias/route resolution, endpoint-pool selection, header handling, body validation, hierarchical config merge, and forwarding — that every other data-plane feature (2.6 streaming, 2.7 CORS, 2.8 rate limiting, 2.9 plugin execution) extends rather than duplicates. Splitting resolution, pool selection, header handling, and forwarding into separate entries would create sub-features that share one inbound request and one outbound connection and so cannot be exercised or tested independently of each other; keeping them as one entry is a granularity choice, not an unmanaged size overrun.

## 2. Entries

### 2.1 [Gear Foundation and Configuration](features/gear-foundation.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-gear-foundation`

- **Purpose**: Establish the `oagw` gear as a loadable, addressable component of the host ToolKit runtime, expose its typed gear-configuration surface, provide the single in-process configuration-store abstraction shared by the Control Plane (write path) and Data Plane (read path), and implement the cross-cutting RFC 9457 error-rendering and error-source-distinction contract every later feature's error paths depend on. This feature has no user-facing behavior of its own — it is the substrate every other feature is built on.

- **Depends On**: None

- **Phases**: single phase — gear bootstrap, configuration surface, config-store abstraction, and the RFC 9457 error contract are delivered together as one implementable/testable unit

- **Scope**:
  - Gear registration/bootstrap with the host ToolKit runtime so the gear's router is mountable by the api-gateway host under `/oagw/v1/...` (gear-relative; see Overview correction 1)
  - A typed gear-configuration surface read from the server YAML `oagw.config` stanza, accepting at minimum `proxy_timeout_secs` (integer seconds), `allow_http_upstream` (boolean), and `ssrf_policy.enabled` (boolean) — the exact keys the graded server configuration `config/e2e-local.yaml` sets under `gears.oagw.config` (source: `config/e2e-local.yaml`)
  - A single in-process configuration-store abstraction shared by the Control Plane (2.2–2.4) and Data Plane (2.5), consistent with the CP/DP split described in `cpt-cf-oagw-adr-state-management`
  - An error domain that renders every gateway-originated error as `application/problem+json` (RFC 9457: `type`/`title`/`status`/`detail`/`instance`, plus the `upstream_id`/`host`/`path`/`retry_after_seconds`/`trace_id` extension fields) using the GTS `type` identifier documented for each status/error pair in the Error Response Format table
  - Setting `X-OAGW-Error-Source: gateway` on every gateway-originated error response
  - A structured JSON audit-log scaffold (fields: `timestamp`, `level`, `event`, `request_id`, `tenant_id`, `principal_id`, `host`, `path`, `method`, `status`, `duration_ms`) and request-correlation-ID plumbing that later features attach their own values to
  - Single-executable ToolKit deployment packaging

- **Out of scope**:
  - Setting `X-OAGW-Error-Source: upstream` and passing upstream error bodies through verbatim — that half of the error-source contract belongs to proxy-core (2.5), the only feature that receives upstream responses
  - Any upstream/route/plugin CRUD logic or schema (2.2–2.4)
  - Actual proxy request handling (2.5–2.9)
  - L1/L2 cache population and invalidation mechanics beyond establishing where CP state and DP state each live — the read/write hot paths that exercise the cache are implemented in 2.2/2.3/2.5
  - Per-request Prometheus metric emission and correlation-ID population on the proxy hot path — the request these are attached to is owned by proxy-core (2.5)

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-error-codes`
  - [ ] `p1` - `cpt-cf-oagw-nfr-high-availability` — partial: this feature delivers the gear's addressable/loadable bootstrap with the host runtime and the consistent RFC 9457 error envelope that availability signaling depends on; the circuit-breaker half of this NFR that `PRD.md` also requires is out of scope for this decomposition round (see 2.5's Out of scope and `DESIGN.md` §4.7 future work)

- **Design Principles Covered**:

  - `cpt-cf-oagw-principle-rfc9457`
  - `cpt-cf-oagw-principle-error-source`

- **Design Constraints Covered**:

  - `cpt-cf-oagw-constraint-toolkit-deploy`

- **Domain Model Entities**:
  - Gear configuration (`OagwConfig`)
  - Problem Details error envelope
  - Error taxonomy / GTS error `type` catalog
  - Shared configuration-store handle

- **Design Components**:

  - `cpt-cf-oagw-design-drivers`
  - `cpt-cf-oagw-design-layers`
  - `cpt-cf-oagw-tech-dependencies`
  - `cpt-cf-oagw-design-overview`
  - `cpt-cf-oagw-design-dependencies`
  - `cpt-cf-oagw-component-model`
  - `cpt-cf-oagw-interface-api`
  - `cpt-cf-oagw-adr-error-source-distinction`
  - `cpt-cf-oagw-adr-state-management`
  - `cpt-cf-oagw-adr-data-plane-caching`

- **API**:
  - None (this feature exposes no REST endpoints of its own; the router mount point and error envelope it provides are consumed by every endpoint defined in 2.2–2.9)

- **Sequences**: None

- **Data**: None

### 2.2 [Upstream Management API](features/upstream-management.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-upstream-management`

- **Purpose**: Deliver the five CRUD operations for Upstream configuration resources — the tenant-scoped root object every proxy request ultimately resolves to — including field validation against `upstream.v1.schema.json`, alias derivation/enforcement, enable/disable semantics, and tenant-scoped visibility.

- **Depends On**: `cpt-cf-oagw-feature-gear-foundation`

- **Phases**: single phase — the five CRUD operations, alias derivation/validation, and tenant scoping are delivered together as one implementable/testable unit

- **Scope**:
  - `POST` / `GET` (list) / `GET` (by id) / `PUT` / `DELETE` `/oagw/v1/upstreams` per the Management API table
  - Request/response body validated against `upstream.v1.schema.json`: `server.endpoints[]` (scheme/host/port), `protocol`, `auth` (type/sharing/config), `headers`, `plugins` (sharing/items, persisted as opaque GTS identifiers or UUIDs — execution is 2.9's concern), `rate_limit` and `cors` field persistence (sharing modes and threshold fields only — merge computation and enforcement belong to 2.5/2.7/2.8), `tags`
  - Endpoint `scheme` accepts `https`, `wss`, `wt`, `grpc` per the schema, **and additionally `http`** per Overview correction 2: this feature's create/update-time validation governs only which schemes are syntactically legal to declare; whether OAGW actually opens a plaintext connection to an `http` endpoint is gated by the `allow_http_upstream` gear-config flag (owned by 2.1) and enforced at connect time by proxy-core (2.5)
  - Alias auto-derivation for hostname-based endpoints (single hostname, or multiple hostnames sharing a PSL-validated registrable common suffix), explicit-alias requirement and validation for IP-based/non-derivable endpoints, alias normalization (ASCII lowercase, trailing dot stripped), RFC 1123 hostname validation, and alias immutability on update per the documented transition table (any endpoint change that would alter the derived alias is rejected)
  - Uniqueness of `(tenant_id, alias)`; `409 Conflict` on collision; alias match to an ancestor's alias is treated as a bind request gated by the `oagw:upstream:bind` permission and the ancestor's sharing mode
  - `enabled` boolean (default `true`) with cascading disable semantics from ancestor to descendant tenants (a descendant cannot re-enable an ancestor-disabled upstream); this feature owns the field, its default, its persistence, and the ancestor-cascade validation rule at write time — enforcement of `enabled: false` against live proxy requests is 2.5's concern
  - Multi-endpoint pool validation: all endpoints declared on one upstream must share `protocol`, `scheme`, and `port`
  - Tenant scoping: all five operations strictly scoped to the calling tenant; ancestor upstreams are invisible (404) via the management API

- **Out of scope**:
  - Proxy-time alias resolution, tenant-hierarchy walk, and shadowing (2.5)
  - Enforcement of `enabled: false` against live proxy requests (2.5)
  - Execution of the Auth/Guard/Transform plugins referenced by `plugins.items[]` (2.9) — this feature validates that referenced identifiers are well-formed GTS identifiers or UUIDs, not that they resolve to an installed, executable plugin
  - Rate-limit and CORS *enforcement* (2.8, 2.7) — this feature only persists their configuration fields
  - DNS resolution, IP pinning, and other network-level SSRF controls (explicitly out of scope per `DESIGN.md` §4.5) — this feature covers only the schema-level scheme allowlist

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-upstream-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
  - [ ] `p1` - `cpt-cf-oagw-nfr-multi-tenancy`
  - [ ] `p1` - `cpt-cf-oagw-nfr-ssrf-protection`
  - [ ] `p1` - `cpt-cf-oagw-usecase-configure-upstream`
  - [ ] `p1` - `cpt-cf-oagw-interface-management-api`

- **Design Principles Covered**:

  - `cpt-cf-oagw-principle-tenant-scope`

- **Design Constraints Covered**:

  - `cpt-cf-oagw-constraint-multi-sql`
  - `cpt-cf-oagw-constraint-https-only`

- **Domain Model Entities**:
  - Upstream
  - ServerConfig
  - Endpoint

- **Design Components**:

  - `cpt-cf-oagw-design-domain-model`
  - `cpt-cf-oagw-component-model`
  - `cpt-cf-oagw-interface-api`

- **API**:
  - POST /oagw/v1/upstreams
  - GET /oagw/v1/upstreams
  - GET /oagw/v1/upstreams/{id}
  - PUT /oagw/v1/upstreams/{id}
  - DELETE /oagw/v1/upstreams/{id}

- **Sequences**: None

- **Data**:

  - `cpt-cf-oagw-db-schema`

### 2.3 [Route Management API](features/route-management.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-route-management`

- **Purpose**: Deliver the five CRUD operations for Route resources, which attach match rules to an upstream and control which inbound proxy requests are matched to it, including the HTTP match dialect, match-rule uniqueness enforcement, and upstream ownership/immutability rules.

- **Depends On**: `cpt-cf-oagw-feature-gear-foundation`, `cpt-cf-oagw-feature-upstream-management`

- **Phases**: single phase — the five CRUD operations, match-rule validation, and ownership/immutability rules are delivered together as one implementable/testable unit

- **Scope**:
  - `POST` / `GET` (list) / `GET` (by id) / `PUT` / `DELETE` `/oagw/v1/routes` per the Management API table
  - Request/response body validated against `route.v1.schema.json`: `upstream_id`, `match` (exactly one of `http`|`grpc`), `plugins` (sharing/items, persisted only — 2.9 owns execution), `rate_limit` (persisted only — 2.8 owns enforcement), `tags`
  - `match.http`: `methods` allowlist, `path`, `query_allowlist`, `path_suffix_mode` (`disabled`|`append`)
  - `match.grpc`: `service`/`method` accepted and persisted per the schema's `oneOf`, even though no gRPC proxy code path exists yet (`DESIGN.md` states gRPC matching is "planned/Phase 3 — no gRPC proxy code path is currently implemented or reachable"); this feature's responsibility is limited to schema-conformant persistence, not routing behavior
  - Match-rule uniqueness within an upstream: no two enabled routes under the same upstream may share `(path prefix, priority)` for the same method — `409 Conflict` on collision. A `priority` field is accepted and persisted alongside `match.http`, consistent with the Route domain-model entity's `+Int priority` attribute, even though it is not separately enumerated among the excerpted top-level schema properties
  - `upstream_id` ownership validation at create time (must belong to the calling tenant; `400 ValidationError` otherwise) and immutability thereafter (absent from the update DTO)
  - `enabled` boolean per the Route domain-model entity (`+Boolean enabled`); this field is accepted as a schema-compatible addition since the route schema's root object does not restrict additional properties. Exclusion of disabled routes from route matching is 2.5's concern; this feature owns the field, its default, and its persistence
  - Tenant scoping identical in shape to 2.2: all operations scoped to the calling tenant, ancestor routes 404 via the management API

- **Out of scope**:
  - Proxy-time route matching (longest-path-prefix + priority selection, descendant-priority-over-ancestor) — 2.5
  - Any gRPC request routing or forwarding — out of scope platform-wide per `PRD.md` §4.2 and `DESIGN.md` §4.7 item 7
  - Plugin execution and rate-limit enforcement referenced by this resource — 2.9 and 2.8 respectively

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-route-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
  - [ ] `p1` - `cpt-cf-oagw-nfr-multi-tenancy`
  - [ ] `p1` - `cpt-cf-oagw-usecase-configure-route`
  - [ ] `p1` - `cpt-cf-oagw-interface-management-api`

- **Design Principles Covered**:

  - `cpt-cf-oagw-principle-tenant-scope`

- **Design Constraints Covered**:

  - `cpt-cf-oagw-constraint-multi-sql`

- **Domain Model Entities**:
  - Route
  - HTTP match (methods, path, query allowlist, path-suffix mode)
  - gRPC match (schema-conformant, non-functional in this round)

- **Design Components**:

  - `cpt-cf-oagw-design-domain-model`
  - `cpt-cf-oagw-component-model`
  - `cpt-cf-oagw-interface-api`

- **API**:
  - POST /oagw/v1/routes
  - GET /oagw/v1/routes
  - GET /oagw/v1/routes/{id}
  - PUT /oagw/v1/routes/{id}
  - DELETE /oagw/v1/routes/{id}

- **Sequences**: None

- **Data**:

  - `cpt-cf-oagw-db-schema`

### 2.4 [Plugin Management API](features/plugin-management.md) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-oagw-feature-plugin-management`

- **Purpose**: Deliver the create/list/get/delete lifecycle (no update — plugins are immutable) for custom Starlark plugin resources, plus the plugin identification model (GTS identifier parsing, UUID-backed vs. named resolution) that upstream-management, route-management, and plugin-execution all depend on to reference plugins.

- **Depends On**: `cpt-cf-oagw-feature-gear-foundation`

- **Phases**: single phase — the create/list/get/delete lifecycle and the plugin identification model are delivered together as one implementable/testable unit

- **Scope**:
  - `POST` / `GET` (list) / `GET` (by id) / `DELETE` `/oagw/v1/plugins` and `GET /oagw/v1/plugins/{id}/source`
  - Anonymous GTS identifier issuance on create: `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`
  - Plugin immutability: no replace operation exists for this resource; changes are made by creating a new plugin and re-binding references
  - `DELETE` returns `409 PluginInUse` with the documented `referenced_by: { upstreams: [...], routes: [...] }` body when the plugin is still bound by any upstream or route (scanning upstream/route plugin bindings and the scalar auth-plugin reference columns)
  - Plugin identification model: `plugin_ref` (canonical GTS identifier string) and `plugin_uuid` (nullable UUID extracted when UUID-backed) storage; the resolution algorithm that parses the GTS identifier's post-`~` instance part and classifies it as UUID-backed (custom, looked up in storage) vs. named (registry-resolved) — this feature owns identification/lookup; actually invoking a resolved plugin is 2.9's concern
  - Named/built-in plugin GTS identifiers (`noop`, `apikey`, `oauth2_client_cred`, `oauth2_client_cred_basic`, `required_headers`, `request_id`, and the catalog-only identifiers `basic`, `bearer`, `timeout`, `cors`, `logging`, `metrics`) are recognized as valid `plugin_ref` values at this layer so that upstream/route CRUD (2.2/2.3) can bind them, without this feature executing them
  - Garbage-collection bookkeeping fields (`gc_eligible_at`, `last_used_at`) persisted per the domain model
  - Tenant scoping identical in shape to 2.2/2.3

- **Out of scope**:
  - Starlark sandbox execution of custom plugin source (parsing/interpreting the script; enforcing no-network/no-file-I/O; timeout/memory limits). `cpt-cf-oagw-nfr-starlark-sandbox` is explicitly deferred beyond this decomposition round: this feature only stores and identifies plugins, and no execution engine for custom plugins exists in any of the nine features in this round (2.9 wires only the compiled built-in plugins), so a sandbox with nothing to sandbox has no independently testable behavior yet
  - The periodic garbage-collection job that deletes plugins once `gc_eligible_at` has passed — background job scheduling is not a REST-observable boundary for this round
  - Auth/Guard/Transform plugin *execution* against live requests (2.9)
  - The execution-order and built-in-registry half of `cpt-cf-oagw-adr-plugin-system` (2.9); this feature covers only the identification/lifecycle half of that ADR

- **Requirements Covered**:

  - [ ] `p2` - `cpt-cf-oagw-nfr-multi-tenancy`
  - [ ] `p2` - `cpt-cf-oagw-nfr-starlark-sandbox` — partial: plugin source storage and identification only; sandboxed execution is explicitly deferred beyond this decomposition round, see Out of scope
  - [ ] `p2` - `cpt-cf-oagw-interface-management-api`

- **Design Principles Covered**:

  - `cpt-cf-oagw-principle-plugin-immutable`
  - `cpt-cf-oagw-principle-tenant-scope`

- **Design Constraints Covered**:

  - `cpt-cf-oagw-constraint-multi-sql`

- **Domain Model Entities**:
  - Plugin
  - Plugin identification (`plugin_ref` / `plugin_uuid`)

- **Design Components**:

  - `cpt-cf-oagw-design-domain-model`
  - `cpt-cf-oagw-component-model`
  - `cpt-cf-oagw-interface-api`
  - `cpt-cf-oagw-adr-plugin-system`

- **API**:
  - POST /oagw/v1/plugins
  - GET /oagw/v1/plugins
  - GET /oagw/v1/plugins/{id}
  - DELETE /oagw/v1/plugins/{id}
  - GET /oagw/v1/plugins/{id}/source

- **Sequences**: None

- **Data**:

  - `cpt-cf-oagw-db-schema`

### 2.5 [Proxy Request Resolution and Forwarding](features/proxy-core.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-proxy-core`

- **Purpose**: Implement the core proxy request path — `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?query]` alias/route resolution through the tenant chain, multi-endpoint pool selection including the `X-OAGW-Target-Host` behavior matrix, the three header categories and Host/`:authority` replacement, body-size/Content-Length/Transfer-Encoding validation, plain HTTP forwarding in both directions, hierarchical configuration merge, enable/disable enforcement, and the gateway-vs-upstream error split with base observability.

- **Depends On**: `cpt-cf-oagw-feature-gear-foundation`, `cpt-cf-oagw-feature-upstream-management`, `cpt-cf-oagw-feature-route-management`

- **Phases**: this entry's Scope is large enough to warrant a milestone breakdown rather than a single monolithic phase:
  1. Resolution — alias/route resolution through the tenant hierarchy, `enabled` enforcement, and hierarchical configuration merge
  2. Request/response mechanics — `X-OAGW-Target-Host` handling, header processing, body validation, query/path-suffix enforcement, and plain HTTP forwarding (including `allow_http_upstream` enforcement)
  3. Error split and observability — the gateway-vs-upstream error split, correlation-ID assignment, audit-log emission, and base metrics

- **Scope**:
  - `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]` request classification: HTTP-protocol upstreams use method allowlist + longest-path-prefix + priority match
  - Alias resolution: normalize the inbound alias (lowercase, trailing dot stripped), walk the tenant hierarchy descendant→root, closest enabled upstream wins (shadowing); ancestor constraints configured with `sharing: enforce` remain active after shadowing
  - Route matching: longest path-prefix among enabled routes for the resolved upstream and request method; descendant routes take priority over inherited ancestor routes
  - `enabled: false` enforcement: disabled upstream → `503`; disabled route excluded from matching
  - Configuration merge order Upstream (base) < Route < Tenant, per the documented sharing-mode rules for auth/rate-limit/plugins/CORS, and the always-additive tag union
  - `X-OAGW-Target-Host` behavior matrix (single endpoint, header present/absent; multi-endpoint explicit alias, header present/absent — targeted vs. round robin; multi-endpoint common-suffix alias, header required) and its three `400` errors: `MissingTargetHost`, `InvalidTargetHost`, `UnknownTargetHost`
  - Round-robin distribution across a multi-endpoint pool when `X-OAGW-Target-Host` is absent or not required
  - Header processing: routing header (`X-OAGW-Target-Host`) consumed and stripped; hop-by-hop headers (`Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`) stripped; `Host` replaced with the upstream host on HTTP/1.1, `:authority` replaced on HTTP/2; passthrough headers forwarded per the upstream's `headers.request`/`headers.response` set/add/remove/passthrough rules
  - Body validation: `Content-Length` well-formedness and match to actual size (`400`), hard 100MB limit rejected before buffering (`413`), unsupported `Transfer-Encoding` rejected (`400`, only `chunked` supported)
  - Query-parameter allowlist enforcement (`match.http.query_allowlist`) and `path_suffix_mode` enforcement (`disabled` rejects a supplied suffix; `append` appends it to `match.http.path`)
  - Plain HTTP/HTTPS request forwarding to the upstream and the upstream's response back to the caller, including the request timeout sourced from gear-foundation's `proxy_timeout_secs` config
  - Gateway-error vs. upstream-error split: gateway-originated errors use gear-foundation's RFC 9457 envelope with `X-OAGW-Error-Source: gateway`; upstream responses (including upstream error statuses) are passed through unchanged with `X-OAGW-Error-Source: upstream`
  - Enforcement of `allow_http_upstream`: when `false` (platform default), a plaintext `http`/`ws` connection to an endpoint is refused even if the upstream record declares that scheme (2.2 validates only that the scheme is legal to *declare*); when `true` (as in `config/e2e-local.yaml`), the plaintext connection is permitted — the "actually connect" half of the corrected `constraint-https-only` reading from Overview correction 2
  - `constraint-no-direct-internet`: OAGW is the only path through which gear code reaches external hosts within this system's scope
  - Request correlation-ID assignment and structured audit-log emission populated with per-request values, plus the base Prometheus counters/histograms keyed off this request

- **Out of scope**:
  - SSE/WebSocket streaming semantics and connection lifecycle — 2.6
  - CORS preflight and origin/method validation — 2.7 layers its checks around this path; this feature forwards non-preflight, non-cross-origin requests as described above
  - Rate-limit token-bucket evaluation and `429`/`X-RateLimit-*` responses — 2.8
  - Auth/Guard/Transform plugin invocation — 2.9 (this feature defines where in the flow plugins run, per the sequence; the plugins and their registries are 2.9's deliverable)
  - gRPC request classification/forwarding — no code path exists or is planned for this round
  - DNS resolution and IP-pinning SSRF controls beyond the scheme allowlist — explicitly out of scope per `DESIGN.md` §4.5
  - Circuit breaker and automatic retries — excluded by `principle-no-retry` and `DESIGN.md` §4.7 future work

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-request-proxy`
  - [x] `p1` - `cpt-cf-oagw-fr-alias-resolution`
  - [ ] `p1` - `cpt-cf-oagw-fr-header-transform`
  - [x] `p1` - `cpt-cf-oagw-fr-config-layering`
  - [x] `p1` - `cpt-cf-oagw-fr-hierarchical-config`
  - [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`
  - [ ] `p1` - `cpt-cf-oagw-nfr-low-latency`
  - [ ] `p1` - `cpt-cf-oagw-nfr-multi-tenancy`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
  - [ ] `p1` - `cpt-cf-oagw-nfr-ssrf-protection`
  - [ ] `p1` - `cpt-cf-oagw-nfr-observability`
  - [ ] `p1` - `cpt-cf-oagw-usecase-proxy-request`
  - [ ] `p1` - `cpt-cf-oagw-interface-proxy-api`

- **Design Principles Covered**:

  - `cpt-cf-oagw-principle-no-retry`
  - `cpt-cf-oagw-principle-no-cache`
  - `cpt-cf-oagw-principle-tenant-scope`

- **Design Constraints Covered**:

  - `cpt-cf-oagw-constraint-body-limit`
  - `cpt-cf-oagw-constraint-no-direct-internet`
  - `cpt-cf-oagw-constraint-https-only`

- **Domain Model Entities**:
  - Proxy request/response context
  - Resolved route match
  - Endpoint pool (round-robin state)

- **Design Components**:

  - `cpt-cf-oagw-component-model`
  - `cpt-cf-oagw-interface-api`
  - `cpt-cf-oagw-adr-request-routing`

- **API**:
  - {METHOD} /oagw/v1/proxy/{alias}
  - {METHOD} /oagw/v1/proxy/{alias}/{path_suffix}

- **Sequences**:

  - `cpt-cf-oagw-seq-proxy-flow`

- **Data**: None

### 2.6 [Streaming and Protocol Upgrades](features/proxy-streaming.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-proxy-streaming`

- **Purpose**: Extend the proxy path with two long-lived transport modes — Server-Sent-Event response streaming forwarded to the client without buffering, and WebSocket protocol-upgrade proxying — both with correct connection-lifecycle handling and the aborted-stream gateway error.

- **Depends On**: `cpt-cf-oagw-feature-proxy-core`

- **Phases**: single phase — SSE response streaming and WebSocket protocol-upgrade proxying are delivered together as one implementable/testable unit

- **Scope**:
  - SSE: detect `text/event-stream` upstream responses and forward each event to the client as received, without buffering the full response; connection lifecycle covers clean open, clean close when the upstream closes, and closing the client connection when the upstream closes unexpectedly
  - Client-initiated disconnect during an SSE stream closes the corresponding upstream connection
  - WebSocket: proxy the `Upgrade: websocket` handshake and the resulting bidirectional frame stream end-to-end between client and upstream, reusing proxy-core's alias/route resolution and header rules for the initial handshake request
  - `StreamAborted` (`502`) gateway error when an in-progress stream connection is aborted

- **Out of scope**:
  - WebTransport session flows: the endpoint schema reserves a `wt` scheme value, but no design section (Component Model, Domain Model, or Interactions & Sequences) describes a WebTransport proxy engine binding; there is no documented behavior to implement against in this round
  - gRPC bidirectional/server/client-streaming proxying — `DESIGN.md` states no gRPC proxy code path is implemented or reachable; gRPC support is Phase-3/4 future work per `PRD.md` §4.2 and `DESIGN.md` §4.7 item 7 ("Requires prototype")
  - Rate limiting and CORS applied to streaming connections — 2.7/2.8 apply their checks at connection-open time using the same mechanism as non-streaming requests; no additional per-frame enforcement is introduced here
  - HTTP/3 (QUIC) multiplexing — explicit `DESIGN.md` §4.5 future work

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-streaming` — partial: server-sent events and WebSocket upgrade proxying delivered; WebTransport is deferred, see Out of scope (no design section binds a WebTransport proxy engine)
  - [ ] `p1` - `cpt-cf-oagw-usecase-sse-streaming`

- **Design Principles Covered**:

  - `cpt-cf-oagw-principle-no-cache`

- **Design Constraints Covered**: None

- **Domain Model Entities**:
  - Stream session
  - SSE event-forwarding state
  - WebSocket upgrade context

- **Design Components**:

  - `cpt-cf-oagw-component-model`

- **API**:
  - {METHOD} /oagw/v1/proxy/{alias}/{path_suffix} (SSE response streaming)
  - GET /oagw/v1/proxy/{alias}/{path_suffix} (WebSocket upgrade)

- **Sequences**: None

- **Data**: None

### 2.7 [CORS Handling](features/cors-handling.md) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-oagw-feature-cors-handling`

- **Purpose**: Implement OAGW's built-in CORS handler: a fast preflight path answered before any upstream resolution, and origin/method validation of actual cross-origin requests performed after upstream resolution but before forwarding, per the effective Upstream `cors` configuration (CORS configuration lives on the Upstream only — see §2.3 and `cpt-cf-oagw-feature-route-management`; the Route schema has no top-level `cors` property).

- **Depends On**: `cpt-cf-oagw-feature-proxy-core`

- **Phases**: single phase — the preflight fast path and post-resolution origin/method validation are delivered together as one implementable/testable unit

- **Scope**:
  - Preflight fast path: detect `OPTIONS` + `Origin` + `Access-Control-Request-Method`, return `204 No Content` immediately, echoing the requested origin/method/headers, without upstream resolution, tenant-context extraction, auth, or plugin execution; still subject to infrastructure-level rate limiting
  - `Access-Control-Max-Age` and `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers` on preflight responses
  - Actual cross-origin request validation, performed after upstream resolution and before forwarding: origin membership in `allowed_origins` (`403` if not), method membership in `allowed_methods` (`403` if not)
  - `Vary: Origin` on every response for a resolved upstream/route pair whose Upstream-level CORS is enabled, to prevent cache poisoning
  - Runtime rejection of an *effective* (post hierarchical-merge) configuration that combines `allow_credentials: true` with a wildcard origin — the JSON Schema's `if`/`then` clause already blocks this at the single-resource level; this feature is the counterpart that also rejects the equivalent case after CORS configuration is merged across the tenant hierarchy
  - CORS hierarchical merge: `inherit` unions child and parent origins; `enforce` prevents the child from adding origins, reusing the same sharing-mode plumbing as auth/rate-limit/plugins from proxy-core's configuration-layering

- **Out of scope**:
  - The Upstream CRUD endpoints that persist the `cors` field — 2.2 (the Route CRUD endpoints, 2.3, accept but never persist a `cors` object a caller may send; `route.v1.schema.json` has no top-level `cors` property)
  - Non-CORS request validation (guard rules unrelated to origin/method) — 2.5/2.9

- **Requirements Covered**: None (CORS has no dedicated PRD `fr-`/`nfr-` identifier; its behavior is scoped entirely by `cpt-cf-oagw-adr-cors` and the Component Model's Guard Rules)

- **Design Principles Covered**: None

- **Design Constraints Covered**: None

- **Domain Model Entities**:
  - CORS policy (Upstream `cors` field)
  - Preflight decision

- **Design Components**:

  - `cpt-cf-oagw-component-model`
  - `cpt-cf-oagw-adr-cors`

- **API**:
  - OPTIONS /oagw/v1/proxy/{alias}/{path_suffix}
  - {METHOD} /oagw/v1/proxy/{alias}/{path_suffix} (actual cross-origin request; CORS check layered on the proxy-core forwarding path)

- **Sequences**: None

- **Data**: None

### 2.8 [Rate Limiting](features/rate-limiting.md) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-oagw-feature-rate-limiting`

- **Purpose**: Enforce upstream- and route-level rate limits on the proxy path using an in-memory token-bucket limiter with dual-rate (sustained + burst) configuration, hierarchical `min()` budget allocation across the tenant chain, and the documented `429`/`X-RateLimit-*`/`Retry-After` response contract.

- **Depends On**: `cpt-cf-oagw-feature-proxy-core`

- **Phases**: single phase — the token-bucket limiter, hierarchical budget allocation, and the `429`/`X-RateLimit-*` response contract are delivered together as one implementable/testable unit

- **Scope**:
  - Token-bucket algorithm (default) per the `rate_limit.algorithm` field; `sliding_window` accepted as a configuration value
  - Dual-rate configuration: `sustained.rate`/`sustained.window` and `burst.capacity` (defaulting to `sustained.rate` when unspecified)
  - `scope` selection (`global`|`tenant`|`user`|`ip`|`route`) determining the counter key
  - `cost` (tokens consumed per request, default 1) for weighted endpoints
  - `strategy: reject` returns `429 RateLimitExceeded` with `Retry-After` and `X-RateLimit-*` response headers (limit/remaining/reset); `strategy: queue`/`strategy: degrade` are accepted configuration values whose queuing/degradation behavior is delivered at the basic level described in the PRD's alternative flows (bounded queue / reduced-functionality marker)
  - Hierarchical budget allocation: `effective_rate = min(selected_rate, route_rate, all_ancestor_enforced_rates)`, reusing the same `enforce` sharing-mode semantics as auth/CORS/plugins
  - Per-instance (in-process) counters, owned by the Data Plane per `cpt-cf-oagw-adr-state-management`

- **Out of scope**:
  - Cross-instance synchronization of rate-limit counters via Redis (the "Hybrid Local + Periodic Sync" distribution mode in `cpt-cf-oagw-adr-rate-limiting`) — the ADR defers distributed sync to a future iteration; this feature delivers the local-only (per-instance) mode the ADR describes as the starting point
  - The upstream/route CRUD endpoints that persist the `rate_limit` field — 2.2/2.3
  - Rate-limit-specific Prometheus metrics beyond what proxy-core's base observability hooks already expose

- **Requirements Covered**:

  - [ ] `p2` - `cpt-cf-oagw-fr-rate-limiting`
  - [ ] `p2` - `cpt-cf-oagw-usecase-rate-limit-exceeded`

- **Design Principles Covered**: None

- **Design Constraints Covered**: None

- **Domain Model Entities**:
  - Token bucket
  - Rate-limit scope
  - Rate-limit counter

- **Design Components**:

  - `cpt-cf-oagw-component-model`
  - `cpt-cf-oagw-adr-rate-limiting`

- **API**:
  - {METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}] (rate limit evaluated before forwarding; adds `X-RateLimit-*`/`Retry-After` response headers)

- **Sequences**: None

- **Data**: None

### 2.9 [Plugin Execution Chain and Built-in Plugins](features/plugin-execution.md) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-oagw-feature-plugin-execution`

- **Purpose**: Implement the deterministic Auth → Guard → Transform(request) → upstream call → Transform(response/error) execution chain on the proxy path, the in-process registries of built-in plugins, outbound credential injection sourced from the credential store, and the required-headers guard plugin's documented request/response behavior.

- **Depends On**: `cpt-cf-oagw-feature-plugin-management`, `cpt-cf-oagw-feature-proxy-core`

- **Phases**: this entry's Scope spans several plugin kinds, so a brief milestone breakdown is more honest than a single phase:
  1. Chain wiring — execution order (Auth → Guards → Transform → upstream call → Transform), chain concatenation, and credential resolution via `cred_store`
  2. Built-in Auth plugins — `noop`, `apikey`, `oauth2_client_cred`/`oauth2_client_cred_basic`, and the reserved `basic`/`bearer` identifiers
  3. Built-in Guard/Transform plugins — `required_headers`, `request_id`, and the catalog-only `timeout`/`cors`/`logging`/`metrics` identifiers

- **Scope**:
  - Execution order: Auth → Guards → Transform(`on_request`) → upstream HTTP call → Transform(`on_response`/`on_error`)
  - Upstream-before-route ordering when concatenating plugin chains: `[U1, U2] + [R1, R2] => [U1, U2, R1, R2]`
  - `AuthPlugin` invocation: one resolved plugin per upstream, executed once before guards
  - Built-in Auth plugins: `noop`; `apikey` (header/query injection); `oauth2_client_cred` (Form client auth) and `oauth2_client_cred_basic` (Basic client auth) per `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin` — one-shot token-endpoint exchange, an internal token cache keyed to avoid cross-tenant/cross-subject collisions, default 5-minute cache TTL, credentials sourced via `cred_store` and never logged
  - `basic`/`bearer` auth GTS identifiers recognized as reserved/catalog-only: selecting either as `auth.plugin_type` fails resolution with an "unknown auth plugin" error rather than silently no-op'ing
  - `GuardPlugin` invocation: multiple per upstream/route, any of which can reject the request (before it reaches upstream) or reject the response (before it reaches the caller)
  - Built-in Guard plugin: `required_headers` — independent `required_request_headers`/`required_response_headers` comma-separated configuration; fail-open when a phase's key is absent/blank; case-insensitive header-name matching; first-missing-header short-circuit; `400`+`REQUIRED_HEADER_MISSING` on the request phase and `502`+`REQUIRED_HEADER_MISSING` on the response phase, per `cpt-cf-oagw-adr-required-headers-guard-plugin`
  - `timeout`/`cors` guard GTS identifiers recognized as catalog-only (core Data Plane functionality, not bindable via `plugins.items[].plugin_ref`)
  - `TransformPlugin` invocation: multiple per upstream/route, executed in configured order across declared phases (`on_request`/`on_response`/`on_error`)
  - Built-in Transform plugin: `request_id` (X-Request-ID injection/propagation)
  - `logging`/`metrics` transform GTS identifiers recognized as catalog-only (core Data Plane instrumentation, not resolvable via the transform registry)
  - Credential resolution flow: OAGW resolves `secret_ref` via `cred_store`; `cred_store` checks tenant accessibility (own or ancestor-shared); an inaccessible/missing secret is turned into `401 AuthenticationFailed`
  - GTS type-registration touchpoints this feature's built-in plugins rely on for their `ctx.config` schemas being recognized by the types registry

- **Out of scope**:
  - Custom Starlark plugin execution/sandboxing — deferred; see 2.4's Out of scope for the rationale. This feature wires only the compiled built-in plugins into the registries
  - Persisting plugin/binding records — 2.2/2.3/2.4
  - The proxy request/response forwarding mechanics the chain runs inside — 2.5

- **Requirements Covered**:

  - [ ] `p2` - `cpt-cf-oagw-fr-plugin-system`
  - [ ] `p2` - `cpt-cf-oagw-fr-builtin-plugins`
  - [ ] `p2` - `cpt-cf-oagw-fr-auth-injection`
  - [ ] `p2` - `cpt-cf-oagw-nfr-credential-isolation`
  - [ ] `p2` - `cpt-cf-oagw-contract-cred-store`
  - [ ] `p2` - `cpt-cf-oagw-contract-types-registry`

- **Design Principles Covered**:

  - `cpt-cf-oagw-principle-cred-isolation`

- **Design Constraints Covered**: None

- **Domain Model Entities**:
  - Auth / Guard / Transform plugin
  - Plugin execution chain
  - Credential reference (`secret_ref`)

- **Design Components**:

  - `cpt-cf-oagw-component-model`
  - `cpt-cf-oagw-adr-plugin-system`
  - `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`
  - `cpt-cf-oagw-adr-required-headers-guard-plugin`

- **API**:
  - {METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}] (Auth/Guard/Transform chain executed as part of request handling)

- **Sequences**:

  - `cpt-cf-oagw-seq-proxy-flow`

- **Data**: None

---

## 3. Feature Dependencies

```text
cpt-cf-oagw-feature-gear-foundation
    │
    ├─→ cpt-cf-oagw-feature-upstream-management
    │       │
    │       └─→ cpt-cf-oagw-feature-route-management
    │               │
    │               └─→ cpt-cf-oagw-feature-proxy-core
    │                       │
    │                       ├─→ cpt-cf-oagw-feature-proxy-streaming
    │                       ├─→ cpt-cf-oagw-feature-cors-handling
    │                       ├─→ cpt-cf-oagw-feature-rate-limiting
    │                       └─→ cpt-cf-oagw-feature-plugin-execution
    │                               ↑
    └─→ cpt-cf-oagw-feature-plugin-management ──────────┘
```

**Dependency Rationale**:

- `cpt-cf-oagw-feature-upstream-management` requires `cpt-cf-oagw-feature-gear-foundation`: upstream CRUD needs the gear's configuration store and RFC 9457 error envelope to exist before it can persist resources or report validation errors.
- `cpt-cf-oagw-feature-route-management` requires `cpt-cf-oagw-feature-gear-foundation`: same error/config-store dependency as upstream-management.
- `cpt-cf-oagw-feature-route-management` requires `cpt-cf-oagw-feature-upstream-management`: a Route's `upstream_id` must reference an existing, tenant-owned Upstream; route create/update validates that ownership.
- `cpt-cf-oagw-feature-plugin-management` requires `cpt-cf-oagw-feature-gear-foundation`: plugin CRUD needs the same configuration store and error envelope; it has no dependency on upstream/route management because plugin resources are identified independently and only referenced (not owned) by upstream/route bindings.
- `cpt-cf-oagw-feature-proxy-core` requires `cpt-cf-oagw-feature-gear-foundation`, `cpt-cf-oagw-feature-upstream-management`, `cpt-cf-oagw-feature-route-management`: the proxy path resolves an alias to an Upstream and a Route before it can forward anything, and reports errors through gear-foundation's envelope.
- `cpt-cf-oagw-feature-proxy-streaming` requires `cpt-cf-oagw-feature-proxy-core`: streaming reuses proxy-core's alias/route resolution and header handling for the initial request/handshake before switching to a streaming transfer mode.
- `cpt-cf-oagw-feature-cors-handling` requires `cpt-cf-oagw-feature-proxy-core`: actual-request CORS validation runs after proxy-core's upstream resolution and before its forwarding step.
- `cpt-cf-oagw-feature-rate-limiting` requires `cpt-cf-oagw-feature-proxy-core`: rate-limit evaluation needs the resolved upstream/route/tenant context that proxy-core produces to select the correct counter scope and enforced ancestor limits.
- `cpt-cf-oagw-feature-plugin-execution` requires `cpt-cf-oagw-feature-plugin-management`: the execution chain resolves `plugin_ref`/`plugin_uuid` bindings that plugin-management defines and persists.
- `cpt-cf-oagw-feature-plugin-execution` requires `cpt-cf-oagw-feature-proxy-core`: the Auth/Guard/Transform chain executes inside the proxy request/response lifecycle that proxy-core owns.
- `cpt-cf-oagw-feature-proxy-streaming`, `cpt-cf-oagw-feature-cors-handling`, `cpt-cf-oagw-feature-rate-limiting`, and `cpt-cf-oagw-feature-plugin-execution` are independent of each other (given `proxy-core` and, for `plugin-execution`, `plugin-management`) and can be developed in parallel.
- `cpt-cf-oagw-feature-plugin-management` is independent of `cpt-cf-oagw-feature-upstream-management` and `cpt-cf-oagw-feature-route-management` (given `gear-foundation`) and can be developed in parallel with both.
