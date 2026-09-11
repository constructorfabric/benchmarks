# Feature: Upstream Management


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Authorization Disposition](#15-authorization-disposition)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Create Upstream](#create-upstream)
  - [List Upstreams](#list-upstreams)
  - [Get Upstream](#get-upstream)
  - [Replace Upstream](#replace-upstream)
  - [Delete Upstream](#delete-upstream)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Validate Upstream Payload](#validate-upstream-payload)
  - [Derive and Resolve Alias](#derive-and-resolve-alias)
  - [Propagate Enable/Disable State](#propagate-enabledisable-state)
  - [Cascade Delete Upstream Routes](#cascade-delete-upstream-routes)
- [4. States (CDSL)](#4-states-cdsl)
  - [Upstream Availability State Machine](#upstream-availability-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Create Upstream](#create-upstream-1)
  - [Scheme Acceptance Widening](#scheme-acceptance-widening)
  - [Alias Derivation and Immutability](#alias-derivation-and-immutability)
  - [Alias Uniqueness](#alias-uniqueness)
  - [Payload Validation Errors](#payload-validation-errors)
  - [CORS Wildcard/Credentials Validation](#cors-wildcardcredentials-validation)
  - [List Upstreams](#list-upstreams-1)
  - [Get Upstream](#get-upstream-1)
  - [Replace Upstream](#replace-upstream-1)
  - [Delete Upstream and Cascade Routes](#delete-upstream-and-cascade-routes)
  - [Enable/Disable Propagation](#enabledisable-propagation)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-um-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-oagw-feature-upstream-management`
## 1. Feature Context

### 1.1 Overview

This feature provides tenant-scoped CRUD management of Upstream resources — the server-endpoint, protocol, auth, and policy configuration that every proxy request ultimately targets — including field-level schema validation, alias derivation/immutability rules, and enable/disable propagation across the tenant hierarchy.

### 1.2 Purpose

Upstreams are the fundamental configuration unit of OAGW: before any request can be proxied, an operator or tenant administrator must define where it goes (server endpoints, protocol), how it authenticates, and under what policies (headers, rate limits, CORS, plugins) it runs. This feature realizes that control-plane surface — the four write/read operations (`POST`, `GET`, `PUT`, `DELETE`) against `/oagw/v1/upstreams` — and the validation, derivation and lifecycle rules that keep the resulting configuration well-formed and safely scoped per tenant. Route-level enable/disable, and alias resolution performed at proxy request time (the tenant-hierarchy shadowing search from descendant to root), are explicitly out of scope here and belong to Route Management and HTTP Request Proxying respectively; this feature only defines the configuration and derivation rules those later features consume.

**Requirements**: `cpt-cf-oagw-fr-upstream-mgmt`, `cpt-cf-oagw-fr-enable-disable`, `cpt-cf-oagw-fr-alias-resolution`, `cpt-cf-oagw-fr-hierarchical-config`, `cpt-cf-oagw-nfr-multi-tenancy`

**Principles**: `cpt-cf-oagw-principle-tenant-scope`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Creates, lists, reads, replaces and deletes global upstream configurations; may bind against an ancestor tenant's alias and enforce configuration on descendants. |
| `cpt-cf-oagw-actor-tenant-admin` | Creates, lists, reads, replaces and deletes upstream configurations scoped to their own tenant hierarchy, within the sharing-mode permissions granted by ancestor tenants. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Dependencies**: Gear Foundation and Configuration feature (`cpt-cf-oagw-feature-gear-foundation`) — supplies gear registration, typed configuration, shared gear state and the RFC 9457 error-mapping conventions this feature's error responses use.

### 1.5 Authorization Disposition

Generic request authentication and coarse authorization are performed by the host runtime before a request reaches this feature; this feature does not re-implement them. This feature owns exactly one OAGW-specific authorization decision of its own: the bind-permission check on Create Upstream described in the bind-to-ancestor error branches below, which fails with `403` through `cpt-cf-oagw-algo-gf-error-mapping`'s `permission denied` category (`cpt-cf-oagw-dod-gf-tenant-context`, `cpt-cf-oagw-dod-gf-error-mapping`). In the graded configuration the bind path — and therefore this permission check — is exercised only when an ancestor-owned upstream at a visible (non-`private`) sharing configuration already exists for the resolved alias; an ordinary tenant-scoped create never evaluates bind permission at all.

Beyond that one bind-permission decision, this feature performs no further OAGW-specific permission checks of its own: the `:create` write itself is gated only by the host runtime's coarse authorization, never by a feature-owned precondition. In this configuration the bind-permission check itself is never reached either — see the tenant-hierarchy deferral below — so this feature currently raises no `403` of its own at all; the 401 and 403 canonical categories exist in `cpt-cf-oagw-algo-gf-error-mapping` and are exercised by unit tests covering other, already-served cases, but no code path in this feature evaluates `oagw:upstream:bind` or otherwise raises `permission denied` in this configuration.

**Tenant-hierarchy deferral**: this gear has no access to a tenant-hierarchy source in this configuration, so every mechanism above that depends on walking from the calling tenant toward the root is not served. Concretely: the ancestor-alias bind path — and therefore the `oagw:upstream:bind` permission check that gates it — is described above but is never reachable, because there is no ancestor tenant to resolve against; sharing-mode evaluation (`private` / `inherit` / `enforce` on `auth.sharing`, `plugins.sharing`, `rate_limit.sharing`, `cors.sharing`) is accepted and stored at write time (`cpt-cf-oagw-algo-um-validate-payload`) but is not acted upon at request time — the field is inert; ancestor-disable propagation and the `DisabledByAncestor` effective state described in `cpt-cf-oagw-algo-um-enable-disable-propagation` and `cpt-cf-oagw-state-um-availability` do not occur, since there is no ancestor to disable from; and the hierarchical rate-limit merge that `cpt-cf-oagw-feature-traffic-policy` defines is likewise unreachable (see that feature's §1.5). Alias resolution, enable/disable, and rate limiting therefore all operate within the calling tenant only in this configuration.

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-oagw-usecase-configure-upstream`

### Create Upstream

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-um-create-upstream`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A hostname-based endpoint is submitted without an `alias`; the system derives the alias and returns `201` with the server-generated `id` and the derived alias.
- A hostname-based endpoint is submitted with a caller-supplied `alias` that exactly matches the value the system would have derived; the system accepts it as an idempotent no-op and returns `201`.
- An IP-literal endpoint is submitted together with a caller-supplied `alias`; the system accepts the explicit alias as-is (normalized) and returns `201`.
- An endpoint is submitted with `scheme: "http"` and `port: 80`; the system accepts the plaintext scheme value at validation time and returns `201`. (Whether a plaintext connection is later attempted is a data-plane question owned by HTTP Request Proxying and gated by `allow_http_upstream`; this flow only governs acceptance of the field value.)
- The submitted `alias` matches an existing upstream owned by an ancestor tenant whose sharing configuration exposes at least one of `auth.sharing`/`plugins.sharing`/`rate_limit.sharing` as `inherit` or `enforce` (i.e., is not entirely `private`); the system treats the request as a bind to that ancestor's alias, subject to those sharing modes (`enforce` blocks descendant override) and the caller holding bind permission, and returns `201`.
- The submitted `alias` matches an existing upstream owned by an ancestor tenant whose sharing configuration is entirely `private` across `auth`, `plugins`, and `rate_limit`; that ancestor is not visible for binding, so the system does NOT bind and instead proceeds as an ordinary tenant-scoped create, unaffected by the ancestor's alias or configuration, and returns `201`.

**Error Scenarios**:
- The caller-supplied `alias` differs from the alias the system would derive for a hostname-based endpoint set: `400` problem+json naming the `alias` field.
- All endpoints are IP-literal (or otherwise non-derivable) and no `alias` is supplied: `400` problem+json naming the `alias` field as required.
- The resolved alias (derived or explicit) already exists for another upstream owned by the same tenant: `409` alias-conflict.
- The payload violates the upstream schema (missing `server` or `protocol`, unknown property, invalid enum value, malformed endpoint): `400` problem+json naming the offending field.
- `cors.allow_credentials` is `true` while `cors.allowed_origins` contains the wildcard `"*"`: `400` problem+json naming the `cors` field, rejected at write time.
- The resolved alias matches a visible (non-`private`) ancestor-tenant upstream, but the caller does not hold `oagw:upstream:bind` permission: `403` problem+json (per `cpt-cf-oagw-algo-gf-error-mapping`'s `permission denied` category) — the create does not fall back to an ordinary tenant-scoped create in this case, since the alias is genuinely contested with a visible ancestor.

**Steps**:
1. [ ] - `p1` - Operator or tenant administrator sends `POST /oagw/v1/upstreams` with a body containing at least `server` and `protocol` - `inst-um-create-1`
2. [ ] - `p1` - System validates the payload against the upstream field contract (see `cpt-cf-oagw-algo-um-validate-payload`) - `inst-um-create-2`
3. [ ] - `p1` - **IF** payload validation fails - `inst-um-create-3`
   1. [ ] - `p1` - **RETURN** `400` problem+json naming the offending field - `inst-um-create-4`
4. [ ] - `p1` - System derives or validates the `alias` for the endpoint set (see `cpt-cf-oagw-algo-um-derive-alias`) - `inst-um-create-5`
5. [ ] - `p1` - **IF** the resolved alias cannot be determined, or a caller-supplied alias mismatches the derived value - `inst-um-create-6`
   1. [ ] - `p1` - **RETURN** `400` problem+json naming the `alias` field - `inst-um-create-7`
6. [ ] - `p1` - **IF** the resolved alias matches an upstream already owned by an ancestor tenant - `inst-um-create-8`
   1. [ ] - `p1` - **IF** that ancestor upstream's sharing configuration is entirely `private` across `auth.sharing`, `plugins.sharing`, and `rate_limit.sharing` (blocking descendant visibility) - `inst-um-create-8a`
      1. [ ] - `p1` - Treat the ancestor as not found for binding purposes and fall through to the ordinary tenant-scoped create path (the **ELSE** branch below), unaffected by the ancestor's alias or configuration - `inst-um-create-8b`
   2. [ ] - `p1` - **ELSE IF** the caller does not hold `oagw:upstream:bind` permission - `inst-um-create-8c`
      1. [ ] - `p1` - **RETURN** `403` problem+json via `cpt-cf-oagw-algo-gf-error-mapping`'s `permission denied` category - `inst-um-create-8d`
   3. [ ] - `p1` - **ELSE** - `inst-um-create-8e`
      1. [ ] - `p1` - System validates the ancestor's `auth`/`plugins`/`rate_limit` sharing modes for this create (`enforce` blocks descendant override) and proceeds with the bind - `inst-um-create-9`
7. [ ] - `p1` - **ELSE** (no ancestor match, including a private-sharing ancestor falling through from step 6.1) - `inst-um-create-10`
   1. [ ] - `p1` - System checks the resolved alias for uniqueness within the calling tenant - `inst-um-create-11`
8. [ ] - `p1` - **IF** the resolved alias already exists for the calling tenant - `inst-um-create-12`
   1. [ ] - `p1` - **RETURN** `409` alias-conflict - `inst-um-create-13`
9. [ ] - `p1` - System assigns a server-generated `id`, applies field defaults (`enabled: true`, `auth.sharing`/`plugins.sharing`/`rate_limit.sharing`/`cors.sharing: private`, `cors.enabled: false`, endpoint `port: 443`), and stores the upstream scoped to the calling tenant - `inst-um-create-14`
10. [ ] - `p1` - **RETURN** `201` with the created upstream representation - `inst-um-create-15`

### List Upstreams

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-um-list-upstreams`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Caller requests the list with no query parameters; the system returns up to 50 upstreams owned by the calling tenant, ordered by creation.
- Caller supplies `$top`, `$skip`, `$filter`, `$select` and/or `$orderby`; the system applies them and returns a matching page.

**Error Scenarios**:
- `$top` is supplied above the maximum: the system clamps the effective page size to 100 rather than erroring.

**Steps**:
1. [ ] - `p1` - Operator or tenant administrator sends `GET /oagw/v1/upstreams` with optional `$filter`, `$select`, `$orderby`, `$top`, `$skip` - `inst-um-list-1`
2. [ ] - `p1` - System resolves `$top` to the caller's value, or `50` if absent, clamped to a maximum of `100` - `inst-um-list-2`
3. [ ] - `p1` - System reads upstreams owned by the calling tenant, applying `$filter`, `$orderby`, `$skip` and the resolved `$top` - `inst-um-list-3`
4. [ ] - `p1` - **IF** `$select` is supplied - `inst-um-list-4`
   1. [ ] - `p1` - System projects only the requested fields per upstream - `inst-um-list-5`
5. [ ] - `p1` - **RETURN** `200` with the resulting page of upstreams - `inst-um-list-6`

### Get Upstream

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-um-get-upstream`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Caller requests an upstream `id` owned by the calling tenant; the system returns its full representation.

**Error Scenarios**:
- The `id` does not exist, or exists but is owned by a different tenant: `404` (ancestor and unrelated-tenant upstreams are equally invisible through the management API).

**Steps**:
1. [ ] - `p1` - Operator or tenant administrator sends `GET /oagw/v1/upstreams/{id}` - `inst-um-get-1`
2. [ ] - `p1` - System reads the upstream by `id`, scoped to the calling tenant - `inst-um-get-2`
3. [ ] - `p1` - **IF** no upstream with that `id` exists for the calling tenant - `inst-um-get-3`
   1. [ ] - `p1` - **RETURN** `404` (identical response whether the `id` is unknown or belongs to another tenant, including an ancestor) - `inst-um-get-4`
4. [ ] - `p1` - **ELSE** - `inst-um-get-5`
   1. [ ] - `p1` - **RETURN** `200` with the upstream representation - `inst-um-get-6`

### Replace Upstream

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-um-replace-upstream`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Caller submits a full replacement body for an upstream it owns, with the same `alias` as currently stored; the system overwrites all fields and returns `200`.
- Caller omits an optional field present on the stored upstream (e.g. `tags`, `headers`); the system clears that field to its schema default rather than preserving the previous value.

**Error Scenarios**:
- The `id` does not exist, or belongs to another tenant (including an ancestor): `404`.
- The body's `alias` differs from the upstream's current, stored alias: `400` problem+json naming the `alias` field (`alias` is immutable after creation). This also covers the non-derivable-with-different-explicit-alias transition: neither the stored nor the replacement `server.endpoints` are hostname-derivable, but the caller supplies a different explicit `alias` than the one currently stored.
- The replacement's `server.endpoints` remain hostname-derivable but recompute (per `cpt-cf-oagw-algo-um-derive-alias`) to a value different from the upstream's stored `alias`: `400` problem+json naming both the `alias` and `server` fields — independent of what the body's literal `alias` field contains.
- The replacement's `server.endpoints` change from hostname-derivable (the basis of the currently stored `alias`) to non-derivable (for example a hostname endpoint replaced by an IP literal): `400` problem+json naming both the `alias` and `server` fields — a derivable-to-non-derivable endpoint-set transition is rejected rather than silently keeping the old alias.
- The body includes `enabled: true` while an ancestor-tenant upstream in this resource's bind lineage is currently disabled: `400` problem+json naming the `enabled` field (see `cpt-cf-oagw-algo-um-enable-disable-propagation`).
- The replacement body otherwise fails schema validation, including the `scheme` enum (still accepting the widened `http`/`ws` set) and the CORS wildcard/credentials conflict: `400` problem+json naming the offending field.

**Steps**:
1. [ ] - `p1` - Operator or tenant administrator sends `PUT /oagw/v1/upstreams/{id}` with a full upstream body - `inst-um-replace-1`
2. [ ] - `p1` - System reads the existing upstream by `id`, scoped to the calling tenant - `inst-um-replace-2`
3. [ ] - `p1` - **IF** no upstream with that `id` exists for the calling tenant - `inst-um-replace-3`
   1. [ ] - `p1` - **RETURN** `404` - `inst-um-replace-4`
4. [ ] - `p1` - System validates the replacement payload against the upstream field contract (see `cpt-cf-oagw-algo-um-validate-payload`) - `inst-um-replace-5`
5. [ ] - `p1` - **IF** payload validation fails - `inst-um-replace-6`
   1. [ ] - `p1` - **RETURN** `400` problem+json naming the offending field - `inst-um-replace-7`
6. [ ] - `p1` - **IF** the body's `alias` differs from the stored `alias` - `inst-um-replace-8`
   1. [ ] - `p1` - **RETURN** `400` problem+json naming the `alias` field - `inst-um-replace-9`
7. [ ] - `p1` - **ELSE** re-run the alias-derivation/consistency check for the replacement's `server.endpoints` against the stored `alias` (`cpt-cf-oagw-algo-um-derive-alias`'s replace-time steps) - `inst-um-replace-9a`
8. [ ] - `p1` - **IF** that check rejects the replacement (the recomputed alias differs from the stored alias, or a derivable-to-non-derivable endpoint-set transition is detected) - `inst-um-replace-9b`
   1. [ ] - `p1` - **RETURN** `400` problem+json naming the `alias` and `server` fields - `inst-um-replace-9c`
9. [ ] - `p1` - **IF** the body sets `enabled: true` while ancestor-disable propagation is currently in effect for this resource - `inst-um-replace-10`
   1. [ ] - `p1` - **RETURN** `400` problem+json naming the `enabled` field - `inst-um-replace-11`
10. [ ] - `p1` - System overwrites the stored upstream with the replacement body, `id` and `alias` unchanged, and clears any optional field the body omits to its schema default - `inst-um-replace-12`
11. [ ] - `p1` - **RETURN** `200` with the replaced upstream representation - `inst-um-replace-13`

### Delete Upstream

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-um-delete-upstream`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Caller deletes an upstream it owns; the system removes the upstream and every route registered against it, then returns `204`.

**Error Scenarios**:
- The `id` does not exist, or belongs to another tenant (including an ancestor): `404`.

**Steps**:
1. [ ] - `p1` - Operator or tenant administrator sends `DELETE /oagw/v1/upstreams/{id}` - `inst-um-delete-1`
2. [ ] - `p1` - System reads the existing upstream by `id`, scoped to the calling tenant - `inst-um-delete-2`
3. [ ] - `p1` - **IF** no upstream with that `id` exists for the calling tenant - `inst-um-delete-3`
   1. [ ] - `p1` - **RETURN** `404` - `inst-um-delete-4`
4. [ ] - `p1` - **ELSE** - `inst-um-delete-5`
   1. [ ] - `p1` - System cascades the deletion to every route registered against this upstream (see `cpt-cf-oagw-algo-um-cascade-delete-routes`), then deletes the upstream itself - `inst-um-delete-6`
5. [ ] - `p1` - **RETURN** `204` with an empty body - `inst-um-delete-7`

## 3. Processes / Business Logic (CDSL)

### Validate Upstream Payload

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-um-validate-payload`

**Input**: A create or replace request body for an Upstream.

**Output**: A validated, defaulted Upstream field set, or a `400` problem+json error naming the offending field.

**Steps**:
1. [ ] - `p1` - Reject the payload if `server` or `protocol` is absent — both are required top-level fields - `inst-um-validate-1`
2. [ ] - `p1` - Reject the payload if it contains any property outside the documented field set (`enabled`, `alias`, `tags`, `server`, `protocol`, `auth`, `headers`, `plugins`, `rate_limit`, `cors`) - `inst-um-validate-2`
3. [ ] - `p1` - Validate `server.endpoints` contains at least one entry, and for each endpoint validate `host` is a hostname, IPv4 or IPv6 literal, `scheme` is one of `https`, `wss`, `wt`, `grpc`, `http` or `ws` (the widened set — see `cpt-cf-oagw-dod-um-scheme-widening`), defaulting to `https` when absent, and `port` is an integer in `1..65535`, defaulting to `443` when absent - `inst-um-validate-3`
4. [ ] - `p1` - Validate `protocol` is one of the two supported protocol GTS identifiers (HTTP or gRPC) - `inst-um-validate-4`
5. [ ] - `p1` - Default `enabled` to `true` when absent - `inst-um-validate-5`
6. [ ] - `p1` - Validate `tags`, when present, is an array of lowercase, hyphen/underscore-safe strings - `inst-um-validate-6`
7. [ ] - `p1` - Validate `auth.sharing`, `plugins.sharing`, `rate_limit.sharing` and `cors.sharing`, when present, are one of `private`, `inherit`, `enforce`, each defaulting to `private` when absent - `inst-um-validate-7`
8. [ ] - `p1` - Validate `headers.request`/`headers.response` `set`/`add`/`remove` shapes and `headers.request.passthrough`, defaulting `passthrough` to `none` when absent - `inst-um-validate-8`
9. [ ] - `p1` - Validate `rate_limit.sustained.rate` is present when `rate_limit` is supplied, defaulting `rate_limit.algorithm` to `token_bucket`, `rate_limit.scope` to `tenant`, `rate_limit.strategy` to `reject`, `rate_limit.cost` to `1`, and `rate_limit.burst.capacity` to `rate_limit.sustained.rate` when absent - `inst-um-validate-9`
10. [ ] - `p1` - Validate `cors.enabled`, defaulting to `false` when absent; when present, default `cors.allowed_methods` to `["GET", "POST"]` and `cors.expose_headers` to `[]` - `inst-um-validate-10`
11. [ ] - `p1` - **IF** `cors.allow_credentials` is `true` and `cors.allowed_origins` contains the wildcard `"*"` - `inst-um-validate-11`
    1. [ ] - `p1` - **RETURN** `400` problem+json naming the `cors` field - `inst-um-validate-12`
12. [ ] - `p1` - **CATCH** any other schema violation - `inst-um-validate-13`
    1. [ ] - `p1` - **RETURN** `400` problem+json naming the specific offending field path - `inst-um-validate-14`
13. [ ] - `p1` - **RETURN** the validated, defaulted field set - `inst-um-validate-15`

### Derive and Resolve Alias

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-um-derive-alias`

**Input**: `server.endpoints[]` and an optional caller-supplied `alias`, at create time; or, at replace time, the replacement's `server.endpoints[]` together with the upstream's previously stored `alias`.

**Output**: A resolved, tenant-unique `alias`, or a `400` problem+json error naming the `alias` field (and, at replace time, the `server` field alongside it).

**Steps**:
1. [ ] - `p1` - **IF** every endpoint's `host` is a hostname (not an IP literal) - `inst-um-alias-1`
   1. [ ] - `p1` - **IF** there is exactly one distinct hostname - `inst-um-alias-2`
      1. [ ] - `p1` - Derive the candidate alias as that hostname, omitting the port when the endpoint's port is the scheme's standard port (`80` for `http`, `443` for `https`/`wss`/`wt`/`grpc`, `80` for `ws`), otherwise appending `:{port}` - `inst-um-alias-3`
   2. [ ] - `p1` - **ELSE** - `inst-um-alias-4`
      1. [ ] - `p1` - Compute the longest common domain suffix (at least 2 labels) across all distinct hostnames, validated against the public suffix list to reject a bare public suffix (e.g. `co.uk`) - `inst-um-alias-5`
      2. [ ] - `p1` - **IF** a valid common suffix exists - `inst-um-alias-6`
         1. [ ] - `p1` - Derive the candidate alias as that suffix, omitting the port when shared and standard, otherwise appending `:{port}` - `inst-um-alias-7`
      3. [ ] - `p1` - **ELSE** - `inst-um-alias-8`
         1. [ ] - `p1` - Mark the alias as non-derivable - `inst-um-alias-9`
2. [ ] - `p1` - **ELSE** - `inst-um-alias-10`
   1. [ ] - `p1` - Mark the alias as non-derivable (any IP-literal endpoint forces explicit alias) - `inst-um-alias-11`
3. [ ] - `p1` - **IF** a candidate alias was derived - `inst-um-alias-12`
   1. [ ] - `p1` - Normalize the candidate to ASCII lowercase with any trailing dot stripped - `inst-um-alias-13`
   2. [ ] - `p1` - **IF** the caller supplied no `alias` - `inst-um-alias-14`
      1. [ ] - `p1` - **RETURN** the normalized candidate as the resolved alias - `inst-um-alias-15`
   3. [ ] - `p1` - **ELSE IF** the caller-supplied `alias`, normalized the same way, exactly equals the candidate - `inst-um-alias-16`
      1. [ ] - `p1` - **RETURN** the candidate as the resolved alias (idempotent no-op) - `inst-um-alias-17`
   4. [ ] - `p1` - **ELSE** - `inst-um-alias-18`
      1. [ ] - `p1` - **RETURN** `400` problem+json naming the `alias` field - `inst-um-alias-19`
4. [ ] - `p1` - **ELSE** - `inst-um-alias-20`
   1. [ ] - `p1` - **IF** the caller supplied no `alias` - `inst-um-alias-21`
      1. [ ] - `p1` - **RETURN** `400` problem+json naming the `alias` field as required - `inst-um-alias-22`
   2. [ ] - `p1` - **ELSE** - `inst-um-alias-23`
      1. [ ] - `p1` - Normalize the caller-supplied `alias` to ASCII lowercase with any trailing dot stripped and **RETURN** it as the resolved alias - `inst-um-alias-24`
5. [ ] - `p1` - **RETURN** `409` alias-conflict if the resolved alias already exists for another upstream owned by the calling tenant - `inst-um-alias-25`
6. [ ] - `p1` - **AT REPLACE TIME** (invoked by `cpt-cf-oagw-flow-um-replace-upstream` once the body's literal `alias` has already been confirmed equal to the stored `alias`), recompute a candidate alias from the replacement's `server.endpoints` using steps 1-2 above (derivation only, no caller-supplied-alias comparison) - `inst-um-alias-26`
7. [ ] - `p1` - **IF** the replacement's endpoint set is hostname-derivable and the recomputed candidate (normalized as in step 3.1) differs from the stored `alias` - `inst-um-alias-27`
   1. [ ] - `p1` - **RETURN** `400` problem+json naming the `alias` and `server` fields — an endpoint change that would alter the derived alias is rejected, independent of what the body's `alias` field contains - `inst-um-alias-28`
8. [ ] - `p1` - **ELSE IF** the replacement's endpoint set is non-derivable (any IP-literal endpoint, or hostnames sharing no valid common suffix) while the stored `alias` is itself equal to the value that would have been derived from the previously stored `server.endpoints` (i.e., the upstream's alias was originally auto-derived, not explicit) - `inst-um-alias-29`
   1. [ ] - `p1` - **RETURN** `400` problem+json naming the `alias` and `server` fields — a derivable-to-non-derivable endpoint-set transition is rejected rather than silently keeping the old alias - `inst-um-alias-30`
9. [ ] - `p1` - **ELSE** (the endpoint set is non-derivable and the previously stored `alias` was itself explicit, i.e., the non-derivable-with-different-explicit-alias case; or the endpoint set is hostname-derivable and the recomputed candidate matches the stored `alias`) - `inst-um-alias-31`
   1. [ ] - `p1` - **RETURN** the stored `alias` unchanged (the ordinary alias-immutability check already performed by the Replace flow is sufficient in this branch) - `inst-um-alias-32`

### Propagate Enable/Disable State

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-um-enable-disable-propagation`

**Input**: An upstream resource and, for a write, the caller's requested `enabled` value.

**Output**: The resource's effective enabled state, or a rejection of a write that would violate propagation.

**Steps**:
1. [ ] - `p1` - Determine whether this upstream was created as a bind against an ancestor tenant's upstream sharing the same alias - `inst-um-enable-1`
2. [ ] - `p1` - **IF** it was bound to an ancestor upstream and that ancestor's own effective state (recursively applying this same process) is disabled - `inst-um-enable-2`
   1. [ ] - `p1` - Treat the effective state as `DisabledByAncestor`, regardless of this resource's own `enabled` value - `inst-um-enable-3`
3. [ ] - `p1` - **ELSE** - `inst-um-enable-4`
   1. [ ] - `p1` - Treat the effective state as `Active` when this resource's own `enabled` is `true`, otherwise `DisabledBySelf` - `inst-um-enable-5`
4. [ ] - `p1` - **IF** this is a write that sets `enabled: true` and the effective state before the write is `DisabledByAncestor` - `inst-um-enable-6`
   1. [ ] - `p1` - **RETURN** rejection: `400` problem+json naming the `enabled` field — a descendant cannot re-enable a resource an ancestor has disabled - `inst-um-enable-7`
5. [ ] - `p1` - **RETURN** the effective state (consumed by HTTP Request Proxying to answer proxy requests with `503` when not `Active`; route-level enable/disable is a separate mechanism owned by Route Management) - `inst-um-enable-8`

### Cascade Delete Upstream Routes

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-um-cascade-delete-routes`

**Input**: The `id` of an upstream being deleted.

**Output**: Removal of the upstream and every route registered against it.

**Steps**:
1. [ ] - `p1` - Find every route whose `upstream_id` equals this upstream's `id`, scoped to the calling tenant - `inst-um-cascade-1`
2. [ ] - `p1` - **FOR EACH** matching route - `inst-um-cascade-2`
   1. [ ] - `p1` - Remove the route and its match rules, method allowlist and tags - `inst-um-cascade-3`
3. [ ] - `p1` - Remove the upstream's own tags and plugin bindings, then remove the upstream record itself, as a single atomic operation - `inst-um-cascade-4`
4. [ ] - `p1` - **RETURN** completion (the caller flow returns `204`) - `inst-um-cascade-5`

## 4. States (CDSL)

### Upstream Availability State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-um-availability`

**States**: Active, DisabledBySelf, DisabledByAncestor

**Initial State**: Active

**Transitions**:
1. [ ] - `p1` - **FROM** Active **TO** DisabledBySelf **WHEN** the resource owner replaces the upstream with `enabled: false` - `inst-um-state-1`
2. [ ] - `p1` - **FROM** Active **TO** DisabledByAncestor **WHEN** an ancestor-tenant upstream in this resource's bind lineage transitions to a disabled effective state - `inst-um-state-2`
3. [ ] - `p1` - **FROM** DisabledBySelf **TO** Active **WHEN** the resource owner replaces the upstream with `enabled: true` and no ancestor-tenant upstream in this resource's bind lineage is currently disabled - `inst-um-state-3`
4. [ ] - `p1` - **FROM** DisabledByAncestor **TO** Active **WHEN** the ancestor-tenant upstream returns to an `Active` effective state and this resource's own `enabled` value is `true` (a descendant's attempt to force this transition while the ancestor is still disabled is rejected — see `cpt-cf-oagw-algo-um-enable-disable-propagation`) - `inst-um-state-4`

## 5. Definitions of Done

### Create Upstream

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-um-create`

The system **MUST** implement `POST /oagw/v1/upstreams`, assigning a server-generated `id`, applying the documented field defaults, and scoping the created upstream to the calling tenant. In this configuration the ancestor-alias bind path (and its `oagw:upstream:bind` permission check) is not served — the gear has no access to a tenant-hierarchy source — so every create resolves within the calling tenant only, and the `sharing` fields it defaults to `private` are accepted and stored without being acted upon.

**Implements**:
- `cpt-cf-oagw-flow-um-create-upstream`

**Constraints**: `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- DB: `cpt-cf-oagw-db-schema`
- DB Table: `oagw_upstream`, `oagw_upstream_tag`
- Entities: `Upstream`, `ServerConfig`, `Endpoint`, `AuthConfig`, `HeadersConfig`, `RateLimitConfig`, `CorsConfig`, `PluginsConfig`

### Scheme Acceptance Widening

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-um-scheme-widening`

The system **MUST** accept `http` and `ws` as valid values of `server.endpoints[].scheme`, in addition to `https`, `wss`, `wt` and `grpc`, at payload-validation time on both create and replace. An upstream declared with `{"scheme": "http", "port": 80}` **MUST** be created successfully with `201`. This acceptance is independent of whether a plaintext connection is ever actually made to that endpoint — that separate question is owned by HTTP Request Proxying and gated by the `allow_http_upstream` configuration key.

**Implements**:
- `cpt-cf-oagw-flow-um-create-upstream`
- `cpt-cf-oagw-flow-um-replace-upstream`

**Constraints**: `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `PUT /oagw/v1/upstreams/{id}`
- Entities: `ServerConfig`, `Endpoint`

### Alias Derivation and Immutability

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-um-alias-derivation`

The system **MUST** derive the `alias` from endpoint hostnames when every endpoint is hostname-based, require an explicit `alias` when it cannot be derived (any IP-literal endpoint, or hostnames sharing no valid common suffix), accept a caller-supplied alias that exactly matches the derived value as an idempotent no-op, reject a caller-supplied alias that differs from the derived value with `400`, normalize every alias to ASCII lowercase with trailing dots stripped, and treat `alias` as immutable once the upstream exists (a `PUT` body whose `alias` differs from the stored value is rejected with `400`). On replace, the system **MUST** additionally recompute the alias from the replacement's `server.endpoints` and reject with `400` naming `alias` and `server` whenever that recomputed value differs from the stored alias — independent of the body's literal `alias` field — and **MUST** likewise reject a derivable-to-non-derivable endpoint-set transition, so that no endpoint change can silently invalidate the stored alias's derivation basis.

**Implements**:
- `cpt-cf-oagw-flow-um-create-upstream`
- `cpt-cf-oagw-flow-um-replace-upstream`
- `cpt-cf-oagw-algo-um-derive-alias`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `PUT /oagw/v1/upstreams/{id}`
- DB: `cpt-cf-oagw-db-schema`
- DB Table: `oagw_upstream`
- Entities: `Upstream`, `Endpoint`

### Alias Uniqueness

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-um-alias-conflict`

The system **MUST** enforce alias uniqueness per tenant: a create whose resolved alias already exists for another upstream owned by the calling tenant **MUST** return `409`.

**Implements**:
- `cpt-cf-oagw-flow-um-create-upstream`
- `cpt-cf-oagw-algo-um-derive-alias`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- DB: `cpt-cf-oagw-db-schema`
- DB Table: `oagw_upstream`
- Entities: `Upstream`

### Payload Validation Errors

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-um-validation-errors`

The system **MUST** reject a create or replace payload that violates the upstream field contract with `400` and an `application/problem+json` body that names the specific offending field.

**Implements**:
- `cpt-cf-oagw-flow-um-create-upstream`
- `cpt-cf-oagw-flow-um-replace-upstream`
- `cpt-cf-oagw-algo-um-validate-payload`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `PUT /oagw/v1/upstreams/{id}`
- Entities: `Upstream`

### CORS Wildcard/Credentials Validation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-um-cors-validation`

The system **MUST** reject, at write time, an upstream whose `cors.allow_credentials` is `true` while `cors.allowed_origins` contains the wildcard `"*"`, with `400` naming the `cors` field.

**Implements**:
- `cpt-cf-oagw-algo-um-validate-payload`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `PUT /oagw/v1/upstreams/{id}`
- Entities: `CorsConfig`

### List Upstreams

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-um-list`

The system **MUST** implement `GET /oagw/v1/upstreams`, returning only upstreams owned by the calling tenant, supporting `$filter`, `$select`, `$orderby`, `$top` (default `50`, clamped to a maximum of `100`) and `$skip`.

**Implements**:
- `cpt-cf-oagw-flow-um-list-upstreams`

**Touches**:
- API: `GET /oagw/v1/upstreams`
- DB: `cpt-cf-oagw-db-schema`
- DB Table: `oagw_upstream`
- Entities: `Upstream`

### Get Upstream

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-um-get`

The system **MUST** implement `GET /oagw/v1/upstreams/{id}`, returning `200` with the full representation for an upstream owned by the calling tenant, and `404` when the `id` is unknown or owned by a different tenant (including an ancestor).

**Implements**:
- `cpt-cf-oagw-flow-um-get-upstream`

**Touches**:
- API: `GET /oagw/v1/upstreams/{id}`
- DB: `cpt-cf-oagw-db-schema`
- DB Table: `oagw_upstream`
- Entities: `Upstream`

### Replace Upstream

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-um-replace`

The system **MUST** implement `PUT /oagw/v1/upstreams/{id}` as a full replacement: `id` and `alias` immutable, omitted optional fields cleared to their schema defaults, the same validation rules as create re-applied, and `404` when the `id` is unknown or owned by a different tenant.

**Implements**:
- `cpt-cf-oagw-flow-um-replace-upstream`

**Touches**:
- API: `PUT /oagw/v1/upstreams/{id}`
- DB: `cpt-cf-oagw-db-schema`
- DB Table: `oagw_upstream`, `oagw_upstream_tag`
- Entities: `Upstream`, `ServerConfig`, `Endpoint`, `AuthConfig`, `HeadersConfig`, `RateLimitConfig`, `CorsConfig`, `PluginsConfig`

### Delete Upstream and Cascade Routes

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-um-delete`

The system **MUST** implement `DELETE /oagw/v1/upstreams/{id}`, returning `204` and removing every route registered against that upstream in the same atomic operation, and `404` when the `id` is unknown or owned by a different tenant.

**Implements**:
- `cpt-cf-oagw-flow-um-delete-upstream`
- `cpt-cf-oagw-algo-um-cascade-delete-routes`

**Touches**:
- API: `DELETE /oagw/v1/upstreams/{id}`
- DB: `cpt-cf-oagw-db-schema`
- DB Table: `oagw_upstream`, `oagw_route`
- Entities: `Upstream`

### Enable/Disable Propagation

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-um-enable-disable-propagation`

The system **MUST** support the `enabled` boolean field (default `true`) on upstreams, must reject a write that attempts to re-enable a resource while an ancestor-tenant upstream in its bind lineage remains disabled, and must propagate an ancestor's disable to every descendant so that the descendant's effective state is disabled regardless of its own `enabled` value. Route-level enable/disable is a separate mechanism owned by Route Management and is out of scope here. In this configuration the gear has no access to a tenant-hierarchy source, so no upstream ever has an ancestor to be bound to: ancestor-disable propagation and the `DisabledByAncestor` effective state are therefore not served, and every upstream's effective state resolves from its own `enabled` value alone (`Active` or `DisabledBySelf`).

**Implements**:
- `cpt-cf-oagw-flow-um-replace-upstream`
- `cpt-cf-oagw-algo-um-enable-disable-propagation`
- `cpt-cf-oagw-state-um-availability`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `PUT /oagw/v1/upstreams/{id}`
- DB: `cpt-cf-oagw-db-schema`
- DB Table: `oagw_upstream`
- Entities: `Upstream`

## 6. Acceptance Criteria

- [ ] `POST /oagw/v1/upstreams` with a valid hostname-endpoint body and no `alias` returns `201` with a server-generated `id` and the auto-derived `alias`.
- [ ] `POST /oagw/v1/upstreams` with `server.endpoints[0].scheme` set to `"http"` and `port` set to `80` returns `201`, demonstrating scheme acceptance is widened beyond the TLS family.
- [ ] `POST /oagw/v1/upstreams` with `server.endpoints[0].scheme` set to `"ws"` returns `201`.
- [ ] `POST /oagw/v1/upstreams` for a hostname endpoint whose caller-supplied `alias` exactly matches the derivable value returns `201` (idempotent no-op), while a caller-supplied `alias` that differs from the derivable value returns `400` naming the `alias` field.
- [ ] `POST /oagw/v1/upstreams` with an IP-literal `server.endpoints[0].host` and no `alias` returns `400` naming the `alias` field as required; the same request with an explicit `alias` returns `201`.
- [ ] `POST /oagw/v1/upstreams` whose resolved alias already exists for another upstream owned by the same tenant returns `409`.
- [ ] `POST /oagw/v1/upstreams` missing the required `server` or `protocol` field returns `400` with an `application/problem+json` body naming that field.
- [ ] `POST /oagw/v1/upstreams` with `cors.allow_credentials: true` and `cors.allowed_origins: ["*"]` returns `400` naming the `cors` field.
- [ ] `GET /oagw/v1/upstreams` with no query parameters returns `200` with at most 50 results; a `$top` value above 100 is clamped to 100 results.
- [ ] `GET /oagw/v1/upstreams/{id}` for an upstream owned by a different tenant, or for an ancestor tenant's upstream, returns `404` with the same body shape as an unknown `id`.
- [ ] `PUT /oagw/v1/upstreams/{id}` with a body `alias` different from the stored `alias` returns `400` naming the `alias` field; `id` and `alias` are unchanged after any successful replacement.
- [ ] `PUT /oagw/v1/upstreams/{id}` omitting a previously-set optional field (e.g. `tags`) returns `200` with that field cleared to its schema default in the stored representation.
- [ ] `PUT /oagw/v1/upstreams/{id}` for an unknown `id`, or one owned by a different tenant, returns `404`.
- [ ] `DELETE /oagw/v1/upstreams/{id}` returns `204`, and a subsequent `GET` for any route previously registered against that upstream returns `404`.
- [ ] A `PUT /oagw/v1/upstreams/{id}` that sets `enabled: true` while an ancestor-tenant upstream in this resource's bind lineage is disabled returns `400` naming the `enabled` field.
- [ ] `PUT /oagw/v1/upstreams/{id}` with the body's `alias` left equal to the stored value but `server.endpoints[0].host` changed to a different hostname (so the recomputed derived alias differs from the stored alias) returns `400` naming the `alias` and `server` fields.
- [ ] `PUT /oagw/v1/upstreams/{id}` for an upstream whose alias was originally derived from a hostname endpoint, replacing `server.endpoints` with an IP-literal endpoint (and leaving the body's `alias` equal to the stored value), returns `400` naming the `alias` and `server` fields.
- [ ] `POST /oagw/v1/upstreams` whose resolved alias matches a visible (non-`private`-sharing) ancestor-tenant upstream, submitted by a caller lacking `oagw:upstream:bind` permission, returns `403`.
- [ ] `POST /oagw/v1/upstreams` whose resolved alias matches an ancestor-tenant upstream configured with `private` sharing on `auth`, `plugins`, and `rate_limit` returns `201` as an ordinary tenant-scoped create rather than a bind, and does not require bind permission.
