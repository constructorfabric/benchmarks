# Feature: Plugin Execution Chain and Built-in Plugins


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Proxied Request Whose Upstream Requires Injected Credentials](#proxied-request-whose-upstream-requires-injected-credentials)
  - [Request Rejected by a Guard Plugin](#request-rejected-by-a-guard-plugin)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Built-in Plugin Registry Initialization](#built-in-plugin-registry-initialization)
  - [Plugin Binding Resolution](#plugin-binding-resolution)
  - [Chain Assembly and Ordering](#chain-assembly-and-ordering)
  - [Chain Execution and Short-Circuit Mapping](#chain-execution-and-short-circuit-mapping)
  - [Auth Plugin Invocation and Credential Injection](#auth-plugin-invocation-and-credential-injection)
  - [Credential Resolution via the Credential Store](#credential-resolution-via-the-credential-store)
  - [Client-Credentials Token Acquisition and Caching](#client-credentials-token-acquisition-and-caching)
  - [Guard Evaluation per Phase](#guard-evaluation-per-phase)
  - [Transform Application per Phase](#transform-application-per-phase)
- [4. States (CDSL)](#4-states-cdsl)
  - [Cached Client-Credentials Token State Machine](#cached-client-credentials-token-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Deterministic Chain Execution Order](#deterministic-chain-execution-order)
  - [Upstream-Before-Route Chain Concatenation](#upstream-before-route-chain-concatenation)
  - [Plugin Binding Resolution and the Unresolvable-Identifier Contract](#plugin-binding-resolution-and-the-unresolvable-identifier-contract)
  - [Built-in Plugin Registries](#built-in-plugin-registries)
  - [Catalog-Only Identifiers Must Fail to Bind](#catalog-only-identifiers-must-fail-to-bind)
  - [Guard Rejection Short-Circuit and Response Mapping](#guard-rejection-short-circuit-and-response-mapping)
  - [Required-Headers Guard Behaviour](#required-headers-guard-behaviour)
  - [Outbound Credential Injection from References](#outbound-credential-injection-from-references)
  - [Secret Non-Disclosure in Logs, Errors and Audit Records](#secret-non-disclosure-in-logs-errors-and-audit-records)
  - [Client-Credentials Auth Plugin Variants and Configuration Validation](#client-credentials-auth-plugin-variants-and-configuration-validation)
  - [Token Cache Semantics](#token-cache-semantics)
  - [Retry on Upstream 401 Stays Deferred](#retry-on-upstream-401-stays-deferred)
  - [Request-ID Transform Plugin](#request-id-transform-plugin)
  - [GTS Type-Registration Touchpoints for Built-in Plugin Configuration](#gts-type-registration-touchpoints-for-built-in-plugin-configuration)
  - [No Custom Starlark Plugin Execution](#no-custom-starlark-plugin-execution)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p2` - **ID**: `cpt-cf-oagw-featstatus-plugin-execution-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-plugin-execution`
## 1. Feature Context

### 1.1 Overview

This feature executes the deterministic Auth -> Guards -> Transform(`on_request`) -> upstream call -> Transform(`on_response`/`on_error`) plugin chain inside the proxy request path owned by `cpt-cf-oagw-feature-proxy-core`, stands up the in-process registries of the genuinely registry-resolvable built-in plugins (auth `noop`/`apikey`/`oauth2_client_cred`/`oauth2_client_cred_basic`, guard `required_headers`, transform `request_id`), and injects outbound credentials that are resolved from `cred_store` through `secret_ref`/`cred://` references rather than stored inside OAGW.

### 1.2 Purpose

Without this feature the proxy path forwards requests verbatim: an upstream that requires an API key or an OAuth2 client-credentials bearer token cannot be reached, and no operator-configured guard or transform has any effect. This feature turns the plugin bindings that `cpt-cf-oagw-feature-upstream-management`, `cpt-cf-oagw-feature-route-management` and `cpt-cf-oagw-feature-plugin-management` persist and identify into executed behaviour on a live request, and is the only feature in this decomposition round that handles secret material.

**Requirements Covered**:

- [ ] `p2` - `cpt-cf-oagw-fr-plugin-system`
- [ ] `p2` - `cpt-cf-oagw-fr-builtin-plugins`
- [ ] `p2` - `cpt-cf-oagw-fr-auth-injection`
- [ ] `p2` - `cpt-cf-oagw-nfr-credential-isolation`
- [ ] `p2` - `cpt-cf-oagw-contract-cred-store`
- [ ] `p2` - `cpt-cf-oagw-contract-types-registry`

Citation priority note: `cpt-cf-oagw-fr-auth-injection`, `cpt-cf-oagw-nfr-credential-isolation`, `cpt-cf-oagw-contract-cred-store`, and `cpt-cf-oagw-contract-types-registry` are cited above at `p2`, following `DECOMPOSITION.md` §2.9's own markers for these citations, even though `PRD.md` defines each identifier itself at `p1`. This file follows the DECOMPOSITION round's markers rather than the PRD's original priorities; the drift originates upstream and is not corrected here.

**Design Principles Covered**:

- `cpt-cf-oagw-principle-cred-isolation`

**Design Constraints Covered**: none. `DECOMPOSITION.md` §2.9 lists no design constraint for this entry; the constraints that bear on the surrounding request (`cpt-cf-oagw-constraint-body-limit`, `cpt-cf-oagw-constraint-no-direct-internet`, `cpt-cf-oagw-constraint-https-only`) are owned and enforced by `cpt-cf-oagw-feature-proxy-core`.

**Design Components**:

- `cpt-cf-oagw-component-model`
- `cpt-cf-oagw-adr-plugin-system`
- `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`
- `cpt-cf-oagw-adr-required-headers-guard-plugin`

**Sequences**:

- `cpt-cf-oagw-seq-proxy-flow`

**Milestones** (per `DECOMPOSITION.md` §2.9 Phases):

1. **Chain wiring** — execution order, chain concatenation, binding resolution, and credential resolution via `cred_store` (§3 `cpt-cf-oagw-algo-plugin-registry-init`, `-binding-resolve`, `-chain-assemble`, `-chain-execute`, `-cred-resolve`).
2. **Built-in auth plugins** — `noop`, `apikey`, both client-credentials variants, and the reserved `basic`/`bearer` identifiers (§3 `cpt-cf-oagw-algo-plugin-auth-invoke`, `-token-acquire`; §4 `cpt-cf-oagw-state-plugin-token-cache-entry`).
3. **Built-in guard/transform plugins** — `required_headers`, `request_id`, and the catalog-only `timeout`/`cors`/`logging`/`metrics` identifiers (§3 `cpt-cf-oagw-algo-plugin-guard-evaluate`, `-transform-apply`).

**Two overrides carried into this feature, superseding what `PRD.md`/`DESIGN.md` tabulate** (per `DECOMPOSITION.md` §1 Overview corrections 1 and 2):

1. The proxy path this chain runs inside is gear-relative: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]`, with no leading `/api` segment contributed by the gear. Every path in this feature uses that form.
2. `http` is a legal endpoint scheme under the graded configuration (`allow_http_upstream: true`); `cpt-cf-oagw-constraint-https-only` is the default posture that the flag lifts. The consequence this feature must state, and must not silently "fix": when an upstream endpoint is plaintext `http`, the credential this feature injects (API key header/query value, or `Authorization: Bearer <token>`) travels to that upstream in cleartext on the wire. This feature adds no restriction the documents do not impose — it neither refuses the injection nor downgrades the credential — and connection-scheme gating remains `cpt-cf-oagw-feature-proxy-core`'s concern.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Sends the proxied request whose credentials are injected transparently and whose guard rejections are reported back as gateway errors. |
| `cpt-cf-oagw-actor-cred-store` | Resolves `cred://` / `secret_ref` references to secret material, and decides tenant accessibility (own secret or ancestor-shared) before returning anything. |
| `cpt-cf-oagw-actor-types-registry` | Holds the GTS type/schema registrations for the built-in plugin identifiers and their `ctx.config` schemas that this feature's plugins rely on. |
| `cpt-cf-oagw-actor-upstream-service` | Receives the credentialed, guarded and transformed request; its response is subject to response-phase guards and transforms before reaching the caller. |
| `cpt-cf-oagw-actor-tenant-admin` | Binds plugins with their `config` objects onto an upstream/route and thereby determines which chain this feature assembles and executes. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **ADRs**: [ADR/0002-plugin-system.md](../ADR/0002-plugin-system.md), [ADR/0008-oauth2-client-credentials-auth-plugin.md](../ADR/0008-oauth2-client-credentials-auth-plugin.md), [ADR/0009-required-headers-guard-plugin.md](../ADR/0009-required-headers-guard-plugin.md)
- **Dependencies**: `cpt-cf-oagw-feature-plugin-management` (plugin identification model: `plugin_ref` parsing, UUID-backed vs. named classification) and `cpt-cf-oagw-feature-proxy-core` (the resolved request path, merged configuration, forwarding mechanics, and — via `cpt-cf-oagw-feature-gear-foundation` — the RFC 9457 error envelope and `X-OAGW-Error-Source: gateway` header this feature's rejections are rendered through); see `DECOMPOSITION.md` §3 Feature Dependencies

## 2. Actor Flows (CDSL)

Both flows below are the same proxy request from the application developer's point of view — `cpt-cf-oagw-usecase-proxy-request` step 6 ("System executes plugin chain (Auth -> Guard -> Transform)") — seen once on the success path with injected credentials and once on the guard-rejection path. Resolution, forwarding and response passthrough steps belong to `cpt-cf-oagw-feature-proxy-core` and appear here only as the surrounding context the chain runs inside.

**Use cases** — context, not re-claimed coverage: `DECOMPOSITION.md` assigns both identifiers below to entry 2.5 (`cpt-cf-oagw-feature-proxy-core`), and entry 2.9's Requirements Covered list does not include them. They are cited here only because the two flows are the plugin-chain segment of that same use case and endpoint:

- [ ] `p1` - `cpt-cf-oagw-usecase-proxy-request`
- [ ] `p1` - `cpt-cf-oagw-interface-proxy-api`

### Proxied Request Whose Upstream Requires Injected Credentials

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-credentialed-proxy`

**RF-013 note**: every step below now runs for real (RF-001) except `inst-flow-cred-proxy-16`'s plugin-identifier audit detail -- see that step's own note. This parent stays unchecked, per `cfs validate`'s parent/child consistency rule, until that one step closes.

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The developer calls `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` against an upstream whose `auth.type` is `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1`; on the first such request the plugin resolves `client_id_ref`/`client_secret_ref` through `cred_store`, exchanges them at the token endpoint, caches the token, injects `Authorization: Bearer <token>`, and the upstream answers `200`.
- A second request for the same `(subject_tenant_id, subject_id, auth_method, config_hash)` tuple is served from the token cache with no `cred_store` lookup and no token-endpoint call.
- An upstream whose `auth.type` is `...cf.core.oagw.apikey.v1` has the referenced key injected as a request header or query parameter; an upstream whose `auth.type` is `...cf.core.oagw.noop.v1` is called with no credential added and no error.

**Error Scenarios**:
- The upstream's `auth.type` is `...cf.core.oagw.basic.v1` or `...cf.core.oagw.bearer.v1` (catalog-only, no backing implementation), or any other identifier absent from the auth registry: resolution fails with an "unknown auth plugin" failure reported as `PluginNotFound` -> `503` / `gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1`; no upstream call is made.
- The referenced secret does not exist or is not accessible to the calling tenant: `cred_store` returns a failure and the request is answered `401` / `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1` (`AuthenticationFailed`), with no secret material or reference value in the response body.
- The token endpoint (or OIDC discovery) is unreachable or rejects the client credentials: the auth plugin fails and the request is answered `401 AuthenticationFailed` per `cpt-cf-oagw-usecase-proxy-request`'s "Auth plugin fails" alternative flow; the failed fetch is not cached.
- The upstream answers `401` because the credential was refused: the response is passed through unchanged with `X-OAGW-Error-Source: upstream` by `cpt-cf-oagw-feature-proxy-core`. This feature performs no retry and no cache eviction — retry-on-401 is deferred by `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`.

**Steps**:
1. [x] - `p1` - Developer sends `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]` with the tenant/principal security context established by the host - `inst-flow-cred-proxy-01`
2. [x] - `p1` - API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` handed to the Data Plane, which resolves the upstream, the route and the merged effective configuration (owned by `cpt-cf-oagw-feature-proxy-core`; this feature consumes its output: the single `auth` binding plus the concatenated guard/transform bindings) - `inst-flow-cred-proxy-02`
3. [x] - `p1` - Assemble the execution chain from the resolved bindings via `cpt-cf-oagw-algo-plugin-chain-assemble` - `inst-flow-cred-proxy-03`
4. [x] - `p1` - Invoke the resolved auth plugin exactly once, before any guard, via `cpt-cf-oagw-algo-plugin-auth-invoke` - `inst-flow-cred-proxy-04`
5. [x] - `p1` - **IF** the auth binding cannot be resolved to a registered implementation - `inst-flow-cred-proxy-05`
   1. [x] - `p1` - **RETURN** `503` `PluginNotFound` (`gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1`) with `X-OAGW-Error-Source: gateway`, naming the unresolvable plugin identifier but no configuration values, and make no upstream call - `inst-flow-cred-proxy-06`
6. [x] - `p1` - **IF** the auth plugin fails (secret inaccessible or absent, token exchange failed) - `inst-flow-cred-proxy-07`
   1. [x] - `p1` - **RETURN** `401` `AuthenticationFailed` (`gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`) with `X-OAGW-Error-Source: gateway` and a `detail` that names neither the secret material nor the resolved credential, and make no upstream call - `inst-flow-cred-proxy-08`
7. [x] - `p1` - Run every guard in the assembled guard chain in order for the request phase via `cpt-cf-oagw-algo-plugin-guard-evaluate`; a rejection short-circuits into `cpt-cf-oagw-flow-plugin-guard-rejection` - `inst-flow-cred-proxy-09`
8. [x] - `p1` - Run every transform in the assembled transform chain in order for the `on_request` phase via `cpt-cf-oagw-algo-plugin-transform-apply` - `inst-flow-cred-proxy-10`
9. [x] - `p1` - Forward the credentialed, guarded, transformed request to `cpt-cf-oagw-actor-upstream-service` (forwarding mechanics owned by `cpt-cf-oagw-feature-proxy-core`) - `inst-flow-cred-proxy-11`
10. [x] - `p1` - **IF** an upstream response was received - `inst-flow-cred-proxy-12`
    1. [x] - `p1` - Run the response-phase guards, then the `on_response` transforms, over that response - `inst-flow-cred-proxy-13`
11. [x] - `p1` - **ELSE** (the upstream call itself failed: connection, protocol or timeout error raised by `cpt-cf-oagw-feature-proxy-core`) - `inst-flow-cred-proxy-14`
    1. [x] - `p1` - Run the `on_error` transforms over the error context, leaving the error's status and GTS `type` as raised - `inst-flow-cred-proxy-15`
12. [ ] - `p1` - Emit the audit record and metrics for the request through `cpt-cf-oagw-feature-gear-foundation`'s scaffold, recording which plugin identifiers ran and, on failure, which plugin rejected — never any credential reference value or secret material - `inst-flow-cred-proxy-16`
    - **Remains unchecked** (RF-013): the audit record/metrics `cpt-cf-oagw-feature-gear-foundation` emits per request (`crate::proxy::observe::observe_completion`) does not carry which plugin identifiers ran or which one rejected -- only host/path/method/status/duration/error-type. Every other step of this flow now runs for real (RF-001); this one step's plugin-identifier audit detail was never in scope for that fix and remains a separate, open gap.
13. [x] - `p1` - **RETURN** the upstream's response to the developer unchanged except for the transforms applied in step 10.1 - `inst-flow-cred-proxy-17`

### Request Rejected by a Guard Plugin

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-guard-rejection`

**Remains unchecked (RF-001/RF-013)**: the guard *evaluation* machinery this flow describes now genuinely runs on every real request (`cpt-cf-oagw-algo-plugin-guard-evaluate`, wired for real via `crate::plugins::execute`) -- no longer the dead code the narrow `plugins::chain` adapters left it as. What still cannot be exercised through the real management API is a *rejecting* `required_headers` binding: the frozen `upstream.v1.schema.json`/`route.v1.schema.json` declare `plugins.items[]` as bare identifier strings only (`oneOf: gts-identifier | uuid`), with no per-item `config` slot for `required_request_headers`/`required_response_headers` -- unlike `auth.config`, which the schema does carry inline. ADR-0009's own "Upstream Configuration Example" documents an object-shaped `items[]` entry (`{"plugin_ref": ..., "config": {...}}`) that contradicts the frozen schema. Closing this needs a wire-format change to `src/model/{upstream,route}.rs` (a sibling entry's file ownership) and, most likely, to the frozen schema files themselves (a "never edit" hard constraint). Until then, a `required_headers` guard bound through a real Upstream/Route always observes an absent `config` and fails open by design (`cpt-cf-oagw-algo-plugin-guard-evaluate`'s own documented fail-open-on-absent-key behaviour) -- this flow's *reject* scenarios are proven only through the direct, config-bearing unit tests inline in `src/plugins/{guard,execute}.rs`, not end to end.

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The upstream binds `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1` with `required_request_headers: "x-correlation-id,accept"`; the developer's request carries both (in any letter case) and is forwarded normally.
- The same upstream binds no `required_response_headers` value; the response phase of that same plugin instance is a no-op and the upstream's response is returned untouched (independent fail-open per phase).

**Error Scenarios**:
- The request omits `x-correlation-id`: the guard rejects with `400` and error code `REQUIRED_HEADER_MISSING`, naming only `x-correlation-id` (the first missing header) even though later names in the list may also be missing; no upstream call is made and no `on_request` transform runs.
- The upstream's response omits `content-type` while `required_response_headers: "content-type"` is configured: the guard rejects on the response phase with `502` and error code `REQUIRED_HEADER_MISSING`; the upstream's body is not returned to the caller.
- A bound guard identifier is `...cf.core.oagw.timeout.v1` or `...cf.core.oagw.cors.v1` (catalog-only): the binding fails to resolve and the request is answered `503 PluginNotFound` rather than being forwarded with that guard silently skipped.

**Steps**:
1. [ ] - `p1` - Developer sends `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` omitting a header the upstream's (or route's) `required_headers` binding requires - `inst-flow-guard-reject-01`
2. [ ] - `p1` - The chain reaches the guard phase with the auth plugin already executed (per `cpt-cf-oagw-flow-plugin-credentialed-proxy` steps 4-7) - `inst-flow-guard-reject-02`
3. [ ] - `p1` - **FOR EACH** guard in the assembled guard chain, in chain order (upstream-bound guards first, then route-bound guards) - `inst-flow-guard-reject-03`
   1. [ ] - `p1` - Evaluate its request phase via `cpt-cf-oagw-algo-plugin-guard-evaluate` - `inst-flow-guard-reject-04`
   2. [ ] - `p1` - **IF** the guard returns a rejection - `inst-flow-guard-reject-05`
      1. [ ] - `p1` - Stop the chain immediately: no later guard runs, no `on_request` transform runs, and no upstream connection is opened - `inst-flow-guard-reject-06`
      2. [ ] - `p1` - Map the rejection's status and error code onto a gateway-originated response through `cpt-cf-oagw-feature-gear-foundation`'s RFC 9457 envelope, with `X-OAGW-Error-Source: gateway` and a `detail` naming the single first-missing header - `inst-flow-guard-reject-07`
      3. [ ] - `p1` - **RETURN** that response to the developer (`400` + `REQUIRED_HEADER_MISSING` for a request-phase rejection) - `inst-flow-guard-reject-08`
4. [ ] - `p1` - **IF** no guard rejected the request - `inst-flow-guard-reject-09`
   1. [ ] - `p1` - Continue at `cpt-cf-oagw-flow-plugin-credentialed-proxy` step 8 (`on_request` transforms, then the upstream call) - `inst-flow-guard-reject-10`
5. [ ] - `p1` - On the returning path, evaluate each guard's response phase in the same chain order; a response-phase rejection short-circuits the return path and yields `502` + `REQUIRED_HEADER_MISSING` instead of the upstream's response, again with `X-OAGW-Error-Source: gateway` - `inst-flow-guard-reject-11`
6. [ ] - `p1` - Record the rejecting plugin identifier, the phase, and the offending header name in the audit record - `inst-flow-guard-reject-12`

## 3. Processes / Business Logic (CDSL)

### Built-in Plugin Registry Initialization

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-registry-init`

**Input**: the gear's resolved configuration (specifically the token-cache settings `token_cache_ttl_secs` and `token_cache_capacity`), the `cred_store` client handle, and the optional HTTP client configuration used for token exchange.

**Output**: three in-process registries — auth, guard, transform — each mapping a registry-resolvable GTS plugin identifier to exactly one implementation, constructed once at Data Plane initialization and immutable thereafter.

The registries are the authoritative answer to "does this identifier have a backing implementation?". Per `cpt-cf-oagw-adr-plugin-system`, only the following identifiers are inserted:

| Registry | Registry-resolvable identifier |
|---|---|
| Auth | `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1` |
| Auth | `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1` |
| Auth | `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1` (`Form` client auth) |
| Auth | `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1` (`Basic` client auth) |
| Guard | `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1` |
| Transform | `gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1` |

The following identifiers are **catalog-only** and MUST NOT be inserted into any registry, because they have no backing trait implementation (`basic`, `bearer`) or are core Data Plane functionality implemented outside the plugin traits (`timeout`, `cors` — core policy/`Upstream.cors`; `logging`, `metrics` — core instrumentation):

| Registry that must NOT contain it | Catalog-only identifier | Why (per `cpt-cf-oagw-adr-plugin-system`) |
|---|---|---|
| Auth | `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1` | Reserved GTS identifier cataloged in the types registry with no backing `AuthPlugin` implementation |
| Auth | `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1` | Reserved GTS identifier cataloged in the types registry with no backing `AuthPlugin` implementation |
| Guard | `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1` | Request timeout is core Data Plane logic (gear-level configuration), not a guard trait implementation |
| Guard | `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1` | CORS is core Data Plane logic configured via the `cors` field, per `cpt-cf-oagw-adr-cors` |
| Transform | `gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1` | Logging is core Data Plane instrumentation, not a transform trait implementation |
| Transform | `gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1` | Prometheus metrics collection is core Data Plane instrumentation, not a transform trait implementation |

Silently accepting a catalog-only identifier — by inserting a no-op stand-in, or by skipping the plugin at execution time — would let an operator deploy a configuration that appears to authenticate or guard traffic and does not. That is the specific failure this algorithm exists to prevent.

Note on an internal contradiction in `cpt-cf-oagw-adr-plugin-system`: that ADR's own illustrative plugin-loading code snippet registers a `basic` auth plugin (`auth_plugins.insert("basic".into(), Arc::new(BasicAuthPlugin))`), which contradicts its normative prose stating `basic`/`bearer` are "reserved GTS identifiers cataloged in the types-registry with no backing `AuthPlugin` implementation." This algorithm follows the normative prose, corroborated by `DESIGN.md` and `PRD.md`, not the illustrative snippet: `basic` and `bearer` are catalog-only and MUST NOT be inserted into any registry. A future maintainer who instead follows the snippet would reintroduce the no-op stand-in this algorithm exists to prevent.

**Steps**:
1. [x] - `p1` - Construct the `noop` and `apikey` auth plugin implementations and insert them under their GTS identifiers - `inst-registry-init-01`
2. [x] - `p1` - Construct the client-credentials auth plugin twice — once with `Form` client auth, once with `Basic` — passing both the same token-cache time-to-live and capacity, and insert each under its own GTS identifier - `inst-registry-init-02`
3. [x] - `p1` - Construct the stateless `required_headers` guard implementation and insert it as the only entry of the guard registry - `inst-registry-init-03`
4. [x] - `p1` - Construct the `request_id` transform implementation and insert it as the only entry of the transform registry - `inst-registry-init-04`
5. [x] - `p1` - **FOR EACH** catalog-only identifier in the table above - `inst-registry-init-05`
   1. [x] - `p1` - Assert it is absent from every registry (no no-op stand-in, no alias to another implementation) - `inst-registry-init-06`
6. [x] - `p1` - **RETURN** the three immutable registries, shared by every request handled by this Data Plane instance - `inst-registry-init-07`

### Plugin Binding Resolution

- [x] `p1` - **ID**: `cpt-cf-oagw-algo-plugin-binding-resolve`

**Input**: one binding from the merged effective configuration — its `plugin_ref` (canonical GTS identifier string), its nullable `plugin_uuid`, its `config` object, and the plugin kind expected at this position (auth, guard or transform).

**Output**: either a resolved implementation paired with the binding's `config`, or a resolution failure carrying the offending identifier.

Identifier parsing and the UUID-backed-vs-named classification are `cpt-cf-oagw-feature-plugin-management`'s model and are reused unchanged here; this algorithm only turns a classified reference into something executable.

**Steps**:
1. [x] - `p1` - Parse `plugin_ref` and confirm its base type matches the expected kind at this position (an `auth_plugin` identifier bound at a guard position, or vice versa, is a resolution failure) - `inst-binding-resolve-01`
2. [x] - `p1` - **IF** the identifier's post-`~` instance part is a UUID (a UUID-backed custom plugin) - `inst-binding-resolve-02`
   1. [x] - `p1` - Report a resolution failure without fetching or evaluating the stored plugin source: whether or not the record exists in storage, no execution engine for custom Starlark plugin source exists in this decomposition round (see §5 `cpt-cf-oagw-dod-plugin-no-custom-execution`), so the outcome is the same and such a binding MUST fail closed rather than be skipped or partially applied - `inst-binding-resolve-03`
   2. [x] - `p1` - Record the binding's identifier and this reason in the audit record so an operator can tell a custom-plugin binding apart from a typo'd named identifier - `inst-binding-resolve-04`
3. [x] - `p1` - **ELSE** (a named identifier) look the identifier up in the registry for the expected kind, built by `cpt-cf-oagw-algo-plugin-registry-init` - `inst-binding-resolve-05`
   1. [x] - `p1` - **IF** the registry has no entry for it (any catalog-only identifier, any identifier of a gear that is not deployed, any typo) - `inst-binding-resolve-06`
      1. [x] - `p1` - Report a resolution failure whose message states the kind-specific "unknown auth plugin" / "unknown guard plugin" / "unknown transform plugin" condition and echoes only the identifier - `inst-binding-resolve-07`
4. [x] - `p1` - **RETURN** on failure: `PluginNotFound` -> HTTP `503`, GTS `type` `gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1`, rendered through `cpt-cf-oagw-algo-error-render` with `X-OAGW-Error-Source: gateway`; the request is never forwarded with an unresolved binding omitted - `inst-binding-resolve-08`
5. [x] - `p1` - **RETURN** on success: the resolved implementation plus the binding's `config` object, ready to be placed at its position in the assembled chain - `inst-binding-resolve-09`

### Chain Assembly and Ordering

- [x] `p1` - **ID**: `cpt-cf-oagw-algo-plugin-chain-assemble`

**Input**: the merged effective configuration produced by `cpt-cf-oagw-feature-proxy-core` for this request — the single effective `auth` binding (`auth_plugin_ref`/`auth_plugin_uuid` plus its `config`), the ordered upstream-bound `plugins.items[]` list, and the ordered route-bound `plugins.items[]` list.

**Output**: an ordered execution plan: exactly zero-or-one resolved auth plugin, one ordered guard list, and one ordered transform list; or the first resolution failure encountered.

Concatenation is upstream-before-route and stable: `[U1, U2] + [R1, R2] => [U1, U2, R1, R2]`, matching the additive plugin merge strategy in `DESIGN.md`'s hierarchical-configuration table (`ancestor.plugins + descendant.plugins`). Relative order **within** each source list is the binding `position` order and is never reordered, deduplicated, or sorted by kind: the same identifier bound twice runs twice, once per binding, with its own `config`.

**Steps**:
1. [x] - `p1` - Concatenate the upstream-bound binding list with the route-bound binding list, upstream entries first, preserving each list's internal `position` order - `inst-chain-assemble-01`
2. [x] - `p1` - **FOR EACH** binding in the concatenated list, in order - `inst-chain-assemble-02`
   1. [x] - `p1` - Resolve it via `cpt-cf-oagw-algo-plugin-binding-resolve`, using the binding's base type to determine its expected kind - `inst-chain-assemble-03`
   2. [x] - `p1` - **IF** resolution fails - `inst-chain-assemble-04`
      1. [x] - `p1` - Abandon assembly and propagate the `503 PluginNotFound` failure; the request is not forwarded - `inst-chain-assemble-05`
   3. [x] - `p1` - **ELSE** append the resolved entry to the guard list or the transform list according to its kind, preserving its index within the concatenated list - `inst-chain-assemble-06`
3. [x] - `p1` - **IF** the effective configuration carries an `auth` binding - `inst-chain-assemble-07`
   1. [x] - `p1` - Resolve it via `cpt-cf-oagw-algo-plugin-binding-resolve` at the auth position; there is at most one auth plugin per request, so no ordering question arises for this kind - `inst-chain-assemble-08`
4. [x] - `p1` - **ELSE** record that this request has no auth plugin, which is distinct from having the `noop` plugin bound and is equally valid: neither injects a credential, and neither is an error - `inst-chain-assemble-09`
5. [x] - `p1` - **RETURN** the execution plan (auth slot, ordered guard list, ordered transform list) - `inst-chain-assemble-10`

### Chain Execution and Short-Circuit Mapping

- [x] `p1` - **ID**: `cpt-cf-oagw-algo-plugin-chain-execute`

**Input**: the execution plan from `cpt-cf-oagw-algo-plugin-chain-assemble` and the mutable request context of the in-flight proxy request.

**Output**: either a request handed to `cpt-cf-oagw-feature-proxy-core` for forwarding and, later, a response handed back to the caller; or a single gateway-originated rejection response.

The order is fixed and is not configuration-dependent: auth, then guards, then `on_request` transforms, then the upstream call, then response-phase guards and `on_response` transforms — with `on_error` transforms on the failure path instead. No plugin may run twice in one phase, and no phase may be skipped for a plugin that declares it.

**Steps**:
1. [x] - `p1` - **IF** the plan has an auth plugin - `inst-chain-execute-01`
   1. [x] - `p1` - Invoke it exactly once via `cpt-cf-oagw-algo-plugin-auth-invoke`, before any guard - `inst-chain-execute-02`
   2. [x] - `p1` - **IF** it fails - `inst-chain-execute-03`
      1. [x] - `p1` - **RETURN** `401 AuthenticationFailed` and stop; no guard, transform or upstream call runs - `inst-chain-execute-04`
2. [x] - `p1` - **FOR EACH** guard in the plan's guard list, in order - `inst-chain-execute-05`
   1. [x] - `p1` - Evaluate its request phase via `cpt-cf-oagw-algo-plugin-guard-evaluate` - `inst-chain-execute-06`
   2. [x] - `p1` - **IF** the decision is a rejection - `inst-chain-execute-07`
      1. [x] - `p1` - **RETURN** the rejection's status and error code rendered through `cpt-cf-oagw-algo-error-render` with `X-OAGW-Error-Source: gateway`, and stop: remaining guards, all transforms, and the upstream call are skipped - `inst-chain-execute-08`
3. [x] - `p1` - **FOR EACH** transform in the plan's transform list that declares the `on_request` phase, in order - `inst-chain-execute-09`
   1. [x] - `p1` - Apply it via `cpt-cf-oagw-algo-plugin-transform-apply` - `inst-chain-execute-10`
4. [x] - `p1` - Hand the mutated request to `cpt-cf-oagw-feature-proxy-core` for the upstream call - `inst-chain-execute-11`
5. [x] - `p1` - **IF** the upstream call produced a response - `inst-chain-execute-12`
   1. [x] - `p1` - **FOR EACH** guard in the plan's guard list, in the same order, evaluate its response phase - `inst-chain-execute-13`
      1. [x] - `p1` - **IF** the decision is a rejection, **RETURN** its status and error code (the upstream's body is discarded and not forwarded) and stop - `inst-chain-execute-14`
   2. [x] - `p1` - **FOR EACH** transform declaring the `on_response` phase, in order, apply it to the response - `inst-chain-execute-15`
6. [x] - `p1` - **ELSE** (the upstream call failed) - `inst-chain-execute-16`
   1. [x] - `p1` - **FOR EACH** transform declaring the `on_error` phase, in order, apply it to the error context, without changing the error's status or GTS `type` - `inst-chain-execute-17`
7. [x] - `p1` - **RETURN** the response (or the error) to `cpt-cf-oagw-feature-proxy-core` for delivery to the caller - `inst-chain-execute-18`

### Auth Plugin Invocation and Credential Injection

- [x] `p1` - **ID**: `cpt-cf-oagw-algo-plugin-auth-invoke`

**Input**: the resolved auth plugin, its binding `config` object, and the mutable request context (headers, query, security context).

**Output**: the request context with a credential injected (or deliberately untouched, for `noop`), or an auth failure.

Dispatch is by resolved identifier: `noop` injects nothing; `apikey` injects the credential resolved from its configured reference into either a request header or a query parameter, as its binding `config` directs (`cpt-cf-oagw-fr-builtin-plugins` specifies header/query placement; the source documents enumerate `ctx.config` key names only for the client-credentials plugins in `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin` and for the guard in `cpt-cf-oagw-adr-required-headers-guard-plugin`, so no further `apikey` configuration surface is asserted by this feature); both client-credentials variants inject `Authorization: Bearer <token>` obtained through `cpt-cf-oagw-algo-plugin-token-acquire`.

**Steps**:
1. [x] - `p1` - **IF** the resolved plugin is `noop` - `inst-auth-invoke-01`
   1. [x] - `p1` - **RETURN** success with the request context unmodified (no header added, no `cred_store` call made) - `inst-auth-invoke-02`
2. [x] - `p1` - **IF** the resolved plugin is `apikey` - `inst-auth-invoke-03`
   1. [x] - `p1` - Resolve the configured credential reference via `cpt-cf-oagw-algo-plugin-cred-resolve` - `inst-auth-invoke-04`
   2. [x] - `p1` - Inject the resolved value into the configured request header, or the configured query parameter, replacing any inbound value at that position so that a caller cannot pre-seed or observe the credential slot - `inst-auth-invoke-05`
3. [x] - `p1` - **IF** the resolved plugin is either client-credentials variant - `inst-auth-invoke-06`
   1. [x] - `p1` - Obtain a bearer token via `cpt-cf-oagw-algo-plugin-token-acquire`, passing the variant's client-auth method (`Form` or `Basic`) - `inst-auth-invoke-07`
   2. [x] - `p1` - Set the `Authorization` request header to `Bearer <token>`, replacing any inbound `Authorization` value - `inst-auth-invoke-08`
4. [x] - `p1` - **CATCH** any failure from credential resolution or token acquisition - `inst-auth-invoke-09`
   1. [x] - `p1` - **RETURN** an auth failure mapped to `401 AuthenticationFailed`; the failure message, the audit record and every log line MUST reference at most the credential reference key, never the resolved secret or token - `inst-auth-invoke-10`
5. [x] - `p1` - **RETURN** success; the injected credential exists only in this request's outbound header/query and is never persisted by OAGW - `inst-auth-invoke-11`

### Credential Resolution via the Credential Store

- [x] `p1` - **ID**: `cpt-cf-oagw-algo-plugin-cred-resolve`

**Input**: one credential reference (`secret_ref`, e.g. `cred://partner-openai-key`, or the `client_id_ref`/`client_secret_ref` values of a client-credentials binding) and the request's security context.

**Output**: the secret material for the current request, held in a redacting, zero-on-drop wrapper; or an inaccessible/absent-secret failure.

Per `cpt-cf-oagw-principle-cred-isolation` and `cpt-cf-oagw-nfr-credential-isolation`, OAGW stores references only: it never persists secret material, and it never manages secret sharing — accessibility is `cpt-cf-oagw-actor-cred-store`'s decision. `DECOMPOSITION.md` §2.9 and `DESIGN.md`'s secret-access-control flow both specify the failure mapping used here: an inaccessible or missing secret becomes `401 AuthenticationFailed` on this path. (The error catalog's `SecretNotFound` -> `500` row is owned by `cpt-cf-oagw-dod-error-envelope` in `cpt-cf-oagw-feature-gear-foundation`; this feature does not emit it on the proxy plugin path, because doing so would report a tenant's configuration or authorization problem as a gateway fault.)

**Steps**:
1. [x] - `p1` - Read the reference string from the binding `config`; treat an absent or empty reference where the plugin requires one as a configuration failure mapped to `401 AuthenticationFailed` - `inst-cred-resolve-01`
2. [x] - `p1` - **TRY** resolve the reference through the `cred_store` client (`cpt-cf-oagw-contract-cred-store`), passing the request's security context so that `cred_store` can evaluate tenant accessibility (own secret, or ancestor-shared by policy) - `inst-cred-resolve-02`
3. [x] - `p1` - **CATCH** not-found, not-accessible, or transport failure from `cred_store` - `inst-cred-resolve-03`
   1. [x] - `p1` - Log the failure with the reference key and the failure kind only, and **RETURN** a failure mapped to `401` `AuthenticationFailed` (`gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`) - `inst-cred-resolve-04`
4. [x] - `p1` - Wrap the returned material in a redacting, zero-on-drop secret wrapper whose debug/display rendering never exposes the value, so that an accidental log or error interpolation cannot leak it - `inst-cred-resolve-05`
5. [x] - `p1` - **RETURN** the wrapped secret, scoped to this request (or, for a cached bearer token, to the cache entry described in `cpt-cf-oagw-state-plugin-token-cache-entry`); resolution results other than that cached token are not memoized, so every request re-checks accessibility with `cred_store` - `inst-cred-resolve-06`

### Client-Credentials Token Acquisition and Caching

- [x] `p1` - **ID**: `cpt-cf-oagw-algo-plugin-token-acquire`

**Input**: the binding `config` of a client-credentials auth plugin, the variant's client-auth method (`Form` or `Basic`), the request's security context, the gear-level token-cache settings, and the gear's configured `proxy_timeout_secs` (per `cpt-cf-oagw-feature-gear-foundation`), which bounds the outbound token-endpoint/discovery call below. No dedicated timeout configuration key is introduced for this call: the gear's configuration surface documents only `proxy_timeout_secs`, `allow_http_upstream`, and `ssrf_policy.enabled`, and this feature reuses the first of those rather than adding a fourth key.

**Output**: a bearer token for this `(subject_tenant_id, subject_id, auth_method, config_hash)` tuple; or a failure mapped to `401 AuthenticationFailed`.

Configuration surface, per `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`:

| `ctx.config` key | Required | Meaning |
|---|---|---|
| `token_endpoint` | Mutually exclusive with `issuer_url` | Direct token endpoint URL |
| `issuer_url` | Mutually exclusive with `token_endpoint` | OIDC issuer URL, token endpoint obtained by Discovery |
| `client_id_ref` | Yes | `cred://` reference for the `client_id` |
| `client_secret_ref` | Yes | `cred://` reference for the `client_secret` |
| `scopes` | No | Space-separated OAuth2 scopes |

Gear-level token-cache settings, per the same ADR: `token_cache_ttl_secs` (default `300`, i.e. five minutes) is a **ceiling**, and `token_cache_capacity` (default `10000`) bounds the number of entries. The effective time-to-live is `min(token_cache_ttl_secs, expires_in - 30s safety margin)` using the `expires_in` the identity provider reported; a token whose `expires_in` is at or below the 30-second margin is **not cached at all** and is used for the current request only. The ceiling is deliberately short because no cache-invalidation mechanism exists in this round: a revoked or rotated credential stays cached until its entry expires.

**Steps**:
1. [x] - `p1` - Parse the binding `config` into the keys tabulated above - `inst-token-acquire-01`
2. [x] - `p1` - **IF** both `token_endpoint` and `issuer_url` are present, or neither is - `inst-token-acquire-02`
   1. [x] - `p1` - **RETURN** a configuration failure mapped to `401 AuthenticationFailed` naming the violated exclusive-or; no `cred_store` lookup and no token-endpoint call are attempted - `inst-token-acquire-03`
3. [x] - `p1` - **IF** `client_id_ref` or `client_secret_ref` is missing - `inst-token-acquire-04`
   1. [x] - `p1` - **RETURN** a configuration failure mapped to `401 AuthenticationFailed` naming the missing key - `inst-token-acquire-05`
4. [x] - `p1` - Build the cache key as the composition of `subject_tenant_id` (cross-tenant isolation), `subject_id` (cross-subject isolation for the credential store's `private` sharing mode), a tag for the client-auth method (so the `Form` and `Basic` variants cannot share an entry even with byte-identical configuration), and a deterministic hash of the binding `config`'s sorted key/value pairs (so differing endpoints or `scopes` get distinct entries) - `inst-token-acquire-06`
5. [x] - `p1` - Look the key up in the token cache - `inst-token-acquire-07`
   1. [x] - `p1` - **IF** an entry is returned - `inst-token-acquire-08`
      1. [x] - `p1` - Verify the key stored inside the entry equals the lookup key; the cache hashes keys to a fixed-width value and does not compare keys for collision resolution, so this verification is the collision-safety boundary - `inst-token-acquire-09`
      2. [x] - `p1` - **IF** the stored key does not equal the lookup key, treat the lookup as a miss and continue; a hash collision MUST never yield another tenant's or subject's token - `inst-token-acquire-10`
      3. [x] - `p1` - **ELSE RETURN** the cached token without any `cred_store` lookup and without contacting the identity provider - `inst-token-acquire-11`
6. [x] - `p1` - On a miss, resolve `client_id_ref` and then `client_secret_ref` via `cpt-cf-oagw-algo-plugin-cred-resolve` (both are re-resolved on every miss; resolved client credentials are never cached) - `inst-token-acquire-12`
7. [x] - `p1` - **TRY** perform a single client-credentials token exchange against `token_endpoint` — or against the endpoint discovered from `issuer_url` — sending the client credentials in the request body for the `Form` variant or in an `Authorization` header for the `Basic` variant, with `scopes` when configured, and obtain the bearer value together with the reported `expires_in`; both the discovery call (when `issuer_url` is used) and the token-exchange call are bounded by the gear's configured `proxy_timeout_secs`, so an unresponsive identity provider cannot stall this step, and by extension the in-flight proxy request, past that bound - `inst-token-acquire-13`
8. [x] - `p1` - **CATCH** discovery failure, transport failure, a `proxy_timeout_secs` expiry, or a non-success token-endpoint response - `inst-token-acquire-14`
   1. [x] - `p1` - Do not write anything to the cache (failed fetches are never cached, so the next request for the same key retries the provider) and **RETURN** a failure mapped to `401 AuthenticationFailed` whose message carries no credential material and no raw provider response body - `inst-token-acquire-15`
9. [x] - `p1` - Compute the effective time-to-live as `min(token_cache_ttl_secs, expires_in - 30s)` - `inst-token-acquire-16`
10. [x] - `p1` - **IF** the effective time-to-live is not positive (`expires_in` at or below the 30-second safety margin) - `inst-token-acquire-17`
    1. [x] - `p1` - **RETURN** the token for this request only, writing no cache entry - `inst-token-acquire-18`
11. [x] - `p1` - **ELSE** store the token under the cache key together with a copy of that key (for the step 5.1.1 verification), in a zero-on-drop wrapper, with the computed effective time-to-live, in a cache bounded by `token_cache_capacity` - `inst-token-acquire-19`
12. [x] - `p1` - **RETURN** the token to `cpt-cf-oagw-algo-plugin-auth-invoke` for injection. Retry-on-401 against the upstream is explicitly deferred by `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin` and MUST NOT be added here: this algorithm neither observes the upstream's status nor evicts an entry in response to one - `inst-token-acquire-20`

### Guard Evaluation per Phase

- [x] `p1` - **ID**: `cpt-cf-oagw-algo-plugin-guard-evaluate`

**Input**: one resolved guard plugin, its binding `config`, the phase being evaluated (request or response), and the headers of the request (request phase) or of the upstream's response (response phase).

**Output**: an allow decision, or a rejection carrying an HTTP status and an error code.

For the only registry-resolvable guard, `required_headers`, the configuration surface is exactly two independent keys per `cpt-cf-oagw-adr-required-headers-guard-plugin`: `required_request_headers` (comma-separated names checked on the request phase) and `required_response_headers` (comma-separated names checked on the response phase). Neither is required. Matching is case-insensitive and checks **presence only** — a header present with an empty value satisfies the check, and header values are never inspected or compared. Each phase fails open independently: configuring one key does not enable the other phase, and a phase whose key is absent, empty, or blank after trimming every entry (for example `", , ,"`) is a no-op that allows.

**Steps**:
1. [x] - `p1` - Read the phase's own configuration key from the binding `config` (`required_request_headers` for the request phase, `required_response_headers` for the response phase) - `inst-guard-evaluate-01`
2. [x] - `p1` - **IF** that key is absent, or its value is empty, or every entry is blank after trimming - `inst-guard-evaluate-02`
   1. [x] - `p1` - **RETURN** allow (fail-open, unconfigured phase); the other phase's configuration is not consulted and is unaffected - `inst-guard-evaluate-03`
3. [x] - `p1` - Parse the value by splitting on `,`, trimming each entry, lowercasing it, and dropping empty entries, preserving the configured order of the surviving names - `inst-guard-evaluate-04`
4. [x] - `p1` - **FOR EACH** required name, in the configured order - `inst-guard-evaluate-05`
   1. [x] - `p1` - Test the phase's header set for a header whose name matches case-insensitively, without inspecting its value - `inst-guard-evaluate-06`
   2. [x] - `p1` - **IF** no such header is present - `inst-guard-evaluate-07`
      1. [x] - `p1` - **RETURN** a rejection immediately, naming only this first missing header and not scanning the remaining names: status `400` with error code `REQUIRED_HEADER_MISSING` on the request phase, status `502` with error code `REQUIRED_HEADER_MISSING` on the response phase - `inst-guard-evaluate-08`
5. [x] - `p1` - **RETURN** allow (every configured name was present) - `inst-guard-evaluate-09`

The rejection's status and error code are what this feature asserts; the surrounding RFC 9457 envelope, its GTS `type` field and the `X-OAGW-Error-Source: gateway` header are rendered by `cpt-cf-oagw-algo-error-render`, and this feature introduces no new error-catalog row.

### Transform Application per Phase

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-transform-apply`

**Input**: one resolved transform plugin, its binding `config`, the phase being applied (`on_request`, `on_response`, or `on_error`), and the mutable request, response or error context for that phase.

**Output**: the context, possibly mutated; transforms do not reject.

The only registry-resolvable transform, `request_id`, propagates the request correlation identifier: on `on_request` it adopts an inbound `X-Request-ID` when the caller supplied one and otherwise injects the correlation identifier assigned by `cpt-cf-oagw-algo-correlation-id`, and on `on_response` it sets the same value on the response so the caller can correlate. It declares no `on_error` phase.

**Steps**:
1. [x] - `p1` - **IF** the plugin does not declare the phase being applied - `inst-transform-apply-01`
   1. [x] - `p1` - **RETURN** the context untouched (a plugin bound without a given phase is skipped for that phase only, not for the whole request) - `inst-transform-apply-02`
2. [x] - `p1` - Apply the plugin's phase behaviour to the context in place, in the chain position assigned by `cpt-cf-oagw-algo-plugin-chain-assemble`, so that a later transform observes the mutations of every earlier one - `inst-transform-apply-03`
3. [x] - `p1` - **IF** the plugin is `request_id` and the phase is `on_request` - `inst-transform-apply-04`
   1. [x] - `p1` - Adopt the inbound `X-Request-ID` value when present; otherwise set the header to this request's correlation identifier - `inst-transform-apply-05`
4. [x] - `p1` - **IF** the plugin is `request_id` and the phase is `on_response` - `inst-transform-apply-06`
   1. [x] - `p1` - Set `X-Request-ID` on the response to the same value used on the request - `inst-transform-apply-07`
5. [x] - `p1` - **CATCH** a failure raised by the transform - `inst-transform-apply-08`
   1. [x] - `p1` - Abort the chain and surface the failure through the gateway error envelope rather than forwarding a half-transformed request or returning a half-transformed response - `inst-transform-apply-09`
6. [x] - `p1` - **RETURN** the mutated context to `cpt-cf-oagw-algo-plugin-chain-execute` - `inst-transform-apply-10`

## 4. States (CDSL)

Of this feature's domain-model entities, only the client-credentials token cache entry has a genuine lifecycle. The plugin execution chain is assembled per request and discarded with it (a value, not a stateful entity); the three registries are built once and immutable; a credential reference (`secret_ref`) is an immutable string owned by upstream/route configuration; the `required_headers` guard and the `request_id` transform are stateless by construction. No state machine is therefore defined for those.

### Cached Client-Credentials Token State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-plugin-token-cache-entry`

**States**: Absent, Fresh, ExpiringWithinMargin, Evicted

**Initial State**: Absent

The subject of this machine is one cache entry, identified by the composed key of `cpt-cf-oagw-algo-plugin-token-acquire` step 4. `Fresh` means the entry is present and within its effective time-to-live, so a verified hit serves it without contacting the credential store or the identity provider. `ExpiringWithinMargin` means the entry has reached the boundary set by `min(token_cache_ttl_secs, expires_in - 30s)` — it is at or inside the 30-second safety margin before the provider-reported expiry, or the configured ceiling has elapsed — and MUST NOT be served, which is precisely how the margin prevents a request being sent with a token that expires in flight. `Evicted` means the entry's storage has been reclaimed (by lazy expiry on the next lookup, or by capacity pressure under `token_cache_capacity`) and its zero-on-drop wrapper has zeroed the token bytes.

**Transitions**:
1. [x] - `p1` - **FROM** Absent **TO** Fresh **WHEN** a token exchange succeeds and the effective time-to-live `min(token_cache_ttl_secs, expires_in - 30s)` is positive, and the entry is stored together with a copy of its key - `inst-state-token-cache-01`
2. [x] - `p1` - **FROM** Absent **TO** Absent **WHEN** a token exchange succeeds but `expires_in` is at or below the 30-second safety margin (the token is used for the current request only and never written to the cache), or when the exchange fails (failed fetches are never cached, so the next request retries the provider) - `inst-state-token-cache-02`
3. [x] - `p1` - **FROM** Fresh **TO** Fresh **WHEN** a lookup hits and the key stored in the entry equals the lookup key: the token is served and the entry is neither refreshed nor extended - `inst-state-token-cache-03`
4. [x] - `p1` - **FROM** Fresh **TO** ExpiringWithinMargin **WHEN** the effective time-to-live elapses, i.e. the entry reaches the 30-second margin before the provider-reported expiry or the `token_cache_ttl_secs` ceiling, whichever came first - `inst-state-token-cache-04`
5. [x] - `p1` - **FROM** ExpiringWithinMargin **TO** Evicted **WHEN** the next lookup for that key observes the expiry (lazy expiry), which the caller treats as a miss and which triggers a fresh exchange - `inst-state-token-cache-05`
6. [x] - `p1` - **FROM** Fresh **TO** Evicted **WHEN** capacity pressure at `token_cache_capacity` reclaims the entry before its time-to-live elapses - `inst-state-token-cache-06`
7. [x] - `p1` - **FROM** Fresh **TO** Evicted **WHEN** the process restarts or the Data Plane is re-initialized: the cache is in-process only and holds no persisted state - `inst-state-token-cache-07`
8. [x] - `p1` - **FROM** Evicted **TO** Absent **WHEN** the entry's storage is reclaimed and its token bytes are zeroed, returning the key to its initial state for the next request - `inst-state-token-cache-08`

There is deliberately no transition out of `Fresh` triggered by an upstream `401`: event-driven or response-driven invalidation is listed as a future consideration by `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`, and adding it here would amount to implementing the deferred retry decision.

## 5. Definitions of Done

**Security** — this is the only feature in the round that touches secret material, so secret handling is a first-class requirement rather than a footnote: credentials exist in OAGW only as `cred://`/`secret_ref` references resolved per request through `cpt-cf-oagw-actor-cred-store` (`cpt-cf-oagw-principle-cred-isolation`); resolved material and bearer tokens are held in redacting, zero-on-drop wrappers; no secret value, no `Authorization` header value, and no token-endpoint response body may appear in any log line, error `detail`, audit record, or metric label (`cpt-cf-oagw-nfr-credential-isolation`, threshold: zero credential exposure); tenant and subject isolation of cached tokens is guaranteed structurally by the cache-key composition plus the on-hit key verification; and, under the graded `allow_http_upstream: true` configuration, injected credentials may traverse a plaintext `http` endpoint in cleartext — this feature documents that consequence and imposes no restriction the source documents do not impose. **Reliability** — every resolution or credential failure fails closed (no upstream call with a missing credential, no request forwarded with an unresolved plugin silently dropped); failed token fetches are not cached so transient identity-provider errors self-heal on the next request; no retry of the client's request is introduced (`cpt-cf-oagw-principle-no-retry`), and retry-on-401 stays deferred. **Data integrity** — this feature persists nothing: it reads plugin bindings written by features 2.2-2.4 and holds only an in-process token cache, so the integrity property it owns is that a cache hit returns a token belonging to exactly the requesting tenant/subject/config tuple, enforced by the on-hit key verification. **Observability** — the audit record for a proxied request identifies the plugin identifiers that ran, and on failure the rejecting plugin, the phase, and (for the guard) the offending header name, using `cpt-cf-oagw-feature-gear-foundation`'s scaffold and correlation identifier; token-cache hits and misses are distinguishable in logs without exposing keys' secret-adjacent content. **Rollback** — not applicable in the schema sense: this feature adds no table, no migration and no persisted state, so disabling it degrades the proxy path to plain forwarding without leaving any state behind; the in-process token cache is lost on restart, which is safe because the next request simply re-fetches. **Performance** — DECOMPOSITION entry 2.9 assigns this feature no dedicated latency NFR of its own (`cpt-cf-oagw-nfr-low-latency` is `cpt-cf-oagw-feature-proxy-core`'s Requirements Covered entry); the chain adds request-time work — credential resolution, guard/transform application, and, on a cache miss, a token-endpoint round trip — to that shared budget, and the one step capable of unbounded latency, the client-credentials token-endpoint/discovery call, is bounded by the gear's configured `proxy_timeout_secs` (§3 `cpt-cf-oagw-algo-plugin-token-acquire`) so a slow identity provider cannot stall the request past that bound; a cache hit adds no network round trip at all. **UX/Accessibility** — not applicable because this feature has no user interface; it is server-side request-processing behavior observed only through HTTP headers, status codes and response bodies. **Compliance/Data Privacy** — not applicable because this feature stores no personal or regulated data of its own: it holds only credential *references* (never persisted, only resolved per request) and cached bearer tokens keyed by tenant/subject identifiers already established elsewhere in the platform, all discarded on process restart.

### Deterministic Chain Execution Order

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-chain-order`

The system **MUST** execute the plugin chain on the resolved proxy request path in exactly this order and no other: the single auth plugin once, then every guard's request phase, then every `on_request` transform, then the upstream call, then every guard's response phase, then every `on_response` transform — substituting the `on_error` transforms for the response-phase steps when the upstream call itself failed. The order **MUST NOT** be influenced by binding configuration, and a plugin **MUST NOT** be invoked for a phase it does not declare, nor invoked twice within one phase.

**Implements**:
- `cpt-cf-oagw-algo-plugin-chain-execute`
- `cpt-cf-oagw-algo-plugin-auth-invoke`
- `cpt-cf-oagw-algo-plugin-transform-apply`
- `cpt-cf-oagw-flow-plugin-credentialed-proxy`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path_suffix}`
- Entities: Plugin execution chain, Auth / Guard / Transform plugin

### Upstream-Before-Route Chain Concatenation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-chain-concat`

The system **MUST** assemble each kind's chain by concatenating the upstream-bound binding list ahead of the route-bound binding list (`[U1, U2] + [R1, R2] => [U1, U2, R1, R2]`), preserving each list's own `position` order, without deduplicating repeated identifiers, reordering by kind, or sorting. There **MUST** be at most one auth plugin per request.

**Implements**:
- `cpt-cf-oagw-algo-plugin-chain-assemble`

**Touches**:
- Entities: Plugin execution chain

### Plugin Binding Resolution and the Unresolvable-Identifier Contract

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-binding-resolution`

The system **MUST** resolve each binding's `plugin_ref`/`plugin_uuid` to a registered implementation of the kind expected at that position, reusing `cpt-cf-oagw-feature-plugin-management`'s identification model, and **MUST** answer any binding it cannot resolve — a catalog-only identifier, an identifier of a non-deployed gear, a kind mismatch, or a UUID-backed custom plugin — with `PluginNotFound`: HTTP `503` and GTS `type` `gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1`, rendered with `X-OAGW-Error-Source: gateway`. The system **MUST NOT** forward the request with an unresolved binding skipped, and **MUST NOT** substitute a no-op.

**Implements**:
- `cpt-cf-oagw-algo-plugin-binding-resolve`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- Entities: Auth / Guard / Transform plugin

(No DB touch: consistent with `DECOMPOSITION.md` §2.9's `Data: None`, resolution reads only the already-merged in-memory binding data handed over by `cpt-cf-oagw-feature-proxy-core` and the in-process registries; it issues no query of its own.)

### Built-in Plugin Registries

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-plugin-builtin-registries`

The system **MUST** build, once at Data Plane initialization, three immutable in-process registries containing exactly the registry-resolvable built-ins: auth `...cf.core.oagw.noop.v1`, `...cf.core.oagw.apikey.v1`, `...cf.core.oagw.oauth2_client_cred.v1` (`Form`) and `...cf.core.oagw.oauth2_client_cred_basic.v1` (`Basic`); guard `...cf.core.oagw.required_headers.v1` as the sole guard entry; transform `...cf.core.oagw.request_id.v1` as the sole transform entry — each under its full GTS identifier as tabulated in `cpt-cf-oagw-algo-plugin-registry-init`.

**Implements**:
- `cpt-cf-oagw-algo-plugin-registry-init`

**Touches**:
- Entities: Auth / Guard / Transform plugin

### Catalog-Only Identifiers Must Fail to Bind

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-plugin-catalog-only-ids`

The system **MUST** keep the catalog-only identifiers out of every registry so that binding one fails resolution: auth `...cf.core.oagw.basic.v1` and `...cf.core.oagw.bearer.v1` (reserved GTS identifiers with no backing implementation), guard `...cf.core.oagw.timeout.v1` and `...cf.core.oagw.cors.v1` (core Data Plane functionality), transform `...cf.core.oagw.logging.v1` and `...cf.core.oagw.metrics.v1` (core Data Plane instrumentation). Selecting one as `auth.plugin_type` **MUST** fail with the "unknown auth plugin" condition surfaced as `503 PluginNotFound` rather than silently behaving as a no-op; the equivalent holds for the guard and transform kinds. Recognizing these identifiers as syntactically valid `plugin_ref` values at write time remains `cpt-cf-oagw-feature-plugin-management`'s behaviour and is unchanged by this feature.

**Implements**:
- `cpt-cf-oagw-algo-plugin-registry-init`
- `cpt-cf-oagw-algo-plugin-binding-resolve`

**Touches**:
- Entities: Auth / Guard / Transform plugin

### Guard Rejection Short-Circuit and Response Mapping

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-guard-short-circuit`

The system **MUST** stop the chain on the first guard rejection: on the request phase no later guard, no transform and no upstream connection may run; on the response phase the upstream's body **MUST NOT** be returned to the caller. The rejection's status and error code **MUST** be rendered as a gateway-originated response through `cpt-cf-oagw-feature-gear-foundation`'s RFC 9457 envelope carrying `X-OAGW-Error-Source: gateway`, and this feature **MUST NOT** introduce a new error-catalog row to do it.

**Implements**:
- `cpt-cf-oagw-algo-plugin-chain-execute`
- `cpt-cf-oagw-flow-plugin-guard-rejection`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- Entities: Plugin execution chain

### Required-Headers Guard Behaviour

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-plugin-required-headers-guard`

The system **MUST** implement the `required_headers` guard with exactly two independent configuration keys, `required_request_headers` and `required_response_headers`, both optional and comma-separated; **MUST** match header names case-insensitively and check presence only, never values; **MUST** fail open per phase independently when that phase's key is absent, empty, or blank after trimming every entry; **MUST** report only the first missing header per rejection; and **MUST** reject with status `400` and error code `REQUIRED_HEADER_MISSING` on the request phase and status `502` and error code `REQUIRED_HEADER_MISSING` on the response phase. The plugin **MUST** remain stateless. Header value validation is out of scope per `cpt-cf-oagw-adr-required-headers-guard-plugin`.

**Implements**:
- `cpt-cf-oagw-algo-plugin-guard-evaluate`
- `cpt-cf-oagw-flow-plugin-guard-rejection`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- Entities: Auth / Guard / Transform plugin

### Outbound Credential Injection from References

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-cred-injection`

The system **MUST** obtain every outbound credential at request time by resolving a `secret_ref`/`cred://` reference through the `cred_store` client (`cpt-cf-oagw-contract-cred-store`), passing the request's security context so that `cred_store` decides tenant accessibility (own or ancestor-shared); it **MUST NOT** store, copy into its own persistence, or accept inline secret material. A reference that is absent, unknown, or inaccessible **MUST** produce `401` `AuthenticationFailed` (`gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`) with no upstream call made. The `apikey` plugin **MUST** inject the resolved value into the configured request header or query parameter, replacing any value the caller supplied at that position; the `noop` plugin **MUST** inject nothing and **MUST NOT** call `cred_store`.

**Implements**:
- `cpt-cf-oagw-algo-plugin-cred-resolve`
- `cpt-cf-oagw-algo-plugin-auth-invoke`
- `cpt-cf-oagw-flow-plugin-credentialed-proxy`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- Entities: Credential reference (`secret_ref`), Auth / Guard / Transform plugin

### Secret Non-Disclosure in Logs, Errors and Audit Records

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-secret-nondisclosure`

The system **MUST** hold resolved secret material and bearer tokens in wrappers whose debug/display rendering is redacted and whose buffers are zeroed on drop, and **MUST** guarantee that no log line, error `detail`, RFC 9457 extension field, audit record, or metric label contains secret material, an `Authorization` header value, a query-parameter credential value, or a raw token-endpoint response body — at most the reference key and the failure kind may be recorded (`cpt-cf-oagw-nfr-credential-isolation`, `cpt-cf-oagw-principle-cred-isolation`). The known residual plaintext documented by `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin` (the request-scoped `Bearer <token>` header string) is accepted as-is and **MUST NOT** be widened by copying that string anywhere else.

**Implements**:
- `cpt-cf-oagw-algo-plugin-cred-resolve`
- `cpt-cf-oagw-algo-plugin-auth-invoke`
- `cpt-cf-oagw-algo-plugin-token-acquire`

**Touches**:
- Entities: Credential reference (`secret_ref`)

### Client-Credentials Auth Plugin Variants and Configuration Validation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-oauth2-variants`

The system **MUST** register both client-credentials variants — `...cf.core.oagw.oauth2_client_cred.v1` sending client credentials in the request body (`Form`) and `...cf.core.oagw.oauth2_client_cred_basic.v1` sending them in an `Authorization` header (`Basic`) — as distinct registry entries differing only in client-auth method and sharing the same cache configuration. It **MUST** accept the `ctx.config` keys `token_endpoint`, `issuer_url`, `client_id_ref`, `client_secret_ref` and `scopes`; **MUST** reject a configuration supplying both `token_endpoint` and `issuer_url`, or neither, as a validation failure mapped to `401 AuthenticationFailed` before any credential lookup or network call; **MUST** require `client_id_ref` and `client_secret_ref`; **MUST** resolve the token endpoint by OIDC Discovery from `issuer_url` when that form is used; and **MUST** perform a single one-shot token exchange per cache miss, spawning no background refresh task. Both the issuer-discovery call and the token-endpoint exchange **MUST** be bounded by the gear's configured `proxy_timeout_secs` (no dedicated timeout configuration key is introduced for this purpose), so that an unresponsive identity provider cannot stall the proxy request past that bound.

**Implements**:
- `cpt-cf-oagw-algo-plugin-token-acquire`
- `cpt-cf-oagw-algo-plugin-auth-invoke`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- Entities: Auth / Guard / Transform plugin, Credential reference (`secret_ref`)

### Token Cache Semantics

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-token-cache`

The system **MUST** cache client-credentials access tokens in an in-process cache bounded by the gear-level key `token_cache_capacity` (default `10000`) with the gear-level key `token_cache_ttl_secs` (default `300`) acting as a ceiling; **MUST** compute each entry's effective time-to-live as `min(token_cache_ttl_secs, expires_in - 30s)` from the provider-reported `expires_in`; **MUST NOT** cache a token whose `expires_in` is at or below the 30-second safety margin (using it for the current request only); **MUST** compose the cache key from `subject_tenant_id`, `subject_id`, the client-auth-method tag, and a deterministic sorted hash of the binding `config`; **MUST** store the key alongside the token and verify equality on every hit, treating a mismatch as a miss so a hash collision can never return another tenant's or subject's token; **MUST** serve a verified hit without any `cred_store` lookup and without contacting the identity provider; and **MUST NOT** cache failed token fetches.

**Implements**:
- `cpt-cf-oagw-algo-plugin-token-acquire`
- `cpt-cf-oagw-state-plugin-token-cache-entry`

**Touches**:
- Entities: Auth / Guard / Transform plugin

### Retry on Upstream 401 Stays Deferred

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-no-retry-on-401`

The system **MUST NOT** implement retry-on-401: when an upstream rejects the injected credential with `401`, that response is passed through by `cpt-cf-oagw-feature-proxy-core` and this feature **MUST NOT** re-issue the request, refresh the credential, or evict the cached token in response. `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin` explicitly defers the plugin-result metadata, trait shape and retry policy this would require, and `cpt-cf-oagw-principle-no-retry` forbids re-issuing the client request; silently adding any of it here is a defect, not an improvement.

**Implements**:
- `cpt-cf-oagw-algo-plugin-token-acquire`
- `cpt-cf-oagw-state-plugin-token-cache-entry`

**Touches**:
- Entities: Auth / Guard / Transform plugin

### Request-ID Transform Plugin

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-plugin-request-id-transform`

The system **MUST** implement the `request_id` transform for the request and response phases: on `on_request` it adopts the caller's `X-Request-ID` when supplied and otherwise injects the request correlation identifier provided by `cpt-cf-oagw-feature-gear-foundation`; on `on_response` it sets the same value on the response. It declares no `on_error` phase and **MUST** be skipped for phases it does not declare.

**Implements**:
- `cpt-cf-oagw-algo-plugin-transform-apply`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- Entities: Auth / Guard / Transform plugin

### GTS Type-Registration Touchpoints for Built-in Plugin Configuration

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-plugin-types-registry-touchpoints`

The system **MUST** rely on the existing `types_registry` registrations (`cpt-cf-oagw-contract-types-registry`) for the built-in plugin GTS identifiers and their `ctx.config` schemas, so that the configuration keys this feature reads — the client-credentials keys and the two `required_headers` keys — are recognized by the registry catalog that also lists the catalog-only identifiers. This feature **MUST NOT** define new GTS types, new schemas, or new endpoints; the registry remains the catalog and the plugin registries remain the executability authority, and the two must not be conflated.

**Implements**:
- `cpt-cf-oagw-algo-plugin-registry-init`

**Touches**:
- Entities: Auth / Guard / Transform plugin

### No Custom Starlark Plugin Execution

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-plugin-no-custom-execution`

The system **MUST NOT** evaluate custom Starlark plugin source or provide an execution sandbox in this feature. Per `DECOMPOSITION.md` §2.9 and §2.4 Out of scope, `cpt-cf-oagw-feature-plugin-management` stores and identifies such plugins but nothing in this round runs them, so a sandbox would have no behaviour to constrain and no independently testable surface; `cpt-cf-oagw-nfr-starlark-sandbox` stays deferred beyond this round. A UUID-backed custom binding encountered on the proxy path **MUST** therefore fail closed through `cpt-cf-oagw-dod-plugin-binding-resolution`'s `503 PluginNotFound` path rather than being skipped, no-op'd, or interpreted.

**Implements**:
- `cpt-cf-oagw-algo-plugin-binding-resolve`

**Touches**:
- Entities: Plugin execution chain

## 6. Acceptance Criteria

- [ ] For a request against an upstream binding one guard and one transform plus an auth plugin, the observed invocation sequence is exactly: auth once, then the guard's request phase, then the transform's `on_request`, then the upstream call, then the guard's response phase, then the transform's `on_response`.
- [ ] When the upstream call fails (for example the upstream is unreachable), the transform's `on_error` phase is invoked and its `on_response` phase is not, and the error's HTTP status and GTS `type` are unchanged by the transform.
- [ ] With guards bound as `[U1, U2]` on the upstream and `[R1, R2]` on the route, the recorded execution order is `U1, U2, R1, R2` in both the request and the response phase; binding the same guard identifier twice on the upstream runs it twice, each with its own `config`.
- [ ] An upstream-bound plugin runs before a route-bound plugin even when the route binding was created first and even when the route's binding list is longer than the upstream's.
- [ ] A request missing `x-correlation-id` against an upstream configured with `required_request_headers: "x-correlation-id,accept"` (and also missing `accept`) is answered `400` with error code `REQUIRED_HEADER_MISSING`, names only `x-correlation-id` in the response, carries `X-OAGW-Error-Source: gateway`, and produces no upstream connection attempt.
- [ ] The same request supplying `X-Correlation-Id` and `ACCEPT` in different letter case, and `x-correlation-id` with an empty value, is forwarded to the upstream (case-insensitive, presence-only matching).
- [ ] An upstream binding `required_headers` with no `required_request_headers` key at all, or with the value `""`, or with the value `", , ,"`, forwards every request unchanged (request-phase fail-open), while its configured `required_response_headers` value continues to be enforced — and the mirror case holds with the two keys swapped.
- [ ] An upstream response lacking `content-type` while `required_response_headers: "content-type"` is configured is answered `502` with error code `REQUIRED_HEADER_MISSING` and `X-OAGW-Error-Source: gateway`, and the upstream's body does not reach the caller.
- [ ] Binding `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1` (or `...cors.v1`, `...logging.v1`, `...metrics.v1`) and issuing a proxy request yields `503` with body `type` `gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1` and `X-OAGW-Error-Source: gateway`, and the request is not forwarded.
- [ ] Setting `auth.type` to `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1` or `...bearer.v1` and issuing a proxy request yields `503` with body `type` `gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1`; the request is not forwarded and no credential is injected.
- [ ] Setting `auth.type` to an identifier that does not exist in the catalog at all, or binding an `auth_plugin` identifier at a `plugins.items[]` position, likewise yields `503 PluginNotFound`.
- [ ] A proxy request against an upstream whose `auth.type` is `...cf.core.oagw.apikey.v1` reaches the upstream carrying the value stored in the credential store under the configured reference, in the configured header or query parameter, and an inbound caller-supplied value at that same position is replaced rather than forwarded.
- [ ] A proxy request whose auth configuration references a secret that does not exist, or one the calling tenant is not permitted to read, is answered `401` with body `type` `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`, no upstream connection is attempted, and neither the response body nor any emitted log line or audit record contains secret material.
- [ ] A proxy request against an upstream whose `auth.type` is `...cf.core.oagw.oauth2_client_cred.v1` performs exactly one token-endpoint exchange (client credentials in the body) and forwards `Authorization: Bearer <token>` to the upstream; the `...oauth2_client_cred_basic.v1` variant performs the exchange with the client credentials in an `Authorization` header instead.
- [ ] A second request for the same tenant, subject and binding configuration, issued while the first token is within its effective time-to-live, is served from the token cache: no additional token-endpoint call and no additional credential-store lookup are observed, and the upstream still receives the same bearer token.
- [ ] A request whose binding configuration differs only in `scopes`, only in the client-auth-method variant, only in the subject, or only in the tenant, causes a separate token-endpoint exchange rather than reusing the other entry's token.
- [ ] A binding configuration supplying both `token_endpoint` and `issuer_url`, or neither, is answered `401 AuthenticationFailed` naming the exclusive-or violation, with no credential-store lookup and no network call performed; a configuration missing `client_id_ref` or `client_secret_ref` is likewise rejected naming the missing key.
- [ ] A binding configured with `issuer_url` resolves its token endpoint through OIDC Discovery and completes the exchange without `token_endpoint` being configured.
- [ ] When the identity provider reports `expires_in` at or below 30 seconds, the token is used for the current request and no cache entry is created: the immediately following request performs another token-endpoint exchange.
- [ ] When the identity provider reports an `expires_in` whose value minus 30 seconds is smaller than `token_cache_ttl_secs`, the entry stops being served at that earlier boundary (a request issued after it triggers a fresh exchange); when `token_cache_ttl_secs` is the smaller value, the entry stops being served at the ceiling instead.
- [ ] A failed token exchange (provider unreachable or returning a non-success status) yields `401 AuthenticationFailed`, writes no cache entry, and the immediately following request retries the provider.
- [ ] A token endpoint (or OIDC discovery endpoint) that never responds does not stall the proxy request past the gear's configured `proxy_timeout_secs`: the request is answered `401 AuthenticationFailed` at or before that bound rather than hanging indefinitely, and no cache entry is written.
- [ ] A cache lookup whose stored entry carries a different key than the lookup key is treated as a miss and triggers a fresh exchange rather than returning the stored token.
- [ ] An upstream that answers `401` to the credentialed request has that response passed through to the caller with `X-OAGW-Error-Source: upstream`, with no re-issued request, no forced token refresh, and no cache eviction observable.
- [ ] With `token_cache_capacity` configured small enough to force eviction, entries are evicted without any request being served another tuple's token, and evicted entries are re-fetched on their next request.
- [ ] With the gear configuration omitting the token-cache keys, the resolved defaults are `token_cache_ttl_secs = 300` and `token_cache_capacity = 10000`.
- [ ] An upstream whose `auth.type` is `...cf.core.oagw.noop.v1`, and an upstream with no `auth` configuration at all, are both proxied successfully with no credential added, no `cred_store` call, and no error.
- [ ] Binding `...cf.core.oagw.request_id.v1` causes the upstream to receive an `X-Request-ID` equal to the caller's value when the caller supplied one, and equal to the request's correlation identifier when the caller did not; the response returned to the caller carries the same value.
- [ ] The guard and transform registries each contain exactly one entry (`required_headers`, `request_id` respectively) and the auth registry exactly four (`noop`, `apikey`, both client-credentials variants), each keyed by its full GTS identifier; no catalog-only identifier is present in any registry.
- [ ] Binding a UUID-backed custom plugin identifier on the proxy path yields `503 PluginNotFound`; no Starlark source is fetched, parsed, or evaluated by this feature.
- [ ] Across every failure path above (unresolvable plugin, inaccessible secret, exclusive-or violation, failed exchange, guard rejection), scanning the emitted log lines, the audit records and the response bodies finds no secret value, no `Authorization` header value and no raw token-endpoint response body.
