# Feature: Alias Resolution, Hierarchical Config Merge and Route Matching


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Proxied Request Configuration Resolution](#proxied-request-configuration-resolution)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Alias Normalization](#alias-normalization)
  - [Alias Lookup with Hierarchy Walk and Shadowing](#alias-lookup-with-hierarchy-walk-and-shadowing)
  - [Per-Field Sharing-Mode Merge](#per-field-sharing-mode-merge)
  - [HTTP Route Selection](#http-route-selection)
  - [Header Transformation Plan Computation](#header-transformation-plan-computation)
  - [Resolved-Configuration Cache Lookup](#resolved-configuration-cache-lookup)
  - [Resolved-Configuration Cache Invalidation](#resolved-configuration-cache-invalidation)
- [4. States (CDSL)](#4-states-cdsl)
  - [Resolved-Configuration Cache Entry State Machine](#resolved-configuration-cache-entry-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Request Alias Normalization](#request-alias-normalization)
  - [Alias Hierarchy Lookup and Shadowing](#alias-hierarchy-lookup-and-shadowing)
  - [Per-Field Sharing-Mode Merge](#per-field-sharing-mode-merge-1)
  - [Tag Union Across Hierarchy](#tag-union-across-hierarchy)
  - [HTTP Route Selection](#http-route-selection-1)
  - [Route-Not-Found Outcome](#route-not-found-outcome)
  - [Upstream-Disabled Outcome](#upstream-disabled-outcome)
  - [Header-Transformation Plan Computation](#header-transformation-plan-computation-1)
  - [Resolved-Configuration Cache Lookup](#resolved-configuration-cache-lookup-1)
  - [Resolved-Configuration Cache Invalidation](#resolved-configuration-cache-invalidation-1)
  - [Enforced Limits Preserved Across Shadowing](#enforced-limits-preserved-across-shadowing)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-config-resolution-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-config-resolution`

## 1. Feature Context

### 1.1 Overview

This feature resolves a proxied request's normalized alias, tenant hierarchy, and matching HTTP route into one merged configuration plan the data plane executes. It also computes the effective header-transformation plan and caches the resolved plan in-process so identical requests skip recomputation.

### 1.2 Purpose

Every proxied request at `/oagw/v1/proxy/{alias}/{path}` identifies its target only by a human-readable alias plus an inbound method and path, so the gateway must turn that shorthand into concrete, merged configuration before any plugin or upstream call runs. This feature walks the tenant hierarchy to find the closest matching upstream, merges upstream, route, and tenant settings per field and sharing mode, selects the matching HTTP route by method and longest path-prefix, and derives the effective header-transformation plan, caching the result so repeat requests avoid recomputation.

Route selection matches on the inbound method plus the longest path prefix, so the method is a selection key and never a post-match guard. A request whose method appears in no candidate route yields the route-not-found outcome, which the HTTP proxy feature renders as `404`. Rejecting a request for a disallowed query parameter or an unsupported path suffix stays a post-match guard owned by the HTTP proxy feature. gRPC-protocol routes are excluded entirely from the route-selection process described here; only HTTP match keys are evaluated.

The descendant-to-root hierarchy walk ignores an upstream whose `enabled` field is false and continues toward the root tenant, so the closest enabled match wins. When an alias resolves only to disabled upstreams, this feature reports an upstream-disabled outcome that the HTTP proxy feature renders as `503`. Route `enabled` and `priority` are application-level fields accepted and stored by the route management feature beyond `route.v1.schema.json`, which defines neither. A stored route carrying no `enabled` value counts as enabled, which keeps the enabled-route selection filter implementable on that basis.

This feature owns the hierarchical merge and emits each merged result exactly once for the rest of the data plane. It emits the single merged `rate_limit` value and the plugin bindings concatenated in the order upstream tier, then route tier, then tenant tier. Downstream features consume those merged values as given and never recompute a sharing mode or re-derive an effective limit.

Rollout and rollback are not applicable at feature level, because this resolution path ships inside the gear binary with no independent deployment toggle.

**Requirements**: `cpt-cf-oagw-fr-alias-resolution`, `cpt-cf-oagw-fr-config-layering`, `cpt-cf-oagw-fr-hierarchical-config`, `cpt-cf-oagw-fr-header-transform`, `cpt-cf-oagw-fr-request-proxy`, `cpt-cf-oagw-nfr-low-latency`

**Principles**: None

Persistence for the upstream, route, and plugin records this feature merges and matches against is in-process for the graded deployment; no database is configured for OAGW. `cpt-cf-oagw-db-schema` is cited only as informing those entities' shape and invariants, not as an actual SQL table this feature queries. Likewise, resolved upstream endpoints may carry the documented `http`/`ws` scheme extension, but whether a plaintext connection is actually opened is decided by `allow_http_upstream` in the HTTP proxy feature, not here.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Sends the proxied request whose alias, merged configuration, and matching route this feature resolves. |
| `cpt-cf-oagw-actor-platform-operator` | Writes upstream, route, or plugin configuration whose control-plane write invalidates the resolved-configuration cache. |
| `cpt-cf-oagw-actor-tenant-admin` | Writes tenant-scoped upstream, route, or plugin configuration whose control-plane write invalidates the resolved-configuration cache. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Design elements**: `cpt-cf-oagw-component-model`
- **Sequences**: `cpt-cf-oagw-seq-proxy-flow`
- **ADRs**: `cpt-cf-oagw-adr-request-routing`, `cpt-cf-oagw-adr-data-plane-caching`, `cpt-cf-oagw-adr-state-management`
- **Dependencies**: `cpt-cf-oagw-feature-route-management` — this feature merges and matches against upstream and route data that must already be creatable and readable before resolution can run. `cpt-cf-oagw-feature-http-proxy` depends on this feature's resolved plan to execute the outbound call, but that execution stays out of this feature's scope.

## 2. Actor Flows (CDSL)

User-facing interaction that starts when an application developer's proxied request arrives and needs its alias, merged configuration, and matching route resolved before execution.

**Use Cases**:
- [ ] `p1` - `cpt-cf-oagw-usecase-proxy-request`

### Proxied Request Configuration Resolution

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-resolve-proxy-request`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The alias resolves to an upstream in the tenant hierarchy, a route matches by method and longest path-prefix, and the merged configuration and header plan are produced or served from cache.

**Error Scenarios**:
- No upstream anywhere in the tenant hierarchy carries the requested alias, or no enabled route matches the inbound method and path, yielding the route-not-found outcome.
- Every tenant tier carrying the requested alias holds only upstreams whose `enabled` field is false, yielding the upstream-disabled outcome.

**Steps**:
1. [ ] - `p1` - Application developer's request (plain HTTP, SSE, or WebSocket upgrade) arrives at `/oagw/v1/proxy/{alias}/{path}` - `inst-resolve-proxy-01`
2. [ ] - `p1` - System normalizes the inbound alias through `cpt-cf-oagw-algo-request-alias-normalize` - `inst-resolve-proxy-02`
3. [ ] - `p1` - System performs a resolved-configuration cache lookup through `cpt-cf-oagw-algo-resolved-config-cache-lookup`, keyed on the calling tenant, normalized alias, method, and path - `inst-resolve-proxy-03`
4. [ ] - `p1` - **IF** the cache lookup hits - `inst-resolve-proxy-04`
   1. [ ] - `p1` - **RETURN** the cached merged configuration, matched route, and header-transformation plan - `inst-resolve-proxy-05`
5. [ ] - `p1` - **ELSE** - `inst-resolve-proxy-06`
   1. [ ] - `p1` - System walks the tenant hierarchy to locate the closest enabled matching upstream through `cpt-cf-oagw-algo-alias-hierarchy-lookup` - `inst-resolve-proxy-07`
6. [ ] - `p1` - **IF** no tenant in the hierarchy holds an upstream carrying the normalized alias - `inst-resolve-proxy-08`
   1. [ ] - `p1` - **RETURN** the route-not-found outcome without populating the cache - `inst-resolve-proxy-09`
7. [ ] - `p1` - **IF** every tier carrying the alias holds only upstreams whose `enabled` field is false - `inst-resolve-proxy-19`
   1. [ ] - `p1` - **RETURN** the upstream-disabled outcome without populating the cache - `inst-resolve-proxy-20`
8. [ ] - `p1` - **ELSE** - `inst-resolve-proxy-10`
   1. [ ] - `p1` - System merges auth, headers, rate_limit, plugins, and cors across the Upstream, Route, and Tenant tiers, and the effective tag set, through `cpt-cf-oagw-algo-sharing-mode-merge` - `inst-resolve-proxy-11`
9. [ ] - `p1` - System selects the matching HTTP route by inbound method and longest path-prefix through `cpt-cf-oagw-algo-http-route-select` - `inst-resolve-proxy-12`
10. [ ] - `p1` - **IF** no enabled route matches the inbound method and path - `inst-resolve-proxy-13`
    1. [ ] - `p1` - **RETURN** the route-not-found outcome without populating the cache - `inst-resolve-proxy-14`
11. [ ] - `p1` - **ELSE** - `inst-resolve-proxy-15`
    1. [ ] - `p1` - System computes the effective header-transformation plan through `cpt-cf-oagw-algo-header-plan-compute` - `inst-resolve-proxy-16`
12. [ ] - `p1` - System stores the merged configuration, matched route, and header-transformation plan in the resolved-configuration cache under the request's cache key - `inst-resolve-proxy-17`
13. [ ] - `p1` - **RETURN** the merged configuration, the matched route, the merged rate limit, the concatenated plugin bindings, and the header plan - `inst-resolve-proxy-18`

## 3. Processes / Business Logic (CDSL)

Internal resolution routines invoked by the proxied-request flow above; they do not interact with actors directly.

### Alias Normalization

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-request-alias-normalize`

**Input**: the raw alias path segment taken from an inbound proxy request URL.

**Output**: the normalized alias string used for hierarchy lookup.

**Steps**:
1. [ ] - `p1` - Strip a single trailing dot from the alias when present - `inst-alias-norm-req-01`
2. [ ] - `p1` - Convert every ASCII character in the alias to lowercase - `inst-alias-norm-req-02`
3. [ ] - `p1` - **RETURN** the normalized alias for hierarchy lookup - `inst-alias-norm-req-03`

### Alias Lookup with Hierarchy Walk and Shadowing

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-alias-hierarchy-lookup`

**Input**: the normalized alias and the calling tenant's position in the tenant hierarchy.

**Output**: the closest enabled matching upstream and its owning tenant, an upstream-disabled outcome, or a not-found result.

**Steps**:
1. [ ] - `p1` - Start the walk at the calling tenant - `inst-alias-lookup-01`
2. [ ] - `p1` - **FOR EACH** tenant visited, walking from the calling tenant toward the root tenant - `inst-alias-lookup-02`
   1. [ ] - `p1` - Search that tenant's own upstream records for one whose alias equals the normalized alias - `inst-alias-lookup-03`
   2. [ ] - `p1` - **IF** a matching upstream record is found and its `enabled` field is not false - `inst-alias-lookup-04`
      1. [ ] - `p1` - **RETURN** that upstream and its owning tenant as the closest enabled match, ending the walk - `inst-alias-lookup-05`
   3. [ ] - `p1` - **ELSE IF** a matching upstream record is found while its `enabled` field is false - `inst-alias-lookup-08`
      1. [ ] - `p1` - Record that a disabled match was seen and continue the walk toward the root tenant - `inst-alias-lookup-09`
3. [ ] - `p1` - **IF** the walk reaches the root tenant having recorded a disabled match and no enabled match - `inst-alias-lookup-10`
   1. [ ] - `p1` - **RETURN** the upstream-disabled outcome naming the normalized alias - `inst-alias-lookup-11`
4. [ ] - `p1` - **IF** the walk reaches the root tenant with no matching upstream found at any tier - `inst-alias-lookup-06`
   1. [ ] - `p1` - **RETURN** a not-found result - `inst-alias-lookup-07`

### Per-Field Sharing-Mode Merge

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-sharing-mode-merge`

**Input**: the resolved upstream's own `auth`, `headers`, `rate_limit`, `plugins`, `cors`, and `tags`; the matched route's `plugins`, `rate_limit`, and `tags`; and the calling tenant's own override values, sharing modes, and tags for these fields.

**Output**: the effective (merged) value for each field, applied in Upstream < Route < Tenant order, plus the effective tag set.

**Steps**:
1. [ ] - `p1` - **FOR EACH** configuration field among `auth`, `headers`, `rate_limit`, `plugins`, and `cors` - `inst-merge-01`
   1. [ ] - `p1` - Start from the Upstream tier's own value for that field as the base - `inst-merge-02`
   2. [ ] - `p1` - **IF** the Route tier defines an override for that field (`rate_limit` or `plugins` only, since `route.v1.schema.json` carries neither `auth`, `headers`, nor `cors`) - `inst-merge-03`
      1. [ ] - `p1` - Apply the Route tier's sharing mode: adopt the Route value under `inherit`, force it under `enforce`, or keep the Upstream value under `private` - `inst-merge-04`
   3. [ ] - `p1` - **IF** the Tenant tier defines its own override for that field - `inst-merge-05`
      1. [ ] - `p1` - **IF** the current value's sharing mode is `private` - `inst-merge-06`
         1. [ ] - `p1` - Keep the current value; the Tenant tier's override is not visible - `inst-merge-07`
      2. [ ] - `p1` - **ELSE IF** the current value's sharing mode is `inherit` - `inst-merge-08`
         1. [ ] - `p1` - Adopt the Tenant tier's override in place of the current value - `inst-merge-09`
      3. [ ] - `p1` - **ELSE** (`enforce`) - `inst-merge-10`
         1. [ ] - `p1` - Keep the current, ancestor-enforced value regardless of the Tenant tier's override - `inst-merge-11`
   4. [ ] - `p1` - **IF** the field is `rate_limit` and both an ancestor-enforced value and a Tenant value exist - `inst-merge-12`
      1. [ ] - `p1` - Set the effective value to the stricter of the two: `effective = min(ancestor.enforced, tenant)` - `inst-merge-13`
   5. [ ] - `p1` - **IF** the field is `plugins` - `inst-merge-14`
      1. [ ] - `p1` - Set the effective value to `upstream.plugins + route.plugins + tenant.plugins`, concatenated in that order, never removing an enforced entry - `inst-merge-15`
   6. [ ] - `p1` - **IF** the field is `cors` and the ancestor's sharing mode for it is `inherit` - `inst-merge-18`
      1. [ ] - `p1` - Set the effective `allowed_origins` to the union of the ancestor's and the descendant's origin lists - `inst-merge-19`
   7. [ ] - `p1` - **ELSE IF** the field is `cors` and the ancestor's sharing mode for it is `enforce` - `inst-merge-20`
      1. [ ] - `p1` - Keep the ancestor's `allowed_origins` unchanged, so the descendant adds no origin of its own - `inst-merge-21`
2. [ ] - `p1` - Compute the effective tag set as `union(upstream.tags, route.tags, tenant.tags)`, independent of sharing mode - `inst-merge-16`
3. [ ] - `p1` - **RETURN** the effective value for every field and the effective tag set - `inst-merge-17`

### HTTP Route Selection

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-http-route-select`

**Input**: the resolved upstream's identifier, its enabled HTTP-match routes, and the inbound request's method and path.

**Output**: the selected route, including its `path_suffix_mode`, or a route-not-found result.

A route counts as enabled when its application-level `enabled` field is true or absent, since `route.v1.schema.json` defines no such property. Route management accepts and stores that field, together with `priority`, beyond the checked-in schema. The inbound method is a selection key here, so a method that no candidate route lists produces route-not-found rather than a guard rejection.

**Steps**:
1. [ ] - `p1` - Filter the upstream's enabled routes to those whose `match.http.methods` contains the inbound method - `inst-route-select-01`
2. [ ] - `p1` - **IF** no route survives the method filter - `inst-route-select-02`
   1. [ ] - `p1` - **RETURN** the route-not-found outcome - `inst-route-select-03`
3. [ ] - `p1` - **ELSE** among the surviving routes, test each route's `match.http.path` as a candidate prefix of the inbound path - `inst-route-select-04`
4. [ ] - `p1` - **IF** no surviving route's `match.http.path` is a prefix of the inbound path - `inst-route-select-05`
   1. [ ] - `p1` - **RETURN** the route-not-found outcome - `inst-route-select-06`
5. [ ] - `p1` - **ELSE** select the route whose matching `match.http.path` is the longest such prefix - `inst-route-select-07`
6. [ ] - `p1` - **RETURN** the selected route together with its `path_suffix_mode`, leaving path-suffix rejection to the executing proxy path - `inst-route-select-08`

### Header Transformation Plan Computation

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-header-plan-compute`

**Input**: the effective (merged) upstream `headers.request` and `headers.response` configuration.

**Output**: an effective header-transformation plan for request and response headers.

**Steps**:
1. [ ] - `p1` - Record the effective `request.set`, `request.add`, and `request.remove` entries from the merged headers configuration - `inst-header-plan-01`
2. [ ] - `p1` - **IF** `request.passthrough` is `allowlist` - `inst-header-plan-02`
   1. [ ] - `p1` - Record `request.passthrough_allowlist` as the set of inbound headers to forward - `inst-header-plan-03`
3. [ ] - `p1` - **ELSE IF** `request.passthrough` is `all` - `inst-header-plan-04`
   1. [ ] - `p1` - Record that every inbound header not otherwise removed is forwarded - `inst-header-plan-05`
4. [ ] - `p1` - **ELSE** (`none`) - `inst-header-plan-06`
   1. [ ] - `p1` - Record that no inbound header is forwarded except those the `set`/`add` rules introduce - `inst-header-plan-07`
5. [ ] - `p1` - Record the effective `response.set`, `response.add`, and `response.remove` entries from the merged headers configuration - `inst-header-plan-08`
6. [ ] - `p1` - **RETURN** the assembled request and response header-transformation plan for the executing proxy path - `inst-header-plan-09`

### Resolved-Configuration Cache Lookup

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-resolved-config-cache-lookup`

**Input**: the calling `tenant_id`, normalized alias, inbound method, and inbound path.

**Output**: a cached resolved plan (merged configuration, matched route, header-transformation plan), or a cache-miss result.

**Steps**:
1. [ ] - `p1` - Compose the cache key from `tenant_id`, normalized alias, method, and path - `inst-cache-lookup-01`
2. [ ] - `p1` - Search the in-process resolved-configuration cache for an entry matching that key - `inst-cache-lookup-02`
3. [ ] - `p1` - **IF** an entry is found - `inst-cache-lookup-03`
   1. [ ] - `p1` - **RETURN** the cached merged configuration, matched route, and header-transformation plan - `inst-cache-lookup-04`
4. [ ] - `p1` - **ELSE** - `inst-cache-lookup-05`
   1. [ ] - `p1` - **RETURN** a cache-miss result, deferring to alias lookup, merge, route selection, and header-plan computation - `inst-cache-lookup-06`

### Resolved-Configuration Cache Invalidation

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-resolved-config-cache-invalidate`

**Input**: notice of a control-plane write (create, replace, or delete) to an upstream, route, or plugin record.

**Output**: an emptied resolved-configuration cache.

**Steps**:
1. [ ] - `p1` - Receive notice that an upstream, route, or plugin record was created, replaced, or deleted - `inst-cache-invalidate-01`
2. [ ] - `p1` - Remove every entry from the in-process resolved-configuration cache, since a single write's effect on shadowed and inherited entries cannot be scoped to specific keys - `inst-cache-invalidate-02`
3. [ ] - `p1` - **RETURN** control to the control-plane write path once the cache is empty - `inst-cache-invalidate-03`

## 4. States (CDSL)

### Resolved-Configuration Cache Entry State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-resolved-config-cache-entry`

**States**: Absent, Populated

**Initial State**: Absent

**Transitions**:
1. [ ] - `p1` - **FROM** Absent **TO** Populated **WHEN** a cache lookup misses and alias lookup, merge, route selection, and header-plan computation all complete successfully - `inst-cache-state-01`
2. [ ] - `p1` - **FROM** Populated **TO** Absent **WHEN** any control-plane write to an upstream, route, or plugin record invalidates the cache - `inst-cache-state-02`
3. [ ] - `p1` - **FROM** Absent **TO** Absent **WHEN** a cache lookup misses and lookup or selection yields the route-not-found or upstream-disabled outcome - `inst-cache-state-03`

## 5. Definitions of Done

### Request Alias Normalization

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-alias-normalization`

The system **MUST** normalize the inbound proxy request's alias path segment to ASCII lowercase with a single trailing dot stripped before any hierarchy lookup runs.

**Implements**:
- `cpt-cf-oagw-flow-resolve-proxy-request`
- `cpt-cf-oagw-algo-request-alias-normalize`

**Touches**:
- Entities: `EffectiveUpstream`

### Alias Hierarchy Lookup and Shadowing

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-alias-hierarchy-lookup`

The system **MUST** walk the tenant hierarchy from the calling tenant toward the root tenant, skip any upstream whose `enabled` field is false, and select the closest remaining tier whose own upstream record carries the normalized alias.

**Implements**:
- `cpt-cf-oagw-flow-resolve-proxy-request`
- `cpt-cf-oagw-algo-alias-hierarchy-lookup`

**Touches**:
- Entities: `EffectiveUpstream`

### Per-Field Sharing-Mode Merge

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-sharing-mode-merge`

The system **MUST** merge `auth`, `headers`, `rate_limit`, `plugins`, and `cors` across the Upstream, Route, and Tenant tiers in that priority order, applying each field's `private`, `inherit`, or `enforce` sharing mode.

The system **MUST** treat three fields as special cases rather than whole-value replacements: `rate_limit` resolves to the stricter of the two values, `plugins` concatenates as upstream, then route, then tenant, and `cors` unions `allowed_origins` under `inherit` while forcing the ancestor's list under `enforce`.

The system **MUST** emit the single merged `rate_limit` value and the concatenated plugin bindings as the resolved plan's output, so no downstream feature recomputes a sharing mode.

**Implements**:
- `cpt-cf-oagw-algo-sharing-mode-merge`
- `cpt-cf-oagw-flow-resolve-proxy-request`

**Touches**:
- Entities: `EffectiveUpstream`, `MatchedRoute`

### Tag Union Across Hierarchy

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-tag-union`

The system **MUST** compute the effective tag set as the union of upstream, route, and tenant tags, so a descendant tenant can add tags but never remove an ancestor's tag.

**Implements**:
- `cpt-cf-oagw-algo-sharing-mode-merge`

**Touches**:
- Entities: `EffectiveUpstream`

### HTTP Route Selection

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-http-route-selection`

The system **MUST** select a route by filtering enabled routes on the inbound method against `match.http.methods`, then choosing the surviving candidate whose `match.http.path` is the longest prefix of the inbound path.

The system **MUST** treat the application-level route field `enabled` as true when it is absent, because `route.v1.schema.json` defines neither `enabled` nor `priority`.

**Implements**:
- `cpt-cf-oagw-algo-http-route-select`

**Touches**:
- Entities: `MatchedRoute`

### Route-Not-Found Outcome

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-not-found-outcome`

The system **MUST** report the route-not-found outcome, rather than a guard rejection, whenever no upstream in the hierarchy carries the requested alias or no enabled route survives method-and-prefix selection.

**Implements**:
- `cpt-cf-oagw-flow-resolve-proxy-request`
- `cpt-cf-oagw-algo-alias-hierarchy-lookup`
- `cpt-cf-oagw-algo-http-route-select`

**Touches**:
- Entities: `EffectiveUpstream`, `MatchedRoute`

### Upstream-Disabled Outcome

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-disabled-outcome`

The system **MUST** report the upstream-disabled outcome, without populating the cache, when every hierarchy tier carrying the requested alias holds only upstreams whose `enabled` field is false.

**Implements**:
- `cpt-cf-oagw-flow-resolve-proxy-request`
- `cpt-cf-oagw-algo-alias-hierarchy-lookup`

**Touches**:
- Entities: `EffectiveUpstream`

### Header-Transformation Plan Computation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-header-transformation-plan`

The system **MUST** compute an effective header-transformation plan enumerating `set`, `add`, `remove`, and `passthrough` (with its allowlist when applicable) from the merged upstream `headers` configuration, for both request and response directions.

**Implements**:
- `cpt-cf-oagw-algo-header-plan-compute`

**Touches**:
- Entities: `EffectiveUpstream`

### Resolved-Configuration Cache Lookup

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-resolved-config-cache-lookup`

The system **MUST** serve a proxy request from the in-process resolved-configuration cache, keyed by `tenant_id`, normalized alias, method, and path, without repeating alias lookup, merge, or route selection on a hit.

**Implements**:
- `cpt-cf-oagw-flow-resolve-proxy-request`
- `cpt-cf-oagw-algo-resolved-config-cache-lookup`

**Touches**:
- Entities: `CacheKey`, `EffectiveUpstream`, `MatchedRoute`

### Resolved-Configuration Cache Invalidation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-resolved-config-cache-invalidation`

The system **MUST** empty the resolved-configuration cache whenever any upstream, route, or plugin record is created, replaced, or deleted, so the next proxy request recomputes resolution.

**Implements**:
- `cpt-cf-oagw-algo-resolved-config-cache-invalidate`

**Touches**:
- Entities: `CacheKey`

### Enforced Limits Preserved Across Shadowing

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-enforced-limits-across-shadowing`

The system **MUST** keep an ancestor's `enforce`-mode rate limit and enforced plugin bindings applied in the effective configuration even when a descendant tenant shadows the same alias with its own upstream.

**Implements**:
- `cpt-cf-oagw-algo-sharing-mode-merge`
- `cpt-cf-oagw-algo-alias-hierarchy-lookup`

**Touches**:
- Entities: `EffectiveUpstream`

## 6. Acceptance Criteria

- [ ] A request alias `Api.OpenAI.COM.` normalizes to `api.openai.com` before hierarchy lookup runs.
- [ ] A proxy request from a sub-sub-tenant resolves alias `api.openai.com` against the sub-sub-tenant's own upstreams first, then the parent tenant's, then the root tenant's; the closest tier holding that alias wins.
- [ ] When no tenant in the hierarchy holds an upstream with the requested alias, resolution reports the route-not-found outcome.
- [ ] When the sub-tenant's upstream for alias `api.openai.com` has `enabled` set to `false`, the walk continues and selects the root tenant's enabled upstream for that alias.
- [ ] When every tier holding alias `api.openai.com` has `enabled` set to `false`, resolution reports the upstream-disabled outcome and populates no cache entry.
- [ ] With `auth.sharing: private` at an ancestor, a descendant's effective configuration never surfaces the ancestor's `auth` value.
- [ ] With `auth.sharing: inherit` at an ancestor and no descendant override, the descendant's effective `auth` equals the ancestor's `auth`.
- [ ] With `rate_limit.sharing: enforce` at rate 10000/min on an ancestor and a descendant `rate_limit` of 500/min, the effective rate is `min(10000, 500) = 500`.
- [ ] With upstream plugins `[U1, U2]` and route plugins `[R1, R2]`, the effective plugin list is `[U1, U2, R1, R2]` in that order.
- [ ] With upstream plugins `[U1]`, route plugins `[R1]`, and tenant plugins `[T1]`, resolution emits the single binding list `[U1, R1, T1]`.
- [ ] With ancestor `cors.sharing: inherit` and `allowed_origins: ["https://app.example.com"]` plus a descendant list `["https://admin.example.com"]`, both origins are effective.
- [ ] With ancestor `cors.sharing: enforce` and `allowed_origins: ["https://app.example.com"]`, a descendant adding `https://admin.example.com` still resolves to only the ancestor's origin.
- [ ] Because `route.v1.schema.json` defines no `auth`, `headers`, or `cors` fields, merging those three fields uses only the Upstream and Tenant tiers, skipping the Route tier.
- [ ] The effective tag set for a descendant with tag `b` under an ancestor with tag `a` is `{a, b}`; the descendant's resolved configuration cannot omit tag `a`.
- [ ] A request whose method is absent from a candidate route's `match.http.methods` excludes that route from selection, so a different enabled route matching the method and the longest path prefix is selected instead.
- [ ] Among two enabled routes with `match.http.path` values `/v1` and `/v1/chat/completions` under the same upstream, a request to `/v1/chat/completions/extra` selects the `/v1/chat/completions` route.
- [ ] When no enabled route survives the method-and-prefix filter, resolution reports the route-not-found outcome, not a guard-style rejection.
- [ ] A `DELETE` request to `/oagw/v1/proxy/api.example.com/v1/items`, whose only route lists methods `GET` and `POST`, reports the route-not-found outcome.
- [ ] A stored route carrying no `enabled` field is a selection candidate, while a route with `enabled` set to `false` is excluded from candidates.
- [ ] A route selected with `path_suffix_mode: disabled` is still selected by method-and-prefix criteria even when the inbound path carries a suffix; suffix rejection is left to the executing proxy path.
- [ ] The effective header-transformation plan enumerates `set`, `add`, `remove`, and, when `passthrough` is `allowlist`, the `passthrough_allowlist` entries from the merged upstream `headers` configuration.
- [ ] Two identical proxy requests (same `tenant_id`, alias, method, and path) resolve alias lookup, merge, and route selection once; the second request is served from the resolved-configuration cache.
- [ ] Two requests differing only in inbound method, or only in inbound path, to the same alias populate distinct resolved-configuration cache entries.
- [ ] Creating, replacing, or deleting any upstream, route, or plugin record empties the resolved-configuration cache, so the next proxy request recomputes resolution instead of returning a stale cached plan.
- [ ] A descendant tenant that shadows an ancestor's alias with its own upstream still has the ancestor's `enforce`-mode rate limit applied: `effective = min(ancestor.enforced, descendant)`.
