# Feature: Upstream Management


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Scope Exclusions](#15-scope-exclusions)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Create Upstream](#create-upstream)
  - [List Upstreams](#list-upstreams)
  - [Get Upstream by Identifier](#get-upstream-by-identifier)
  - [Replace Upstream](#replace-upstream)
  - [Delete Upstream](#delete-upstream)
  - [Enable and Disable an Upstream](#enable-and-disable-an-upstream)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Alias Derivation](#alias-derivation)
  - [Alias Immutability Enforcement](#alias-immutability-enforcement)
  - [Endpoint and Scheme Validation](#endpoint-and-scheme-validation)
  - [Sub-Configuration Validation](#sub-configuration-validation)
  - [List Query Interpretation](#list-query-interpretation)
  - [Effective Enablement Inheritance](#effective-enablement-inheritance)
- [4. States (CDSL)](#4-states-cdsl)
  - [Upstream Record State Machine](#upstream-record-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Upstream CRUD Endpoints](#upstream-crud-endpoints)
  - [Alias Derivation from Endpoints](#alias-derivation-from-endpoints)
  - [Alias Immutability Transition Matrix](#alias-immutability-transition-matrix)
  - [Alias Normalization and Per-Tenant Uniqueness](#alias-normalization-and-per-tenant-uniqueness)
  - [Endpoint, Scheme, and Protocol Validation](#endpoint-scheme-and-protocol-validation)
  - [Authentication Configuration with Credential References](#authentication-configuration-with-credential-references)
  - [Header Transformation Configuration](#header-transformation-configuration)
  - [Tags with Add-Only Union Semantics](#tags-with-add-only-union-semantics)
  - [Enable and Disable Semantics](#enable-and-disable-semantics)
  - [Plugin Sharing References](#plugin-sharing-references)
  - [Nested Rate-Limit and CORS Sub-Configuration](#nested-rate-limit-and-cors-sub-configuration)
  - [OData List Query Parameters](#odata-list-query-parameters)
  - [Strict Tenant Scoping on the Management Surface](#strict-tenant-scoping-on-the-management-surface)
  - [Validation Against the Upstream Schema Shapes](#validation-against-the-upstream-schema-shapes)
  - [Automated Unit Test Coverage](#automated-unit-test-coverage)
  - [Automated Integration Test Coverage](#automated-integration-test-coverage)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [x] `p1` - **ID**: `cpt-cf-oagw-featstatus-upstream-management-implemented`

<!-- reference to DECOMPOSITION entry -->
- [x] `p2` - `cpt-cf-oagw-feature-upstream-management`

## 1. Feature Context

### 1.1 Overview

Delivers the Control Plane CRUD lifecycle for upstream configurations at the gear-relative collection `/oagw/v1/upstreams`: create, list, get by identifier, full-replacement update, and delete, together with alias derivation and the alias-immutability matrix, endpoint and scheme validation, the credential-reference authentication surface, header transformation configuration, tags, plugin references, the nested rate-limit and CORS sub-configurations, and the `enabled` flag with its inheritance semantics.

### 1.2 Purpose

An upstream is the fundamental configuration unit of the gear: every proxy request resolves to one, and no route can be created before an upstream exists. This feature turns the upstream record delivered by `cpt-cf-oagw-feature-gear-foundation` into a managed resource, enforcing at write time the alias derivation and immutability rules that make the alias a stable routing key, the per-tenant alias uniqueness that keeps tenant namespaces independent, and the `enabled` flag whose state the proxy path consults. All operations are tenant-scoped and are served by the `ControlPlaneService` upstream operations of `cpt-cf-oagw-component-model`, routed to the Control Plane per `cpt-cf-oagw-adr-request-routing`, and exposed through the gear-relative route tree registered against `cpt-cf-oagw-interface-api`. The upstream record shapes validated here are those of `schemas/upstream.v1.schema.json`, and the persistence shape is the documented `oagw_upstream` / `oagw_upstream_tag` / `oagw_upstream_plugin` contract of `cpt-cf-oagw-db-schema` held in the in-memory, config-backed repository (graded deviation 5).

**Requirements** (delivered by this feature):

- `p1` - `cpt-cf-oagw-fr-upstream-mgmt`
- `p1` - `cpt-cf-oagw-fr-enable-disable`
- `p1` - `cpt-cf-oagw-usecase-configure-upstream`
- `p1` - `cpt-cf-oagw-interface-management-api`

`cpt-cf-oagw-fr-alias-resolution` is marked done upstream in the PRD, so this feature inherits a satisfied obligation rather than re-delivering it: it implements the write-time half of that requirement (derivation, normalization, and immutability of the alias when an upstream is created or replaced) and leaves the read-time half, proxy-time alias resolution and tenant-hierarchy shadowing, to entry 2.4. Likewise the sharing-mode merge behavior of the sub-configurations validated here is the merge engine delivered by `cpt-cf-oagw-feature-gear-foundation`; this feature validates that the declared modes are legal and stores them.

**Principles**: `cpt-cf-oagw-principle-tenant-scope`

**Sequences**: none. DESIGN.md documents management operations as a linear flow (Client -> API Handler -> `ControlPlaneService` -> Response) rather than a named sequence; `cpt-cf-oagw-seq-proxy-flow` is the only sequence DESIGN.md defines and is owned by entry 2.4.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Creates, replaces, enables, disables, and deletes upstream configurations; supplies the explicit alias for non-derivable endpoint pools; is the actor of `cpt-cf-oagw-usecase-configure-upstream` and the actor the Control Plane routing of `cpt-cf-oagw-adr-request-routing` serves |
| `cpt-cf-oagw-actor-tenant-admin` | Lists, reads, and manages the upstream records owned by its own tenant within the allowed sharing policies, and is the subject of every tenant-scoped read and of the ancestor-invisibility rule |

Both actors are declared on every flow of §2, because `cpt-cf-oagw-fr-upstream-mgmt` names both: both exercise the read and list operations under the shared permission set of DESIGN §3.2, and the mutating operations are additionally permission-gated per operation (see §2 authorization). The table above records the emphasis of each actor; the permission check itself is actor-agnostic and applies to whichever actor carries the request.

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Schemas**: [schemas/upstream.v1.schema.json](../schemas/upstream.v1.schema.json)
- **Dependencies**: `cpt-cf-oagw-feature-gear-foundation` - this feature consumes the gear wiring, the `OagwConfig` model, the shared domain types, the repository traits and their in-memory implementation, the GTS type provisioning, and the credential-reference configuration boundary delivered there.

### 1.5 Scope Exclusions

The following areas are excluded from this feature and remain owned by the entries named below; nothing delivered here pre-implements them:

- Audit emission of configuration writes: owned by `cpt-cf-oagw-feature-observability-and-operability`. This feature performs the configuration writes and emits no audit record for them.
- Control Plane L1 invalidation and Data Plane hot-config cache invalidation: owned by `cpt-cf-oagw-feature-observability-and-operability` per `cpt-cf-oagw-adr-data-plane-caching` and `cpt-cf-oagw-adr-state-management`. This feature only orders its writes against that mechanism, as §2 records, and implements none of it.
- Persistence and database operations: the repository traits, the in-memory config-backed implementation (graded deviation 5), and the documented `oagw_upstream` / `oagw_upstream_tag` / `oagw_upstream_plugin` schema contract are owned by `cpt-cf-oagw-feature-gear-foundation` under `cpt-cf-oagw-db-schema`. This feature writes through that boundary and owns no storage of its own.
- Data privacy: the only sensitive values on this surface are `cred://` references, which are configuration and never resolved here and carry no personal data, so no privacy processing surface is provided.
- Cache integration: OAGW caches no response per `cpt-cf-oagw-principle-no-cache`, and this feature holds no response, record, or query cache of its own.
- Health and diagnostics: the health and readiness surface is owned by `cpt-cf-oagw-feature-observability-and-operability`; this feature contributes no health or readiness endpoint.
- Regulatory compliance: no regulated processing surface exists on this management path.
- Accessibility: the gear exposes no user interface surface, so no accessibility requirement applies here.
- Rollout and rollback: the gear is a single crate with no migration step, so no rollout or rollback behavior is delivered here.
- Performance budget: the latency NFR `cpt-cf-oagw-nfr-low-latency` is proxy-scoped and owned by `cpt-cf-oagw-feature-request-proxy`; this feature declares no latency target of its own.
- Per-collection cardinality maxima: deliberately unbounded by OAGW configuration, and no per-collection maximum is invented here because neither DESIGN nor the PRD specifies one; the PRD open question on the maximum number of endpoints per upstream pool is recorded as open rather than resolved. The bounded resource on the management path is the request body, capped by the platform request-body limit the api-gateway enforces ahead of the handler, and the merge cost downstream is bounded by the record's own collection sizes.

## 2. Actor Flows (CDSL)

- `p1` - `cpt-cf-oagw-usecase-configure-upstream`

The flows below are the management-plane half of that use case; `cpt-cf-oagw-usecase-proxy-request` consumes the records this feature stores but is owned by entry 2.4.

**Inbound authentication**: every management call on this surface requires a validated bearer token delivered by the platform's inbound authentication (DESIGN §3.2, "Inbound Authentication & Authorization"). The caller's `tenant_id` and `principal_id` come from the resolved security context that authentication produces, and the handle that carries that context into the gear is delivered by `cpt-cf-oagw-feature-gear-foundation`; the authentication itself is owned by the api-gateway gear and this feature performs none. A request with an absent or invalid token is rejected before any payload validation, and its rendering is owned by `cpt-cf-oagw-feature-error-handling`, which maps the inbound authentication failure - a missing or invalid bearer token, or an absent security context - onto the existing `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1` type at `401`. No additional authentication error type is minted here.

**Authorization**: the resolved security context is checked for the DESIGN §3.2 permission of the operation before the service call is reached, in every flow below, and the check runs ahead of payload validation.

| Operation | Permission required |
|---|---|
| `POST /oagw/v1/upstreams` | `gts.cf.core.oagw.upstream.v1~:create` |
| `GET /oagw/v1/upstreams` | `gts.cf.core.oagw.upstream.v1~:read` |
| `GET /oagw/v1/upstreams/{id}` | `gts.cf.core.oagw.upstream.v1~:read` |
| `PUT /oagw/v1/upstreams/{id}` | `gts.cf.core.oagw.upstream.v1~:override` |
| `DELETE /oagw/v1/upstreams/{id}` | `gts.cf.core.oagw.upstream.v1~:delete` |

These five are the `{create;override;read;delete}` set DESIGN §3.2 declares for `gts.cf.core.oagw.upstream.v1~`, mapped per operation rather than per actor: both actors of §1.3 are subject to the same gate. A request whose security context is valid but lacks the required permission is rejected at `403` and rendered through the shared canonical permission-denied error surface of the platform - the platform canonical error `PermissionDenied` maps to `403` - with the body rendered by `cpt-cf-oagw-feature-error-handling`; no new OAGW error type is minted for it, and the mapping of both outcomes is already stated by entry 2.5.

**Success status contract**: `POST /oagw/v1/upstreams` returns `201 Created` with the stored record, `GET /oagw/v1/upstreams/{id}` returns `200` with the stored record, `GET /oagw/v1/upstreams` returns `200` with the projected list, `PUT /oagw/v1/upstreams/{id}` returns `200` with the replaced record, and `DELETE /oagw/v1/upstreams/{id}` returns `204 No Content` with an empty body, per DESIGN §3.3. These are the REST status mappings applied by the api-gateway's `OperationBuilder` for the registered routes of `cpt-cf-oagw-interface-api`, not a new API contract, and every failure rendering on this surface is owned by `cpt-cf-oagw-feature-error-handling`.

**Read-back contract**: credential-bearing fields hold `cred://` references only, and a `cred://` reference is configuration, not secret material, so every read of a record returns the references exactly as stored. No request on the management path resolves a reference through `cred_store`; resolution is deferred to request time and owned by `cpt-cf-oagw-feature-plugin-system`. No response, log line, error body, or audit field produced on this surface ever carries resolved secret material or an injected credential value, which is the redaction rule of `cpt-cf-oagw-feature-gear-foundation` applied to the management path.

**Concurrency posture**: the alias-uniqueness check and the multi-row persist of a create execute as one critical section inside the repository write path of `cpt-cf-oagw-feature-gear-foundation`, so a detected collision leaves the store unchanged rather than partially written; a replacement is applied as one atomic record write, so the proxy path observes either the previous or the new record and never a partial one; and cross-instance concurrency control is out of scope per DESIGN §4.7, which is a recorded limitation on the determinism invariant of this surface.

### Create Upstream

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-upstream-management-create`

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- A payload carrying at least one endpoint and a `protocol` is validated, its alias is derived or accepted as supplied, and the record is stored for the calling tenant and returned with its server-generated identifier and its normalized alias.
- A payload for a derivable endpoint pool that omits `alias` is accepted with the derived alias, and the same payload resubmitting the exact derived value is accepted as an idempotent no-op.
- A payload for an IP-based or otherwise non-derivable endpoint pool that supplies an explicit alias is accepted with that alias.
- A create whose derived or supplied alias matches an ancestor tenant's upstream alias is a bind rather than a conflict, per DESIGN §3.3: the create is accepted when the caller holds `oagw:upstream:bind`, resolved through `authz_resolver`; `sharing: private` on the ancestor upstream blocks visibility, so the alias stays available for a local create, and `sharing: enforce` on the ancestor blocks the descendant from overriding ancestor-owned sub-configurations. Uniqueness itself is enforced per `(tenant_id, alias)`, so the descendant record is created and the ancestor alias match is the bind case that proxy-time resolution in entry 2.4 resolves.
- The stored record is returned with its credential-bearing fields holding their `cred://` references exactly as supplied, per the read-back contract of §2.

**Error Scenarios**:
- The payload is not a valid upstream shape: an endpoint host that is neither an RFC 1123 hostname nor an IP address, a port outside 1-65535, a scheme outside the permitted set, a mixed endpoint pool, a missing `protocol`, an unknown field, or a sub-configuration that violates its shape returns a validation error and nothing is stored.
- The endpoint pool is non-derivable and `alias` is omitted: a validation error is returned naming the requirement for an explicit alias.
- The endpoint pool is derivable and the supplied `alias` differs from the derived value: a validation error is returned.
- A credential-bearing field does not hold a `cred://` reference: the record is rejected and the rejected value is never echoed.
- Another upstream with the same `(tenant_id, alias)` already exists: the uniqueness check and the multi-row persist execute as one critical section inside the repository write path, so the create is rejected with `409 Conflict` and the store is left unchanged rather than partially written.
- The request carries no bearer token, an invalid one, or no resolvable security context: it is rejected before payload validation with `401` and `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`, rendered by `cpt-cf-oagw-feature-error-handling`.
- The security context is valid but lacks `gts.cf.core.oagw.upstream.v1~:create`: the create is rejected with `403` through the shared canonical permission-denied error surface, rendered by `cpt-cf-oagw-feature-error-handling`, before the service is invoked and with no new OAGW error type minted.
- The alias matches an ancestor tenant's upstream and the caller lacks `oagw:upstream:bind`: the create is rejected through the same canonical permission-denied surface at `403`, and nothing is stored.

**Steps**:
1. [x] - `p1` - Receive `POST /oagw/v1/upstreams` with the request body and the caller's tenant context - `inst-um-cr-1`
2. [x] - `p1` - Require the validated bearer token and the resolved security context of §2 and check `gts.cf.core.oagw.upstream.v1~:create` before the service call, rejecting an absent or invalid token with `401` and a valid context that lacks the permission with `403` - `inst-um-cr-15`
3. [x] - `p1` - Validate the body against the upstream record shapes: required `server.endpoints` and `protocol`, no unknown fields, and every sub-configuration legal - `inst-um-cr-2`
4. [x] - `p1` - Validate the endpoint pool with `cpt-cf-oagw-algo-upstream-management-endpoint-validation` and the remaining sub-configurations with `cpt-cf-oagw-algo-upstream-management-config-validation` - `inst-um-cr-3`
5. [x] - `p1` - **IF** any validation fails - `inst-um-cr-4`
   1. [x] - `p1` - Return the validation failure carrying the offending field and store nothing - `inst-um-cr-5`
6. [x] - `p1` - Derive the alias with `cpt-cf-oagw-algo-upstream-management-alias-derivation` and reconcile it with any supplied alias - `inst-um-cr-6`
7. [x] - `p1` - **IF** the pool is non-derivable and no alias is supplied - `inst-um-cr-7`
   1. [x] - `p1` - Return a validation error stating that an explicit alias is required for IP-based or non-derivable endpoint pools - `inst-um-cr-8`
8. [x] - `p1` - **IF** the pool is derivable and a supplied alias differs from the derived value - `inst-um-cr-9`
   1. [x] - `p1` - Return a validation error naming the derived value, so the alias is never an arbitrary label - `inst-um-cr-10`
9. [x] - `p1` - Normalize the accepted alias to ASCII lowercase with trailing dots stripped - `inst-um-cr-11`
10. [x] - `p1` - **IF** the normalized alias matches the alias of an ancestor-tenant upstream - `inst-um-cr-16`
   1. [x] - `p1` - Treat the create as a bind per DESIGN §3.3, require `oagw:upstream:bind` resolved through `authz_resolver`, reject the create through the canonical permission-denied surface at `403` when that permission is absent, and apply the ancestor sharing-mode constraints: `sharing: private` on the ancestor upstream blocks visibility so the alias stays available for a local create, and `sharing: enforce` on the ancestor blocks the descendant from overriding ancestor-owned sub-configurations - `inst-um-cr-17`
11. [x] - `p1` - Look up `(tenant_id, alias)` in the repository and, on a hit, reject the create with `409 Conflict`, executing the lookup and the persist of the next step as one critical section so the store is left unchanged on a hit - `inst-um-cr-12`
12. [x] - `p1` - Assign the server-generated identifier, apply the scalar structural defaults (`enabled` true, endpoint `scheme` `https`, endpoint `port` 443, the derived `alias`, the server-assigned `tenant_id`), leave every sub-configuration block the request body omitted absent in the stored record with no implicit empty block persisted, and store the record with its tag rows and ordered plugin binding rows as one atomic write - `inst-um-cr-13`
13. [x] - `p1` - Order the write as a configuration write that triggers Control Plane L1 invalidation and the Data Plane hot-config flush per `cpt-cf-oagw-adr-request-routing`, `cpt-cf-oagw-adr-data-plane-caching`, and `cpt-cf-oagw-adr-state-management`: store write, then CP L1 invalidation, then the DP flush request, then success; the mechanism is owned by `cpt-cf-oagw-feature-observability-and-operability` and is invoked, not implemented, here - `inst-um-cr-18`
14. [x] - `p1` - **RETURN** `201 Created` with the stored upstream record, its identifier, its normalized alias, and the applied scalar defaults - `inst-um-cr-14`

### List Upstreams

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-upstream-management-list`

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- The calling tenant's own upstream records are returned, filtered, projected, ordered, and paginated by the OData query parameters, as `200` with the projected list and the count actually returned.
- An absent `$top` yields at most 50 records, and an explicit `$top` up to 100 is honored.
- An empty result is a successful empty list, not an error.
- Every returned record presents credential-bearing fields as their stored `cred://` references, per the read-back contract of §2.

**Error Scenarios**:
- A query parameter is malformed or out of range, such as `$top` above 100 or a negative `$top` or `$skip`: a validation error naming the parameter is returned.
- An unparseable `$filter`, `$select`, or `$orderby` expression: a validation error is returned rather than an empty result set.
- The list never contains a record owned by another tenant, including an ancestor.
- The request carries no bearer token, an invalid one, or no resolvable security context: it is rejected before query interpretation with `401` and `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`, rendered by `cpt-cf-oagw-feature-error-handling`.
- The security context is valid but lacks `gts.cf.core.oagw.upstream.v1~:read`: the list is rejected with `403` through the shared canonical permission-denied error surface, rendered by `cpt-cf-oagw-feature-error-handling`.

**Steps**:
1. [x] - `p1` - Receive `GET /oagw/v1/upstreams` with the query string and the caller's tenant context - `inst-um-ls-1`
2. [x] - `p1` - Require the validated bearer token and the resolved security context of §2 and check `gts.cf.core.oagw.upstream.v1~:read` before the service call, rejecting an absent or invalid token with `401` and a valid context that lacks the permission with `403` - `inst-um-ls-8`
3. [x] - `p1` - Apply `cpt-cf-oagw-algo-upstream-management-list-query` to interpret `$filter`, `$select`, `$orderby`, `$top`, and `$skip` - `inst-um-ls-2`
4. [x] - `p1` - **IF** a query parameter is malformed or out of range - `inst-um-ls-3`
   1. [x] - `p1` - Return a validation error naming the offending parameter - `inst-um-ls-4`
5. [x] - `p1` - Bind the listing to the caller's tenant identifier so only records owned by that tenant are candidates - `inst-um-ls-5`
6. [x] - `p1` - Apply the filter, ordering, offset, and limit in that sequence and project the selected fields - `inst-um-ls-6`
7. [x] - `p1` - **RETURN** `200` with the projected list and the count actually returned - `inst-um-ls-7`

### Get Upstream by Identifier

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-upstream-management-get`

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- The full record identified by the anonymous GTS identifier `gts.cf.core.oagw.upstream.v1~{uuid}` is returned when it is owned by the calling tenant, as `200` with the stored record.
- The returned record presents its credential-bearing fields as the stored `cred://` references exactly as written, and carries no resolved secret material, per the read-back contract of §2.

**Error Scenarios**:
- The identifier does not exist, or the record is owned by another tenant including an ancestor: not-found is returned and the existence of a foreign record is never disclosed.
- The request carries no bearer token, an invalid one, or no resolvable security context: it is rejected with `401` and `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`, rendered by `cpt-cf-oagw-feature-error-handling`.
- The security context is valid but lacks `gts.cf.core.oagw.upstream.v1~:read`: the read is rejected with `403` through the shared canonical permission-denied error surface, rendered by `cpt-cf-oagw-feature-error-handling`.

**Steps**:
1. [x] - `p1` - Receive `GET /oagw/v1/upstreams/{id}` with the path identifier and the caller's tenant context - `inst-um-gt-1`
2. [x] - `p1` - Require the validated bearer token and the resolved security context of §2 and check `gts.cf.core.oagw.upstream.v1~:read` before the service call, rejecting an absent or invalid token with `401` and a valid context that lacks the permission with `403` - `inst-um-gt-6`
3. [x] - `p1` - Look up the record bound to the caller's tenant identifier - `inst-um-gt-2`
4. [x] - `p1` - **IF** no record is owned by the calling tenant under that identifier - `inst-um-gt-3`
   1. [x] - `p1` - Return not-found without disclosing whether a foreign or ancestor record exists - `inst-um-gt-4`
5. [x] - `p1` - **RETURN** `200` with the stored record, its credential-bearing fields carrying their `cred://` references as stored - `inst-um-gt-5`

### Replace Upstream

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-upstream-management-replace`

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- A full replacement overwrites every field of the caller's own record: an omitted optional sub-configuration block is cleared, meaning it becomes absent in the stored record with no implicit empty block persisted, and `id` and `tenant_id` are preserved.
- An endpoint pool change that recomputes to the existing alias is accepted, including the transition from a non-derivable pool to a derivable pool whose derived alias equals the existing alias.
- A payload that changes no endpoint and resubmits the existing alias is accepted as an idempotent no-op.
- The replaced record is returned with its credential-bearing fields holding their `cred://` references exactly as stored, per the read-back contract of §2.
- A replacement is applied as one atomic record write, so the proxy path observes either the previous or the new record and never a partial one.

**Error Scenarios**:
- The endpoint change would alter the alias, in any direction of the transition matrix: a validation error is returned and the operator is told to delete and re-create the upstream.
- A differing alias is supplied for a non-derivable pool: a validation error is returned and the existing alias is retained.
- The replacement body violates a record shape: a validation error is returned and the stored record is left untouched.
- The identifier is missing or foreign, including an ancestor record: not-found is returned.
- The replacement re-introduces a sub-configuration override against an ancestor upstream bound by alias while the ancestor carries `sharing: enforce`: the ancestor bind constraints are re-validated per DESIGN §3.3 and the override is rejected as a validation error; when the caller also lacks `oagw:upstream:bind`, the rejection is through the canonical permission-denied surface at `403`.
- The request carries no bearer token, an invalid one, or no resolvable security context: it is rejected before payload validation with `401` and `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`, rendered by `cpt-cf-oagw-feature-error-handling`.
- The security context is valid but lacks `gts.cf.core.oagw.upstream.v1~:override`: the replacement is rejected with `403` through the shared canonical permission-denied error surface, rendered by `cpt-cf-oagw-feature-error-handling`.

**Steps**:
1. [x] - `p1` - Receive `PUT /oagw/v1/upstreams/{id}` with the replacement body and the caller's tenant context - `inst-um-rp-1`
2. [x] - `p1` - Require the validated bearer token and the resolved security context of §2 and check `gts.cf.core.oagw.upstream.v1~:override` before the service call, rejecting an absent or invalid token with `401` and a valid context that lacks the permission with `403` - `inst-um-rp-11`
3. [x] - `p1` - Load the record bound to the caller's tenant identifier and, when it does not exist, return not-found without further validation - `inst-um-rp-2`
4. [x] - `p1` - Validate the replacement body with `cpt-cf-oagw-algo-upstream-management-endpoint-validation` and `cpt-cf-oagw-algo-upstream-management-config-validation` - `inst-um-rp-3`
5. [x] - `p1` - **IF** any validation fails - `inst-um-rp-4`
   1. [x] - `p1` - Return the validation failure and leave the stored record unchanged - `inst-um-rp-5`
6. [x] - `p1` - Enforce `cpt-cf-oagw-algo-upstream-management-alias-immutability` against the current record and the replacement endpoint pool - `inst-um-rp-6`
7. [x] - `p1` - **IF** the alias would change - `inst-um-rp-7`
   1. [x] - `p1` - Return a validation error stating that the alias is immutable and that the upstream must be deleted and re-created - `inst-um-rp-8`
8. [x] - `p1` - **IF** the record is bound to an ancestor upstream by alias and the replacement changes an override, an endpoint, or the alias - `inst-um-rp-12`
   1. [x] - `p1` - Re-validate the ancestor bind constraints per DESIGN §3.3, requiring `oagw:upstream:bind` resolved through `authz_resolver` and rejecting an override of a `sharing: enforce` ancestor-owned sub-configuration, with the rejection rendered through the canonical permission-denied surface at `403` when the permission is absent - `inst-um-rp-13`
9. [x] - `p1` - Apply the full replacement, overwriting every field, clearing omitted optional sub-configuration blocks to absent with no implicit empty block persisted, and re-persisting the tag rows and the ordered plugin binding rows as one atomic record write - `inst-um-rp-9`
10. [x] - `p1` - Order the write as a configuration write that triggers Control Plane L1 invalidation and the Data Plane hot-config flush per `cpt-cf-oagw-adr-request-routing`, `cpt-cf-oagw-adr-data-plane-caching`, and `cpt-cf-oagw-adr-state-management`: store write, then CP L1 invalidation, then the DP flush request, then success; the mechanism is owned by `cpt-cf-oagw-feature-observability-and-operability` and is invoked, not implemented, here - `inst-um-rp-14`
11. [x] - `p1` - **RETURN** `200` with the replaced record - `inst-um-rp-10`

### Delete Upstream

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-upstream-management-delete`

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- The caller's own upstream record is removed together with its dependent route records, tag rows, and ordered plugin binding rows, following the cascade relationship of the documented upstream schema contract.
- The route-row cascade is exercised through the foundation repository cascade contract of `cpt-cf-oagw-feature-gear-foundation` (the `UpstreamRepository` delete boundary), whose table shapes are entry 2.1's documented schema contract; this entry supplies only the upstream-side cascade trigger. The route-cascade integration assertion is validated once route records are creatable through `cpt-cf-oagw-feature-route-management` (both entries are delivered in the same crate, so there is no cross-entry runtime dependency).
- The removal is one atomic operation, so the proxy path never observes a half-deleted record.

**Error Scenarios**:
- The identifier is missing or foreign, including an ancestor record: not-found is returned and nothing is removed.
- The request carries no bearer token, an invalid one, or no resolvable security context: it is rejected with `401` and `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`, rendered by `cpt-cf-oagw-feature-error-handling`.
- The security context is valid but lacks `gts.cf.core.oagw.upstream.v1~:delete`: the delete is rejected with `403` through the shared canonical permission-denied error surface, rendered by `cpt-cf-oagw-feature-error-handling`.

**Steps**:
1. [x] - `p1` - Receive `DELETE /oagw/v1/upstreams/{id}` with the path identifier and the caller's tenant context - `inst-um-dl-1`
2. [x] - `p1` - Require the validated bearer token and the resolved security context of §2 and check `gts.cf.core.oagw.upstream.v1~:delete` before the service call, rejecting an absent or invalid token with `401` and a valid context that lacks the permission with `403` - `inst-um-dl-7`
3. [x] - `p1` - Load the record bound to the caller's tenant identifier - `inst-um-dl-2`
4. [x] - `p1` - **IF** no record is owned by the calling tenant under that identifier - `inst-um-dl-3`
   1. [x] - `p1` - Return not-found and remove nothing - `inst-um-dl-4`
5. [x] - `p1` - Remove the record, its dependent route records through the foundation repository cascade contract, its tag rows, and its ordered plugin binding rows in one atomic operation - `inst-um-dl-5`
6. [x] - `p1` - Order the write as a configuration write that triggers Control Plane L1 invalidation and the Data Plane hot-config flush per `cpt-cf-oagw-adr-request-routing`, `cpt-cf-oagw-adr-data-plane-caching`, and `cpt-cf-oagw-adr-state-management`: store write, then CP L1 invalidation, then the DP flush request, then success; the mechanism is owned by `cpt-cf-oagw-feature-observability-and-operability` and is invoked, not implemented, here - `inst-um-dl-8`
7. [x] - `p1` - **RETURN** `204 No Content` as the deletion confirmation with no body content - `inst-um-dl-6`

### Enable and Disable an Upstream

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-upstream-management-enable-disable`

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Surface**: `PUT /oagw/v1/upstreams/{id}` with a full replacement body. `enabled` is settable only through a full replacement under the DESIGN §3.3 full-replacement semantics, so no dedicated enable or disable endpoint is minted and none exists on the route tree.

**Success Scenarios**:
- The owning tenant sets `enabled` to false through a full replacement and the record presents as disabled to every tenant that can reach it; the proxy path then rejects requests for that upstream with `503`, rendered by the shared error contract of entry 2.5 as `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1`.
- The owning tenant sets `enabled` back to true and the record serves proxy traffic again, provided no ancestor disablement governs the record.
- A record held by an ancestor tenant that the ancestor has disabled presents as disabled to every descendant tenant, and no descendant action changes that.
- The replacement is applied as one atomic record write, so the proxy path observes either the previous or the new record and never a partial one.

**Error Scenarios**:
- A tenant other than the owning tenant attempts to change `enabled`, including a descendant addressing an ancestor record: not-found is returned, because ancestor resources are not addressable through the management API, and the disablement stands.
- A disablement that an ancestor tenant holds is unaffected by any descendant write, so an ancestor-disabled resource can never be re-enabled from below.
- The identifier is missing or foreign, including an ancestor record: not-found is returned and nothing is written.
- The replacement body violates a record shape, including an endpoint or sub-configuration the owning tenant did not previously carry: a validation error is returned and the stored record is left unchanged.
- The request carries no bearer token, an invalid one, or no resolvable security context: it is rejected before payload validation with `401` and `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`, rendered by `cpt-cf-oagw-feature-error-handling`.
- The security context is valid but lacks `gts.cf.core.oagw.upstream.v1~:override`: the replacement is rejected with `403` through the shared canonical permission-denied error surface, rendered by `cpt-cf-oagw-feature-error-handling`.

**Steps**:
1. [x] - `p1` - Receive `PUT /oagw/v1/upstreams/{id}` with a full replacement body carrying an `enabled` value for a record owned by the calling tenant - `inst-um-en-1`
2. [x] - `p1` - Require the validated bearer token and the resolved security context of §2 and check `gts.cf.core.oagw.upstream.v1~:override` before the service call, rejecting an absent or invalid token with `401` and a valid context that lacks the permission with `403` - `inst-um-en-10`
3. [x] - `p1` - Load the record bound to the caller's tenant identifier, so that a missing identifier or one owned by another tenant resolves to not-found - `inst-um-en-11`
4. [x] - `p1` - Validate the full replacement body with `cpt-cf-oagw-algo-upstream-management-endpoint-validation` and `cpt-cf-oagw-algo-upstream-management-config-validation`, exactly as the replace flow does - `inst-um-en-12`
5. [x] - `p1` - Enforce `cpt-cf-oagw-algo-upstream-management-alias-immutability` against the current record and the replacement endpoint pool, exactly as the replace flow does - `inst-um-en-13`
6. [x] - `p1` - Apply `cpt-cf-oagw-algo-upstream-management-enabled-inheritance` to determine the effective enablement the record presents to the requesting tenant - `inst-um-en-2`
7. [x] - `p1` - **IF** the record is owned by the calling tenant - `inst-um-en-3`
   1. [x] - `p1` - Store the supplied `enabled` value, defaulting it to true when the field is omitted - `inst-um-en-4`
8. [x] - `p1` - **IF** the record is owned by an ancestor tenant - `inst-um-en-5`
   1. [x] - `p1` - Return not-found and change nothing, so an ancestor disablement cannot be lifted by a descendant - `inst-um-en-6`
9. [x] - `p1` - **IF** the resulting state is disabled - `inst-um-en-7`
   1. [x] - `p1` - Mark the record unavailable to the proxy path so that a proxy request for its alias is rejected with `503`, rendered by entry 2.5 as `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1` - `inst-um-en-8`
10. [x] - `p1` - Order the write as a configuration write that triggers Control Plane L1 invalidation and the Data Plane hot-config flush per `cpt-cf-oagw-adr-request-routing`, `cpt-cf-oagw-adr-data-plane-caching`, and `cpt-cf-oagw-adr-state-management`: store write, then CP L1 invalidation, then the DP flush request, then success; the mechanism is owned by `cpt-cf-oagw-feature-observability-and-operability` and is invoked, not implemented, here - `inst-um-en-14`
11. [x] - `p1` - **RETURN** `200` with the replaced record and its stored and effective enablement - `inst-um-en-9`

## 3. Processes / Business Logic (CDSL)

### Alias Derivation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-upstream-management-alias-derivation`

**Input**: the endpoint pool of an upstream request, each endpoint carrying `scheme`, `host`, and `port`, plus the public-suffix-list data used for suffix validation.
**Output**: a derived alias, or the verdict that the pool is non-derivable and an explicit alias is required.

**Steps**:
1. [x] - `p1` - Normalize every endpoint host to ASCII lowercase and strip a single trailing dot tolerated as FQDN notation - `inst-um-ad-1`
2. [x] - `p1` - Determine the standard port for the pool's scheme: 80 for `http`, and 443 for `https`, `wss`, `wt`, and `grpc` - `inst-um-ad-2`
3. [x] - `p1` - **IF** the pool holds exactly one hostname endpoint - `inst-um-ad-3`
   1. [x] - `p1` - Derive the alias as the hostname when the port is the standard port, and as `hostname:port` when it is not - `inst-um-ad-4`
4. [x] - `p1` - **IF** the pool holds two or more hostname endpoints - `inst-um-ad-5`
   1. [x] - `p1` - Compute the registrable common suffix of the hostnames and accept it only when it carries at least two labels and is a registrable domain on the public suffix list - `inst-um-ad-6`
   2. [x] - `p1` - Derive the alias as the common suffix when every port is standard, and as `suffix:port` when the pool's port is non-standard, so pools sharing a suffix on different ports derive distinct aliases - `inst-um-ad-7`
   3. [x] - `p1` - Verdict non-derivable when the only common suffix is a bare public suffix, such as a two-label country suffix, or when the hostnames share no registrable common suffix - `inst-um-ad-8`
5. [x] - `p1` - Verdict non-derivable when any endpoint host is an IP address - `inst-um-ad-9`
6. [x] - `p1` - Normalize the derived alias to ASCII lowercase with trailing dots stripped, and confirm it matches the alias pattern of the upstream schema - `inst-um-ad-10`
7. [x] - `p1` - **RETURN** the derived alias, or the non-derivable verdict that makes an explicit alias mandatory - `inst-um-ad-11`

### Alias Immutability Enforcement

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-upstream-management-alias-immutability`

**Input**: the stored upstream record with its current endpoint pool and alias, and the replacement endpoint pool with any supplied alias.
**Output**: the alias to store, or a rejection instructing the operator to delete and re-create the upstream.

**Steps**:
1. [x] - `p1` - Classify the stored pool and the replacement pool as derivable or non-derivable using `cpt-cf-oagw-algo-upstream-management-alias-derivation` - `inst-um-ai-1`
2. [x] - `p1` - **IF** the replacement pool is unchanged from the stored pool - `inst-um-ai-2`
   1. [x] - `p1` - Accept the record when no alias is supplied, retaining the stored alias - `inst-um-ai-3`
   2. [x] - `p1` - Accept silently, as an idempotent no-op, when the supplied alias equals the stored alias exactly - `inst-um-ai-4`
   3. [x] - `p1` - Reject with a validation error when the supplied alias differs from the stored alias, because an alias override is not permitted - `inst-um-ai-5`
3. [x] - `p1` - **IF** the stored pool is derivable and the replacement pool is derivable - `inst-um-ai-6`
   1. [x] - `p1` - Accept when the alias derived from the replacement pool equals the stored alias - `inst-um-ai-7`
   2. [x] - `p1` - Reject with a validation error when it differs, instructing the operator to delete and re-create the upstream - `inst-um-ai-8`
4. [x] - `p1` - **IF** the stored pool is derivable and the replacement pool is non-derivable - `inst-um-ai-9`
   1. [x] - `p1` - Reject always, even when an explicit alias equal to the stored alias is supplied - `inst-um-ai-10`
5. [x] - `p1` - **IF** the stored pool is non-derivable and the replacement pool is non-derivable - `inst-um-ai-11`
   1. [x] - `p1` - Retain the stored alias and reject a differing user-supplied alias with a validation error - `inst-um-ai-12`
6. [x] - `p1` - **IF** the stored pool is non-derivable and the replacement pool is derivable - `inst-um-ai-13`
   1. [x] - `p1` - Accept when the alias derived from the replacement pool equals the stored alias - `inst-um-ai-14`
   2. [x] - `p1` - Reject with a validation error when it differs, instructing the operator to delete and re-create the upstream - `inst-um-ai-15`
7. [x] - `p1` - **RETURN** the accepted alias, or the rejection - `inst-um-ai-16`

### Endpoint and Scheme Validation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-upstream-management-endpoint-validation`

**Input**: the endpoint pool and the `protocol` value of an upstream request, and the `allow_http_upstream` value of `OagwConfig`.
**Output**: an accepted pool and protocol, or a validation failure naming the offending endpoint or field.

**Steps**:
1. [x] - `p1` - Require at least one endpoint and reject an empty pool - `inst-um-ev-1`
2. [x] - `p1` - Require `scheme` and `host` on every endpoint, apply the `https` default when `scheme` is omitted, and reject an endpoint carrying a field outside `scheme`, `host`, and `port` - `inst-um-ev-2`
3. [x] - `p1` - Admit only the scheme values `http | https | wss | wt | grpc`, and admit `http` only while `allow_http_upstream` is true, returning a validation error that names the offending endpoint otherwise - `inst-um-ev-3`
4. [x] - `p1` - Validate each host as an RFC 1123 hostname (at most 253 characters, labels of 1 to 63 ASCII alphanumeric or hyphen characters, no leading or trailing hyphen, trailing dot tolerated and stripped) or as an IPv4 or IPv6 address - `inst-um-ev-4`
5. [x] - `p1` - Validate each port as an integer from 1 to 65535, applying the schema default of 443 when omitted - `inst-um-ev-5`
6. [x] - `p1` - Require pool uniformity: every endpoint in one upstream carries the same `protocol`, the same `scheme`, and the same `port`, so the pool can serve as a single load-balance pool - `inst-um-ev-6`
7. [x] - `p1` - Accept `protocol` only as `gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1` or `gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1`, recording the gRPC value as configuration and schema surface only, with no gRPC proxy code path in this decomposition - `inst-um-ev-7`
8. [x] - `p1` - **RETURN** the validated pool and protocol, or the specific validation failure - `inst-um-ev-8`

### Sub-Configuration Validation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-upstream-management-config-validation`

**Input**: the upstream request body with its `auth`, `headers`, `tags`, `rate_limit`, `cors`, and `plugins` sub-configurations.
**Output**: an accepted record payload, or a validation failure naming the offending field.

**Steps**:
1. [x] - `p1` - Reject any field outside the upstream schema property set, so an unknown or misspelled key cannot silently disable a control - `inst-um-cv-1`
2. [x] - `p1` - Validate `auth`: a GTS identifier as the auth plugin type, `sharing` limited to `private`, `inherit`, and `enforce` with `private` as the default, and a configuration object whose credential-bearing fields hold `cred://` references only - `inst-um-cv-2`
3. [x] - `p1` - **IF** a credential-bearing field holds a value that is not a `cred://` reference - `inst-um-cv-3`
   1. [x] - `p1` - Reject the record naming the field without echoing the rejected value, and leave secret resolution through `cred_store` to entry 2.6, so that no `cred_store` call happens anywhere on the management path - `inst-um-cv-4`
4. [x] - `p1` - Accept a `cred://` reference without checking its existence or its tenant accessibility, because both are verified only at request time by `cpt-cf-oagw-feature-plugin-system`'s credential-resolution step; a dangling or foreign-tenant reference is therefore accepted at configuration time and surfaces later as a request-time failure rendered by entry 2.5 as `500` with `gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1`, which is the deliberate configuration-time posture of this surface - `inst-um-cv-13`
5. [x] - `p1` - Validate `headers` as request and response blocks holding only `set`, `add`, `remove`, and, for requests, `passthrough` with the values `none`, `allowlist`, and `all` plus `passthrough_allowlist`; require `passthrough_allowlist` to be meaningful only with `passthrough: allowlist` - `inst-um-cv-5`
6. [x] - `p1` - Validate every header rule before it is persisted: a header name must satisfy RFC 7230 field-name grammar with no separator characters and no control characters, a name or a value must contain no carriage return, no line feed, and no NUL byte, and neither a name nor a value may exceed 4096 bytes, so a rejected rule can never be stored and later applied outbound; the failure is a `400` validation error rendered by `cpt-cf-oagw-feature-error-handling` that names the offending block, `headers.request` or `headers.response`, and the offending key - `inst-um-cv-12`
7. [x] - `p1` - Validate every tag against `^[a-z0-9_-]+$` and store tags as a set of rows keyed by the parent record - `inst-um-cv-6`
8. [x] - `p1` - Validate `rate_limit`: `sharing` within the three sharing modes, `algorithm` within `token_bucket` and `sliding_window`, a required `sustained.rate` of at least 1, `sustained.window` within `second`, `minute`, `hour`, and `day`, `burst.capacity` of at least 1, `scope` within `global`, `tenant`, `user`, `ip`, and `route`, `strategy` within `reject`, `queue`, and `degrade`, and `cost` of at least 1 - `inst-um-cv-7`
9. [x] - `p1` - Validate `cors`: a required `enabled` flag, `sharing` within the three sharing modes, origins each either the literal `*` or a well-formed origin, methods within the documented method set, and the rejection of `allow_credentials: true` combined with a wildcard origin - `inst-um-cv-8`
10. [x] - `p1` - Validate `plugins`: a `sharing` value within the three sharing modes and an ordered `items` list whose entries are either builtin plugin GTS identifiers or custom plugin UUIDs, stored as contiguous positions from zero with the reference stored and the UUID recorded only when the reference is UUID-backed - `inst-um-cv-9`
11. [x] - `p1` - Treat a sub-configuration that is absent as absent rather than substituting an implicit value, so no implicit empty block is persisted and the absent-stays-absent rule of the merge algorithm in `cpt-cf-oagw-feature-gear-foundation` decides inheritance: an ancestor block that is absent contributes nothing to a descendant, and the schema defaults of an absent block, `auth.sharing` `private`, `cors.enabled` false, `plugins.sharing` `private`, and the `rate_limit` defaults, are applied by the merge engine at merge time and never materialized here - `inst-um-cv-10`
12. [x] - `p1` - **RETURN** the validated payload, or the specific validation failure - `inst-um-cv-11`

### List Query Interpretation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-upstream-management-list-query`

**Input**: the OData query parameters of a list request.
**Output**: a filter expression, a field projection, an ordering key, an offset, and a limit, or a validation failure naming the offending parameter.

**Steps**:
1. [x] - `p1` - Parse `$filter` as an OData filter expression over the upstream record fields, such as `alias eq 'api.openai.com'`, and reject an expression that cannot be parsed - `inst-um-lq-1`
2. [x] - `p1` - Parse `$select` as a comma-separated field list and reject a field name that is not part of the upstream record - `inst-um-lq-2`
3. [x] - `p1` - Parse `$orderby` as a field name with an optional `asc` or `desc` direction - `inst-um-lq-3`
4. [x] - `p1` - Parse `$top` with a default of 50 and a maximum of 100, rejecting a value above the maximum or below 1 rather than silently clamping it - `inst-um-lq-4`
5. [x] - `p1` - Parse `$skip` as a non-negative offset - `inst-um-lq-5`
6. [x] - `p1` - **RETURN** the interpreted query, or the validation failure naming the offending parameter - `inst-um-lq-6`

### Effective Enablement Inheritance

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-upstream-management-enabled-inheritance`

**Input**: an upstream record with its owning tenant, the requesting tenant, and the tenant chain between them.
**Output**: the effective enablement the record presents to the requesting tenant, with the reason when it is disabled.

**Steps**:
1. [x] - `p1` - Start from the record's own `enabled` flag, which defaults to true when the field is absent - `inst-um-ei-1`
2. [x] - `p1` - Treat the flag as authoritative for the owning tenant and as inherited unchanged by every descendant tenant, so an ancestor's disablement applies to all descendants - `inst-um-ei-2`
3. [x] - `p1` - Permit a change to the flag only from the owning tenant, and reject a write from any other tenant as not-found so an ancestor disablement cannot be lifted by a descendant - `inst-um-ei-3`
4. [x] - `p1` - **IF** the record is disabled and an ancestor tenant holds the disablement that governs the requesting tenant's view - `inst-um-ei-4`
   1. [x] - `p1` - Report the effective state as disabled with the ancestor as the disabling tenant, so the proxy path rejects requests for the alias with `503` - `inst-um-ei-5`
5. [x] - `p1` - **RETURN** the effective enablement and the disabling tenant when the effective state is disabled - `inst-um-ei-6`

## 4. States (CDSL)

### Upstream Record State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-upstream-management-upstream-lifecycle`

**States**: `absent`, `enabled`, `disabled`, `disabled-by-ancestor`, `removed`. The first three and `removed` are stored states of a record; `disabled-by-ancestor` is a derived effective state and never a stored one: it is the enablement a record owned by an ancestor tenant presents to a descendant tenant when the closest resolvable ancestor record for the same alias is `disabled`, computed at proxy resolution by `cpt-cf-oagw-algo-upstream-management-enabled-inheritance`.
**Initial State**: `absent`
**Transitions**:
1. [x] - `p1` - **FROM** `absent` **TO** `enabled` **WHEN** a create is accepted for a new `(tenant_id, alias)` key with `enabled` true or omitted, which is the default - `inst-um-st-1`
2. [x] - `p1` - **FROM** `absent` **TO** `disabled` **WHEN** a create is accepted with `enabled` false - `inst-um-st-2`
3. [x] - `p1` - **FROM** `enabled` **TO** `disabled` **WHEN** the owning tenant replaces the record with `enabled` false, after which the proxy path rejects requests for the alias with `503` - `inst-um-st-3`
4. [x] - `p1` - **FROM** `disabled` **TO** `enabled` **WHEN** the owning tenant replaces the record with `enabled` true and no ancestor disablement governs the record - `inst-um-st-4`
5. [x] - `p1` - **FROM** `disabled` **TO** `disabled` **WHEN** a tenant other than the owning tenant, including a descendant, attempts to enable the record, because ancestor resources are not addressable through the management API - `inst-um-st-5`
6. [x] - `p1` - **FROM** `enabled` **TO** `enabled` **WHEN** a full replacement is accepted with the alias unchanged - `inst-um-st-6`
7. [x] - `p1` - **FROM** `enabled` **OR** `disabled` **TO** `removed` **WHEN** the owning tenant deletes the record and its dependent route, tag, and plugin binding rows - `inst-um-st-7`
8. [x] - `p1` - **FROM** `removed` **TO** `absent` **WHEN** the in-memory entry is released and the alias key becomes available again within the owning tenant - `inst-um-st-8`
9. [x] - `p1` - **FROM** `enabled` **OR** `disabled` **TO** `disabled-by-ancestor` **WHEN** the closest resolvable ancestor record for the same alias is `disabled` and the requesting path resolves through the descendant; the state is derived at proxy resolution and is never persisted - `inst-um-st-9`
10. [x] - `p1` - **FROM** `disabled-by-ancestor` **TO** `enabled` **OR** `disabled` **WHEN** the ancestor record is re-enabled or removed, which returns the descendant to its own stored state, `enabled` or `disabled` as written by its owning tenant - `inst-um-st-10`

**Invalid transitions**: `disabled-by-ancestor` is never written by a management write of this feature and never appears as the stored value of a record: no create and no replacement stores it, no transition of rows 1 through 8 names it as a target or a source, and it exists only as the derived effective state computed at proxy resolution. A management read of the record returns only its stored state, `enabled` or `disabled`, and the ancestor disablement is reported separately as the effective enablement and the disabling tenant.

## 5. Definitions of Done

### Upstream CRUD Endpoints

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-management-crud-endpoints`

The system **MUST** implement the five upstream operations on the gear-relative collection: `POST /oagw/v1/upstreams` to create, `GET /oagw/v1/upstreams` to list, `GET /oagw/v1/upstreams/{id}` to read, `PUT /oagw/v1/upstreams/{id}` for a full-replacement update, and `DELETE /oagw/v1/upstreams/{id}` to delete, with no leading `/api` segment and with the path identifier carried as the anonymous GTS identifier `gts.cf.core.oagw.upstream.v1~{uuid}`. A create **MUST** assign a server-generated identifier, a replace **MUST** overwrite every field and clear omitted optional sub-configuration blocks to absent, and a delete **MUST** remove the record together with its dependent route, tag, and plugin binding rows in one atomic operation.

Every operation **MUST** require a validated bearer token and **MUST** check the DESIGN §3.2 permission of its operation before the service call - `gts.cf.core.oagw.upstream.v1~:create` for the create, `gts.cf.core.oagw.upstream.v1~:read` for the read and the list, `gts.cf.core.oagw.upstream.v1~:override` for the replacement (and therefore for setting `enabled`), and `gts.cf.core.oagw.upstream.v1~:delete` for the delete - rejecting an absent or invalid token, or an absent security context, with `401` and `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`, and a valid context that lacks the permission with `403` rendered through the shared canonical permission-denied surface, both by `cpt-cf-oagw-feature-error-handling` and with no new OAGW error type minted here. The success statuses **MUST** be `201 Created` with the stored record for the create, `200` with the stored record for the read, `200` with the projected list for the list, `200` with the replaced record for the replacement, and `204 No Content` with an empty body for the delete, and each mutating operation **MUST** return `409 Conflict` when its uniqueness or bind precondition fails. Every response body returned by these operations **MUST** present credential-bearing fields as their stored `cred://` references and carry no resolved secret material.

**Implements**:
- `cpt-cf-oagw-flow-upstream-management-create`
- `cpt-cf-oagw-flow-upstream-management-list`
- `cpt-cf-oagw-flow-upstream-management-get`
- `cpt-cf-oagw-flow-upstream-management-replace`
- `cpt-cf-oagw-flow-upstream-management-delete`
- `cpt-cf-oagw-state-upstream-management-upstream-lifecycle`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `GET /oagw/v1/upstreams`, `GET /oagw/v1/upstreams/{id}`, `PUT /oagw/v1/upstreams/{id}`, `DELETE /oagw/v1/upstreams/{id}`
- Entities: `Upstream`, `ServerConfig`, `Endpoint`
- Tests: integration tests in `tests/upstream_crud.rs` for the five operations, the generated identifier, the full-replacement clearing of omitted fields, the cascade delete, the success status of each operation (`201`, `200`, `200`, `200`, `204`) and the `409` failure of each mutating operation, and the denied outcomes of each operation: a request with no bearer token rejected with `401` and `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`, and a request whose context lacks the operation's permission rejected with `403` through the canonical permission-denied surface

### Alias Derivation from Endpoints

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-management-alias-derivation`

The system **MUST** derive the alias from the endpoint pool: a single hostname endpoint yields the hostname, with the port appended only when it is non-standard for the pool's scheme (80 for `http`, 443 for `https`, `wss`, `wt`, and `grpc`); a multi-hostname pool yields its registrable common suffix, accepted only when the suffix carries at least two labels and is a registrable domain validated against the public suffix list, with the port preserved in the alias when the pool's port is non-standard; and an IP-based pool, a pool whose only common suffix is a bare public suffix, and a pool of heterogeneous hostnames with no common suffix **MUST** all be classified non-derivable, so that an explicit alias is mandatory and its omission returns a validation error. A user-supplied alias that differs from the derived value **MUST** be rejected, and supplying the exact derived value **MUST** be tolerated silently for idempotency.

**Implements**:
- `cpt-cf-oagw-algo-upstream-management-alias-derivation`
- `cpt-cf-oagw-flow-upstream-management-create`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- Entities: `Upstream`, `ServerConfig`, `Endpoint`
- Tests: unit tests in `src/domain/alias_tests.rs` for each derivation row of the DESIGN alias table, including the bare-public-suffix rejection and the `suffix:port` form

### Alias Immutability Transition Matrix

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-management-alias-immutability`

The system **MUST** treat the alias as immutable once set, because it is the routing key of the proxy path, and **MUST** enforce the full transition matrix on replacement: derivable to derivable is accepted only when the recomputed alias equals the stored alias and is rejected otherwise; derivable to non-derivable is rejected always, even when an explicit alias equal to the stored alias is supplied; non-derivable to non-derivable retains the stored alias and rejects a differing user-supplied alias; non-derivable to derivable is accepted only when the derived alias equals the stored alias; and an unchanged endpoint pool accepts an omitted alias and tolerates an exact-match alias as an idempotent no-op. Every rejection **MUST** be a validation error that instructs the operator to delete and re-create the upstream, and this behavior **MUST** supersede any permissive explicit-alias-update reading of the CRUD semantics. Because `enabled` is settable only through a full replacement, the enable and disable path of `cpt-cf-oagw-flow-upstream-management-enable-disable` **MUST** enforce the same matrix over the same validation steps as the replace flow.

**Implements**:
- `cpt-cf-oagw-algo-upstream-management-alias-immutability`
- `cpt-cf-oagw-flow-upstream-management-replace`
- `cpt-cf-oagw-flow-upstream-management-enable-disable`

**Touches**:
- API: `PUT /oagw/v1/upstreams/{id}`
- Entities: `Upstream`, `ServerConfig`, `Endpoint`
- Tests: unit tests in `src/domain/alias_tests.rs` covering every cell of the transition matrix and the exact-match idempotency tolerance, plus integration tests in `tests/upstream_alias.rs`

### Alias Normalization and Per-Tenant Uniqueness

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-management-alias-normalization`

The system **MUST** normalize every alias to ASCII lowercase with trailing dots stripped, resolve aliases case-insensitively, and enforce uniqueness per `(tenant_id, alias)` rather than globally, returning `409 Conflict` when a create collides within the calling tenant and leaving the store unchanged. The uniqueness check and the multi-row persist **MUST** execute as one critical section inside the repository write path, so a detected collision leaves the store unchanged rather than partially written, and cross-instance concurrency control is out of scope per DESIGN §4.7, which is a recorded limitation on the determinism invariant. An alias collision with an ancestor tenant's upstream **MUST NOT** be reported as a conflict at this layer: it is a bind per DESIGN §3.3, accepted when the caller holds `oagw:upstream:bind` resolved through `authz_resolver` and rejected through the canonical permission-denied surface at `403` when that permission is absent, with `sharing: private` on the ancestor upstream blocking visibility so the alias stays available for a local create and `sharing: enforce` on the ancestor blocking the descendant from overriding ancestor-owned sub-configurations. Ancestor records themselves remain invisible to read, replace, and delete, and the ancestor alias match is the case that proxy-time resolution by entry 2.4 resolves.

**Implements**:
- `cpt-cf-oagw-flow-upstream-management-create`
- `cpt-cf-oagw-algo-upstream-management-alias-derivation`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- Entities: `Upstream`
- Tests: unit tests in `src/domain/alias_tests.rs` for normalization and case-insensitive resolution, and integration tests in `tests/upstream_alias.rs` for the `409` on a same-tenant collision, the absence of a conflict against an ancestor alias, the `403` on an ancestor-alias bind whose `oagw:upstream:bind` permission is absent, and two concurrent creates of the same `(tenant_id, alias)` asserting exactly one create succeeds and the store holds one record

### Endpoint, Scheme, and Protocol Validation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-management-endpoint-scheme-validation`

The system **MUST** validate the endpoint pool before any record is stored: at least one endpoint, `scheme` and `host` required with `https` as the scheme default, the scheme limited to `http | https | wss | wt | grpc`, hosts validated as RFC 1123 hostnames or as IPv4 or IPv6 addresses, ports validated as integers from 1 to 65535 with 443 as the default, and the pool uniform in `protocol`, `scheme`, and `port`. The `http` scheme **MUST** be admitted when `oagw.config.allow_http_upstream` is true and rejected with a validation error naming the offending endpoint when it is false, which is the graded lift of the default-TLS posture and not a prohibition on `http`. The `protocol` value **MUST** be limited to the `http` and `grpc` GTS protocol identifiers, with the gRPC value accepted as configuration and schema surface only. The same validation **MUST** run on the enable and disable path, which is a full replacement on the same route.

**Implements**:
- `cpt-cf-oagw-algo-upstream-management-endpoint-validation`
- `cpt-cf-oagw-flow-upstream-management-create`
- `cpt-cf-oagw-flow-upstream-management-replace`
- `cpt-cf-oagw-flow-upstream-management-enable-disable`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `PUT /oagw/v1/upstreams/{id}`
- Entities: `Upstream`, `ServerConfig`, `Endpoint`, `OagwConfig`
- Tests: unit tests in `src/domain/validation_tests.rs` for the scheme set, the `http` gate against `allow_http_upstream`, host and port validation, and pool uniformity

### Authentication Configuration with Credential References

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-management-auth-config`

The system **MUST** accept the `auth` sub-configuration as a plugin type reference, a `sharing` value within `private`, `inherit`, and `enforce` defaulting to `private`, and a configuration object whose credential-bearing fields hold `cred://` references only, rejecting any other value at the configuration boundary without echoing it. The system **MUST NOT** resolve, store, log, or return secret material: resolution of a `cred://` reference through `cred_store` at request time is owned by entry 2.6 under its credential-store contract, and this feature stores the reference verbatim.

The write-time posture is deliberate and **MUST** be recorded as such: no `cred_store` call happens anywhere on the management path, and neither the existence nor the tenant accessibility of a `cred://` reference is verified at configuration time. Both are verified only at request time by `cpt-cf-oagw-feature-plugin-system`'s credential-resolution step, so a dangling or a foreign-tenant reference is accepted here and surfaces later as a request-time failure rendered by entry 2.5 as `500` with `gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1`. The read-back contract follows from the reference being configuration and not secret material: every create, read, and replace response returns the references exactly as stored, and no response, log line, error body, or audit field on this surface carries resolved secret material or an injected credential value, which is the redaction rule of `cpt-cf-oagw-feature-gear-foundation` applied to the management path.

**Implements**:
- `cpt-cf-oagw-algo-upstream-management-config-validation`
- `cpt-cf-oagw-flow-upstream-management-create`
- `cpt-cf-oagw-flow-upstream-management-get`
- `cpt-cf-oagw-flow-upstream-management-replace`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `GET /oagw/v1/upstreams/{id}`, `PUT /oagw/v1/upstreams/{id}`
- Entities: `Upstream`, `AuthConfig`
- Tests: unit tests in `src/domain/validation_tests.rs` for `cred://` acceptance, non-reference rejection, and the sharing-mode set, and integration tests in `tests/upstream_crud.rs` asserting that the `auth.config` member of a `GET` response body holds only `cred://` reference values and no resolved secret material

### Header Transformation Configuration

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-management-header-transform-config`

The system **MUST** accept a `headers` sub-configuration holding a request block and a response block, where a request block carries `set`, `add`, `remove`, `passthrough` with the values `none`, `allowlist`, and `all` defaulting to `none`, and `passthrough_allowlist`, and a response block carries `set`, `add`, and `remove`. The system **MUST** reject a key outside these sets and **MUST** store the rules verbatim for the proxy path to apply; the routing and hop-by-hop stripping behavior that the rules compose with is defined by the header transformation rules and is enforced on the proxy path in entry 2.4.

Because a stored rule is applied outbound, the system **MUST** reject before persistence any header rule whose name or value could corrupt or forge the outbound request: a header name outside RFC 7230 field-name grammar, including a name carrying a separator character or any control character; a name or a value carrying a carriage return, a line feed, or a NUL byte; and a name or a value exceeding 4096 bytes. The rejection **MUST** be a `400` validation error rendered by `cpt-cf-oagw-feature-error-handling` that names the offending block, `headers.request` or `headers.response`, and the offending key.

**Implements**:
- `cpt-cf-oagw-algo-upstream-management-config-validation`
- `cpt-cf-oagw-flow-upstream-management-create`
- `cpt-cf-oagw-flow-upstream-management-replace`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `PUT /oagw/v1/upstreams/{id}`
- Entities: `Upstream`, `HeadersConfig`
- Tests: unit tests in `src/domain/validation_tests.rs` for the accepted key sets, the `passthrough` values, the rejection of unknown header rule keys, a header name and a header value each carrying a carriage return and a line feed and a NUL byte, a name violating RFC 7230 field-name grammar, and a name and a value exceeding 4096 bytes

### Tags with Add-Only Union Semantics

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-management-tags`

The system **MUST** accept tags as a flat list of strings matching `^[a-z0-9_-]+$`, store them as rows keyed by the parent upstream, and declare them as carrying add-only union semantics across the tenant hierarchy: a descendant may add tags and **MUST NOT** remove an inherited tag, and tags carry no sharing mode of their own. Tag application across the hierarchy is performed by the merge engine of `cpt-cf-oagw-feature-gear-foundation`, whose cross-hierarchy add-only union assertion is `cpt-cf-oagw-dod-gear-foundation-merge-engine`; this feature validates the tag syntax, stores the rows, and preserves the union semantics on replacement, and what this surface can observe is exactly that: a tag matching the pattern is accepted and a tag failing it is rejected, tag rows are stored on create, and a replacement that omits a tag removes its row while a replacement that includes it preserves it.

**Implements**:
- `cpt-cf-oagw-algo-upstream-management-config-validation`
- `cpt-cf-oagw-flow-upstream-management-replace`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `PUT /oagw/v1/upstreams/{id}`
- Entities: `Upstream`
- Tests: unit tests in `src/domain/validation_tests.rs` for the tag pattern and for the add-only union behavior across a two-level hierarchy

### Enable and Disable Semantics

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-management-enable-disable`

The system **MUST** support `enabled` as a boolean field defaulting to true, settable only through a full replacement by the owning tenant, on `PUT /oagw/v1/upstreams/{id}` under the DESIGN §3.3 full-replacement semantics and with no dedicated enable or disable endpoint on the route tree; a replacement that changes `enabled` **MUST** therefore run the same endpoint, scheme, and sub-configuration validation and the same alias-immutability enforcement as any other replacement. A disabled upstream **MUST** be unavailable to the proxy path, so that a proxy request for its alias is rejected with `503 Service Unavailable` and rendered by entry 2.5 as `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1`; the enforcement point is the proxy path of entry 2.4, and the stored state this feature maintains is what it consults. Enablement **MUST** inherit down the tenant hierarchy: an upstream disabled by an ancestor tenant presents as disabled to every descendant, and no descendant operation can lift that disablement, because ancestor resources return not-found to descendants through the management API. The `disabled-by-ancestor` presentation **MUST** be a derived effective state only: it is never stored by a management write, it is entered when the closest resolvable ancestor record for the same alias is `disabled` and the request resolves through the descendant, and it clears when that ancestor record is re-enabled or removed, returning the descendant to its own stored state.

**Implements**:
- `cpt-cf-oagw-flow-upstream-management-enable-disable`
- `cpt-cf-oagw-algo-upstream-management-enabled-inheritance`
- `cpt-cf-oagw-state-upstream-management-upstream-lifecycle`

**Touches**:
- API: `PUT /oagw/v1/upstreams/{id}`
- Entities: `Upstream`
- Tests: integration tests in `tests/upstream_enable_disable.rs` for the disabled state and the ancestor-disablement stickiness, for the entering transition into `disabled-by-ancestor` when an ancestor record for the same alias is disabled and its clearing transition back to the descendant's stored state when the ancestor record is re-enabled or removed, and unit tests in `src/domain/services/management_tests.rs` for the effective-enablement computation

### Plugin Sharing References

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-upstream-management-plugin-references`

The system **MUST** accept a `plugins` sub-configuration carrying a `sharing` value within `private`, `inherit`, and `enforce` defaulting to `private`, and an ordered `items` list whose entries are builtin plugin GTS identifiers or custom plugin UUID references, stored as binding rows with contiguous positions from zero, the reference stored on every row, and the UUID recorded only when the reference is UUID-backed. This feature stores and validates the references and their sharing mode; chain composition, registry resolution, catalog-only rejection at binding time, and execution are owned by entry 2.6, and the concatenation of upstream and route chains is the merge engine's rule.

**Implements**:
- `cpt-cf-oagw-algo-upstream-management-config-validation`
- `cpt-cf-oagw-flow-upstream-management-create`
- `cpt-cf-oagw-flow-upstream-management-replace`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `PUT /oagw/v1/upstreams/{id}`
- Entities: `Upstream`, `PluginsConfig`
- Tests: unit tests in `src/domain/validation_tests.rs` for the reference forms, position contiguity, and the sharing-mode set

### Nested Rate-Limit and CORS Sub-Configuration

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-upstream-management-nested-subconfig`

The system **MUST** validate the nested `rate_limit` and `cors` sub-configurations against the upstream schema shapes: `rate_limit` requires `sustained` with a rate of at least 1, and admits `sharing`, `algorithm`, `sustained.window`, `burst.capacity`, `scope` including the `route` value, `strategy`, and `cost`; `cors` requires `enabled`, and admits `sharing`, `allowed_origins`, `allowed_methods`, `expose_headers`, and `allow_credentials`, rejecting `allow_credentials: true` combined with a wildcard origin at validation time. Both sub-configurations carry a `sharing` value within `private`, `inherit`, and `enforce`, whose hierarchical application is the merge engine's responsibility and is not re-decided here.

**Implements**:
- `cpt-cf-oagw-algo-upstream-management-config-validation`
- `cpt-cf-oagw-flow-upstream-management-create`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `PUT /oagw/v1/upstreams/{id}`
- Entities: `Upstream`, `RateLimitConfig`, `CorsConfig`
- Tests: unit tests in `src/domain/validation_tests.rs` for the required fields, the enum values including the `route` scope, and the credentials-with-wildcard rejection

### OData List Query Parameters

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-management-odata-list`

The system **MUST** support `$filter`, `$select`, `$orderby`, `$top`, and `$skip` on `GET /oagw/v1/upstreams`, with `$top` defaulting to 50 and capped at 100 and `$skip` as a non-negative offset. An out-of-range or unparseable parameter **MUST** be rejected with a validation error naming the parameter rather than silently clamped or ignored, the returned list **MUST** contain only records owned by the calling tenant, and the list **MUST** be returned as `200` with the projected fields and the count actually returned, including for an empty result.

**Implements**:
- `cpt-cf-oagw-algo-upstream-management-list-query`
- `cpt-cf-oagw-flow-upstream-management-list`

**Touches**:
- API: `GET /oagw/v1/upstreams`
- Entities: `Upstream`
- Tests: integration tests in `tests/upstream_list_query.rs` for each parameter, the 50 default, the 100 cap, and the rejection of an out-of-range or malformed value

### Strict Tenant Scoping on the Management Surface

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-management-tenant-scoping`

The system **MUST** scope every upstream read and write to the calling tenant, so that a record owned by another tenant, including an ancestor, is never returned, never replaced, and never deleted, and every such access returns not-found without disclosing the foreign record's existence. Ancestor upstreams are therefore invisible to descendants through the management API while remaining reachable to them at proxy time through the tenant-chain walk owned by entry 2.4. Both `cpt-cf-oagw-actor-tenant-admin` and `cpt-cf-oagw-actor-platform-operator` are subject to the same scoping and to the same permission gate: both exercise the read and list operations under the shared permission set of DESIGN §3.2, and the mutating operations are permission-gated per operation, so the scoping rule is actor-agnostic. The ancestor-alias bind of a create is the one ancestor-facing interaction on this surface, and it is governed by `cpt-cf-oagw-dod-upstream-management-alias-normalization` rather than by visibility.

**Implements**:
- `cpt-cf-oagw-flow-upstream-management-get`
- `cpt-cf-oagw-flow-upstream-management-replace`
- `cpt-cf-oagw-flow-upstream-management-delete`
- `cpt-cf-oagw-flow-upstream-management-list`

**Touches**:
- API: `GET /oagw/v1/upstreams`, `GET /oagw/v1/upstreams/{id}`, `PUT /oagw/v1/upstreams/{id}`, `DELETE /oagw/v1/upstreams/{id}`
- Entities: `Upstream`
- Tests: integration tests in `tests/upstream_tenant_scope.rs` asserting zero cross-tenant disclosure for every operation, including the ancestor case

### Validation Against the Upstream Schema Shapes

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-upstream-management-schema-shapes`

The system **MUST** validate every upstream request and every stored record against the shapes of `docs/schemas/upstream.v1.schema.json`: `server.endpoints` required and non-empty with `scheme`, `host`, and `port` per endpoint and no additional properties, `protocol` required as a GTS protocol identifier, the alias pattern `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`, the tag pattern, the sub-configuration property sets, and the record defaults `enabled` true, scheme `https`, `port` 443, `auth.sharing` `private`, `plugins.sharing` `private`, `rate_limit` defaults, and `cors.enabled` false. Unknown properties **MUST** be rejected, and the `http` scheme value admitted here is the extension the graded configuration makes to the schema's scheme enumeration.

The defaults **MUST** be split into the two classes the merge engine relies on, so that schema defaults are never mistaken for stored values. Scalar structural defaults are materialized on write: `enabled` true, endpoint `scheme` `https`, endpoint `port` 443, the server-generated `id`, the derived `alias`, and the server-assigned `tenant_id`. Sub-configuration blocks - `auth`, `headers`, `rate_limit`, `cors`, and `plugins` - stay absent in the stored record when omitted from the request body, with no implicit empty block persisted, and their schema defaults, `auth.sharing` `private`, `cors.enabled` false, `plugins.sharing` `private`, and the `rate_limit` defaults, are applied at merge time by the merge engine of `cpt-cf-oagw-feature-gear-foundation`, so an absent ancestor block is invisible to descendants under the merge algorithm's absent-stays-absent rule and DESIGN §3.3's "omitted optional fields are cleared" reads as absent, not as an empty block. Per-collection cardinality is deliberately unbounded here: no per-collection maximum is invented because neither DESIGN nor the PRD specifies one, and the PRD open question on the maximum number of endpoints per upstream pool is recorded as open rather than resolved; the bounded resource on the management path is the request body, capped by the platform request-body limit the api-gateway enforces ahead of the handler, and merge cost is bounded by the record's own collection sizes.

**Implements**:
- `cpt-cf-oagw-algo-upstream-management-config-validation`
- `cpt-cf-oagw-algo-upstream-management-endpoint-validation`
- `cpt-cf-oagw-flow-upstream-management-create`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `PUT /oagw/v1/upstreams/{id}`
- Entities: `Upstream`, `ServerConfig`, `Endpoint`
- Tests: unit tests in `src/domain/dto_tests.rs` for serialization round-trips against the upstream schema, its scalar defaults, and its additional-property rejection, plus a merge and inheritance test in `src/domain/services/management_tests.rs` asserting that an ancestor record with an omitted `auth`, `headers`, `rate_limit`, `cors`, or `plugins` block contributes nothing to the effective configuration of a descendant and that no implicit empty block was persisted at create time

### Automated Unit Test Coverage

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-management-unit-tests`

The system **MUST** ship unit tests as sibling `*_tests.rs` modules inside the `oagw` crate covering alias derivation for every row of the alias table, every cell of the alias-immutability matrix, alias normalization and the per-tenant uniqueness conflict, endpoint and scheme validation including the `http` gate, the sub-configuration shapes for auth, headers, tags, rate limit, CORS, and plugins, the header rule rejections for a name outside RFC 7230 field-name grammar, a name and a value carrying a carriage return, a line feed, or a NUL byte, and a name or a value exceeding 4096 bytes, the absent-stays-absent merge posture of an omitted sub-configuration block, the OData query interpretation, and the effective-enablement computation, and **MUST NOT** place any test under `testing/e2e/gears/oagw/`, which is out of scope per graded deviation 4.

**Implements**:
- `cpt-cf-oagw-dod-upstream-management-alias-derivation`
- `cpt-cf-oagw-dod-upstream-management-alias-immutability`
- `cpt-cf-oagw-dod-upstream-management-endpoint-scheme-validation`
- `cpt-cf-oagw-dod-upstream-management-nested-subconfig`
- `cpt-cf-oagw-dod-upstream-management-odata-list`

**Touches**:
- API: none
- Entities: `Upstream`, `ServerConfig`, `Endpoint`, `AuthConfig`, `HeadersConfig`, `RateLimitConfig`, `CorsConfig`, `PluginsConfig`
- Tests: `src/domain/alias_tests.rs`, `src/domain/validation_tests.rs`, `src/domain/dto_tests.rs`, `src/domain/services/management_tests.rs`

### Automated Integration Test Coverage

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-management-integration-tests`

The system **MUST** ship integration-style tests inside the crate's `tests/` directory covering the five upstream operations end to end through the gear-relative routes, the alias derivation and immutability outcomes over the HTTP surface, the `409` conflict on a same-tenant alias collision, the not-found behavior for foreign and ancestor records, the OData list parameters, and the enable and disable transitions. It **MUST** additionally cover the authentication and authorization outcomes of every operation - a request with no bearer token rejected with `401` and `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1` before payload validation, and a request whose valid context lacks the operation's permission rejected with `403` through the shared canonical permission-denied surface - the success status of each of the five operations and the `409` failure of each mutating operation, the ancestor-alias bind and its `403` when `oagw:upstream:bind` is absent, a `GET` response whose `auth.config` member holds only `cred://` references, a create accepted against an omitted ancestor sub-configuration block that contributes nothing to the descendant's effective configuration, and a replacement after which the record served for the alias carries the new endpoint and not the old one, which is the observable effect of the write's Control Plane L1 invalidation and Data Plane flush ordering, and **MUST NOT** create `testing/e2e/gears/oagw/`.

**Implements**:
- `cpt-cf-oagw-dod-upstream-management-crud-endpoints`
- `cpt-cf-oagw-dod-upstream-management-alias-normalization`
- `cpt-cf-oagw-dod-upstream-management-enable-disable`
- `cpt-cf-oagw-dod-upstream-management-odata-list`
- `cpt-cf-oagw-dod-upstream-management-tenant-scoping`
- `cpt-cf-oagw-dod-upstream-management-auth-config`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `GET /oagw/v1/upstreams`, `GET /oagw/v1/upstreams/{id}`, `PUT /oagw/v1/upstreams/{id}`, `DELETE /oagw/v1/upstreams/{id}`
- Entities: `Upstream`
- Tests: `tests/upstream_crud.rs`, `tests/upstream_alias.rs`, `tests/upstream_validation.rs`, `tests/upstream_list_query.rs`, `tests/upstream_tenant_scope.rs`, `tests/upstream_enable_disable.rs`

## 6. Acceptance Criteria

- [x] A `POST /oagw/v1/upstreams` with one `https` endpoint on port 443 and no alias stores a record whose alias is the hostname, and the same body resubmitted with the explicit derived alias is accepted as an idempotent no-op (DoD `cpt-cf-oagw-dod-upstream-management-alias-derivation`).
- [x] A single-hostname pool on port 8443 derives `hostname:8443`, and a two-hostname pool `us.vendor.com` plus `eu.vendor.com` derives `vendor.com`, while `foo.co.uk` plus `bar.co.uk` derives nothing and requires an explicit alias (DoD `cpt-cf-oagw-dod-upstream-management-alias-derivation`).
- [x] An IP-based pool without an `alias` field is rejected with a validation error, and the same pool with an explicit alias is accepted (DoD `cpt-cf-oagw-dod-upstream-management-alias-derivation`).
- [x] Every cell of the alias-immutability matrix behaves as specified on `PUT`, and an endpoint change that would alter the alias returns a validation error that names the delete-and-re-create remediation (DoD `cpt-cf-oagw-dod-upstream-management-alias-immutability`).
- [x] An alias is stored ASCII lowercase with trailing dots stripped, resolves case-insensitively, and a second create with the same `(tenant_id, alias)` returns `409 Conflict` while a create whose alias matches an ancestor's record succeeds (DoD `cpt-cf-oagw-dod-upstream-management-alias-normalization`).
- [x] An endpoint pool mixing schemes, ports, or protocols is rejected, and a pool of two endpoints with identical scheme, port, and protocol is accepted (DoD `cpt-cf-oagw-dod-upstream-management-endpoint-scheme-validation`).
- [x] An `http` endpoint is accepted when `allow_http_upstream` is true and rejected with a validation error when it is false, and `wss`, `wt`, and `grpc` are accepted as scheme values with `https` applied when the scheme is omitted (DoD `cpt-cf-oagw-dod-upstream-management-endpoint-scheme-validation`).
- [x] A credential-bearing auth field holding anything other than a `cred://` reference is rejected without echoing the value, and no stored record, log line, or error body contains secret material (DoD `cpt-cf-oagw-dod-upstream-management-auth-config`).
- [x] A `headers` block with `request.set`, `request.add`, `request.remove`, `request.passthrough`, and `response.set` is accepted, and an unknown header rule key or a `passthrough_allowlist` without `passthrough: allowlist` is rejected (DoD `cpt-cf-oagw-dod-upstream-management-header-transform-config`).
- [x] A tag matching `^[a-z0-9_-]+$` is accepted and a tag failing the pattern is rejected, tag rows are stored on create, and a replacement that omits a tag removes its row while a replacement that includes it preserves it; the cross-hierarchy add-only union itself is asserted by `cpt-cf-oagw-dod-gear-foundation-merge-engine` in entry 2.1, which owns the merge engine this surface stores data for (DoD `cpt-cf-oagw-dod-upstream-management-tags`, asserted in `tests/upstream_validation.rs`).
- [x] Replacing a record with `enabled: false` makes the upstream unavailable to the proxy path, a proxy request for its alias then yields `503`, and re-enabling by the owning tenant restores service (DoD `cpt-cf-oagw-dod-upstream-management-enable-disable`).
- [x] An upstream disabled by an ancestor tenant presents as disabled to every descendant, and no descendant operation through the management API lifts that disablement (DoD `cpt-cf-oagw-dod-upstream-management-enable-disable`).
- [x] A `rate_limit` without `sustained.rate`, a `cors` without `enabled`, or a `cors` combining `allow_credentials: true` with a wildcard origin is rejected, and the sharing-mode values are limited to `private`, `inherit`, and `enforce` on every sub-configuration that carries one (DoD `cpt-cf-oagw-dod-upstream-management-nested-subconfig`).
- [x] `plugins.items` accepts builtin GTS identifiers and custom UUID references with contiguous positions from zero, and a gap or a duplicate position is rejected (DoD `cpt-cf-oagw-dod-upstream-management-plugin-references`).
- [x] `GET /oagw/v1/upstreams` honors `$filter`, `$select`, `$orderby`, `$skip`, and `$top` with a default of 50 and a cap of 100, and an out-of-range `$top` or an unparseable `$filter` returns a validation error naming the parameter (DoD `cpt-cf-oagw-dod-upstream-management-odata-list`).
- [x] A read, replace, or delete of a record owned by another tenant, including an ancestor, returns not-found and discloses nothing about the foreign record (DoD `cpt-cf-oagw-dod-upstream-management-tenant-scoping`).
- [x] A request carrying an unknown property, an empty endpoint pool, a port outside 1-65535, or a malformed host is rejected with a validation error and stores nothing (DoD `cpt-cf-oagw-dod-upstream-management-schema-shapes`).
- [x] Deleting an upstream removes its dependent route, tag, and plugin binding rows atomically, and a subsequent read of the identifier returns not-found (DoD `cpt-cf-oagw-dod-upstream-management-crud-endpoints`).
- [x] All tests for this feature live inside the `oagw` crate as sibling `*_tests.rs` modules and files under `tests/`, and no `testing/e2e/gears/oagw/` directory is created (DoD `cpt-cf-oagw-dod-upstream-management-unit-tests`, `cpt-cf-oagw-dod-upstream-management-integration-tests`).
- [x] A request with no bearer token, an invalid one, or no resolvable security context is rejected with `401` and `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1` before payload validation, and a request whose valid context lacks the operation's permission is rejected with `403` through the shared canonical permission-denied surface, for each of the five operations (DoD `cpt-cf-oagw-dod-upstream-management-crud-endpoints`, asserted in `tests/upstream_crud.rs`).
- [x] `POST /oagw/v1/upstreams` returns `201 Created` with the stored record, `GET /oagw/v1/upstreams/{id}` returns `200` with the stored record, `GET /oagw/v1/upstreams` returns `200` with the projected list, `PUT /oagw/v1/upstreams/{id}` returns `200` with the replaced record, and `DELETE /oagw/v1/upstreams/{id}` returns `204 No Content` with an empty body, while each mutating operation returns `409 Conflict` when its uniqueness precondition fails (DoD `cpt-cf-oagw-dod-upstream-management-crud-endpoints`, asserted in `tests/upstream_crud.rs`).
- [x] A create whose alias matches an ancestor tenant's upstream is a bind: it is accepted when the caller holds `oagw:upstream:bind`, rejected with `403` through the canonical permission-denied surface when that permission is absent, and an ancestor upstream carrying `sharing: private` blocks visibility so the alias stays available for a local create (DoD `cpt-cf-oagw-dod-upstream-management-alias-normalization`, asserted in `tests/upstream_alias.rs`).
- [x] Two concurrent creates of the same `(tenant_id, alias)` leave exactly one stored record and return exactly one `409 Conflict` (DoD `cpt-cf-oagw-dod-upstream-management-alias-normalization`, asserted in `tests/upstream_alias.rs`).
- [x] A header rule whose name carries a carriage return, a line feed, or a NUL byte, whose name violates RFC 7230 field-name grammar, or whose name or value exceeds 4096 bytes is rejected with a `400` validation error naming `headers.request` or `headers.response` and the offending key (DoD `cpt-cf-oagw-dod-upstream-management-header-transform-config`, asserted in `src/domain/validation_tests.rs`).
- [x] A create that omits an `auth`, `headers`, `rate_limit`, `cors`, or `plugins` block persists no such block, and an ancestor record with an omitted block contributes nothing to a descendant's effective configuration (DoD `cpt-cf-oagw-dod-upstream-management-schema-shapes`, asserted in `src/domain/services/management_tests.rs`).
- [x] A descendant record resolves as `disabled-by-ancestor` only while the closest resolvable ancestor record for the same alias is `disabled`, that state is never stored by a management write, and re-enabling or removing the ancestor record returns the descendant to its own stored state (DoD `cpt-cf-oagw-dod-upstream-management-enable-disable`, asserted in `tests/upstream_enable_disable.rs`).
- [x] Setting `enabled` on `PUT /oagw/v1/upstreams/{id}` with a body that also violates an endpoint or sub-configuration shape is rejected with a validation error and changes nothing, and no dedicated enable or disable endpoint exists on the route tree (DoD `cpt-cf-oagw-dod-upstream-management-enable-disable`, asserted in `tests/upstream_enable_disable.rs`).
- [x] The `auth.config` member of a `GET /oagw/v1/upstreams/{id}` response body holds only `cred://` reference values, and no management response, log line, or error body carries resolved secret material (DoD `cpt-cf-oagw-dod-upstream-management-auth-config`, asserted in `tests/upstream_crud.rs`).
- [x] After a replacement, the record served for the alias carries the new endpoint and not the old one, following the write's Control Plane L1 invalidation and Data Plane hot-config flush ordering (DoD `cpt-cf-oagw-dod-upstream-management-integration-tests`, asserted in `tests/upstream_alias.rs`).
