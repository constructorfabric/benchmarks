# Feature: Auth Plugins and Rate Limiting


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Execute the Request-Phase Plugin Chain](#execute-the-request-phase-plugin-chain)
  - [Inject Credentials into an Outbound Request](#inject-credentials-into-an-outbound-request)
  - [Enforce a Rate Limit on a Proxied Request](#enforce-a-rate-limit-on-a-proxied-request)
  - [Enforce CORS on an Actual Cross-Origin Request](#enforce-cors-on-an-actual-cross-origin-request)
  - [Run the Response-Phase Plugin Chain](#run-the-response-phase-plugin-chain)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Plugin Chain Resolution and Ordered Execution](#plugin-chain-resolution-and-ordered-execution)
  - [Credential Resolution from the Cred Store](#credential-resolution-from-the-cred-store)
  - [Auth Plugin Credential Injection](#auth-plugin-credential-injection)
  - [OAuth2 Token Cache Lookup and Fill](#oauth2-token-cache-lookup-and-fill)
  - [RequiredHeaders Guard Decision](#requiredheaders-guard-decision)
  - [RequestId Transform](#requestid-transform)
  - [Rate Limit Scope and Effective Cap](#rate-limit-scope-and-effective-cap)
  - [Token Bucket Acquisition](#token-bucket-acquisition)
  - [CORS Actual-Request Enforcement](#cors-actual-request-enforcement)
  - [Response-Phase Plugin Execution](#response-phase-plugin-execution)
- [4. States (CDSL)](#4-states-cdsl)
  - [Cached OAuth2 Access Token State Machine](#cached-oauth2-access-token-state-machine)
  - [Rate Limit Decision State Machine](#rate-limit-decision-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Plugin Chain Execution Order](#plugin-chain-execution-order)
  - [Plugin Resolution through the Entry-2.3 Registries](#plugin-resolution-through-the-entry-23-registries)
  - [Built-in Auth Plugin Credential Injection](#built-in-auth-plugin-credential-injection)
  - [OAuth2 Token Cache](#oauth2-token-cache)
  - [Credential Isolation on the Request Path](#credential-isolation-on-the-request-path)
  - [RequiredHeaders Guard](#requiredheaders-guard)
  - [RequestId Transform](#requestid-transform-1)
  - [Token Bucket Rate Limiting](#token-bucket-rate-limiting)
  - [429 Semantics and Rate Limit Response Headers](#429-semantics-and-rate-limit-response-headers)
  - [Actual-Request CORS Enforcement](#actual-request-cors-enforcement)
  - [Test Layering for the Auth Chain and Rate Limiting](#test-layering-for-the-auth-chain-and-rate-limiting)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-auth-plugins-and-rate-limiting-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-auth-plugins-and-rate-limiting`

<!--
=============================================================================
FEATURE SPECIFICATION
=============================================================================
PURPOSE: Define detailed implementation behavior — flows, algorithms, states,
and implementation requirements that bridge PRD and DESIGN to code.

SCOPE:
  ✓ Actor flows (user-facing interactions, step by step)
  ✓ Processes / Business Logic (incl. internal logic, validation, async jobs, etc)
  ✓ State machines (entity lifecycle)
  ✓ Implementation requirements (what to build)
  ✓ Acceptance criteria (how to verify)

NOT IN THIS DOCUMENT (see other templates):
  ✗ Requirements → PRD.md
  ✗ Architecture, components, APIs → DESIGN.md
  ✗ Why a specific approach was chosen → ADR/

CDSL PSEUDO-CODE:
  Optional. Use for complex flows or when precise behavior must be
  communicated. Skip for simple features to avoid overhead.
=============================================================================
-->
## 1. Feature Context

### 1.1 Overview

This feature is the plugin-execution and policy half of the OAGW data plane. It runs inside the two
hook points the entry-2.4 pipeline provides — the request phase, after the effective configuration
merge and before endpoint selection, and the response phase, after the upstream response is classified
or the error is mapped — and it owns everything that happens inside them: resolving the merged plugin
bindings through the runtime registries entry 2.3 constructs, executing them in the order
Auth → Guards → Transform (request) → upstream call → Transform (response/error) with upstream-attached
plugins before route-attached plugins at every stage, resolving credentials from the credstore by
`cred://` reference at request time and injecting them into the outbound request, enforcing the
per-upstream and per-route rate limits with in-process token buckets, evaluating the RequiredHeaders
guard in both phases, applying the RequestId transform, and enforcing the actual-request half of CORS.
Its outcomes are the `401`, `400`, `403`, `429`, `500` and `503` classes the pipeline maps onto its
error table. This feature registers no endpoint of its own: `{METHOD}
/oagw/v1/proxy/{alias}[/{path_suffix}]` is registered by entry 2.4, and the behaviour this feature
implements is the part of that request's lifetime that runs inside the hooks.

### 1.2 Purpose

Entry 2.5 turns the merged configuration of the pipeline into executed policy. It comes after entry 2.3
because the request chain resolves every binding through the `AuthPluginRegistry`,
`GuardPluginRegistry` and transform registry that entry's bootstrap builds, and after entry 2.4 because
the chain runs at hook points that pipeline defines and hands it the merged configuration. It comes
before entry 2.6 because a response this feature passes through may be classified as streamed and
handed off, and before entry 2.7 because the request context this feature populates — the rate-limit
decision, the degradation flag, the injected auth method tag, the error type — is what entry 2.7
records. This feature owns the request-path half of the shared plugin requirements: the execution
order, the credential injection, the guard decisions, the response-phase transforms, the rate-limit
enforcement and the actual-request CORS decision, per the decomposition's control-plane/data-plane
split.

Shared-requirement split: `cpt-cf-oagw-fr-plugin-system` and `cpt-cf-oagw-fr-builtin-plugins` are
cited by entries 2.3 and 2.5. Entry 2.3 owns the management behaviour (the type catalog, the builtin
registries, the resolvable and catalog-only identifier sets, definition immutability and binding
validation); this entry owns the request-path behaviour (the execution order, upstream-before-route
chain composition, credential injection, guard decisions, plugin timeouts and response-phase
transforms). Nothing in this feature writes a plugin definition, validates a binding on the management
path or registers a builtin.

This feature realizes `cpt-cf-oagw-principle-cred-isolation` as the request-path rule that secret
material exists only inside the credential-resolution and token-fetch steps and never reaches a log
line, an error body or an API response, and `cpt-cf-oagw-principle-plugin-immutable` as the read-only
consumption of plugin definitions: a chain runs against the definition as it was created, and no step
here can mutate, version or rebind one.

**Requirements**:

- [ ] `p1` - `cpt-cf-oagw-fr-auth-injection`
- [ ] `p1` - `cpt-cf-oagw-fr-rate-limiting`
- [ ] `p2` - `cpt-cf-oagw-fr-plugin-system`
- [ ] `p2` - `cpt-cf-oagw-fr-builtin-plugins`
- [ ] `p1` - `cpt-cf-oagw-nfr-high-availability`
- [ ] `p1` - `cpt-cf-oagw-nfr-credential-isolation`
- [ ] `p2` - `cpt-cf-oagw-usecase-rate-limit-exceeded`
- [ ] `p1` - `cpt-cf-oagw-contract-cred-store`

**Principles**: `cpt-cf-oagw-principle-cred-isolation`, `cpt-cf-oagw-principle-plugin-immutable`

**Feature-local deviations and recorded boundaries** (each inherited from the decomposition's
task-level wording, from an ADR, or recorded as a boundary this feature implements against; none is a
new architecture decision taken here):

- Gear-relative proxy path — DECOMPOSITION assumption 1. The request this feature implements behaviour
  for arrives at `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` without the `/api` prefix; the
  `/api/oagw/v1/proxy/...` forms in `cpt-cf-oagw-interface-proxy-api` and the ADRs are the absolute
  form behind an operator gateway. No path is registered here, so the deviation is inherited rather
  than owned. Review owner: OAGW component maintainer (`cf-gears-oagw`).
- Basic and Form select the client-auth method of the token request, not the injected credential —
  ADR 0008 is more precise than the decomposition's compressed wording ("the Form variant injects
  `Authorization: Bearer` and the Basic variant injects `Authorization: Basic`"). Implemented reading:
  both variants inject `Authorization: Bearer <token>` on the proxied request; the variant selects how
  the plugin authenticates to the token endpoint — `Form` places `client_id` and `client_secret` in
  the token request body, `Basic` sends them as `Authorization: Basic` with the base64 encoding of
  `client_id:client_secret`. Review owner: OAGW component maintainer. Validation: an in-crate test
  asserts both variants inject `Authorization: Bearer` on the proxied request and that only the token
  request differs.
- Auth-hook failure classes — the decomposition fixes `401` AuthenticationFailed for an auth failure
  and the entry-2.4 hook list fixes `500` SecretNotFound for a credential failure, without saying which
  failure is which. Implemented classification: an unresolvable or unreadable `cred://` reference, a
  credstore error and a credstore timeout are the `500` class (the credential could not be obtained);
  an IdP that cannot be reached, that rejects the client credentials or that returns a malformed token
  response is the `401` class (the credential was obtained and the outbound authentication failed), per
  the PRD's "auth plugin fails → 401 AuthenticationFailed". Review owner: OAGW component maintainer,
  with the API contract owner as second approver. Validation: an in-crate test asserts each of the two
  classes for each of the two causes.
- Actual-request CORS enforcement is specified here and applied inside the pipeline's request
  validation — DECOMPOSITION entry 2.5 carries the CORS scope line, and the entry-2.4 artifact asserts
  the same enforcement in its own CORS DoD. The split implemented here: this feature owns the CORS
  decision (origin matching rules, method matching, the credential and wildcard interaction, the
  `Vary: Origin` rule and the two `403` problem types) and exposes it as the decision the pipeline
  applies at its request-validation stage, after upstream resolution and before forwarding; the
  preflight `204` and its header echo are entry 2.4's (`cpt-cf-oagw-flow-proxy-preflight`) and are
  referenced here, never re-owned. Review owner: OAGW component maintainer, with the API contract
  owner as second approver. Validation: an in-crate integration test asserts the two `403` types and
  the `Vary: Origin` header on an actual request, and a second test asserts the preflight `204` is
  answered by the entry-2.4 handler without this feature's chain running.
- `queue` and `degrade` strategy semantics are not specified anywhere — ADR 0003 and the upstream
  schema fix the enum values and the default, and the PRD use case fixes one sentence for each, but no
  ADR defines their mechanics. Implemented reading: `reject` returns `429` with the rate-limit headers;
  `queue` holds the request in a bounded in-process wait for tokens and produces the same `429` when
  the bounded depth or the bounded wait is exceeded, so no request is ever held without a bound;
  `degrade` admits the request instead of rejecting it and records the degradation on the request
  context for entry 2.7's rate-limit series, because a gateway cannot reduce the functionality of an
  arbitrary upstream request and no reduce behaviour is specified. Review owner: OAGW component
  maintainer, with the API contract owner as second approver. Validation: in-crate tests assert the
  `429` for `reject`, admission within the bound for `queue`, the same `429` past the bound, and
  admission with the recorded degradation for `degrade`.
- `algorithm: sliding_window` has no distinct implementation — DECOMPOSITION entry 2.5 scopes token
  buckets ("enforce rate limits with token buckets") and ADR 0003 keeps sliding window as an optional
  algorithm. The stored value is accepted by entry 2.2's validation; this feature serves it with the
  token-bucket implementation of the same `sustained` rate/window and `burst.capacity`, and the
  sliding-window property the ADR describes (no boundary burst) is therefore not delivered. Review
  owner: OAGW component maintainer. Validation: an in-crate test asserts a stored `sliding_window`
  configuration is enforced with the same sustained rate and burst capacity and that no second
  algorithm code path is registered.
- The ADR 0003 `budget` block is not consumed — `docs/schemas/upstream.v1.schema.json` declares the
  rate-limit field set entry 2.2 validates (`sharing`, `algorithm`, `sustained`, `burst`, `scope`,
  `strategy`, `cost`, `response_headers`) and carries no `budget` object, so no budget allocation,
  shared-pool or overcommit validation happens at request time. Review owner: OAGW component
  maintainer.
- No separate plugin-timeout configuration key — the PRD requires enforced plugin execution timeouts
  and the entry-2.1 config key set (`proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy.enabled`,
  `token_cache_ttl_secs`, `token_cache_capacity`) contains no plugin-timeout key. Implemented bound:
  every plugin invocation is bounded by the request's remaining `proxy_timeout_secs` budget, so a
  plugin can never extend the request deadline; a plugin that does not return within the remaining
  budget fails its hook and the pipeline classifies the outcome as it classifies any request timeout.
  The PRD's "circuit-break misbehaving plugins" mitigation is not implemented: the only breaker in the
  gear is the upstream breaker entry 2.4 owns. Review owner: OAGW component maintainer. Validation: an
  in-crate test asserts a plugin that outlives the remaining budget fails the hook and that no second
  request is issued.
- Custom Starlark definitions resolve to no runtime instance — DECOMPOSITION assumption 4. Entry 2.3
  stores and serves a custom definition and validates its binding, and its builtin registries carry
  only the builtin plugins, so a `plugins.items[].plugin_ref` naming a UUID-backed custom definition
  resolves to nothing at request time and yields `503` PluginNotFound. The definition stays creatable,
  readable and deletable through entry 2.3. Review owner: OAGW component maintainer. Validation: an
  in-crate test asserts a chain containing a custom definition binding returns `503` and issues no
  upstream call.
- Credential isolation is the boundary this feature enforces and the credstore contract
  `cpt-cf-oagw-contract-cred-store` is the mechanism — the resolution is an in-process call through
  `credstore-sdk` (`CredStoreClientV1`) at request time, tenant-scoped by the calling tenant of the
  request, and the resolved value lives only as long as the injection or the token fetch needs it.
  Recorded residual plaintext: the `Authorization: Bearer <token>` header value is a short-lived plain
  string scoped to the request, as ADR 0008 records, while the cached token itself is a `SecretString`
  zeroed on eviction. Review owner: OAGW component maintainer, with the security reviewer as second
  approver.

Out of scope, restated from the decomposition so the boundary is explicit in this artifact: Starlark
sandbox execution (DECOMPOSITION assumption 4 — no interpreter dependency exists, so no custom plugin
is executed here), and distributed rate-limit synchronization across nodes (ADR 0003's Redis backend —
the buckets are single-node in-process state, and no `rate_limit_sync` configuration is read).

Coverage note: four baselines apply to this feature only as inherited baselines, and none of them is
cited on any DoD field below — the DoDs of this entry carry only the ids this feature owns, so no
**Implements**, **Constraints** or **Principles** field below repeats them.
`cpt-cf-oagw-constraint-toolkit-deploy` is inherited from dependency entry 2.1, which delivers the
gear deployment and the canonical error mapping every outcome below is raised through;
`cpt-cf-oagw-principle-tenant-scope` applies through the shared platform baseline of the credstore's
tenant scoping and the tenant-scoped security context the pipeline hands this feature, not through a
second isolation mechanism defined here; `cpt-cf-oagw-interface-api` is inherited from dependency
entry 2.4 and is the error table and proxy contract this feature produces outcomes for and never
redefines; `cpt-cf-oagw-component-model` is inherited from dependency entry 2.3 and places the chain
executor and the builtin plugins in the `infra/plugin/` layer behind the `domain/plugin/` traits.
`CachedToken` is not one of the four entities entry 2.5 declares: it is declared by ADR 0008 and is
carried here as an internal cache-entry type of this feature's token cache.

**Cross-cutting concerns**:

- Security: no endpoint is registered and no management surface is touched, so the only entry point is
  the proxy request the pipeline has already authenticated and authorized; the credential material this
  feature handles is resolved from the credstore by `cred://` reference at request time, is tenant
  scoped by the credstore contract, is held as `SecretString` where it outlives a single call, and is
  never logged, never returned in a response body and never written to the request context entry 2.7
  records. An auth plugin that fails yields `401`, so a caller cannot distinguish a missing credential
  from an invalid one. CORS enforcement rejects a disallowed origin or method before forwarding, with
  exact, port- and protocol-sensitive origin matching and no regex origins. Injection classes beyond
  that are not applicable here: SQL, XSS and command-injection prevention have no surface in a layer
  that holds no SQL, HTML or shell context, and header values are only inspected for presence, never
  parsed.
- Versioning: this feature registers no version of its own and no breaking change to
  `cpt-cf-oagw-interface-proxy-api`, whose recorded policy is a major version bump for a breaking
  change; the plugin identifiers it consumes are the GTS identifiers of the entry-2.3 type catalog, and
  a new builtin plugin is an additive change to that catalog.
- Reliability: a failure inside a hook is raised to the pipeline and mapped, never swallowed and never
  retried by this feature — a failed token fetch is not cached, so the next request retries the IdP,
  and a credstore outage on a cache miss fails the request while cached tokens continue to be served
  until their TTL expires (ADR 0008). Rate-limit state is process-local and is lost on restart; the
  brief window of unlimited requests that follows a cold start is the accepted PRD risk, and no counter
  is persisted.
- Data integrity: the chain runs against one merged configuration snapshot per request, so the auth
  configuration, the plugin bindings, the CORS configuration and the rate-limit bound it reads are
  mutually consistent and cannot be resolved from different generations; the token cache is keyed so
  that no entry can be served across a tenant or subject boundary, and a cache-key collision is
  treated as a miss rather than served. This feature writes nothing to the store.
- Observability: delegated to entry 2.7. This feature populates the request context fields the
  observability feature records — the rate-limit decision and its scope, the degradation flag, the
  resolved auth method tag, the guard outcome and the error type — and writes no audit line, registers
  no metric family and exposes no endpoint of its own (DECOMPOSITION assumption 9).
- Rollback: no persistence and no migration exist, so rollback is the operational act of redeploying
  the previous executable; the only in-gear recovery actions are the token cache's own expiry and the
  process-local reset of the buckets on restart.
- Test layering: coverage is in-crate Rust tests only — unit tests inside `#[cfg(test)]` modules per
  layer for the chain order, the credential resolution, each auth plugin, the token cache, the guard
  decision, the transform, the bucket arithmetic and the CORS decision — and integration tests under
  the crate's `tests/` directory that boot the gear router and drive the proxy endpoint against a stub
  upstream listener, a stub credstore and a mock IdP provided by the test harness. The
  `testing/e2e/gears/oagw/` directory is not used (DECOMPOSITION assumption 5) and no e2e suite is
  added.
- Compile-time gate: this feature adds no gear, no host feature flag and no gate of its own; its code
  is linked when the crate is linked for inventory registration and the proxy endpoint exists in the
  host executable under the entry-2.1 gate.
- Performance: applicable and owned here. ADR 0003 budgets less than 1ms for a rate-limit check, which
  this feature meets by keeping the buckets in process memory with no I/O on the check path and by
  keying a bucket once per (configuration, scope, scope id) rather than re-deriving it per request; a
  cache hit in the OAuth2 token cache is an in-memory lookup with no credstore and no IdP call, which
  is what keeps the credential injection off the request's latency budget (the ADR 0008 figures are
  100–500ms for an uncached IdP round trip). The plugin-execution cost is bounded by the request's
  remaining `proxy_timeout_secs` budget and can never extend it. The parts of the proxy budget this
  feature does not own are named: alias resolution, matching, merging, selection and header
  transformation belong to the entry-2.4 pipeline, and the non-blocking logging requirement to entry 2.7.
- Compliance/Privacy: not applicable in this feature — nothing is persisted, no personal data is
  processed, and the only identifiers placed on the request context are the tenant and subject
  identifiers the security context already carries plus the rate-limit scope key, which is a tenant,
  subject, peer address or route identifier and never a header or body value. There is no retention,
  residency or subject-right surface here.
- Accessibility: not applicable in this feature — no user-facing interface is authored beyond the
  `application/problem+json` error bodies the entry-2.1 mapping layer serializes, whose machine
  readable `type`, `title` and `detail` fields are the only surface an accessibility concern could
  attach to.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Sends the proxy request whose plugin chain, rate limit and CORS enforcement this feature executes, and consumes the outcomes: `401` AuthenticationFailed, `400` REQUIRED_HEADER_MISSING, the two CORS `403` types, `429` RateLimitExceeded with its headers, `503` PluginNotFound, and the rate-limit headers on a throttled response. |
| `cpt-cf-oagw-actor-cred-store` | Resolves the `cred://` references the auth configuration carries into secret values at request time, tenant-scoped, through the `credstore-sdk` contract `cpt-cf-oagw-contract-cred-store`; it never receives a request that names a secret in any other form. |
| `cpt-cf-oagw-actor-upstream-service` | Receives the request with the injected credential, the applied transforms and the forwarded `X-Request-ID`, and returns the response whose headers the RequiredHeaders guard checks in the response phase. |
| `cpt-cf-oagw-actor-platform-operator` | Owns the gear-level keys this feature reads (`token_cache_ttl_secs`, `token_cache_capacity`) and the global-scope rate-limit posture; observes the rate-limit and degradation series through entry 2.7. |
| `cpt-cf-oagw-actor-tenant-admin` | Owns the upstream and route configuration this feature consumes: the `auth` configuration and its `cred://` references, the `plugins` bindings and their inline configs, the `rate_limit` block with its sharing mode, and the `cors` configuration. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md) — `cpt-cf-oagw-component-model` (the `domain/plugin/` traits,
  the `infra/plugin/` registries and builtin plugins, the plugin system execution order and chain
  composition), `cpt-cf-oagw-seq-proxy-flow` (the request flow whose auth, guard and transform steps
  this feature executes), `cpt-cf-oagw-interface-api` (the error table this feature's outcomes map
  onto, the builtin plugin tables and the catalog-only identifier rule)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) — entry 2.5 and assumptions 1 to 9
- **ADRs**: [0003 Rate Limiting](../ADR/0003-rate-limiting.md)
  (`cpt-cf-oagw-adr-rate-limiting` — token bucket, dual-rate configuration, scope, strategy, cost,
  inheritance and the 429 headers), [0004 CORS](../ADR/0004-cors.md)
  (`cpt-cf-oagw-adr-cors` — actual-request origin and method enforcement, origin matching rules and
  the two `403` types), [0008 OAuth2 Client Credentials Auth Plugin](../ADR/0008-oauth2-client-credentials-auth-plugin.md)
  (`cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin` — the Form and Basic variants, the token
  cache key and TTL rule, the `CachedToken` wrapper and `fetch_token`), and
  [0002 Plugin System](../ADR/0002-plugin-system.md)
  (`cpt-cf-oagw-adr-plugin-system` — the three plugin traits, the execution order and the catalog-only
  identifiers); supporting baseline: [0009 Required Headers Guard Plugin](../ADR/0009-required-headers-guard-plugin.md)
  (`cpt-cf-oagw-adr-required-headers-guard-plugin` — the request and response presence check and its
  phase-specific statuses)
- **Dependencies**: `cpt-cf-oagw-feature-plugin-management` — this feature consumes the
  `AuthPluginRegistry`, `GuardPluginRegistry` and transform registry that feature's bootstrap
  constructs, resolves `plugins.items[].plugin_ref` and `auth.plugin_type` identifiers through
  `cpt-cf-oagw-algo-plugin-identifier-resolve`'s rules at request time, and treats a binding as
  immutable and read-only; `cpt-cf-oagw-feature-proxy-engine` — this feature runs inside that
  feature's request-phase and response-phase hook points, receives the merged configuration of
  `cpt-cf-oagw-algo-config-merge`, and returns every outcome to `cpt-cf-oagw-algo-proxy-error-mapping`
  for serialization
- **Resolved gear dependencies used here**: `credstore` (the only gear dependency called at request
  time, through `credstore-sdk`'s `CredStoreClientV1`); the tenant and subject identifiers come from
  the security context the pipeline supplies, `types-registry` is not called at request time (the
  identifiers were resolved against it when the binding was validated), and the IdP is an external
  HTTPS endpoint reached through the crate's existing `toolkit-http` client stack
- **Platform baselines**: toolkit canonical error contract (`toolkit_canonical_errors::CanonicalError`
  serialized as RFC 9457 `application/problem+json` with GTS `type` identifiers in the
  `gts.cf.core.errors.err.v1~cf.oagw....v1` space) and the `X-OAGW-Error-Source: gateway` value on
  every gateway-generated `401`, `403`, `429`, `500` and `503` this feature causes, both delivered by
  the entry-2.1 cross-cutting layer; `toolkit_auth::oauth2::fetch_token` for the one-shot client
  credentials exchange (no background watcher task); `pingora-memory-cache` for the token cache per
  ADR 0008; `credstore-sdk` `CredStoreClientV1` for `cred://` resolution; `SecretString` for cached
  token material; the gear configuration keys `oagw.config.token_cache_ttl_secs` (recorded default
  `300`) and `oagw.config.token_cache_capacity` (recorded default `10000`) loaded by entry 2.1 per
  ADR 0008's gear-level table; the `dashmap`/`parking_lot`/`arc-swap` primitives already present in the
  crate's `Cargo.toml` for the in-process buckets; the crate's existing `toolkit-http` and `pingora-*`
  client stack for the token fetch

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor and describe the end-to-end flow of a use case. Every
flow below runs inside a hook point the entry-2.4 pipeline provides: the pipeline supplies the merged
configuration and consumes the outcome. This feature raises its failures to the pipeline and serializes
nothing itself, so every error body below is the `application/problem+json` document
`cpt-cf-oagw-algo-proxy-error-mapping` builds and every response carries `X-OAGW-Error-Source`.

**Use cases**: `cpt-cf-oagw-usecase-rate-limit-exceeded`

**Referenced, not covered here**:

- The proxy request use case `cpt-cf-oagw-usecase-proxy-request` is covered by DECOMPOSITION entry 2.4,
  which owns the alias walk, the route match, the configuration merge, the endpoint selection, the
  request and body validation, the header transformations, the upstream call, the circuit breaker and
  the preflight `204`; the flows below start where that pipeline dispatches into its hooks.
- The management half of the plugin system (definition CRUD, the builtin catalog, binding validation
  and the in-use scan) is covered by DECOMPOSITION entry 2.3; this feature only resolves bindings
  through the registries that feature builds.
- The audit record and the metric families derived from the outcomes below are covered by DECOMPOSITION
  entry 2.7; this feature populates the request context and emits nothing itself.
- The management-side rejection of `allow_credentials` combined with a wildcard origin, and of a
  catalog-only or unknown `auth.plugin_type`, is covered by DECOMPOSITION entry 2.2's write validation;
  this feature records both boundaries and never has to serve those configurations.

### Execute the Request-Phase Plugin Chain

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-chain`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:

- An actor sends `{METHOD} /oagw/v1/proxy/{alias}/{path_suffix}` for an enabled upstream and route; the
  request reaches the request-phase hook with the merged configuration, and the chain executes the
  upstream auth plugin once, then the guards, then the request transforms — upstream-attached plugins
  before route-attached plugins at every stage — and returns control to the pipeline with the mutated
  request and no rejection.
- An upstream that binds guard and transform plugins at both the upstream and the route level runs them
  in one ordered sequence: every upstream binding before every route binding, within each stage, in
  binding position order.
- An upstream that binds no plugins at all runs an empty chain: no credential is injected, no guard is
  evaluated, no transform is applied, and the request is forwarded unchanged by this feature.
- A RequiredHeaders binding configured with an absent or blank header list no-ops for its phase
  (fail-open), and the request is forwarded as if the plugin were not bound.

**Error Scenarios**:

- `503` PluginNotFound when a binding names an identifier no runtime registry resolves: a catalog-only
  identifier (`basic`, `bearer`, `timeout`, `cors`, `logging`, `metrics`), or a UUID-backed custom
  Starlark definition that has no runtime instance (DECOMPOSITION assumption 4). No upstream call is
  made.
- `401` AuthenticationFailed when the auth plugin rejects the request, including an IdP that cannot be
  reached or that refuses the client credentials.
- `500` when a `cred://` reference in the auth configuration cannot be resolved (unknown identifier,
  another tenant's credential, credstore error or timeout).
- `400` REQUIRED_HEADER_MISSING when a bound RequiredHeaders guard finds its first missing request
  header. Only that one header is named.
- A plugin invocation that does not return within the request's remaining `proxy_timeout_secs` budget
  fails its hook, and the request never reaches the upstream.

**Steps**:

1. [x] - `p1` - Actor sends the proxy request; the pipeline resolves the alias, matches the route, - `inst-pc-01`
   merges the effective configuration and dispatches into this feature at the request-phase hook, after
   `cpt-cf-oagw-algo-config-merge` and before endpoint selection
2. [x] - `p1` - Receive from the pipeline the merged configuration: the effective `auth` configuration - `inst-pc-02`
   with its `cred://` references, the ordered plugin binding list with upstream bindings before route
   bindings, the effective `cors` configuration, the effective rate-limit bound and the request context

3. [x] - `p1` - Resolve every binding to a runtime instance through the registries with - `inst-pc-03`
   `cpt-cf-oagw-algo-plugin-chain`, without re-validating the binding entry 2.3 already validated -

4. [x] - `p1` - **IF** a binding resolves to no runtime instance - `inst-pc-04`
   1. [x] - `p1` - **RETURN** `503` PluginNotFound to the pipeline; no plugin of the chain runs and no - `inst-pc-05`
      credential is resolved
5. [x] - `p1` - **ELSE** execute the auth stage: run the effective `auth` plugin exactly once with - `inst-pc-06`
   `cpt-cf-oagw-algo-auth-inject`, resolving its credentials with `cpt-cf-oagw-algo-cred-resolve` -

6. [x] - `p1` - **IF** the auth stage rejects - `inst-pc-07`
   1. [x] - `p1` - **RETURN** the rejection to the pipeline: `401` AuthenticationFailed for an IdP or - `inst-pc-08`
      credential refusal, `500` SecretNotFound for a `cred://` reference that cannot be resolved; the
      guards and transforms of this request do not run
7. [x] - `p1` - **ELSE** execute the guard stage: run every bound guard plugin in order, starting with - `inst-pc-09`
   the RequiredHeaders decision of `cpt-cf-oagw-algo-required-headers`
8. [x] - `p1` - **IF** a guard rejects - `inst-pc-10`
   1. [x] - `p1` - **RETURN** the rejection to the pipeline as `400` with the first missing header named - `inst-pc-11`
      and no further guard or transform executed
9. [x] - `p1` - **ELSE** execute the transform stage: run every bound request transform in order, - `inst-pc-12`
   including the RequestId propagation of `cpt-cf-oagw-algo-request-id`
10. [x] - `p1` - Record the resolved auth method tag, the executed plugin identifiers and the guard and - `inst-pc-13`
    transform outcomes on the request context for entry 2.7, carrying no credential material -

11. [x] - `p1` - **RETURN** control to the pipeline with the mutated request header set and query - `inst-pc-14`
    string; the pipeline continues with endpoint selection, validation, transformation and the upstream
    call, and dispatches back into this feature at the response-phase hook

### Inject Credentials into an Outbound Request

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-auth-injection`

**Actor**: `cpt-cf-oagw-actor-app-developer`, `cpt-cf-oagw-actor-cred-store`

**Success Scenarios**:

- An upstream configured with the API key auth plugin and a `cred://` reference has the resolved secret
  injected as the configured request header, `X-API-Key` by default, on the outbound request.
- An API key plugin configured for query injection has the resolved secret appended to the forwarded
  query string as `api_key`, and no header is added.
- An upstream configured with the no-op auth plugin has nothing injected and the stage always succeeds,
  which is the declared way to state that an upstream needs no credential.
- An upstream configured with the OAuth2 client credentials Form variant has its token request answered
  by the IdP with the client credentials in the request body, and the returned token injected as
  `Authorization: Bearer <token>`.
- An upstream configured with the Basic variant has its token request answered with the client
  credentials sent as `Authorization: Basic` carrying the base64 encoding of
  `client_id:client_secret`, and the same `Authorization: Bearer` injection on the proxied request.
- A second request for the same tenant, subject, client-auth method and configuration is served from
  the token cache with no credstore call and no IdP call.
- An IdP that reports an `expires_in` of at most 30 seconds has its token injected for that request and
  not cached, so every request for such a configuration fetches from the IdP again.

**Error Scenarios**:

- `500` SecretNotFound when `cpt-cf-oagw-algo-cred-resolve` cannot resolve a reference: the identifier
  is unknown to the credstore, belongs to another tenant, or the credstore is unreachable or times out.
  Nothing is cached and the reference is the only credential trace left anywhere.
- `401` AuthenticationFailed when the IdP is unreachable, rejects the grant, or returns a response
  without a usable token; the failed fetch is not cached and the next request retries the IdP.
- `503` PluginNotFound when `auth.plugin_type` names an identifier the `AuthPluginRegistry` does not
  resolve, including the catalog-only `basic` and `bearer` identifiers.

**Steps**:

1. [ ] - `p1` - Actor sends the proxy request; the chain's auth stage starts with the effective `auth` - `inst-ai-01`
   configuration and its plugin config pairs
2. [ ] - `p1` - Resolve `auth.plugin_type` through the `AuthPluginRegistry`; a catalog-only or unknown - `inst-ai-02`
   identifier is **RETURN**ed to the pipeline as `503` PluginNotFound
3. [ ] - `p1` - **IF** the resolved plugin is the no-op auth plugin - `inst-ai-03`
   1. [ ] - `p1` - Inject nothing, resolve no credential and succeed, so the request proceeds with the - `inst-ai-04`
      credentials it already carries
4. [ ] - `p1` - **ELSE IF** the resolved plugin is the API key plugin - `inst-ai-05`
   1. [ ] - `p1` - Resolve the configured `cred://` reference with `cpt-cf-oagw-algo-cred-resolve` - - `inst-ai-06`

   2. [ ] - `p1` - **IF** the plugin config selects query injection - `inst-ai-07`
      1. [ ] - `p1` - Append `api_key` with the resolved secret to the forwarded query string and add no - `inst-ai-08`
         header
   3. [ ] - `p1` - **ELSE** - `inst-ai-09`
      1. [ ] - `p1` - Set the configured request header, `X-API-Key` when the config names none, to the - `inst-ai-10`
         resolved secret
5. [ ] - `p1` - **ELSE** the resolved plugin is an OAuth2 client credentials variant - `inst-ai-11`
   1. [ ] - `p1` - Look the token up in the plugin's cache with `cpt-cf-oagw-algo-token-cache` - `inst-ai-12`
   2. [ ] - `p1` - **IF** the cache holds a verified entry for the request's cache key - `inst-ai-13`
      1. [ ] - `p1` - Inject `Authorization: Bearer <token>` from the cached entry, with no credstore - `inst-ai-14`
         call and no IdP call
   3. [ ] - `p1` - **ELSE** - `inst-ai-15`
      1. [ ] - `p1` - Resolve `client_id_ref` and `client_secret_ref` with - `inst-ai-16`
         `cpt-cf-oagw-algo-cred-resolve` and drop both values as soon as the fetch completes -

      2. [ ] - `p1` - Call the IdP once through `toolkit_auth::oauth2::fetch_token`, authenticating the - `inst-ai-17`
         client in the body for the Form variant and with `Authorization: Basic` for the Basic variant,
         requesting the configured space-separated `scopes`
      3. [ ] - `p1` - **IF** the fetch fails, or the response carries no usable token - `inst-ai-18`
         1. [ ] - `p1` - Cache nothing and **RETURN** the failure as `401` AuthenticationFailed - - `inst-ai-19`

      4. [ ] - `p1` - **ELSE** - `inst-ai-20`
         1. [ ] - `p1` - Cache the token with `cpt-cf-oagw-algo-token-cache` under the TTL rule and - `inst-ai-21`
            inject `Authorization: Bearer <token>`
6. [ ] - `p1` - **RETURN** the mutated request header set and query string to the chain, with the - `inst-ai-22`
   resolved secret material absent from the request context

### Enforce a Rate Limit on a Proxied Request

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-rate-limit-enforcement`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:

- An actor sends a request for an upstream or route with a rate limit configured; the bucket for the
  request's scope key is refilled, the request's `cost` is consumed, and the request is forwarded.
- A burst up to `burst.capacity` is admitted inside one window even when the sustained rate would not
  allow it, because the bucket starts full and refills continuously.
- A route-level limit stricter than the upstream-level limit governs; a descendant limit is capped by
  the effective limit of an `enforce` ancestor, and shadowing cannot lift that cap.
- A rejected request receives `429` with `Retry-After` and, when `response_headers` is true, the
  `X-RateLimit-Limit`, `X-RateLimit-Remaining` and `X-RateLimit-Reset` headers.

**Error Scenarios**:

- `429` RateLimitExceeded when the bucket cannot satisfy the request's `cost` under `strategy: reject`,
  with `X-OAGW-Error-Source: gateway` and no upstream call.
- `429` under `strategy: queue` once the bounded queue depth or the bounded wait is exhausted; a request
  is never held without a bound.
- `429` under `strategy: degrade` is not produced — the strategy admits the request and records the
  degradation instead; a configuration that expects a rejection under `degrade` is a configuration
  error, not a gateway behaviour.
- A restart empties every bucket, so the first requests after a cold start are admitted without
  accounting for consumption before the restart.

**Steps**:

1. [ ] - `p1` - Actor sends the proxy request; the chain's guard stage reaches the rate-limit evaluation - `inst-rl-01`
   with the effective rate-limit bound, the configured scope and strategy, and the request context -

2. [ ] - `p1` - Derive the bucket key and the effective cap with `cpt-cf-oagw-algo-rate-limit` - - `inst-rl-02`

3. [ ] - `p1` - Attempt the acquisition with `cpt-cf-oagw-algo-bucket-consume` at the cost configured - `inst-rl-03`
   for this request
4. [ ] - `p1` - **IF** the bucket can satisfy the cost - `inst-rl-04`
   1. [ ] - `p1` - Consume the tokens, record the admission and the remaining tokens on the request - `inst-rl-05`
      context, and continue the chain
5. [ ] - `p1` - **ELSE IF** the configured strategy is `queue` - `inst-rl-06`
   1. [ ] - `p1` - Hold the request in the bounded in-process wait while the bucket refills, bounded by - `inst-rl-07`
      the queue depth and by the request's remaining budget
   2. [ ] - `p1` - **IF** the tokens become available inside both bounds - `inst-rl-08`
      1. [ ] - `p1` - Consume them and continue the chain as an admitted request - `inst-rl-09`
   3. [ ] - `p1` - **ELSE** - `inst-rl-10`
      1. [ ] - `p1` - Release the request and **RETURN** the rejection as described for `reject` - - `inst-rl-11`

6. [ ] - `p1` - **ELSE IF** the configured strategy is `degrade` - `inst-rl-12`
   1. [ ] - `p1` - Admit the request without consuming from the exhausted bucket, record the degradation - `inst-rl-13`
      on the request context for entry 2.7, and continue the chain
7. [ ] - `p1` - **ELSE** (`strategy: reject`) - `inst-rl-14`
   1. [ ] - `p1` - Compute `Retry-After` from the time the bucket needs to satisfy the cost, and the - `inst-rl-15`
      `X-RateLimit-Limit`, `X-RateLimit-Remaining` and `X-RateLimit-Reset` values when
      `response_headers` is true
   2. [ ] - `p1` - **RETURN** the rejection to the pipeline as `429` RateLimitExceeded with no upstream - `inst-rl-16`
      call made
8. [ ] - `p1` - **RETURN** the rate-limit outcome for the response phase, which attaches the rate-limit - `inst-rl-17`
   headers to a throttled response and records nothing else

### Enforce CORS on an Actual Cross-Origin Request

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-cors-actual-request`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:

- A browser sends an actual cross-origin request with an `Origin` that the effective CORS configuration
  allows and a method in `allowed_methods`; the request is forwarded and the response carries
  `Access-Control-Allow-Origin` echoing the request origin, `Access-Control-Expose-Headers` when
  `expose_headers` is configured, `Access-Control-Allow-Credentials: true` only when
  `allow_credentials` is true and the origin matched exactly, and `Vary: Origin`.
- An origin is matched exactly and is both port- and protocol-sensitive: `https://app.example.com`
  matches neither `https://app.example.com:8443` nor `http://app.example.com`, and no pattern or regex
  origin exists to match more.
- A request that carries an `Origin` while `cors.enabled` is false on the effective configuration is
  not a CORS request for this feature: no origin or method check runs and no CORS header is added.

**Error Scenarios**:

- `403` with `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` when the request's origin
  is not in the effective `allowed_origins`; the request is rejected before forwarding and no upstream
  call is made.
- `403` with `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1` when the origin is allowed
  and the method is not in `allowed_methods`.
- A preflight that was answered with the permissive `204` of `cpt-cf-oagw-flow-proxy-preflight` is
  followed by an actual request that is still rejected here, because origin and method enforcement
  happens only on the actual request.
- `allow_credentials` combined with a wildcard `*` origin never reaches this feature: it is rejected at
  management validation time by entry 2.2, and this feature treats the combination as impossible.

**Steps**:

1. [x] - `p1` - Actor sends the actual cross-origin request with an `Origin` header - `inst-co-01`
2. [x] - `p1` - Receive the effective `cors` configuration from the merged configuration the pipeline - `inst-co-02`
   handed the hook, after upstream resolution
3. [x] - `p1` - **IF** `cors.enabled` is false, or the request carries no `Origin` header - `inst-co-03`
   1. [x] - `p1` - Apply no CORS check and add no CORS header, and continue the chain - `inst-co-04`
4. [x] - `p1` - **ELSE** evaluate the request with `cpt-cf-oagw-algo-cors-enforce` - `inst-co-05`
   1. [x] - `p1` - Match the request origin against `allowed_origins` exactly, port- and - `inst-co-06`
      protocol-sensitively, with `*` matching any origin
   2. [x] - `p1` - **IF** the origin is not allowed - `inst-co-07`
      1. [x] - `p1` - **RETURN** the rejection to the pipeline as `403` with - `inst-co-08`
         `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1`, `Vary: Origin` set, and no
         upstream call
   3. [x] - `p1` - **ELSE IF** the request method is not in `allowed_methods` - `inst-co-09`
      1. [x] - `p1` - **RETURN** the rejection to the pipeline as `403` with - `inst-co-10`
         `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`, `Vary: Origin` set, and no
         upstream call
   4. [x] - `p1` - **ELSE** mark the request as CORS-allowed with the headers the response phase must - `inst-co-11`
      add, and continue the chain
5. [x] - `p1` - **RETURN** the CORS decision to the chain; the response phase adds - `inst-co-12`
   `Access-Control-Allow-Origin`, the configured `Access-Control-Expose-Headers`,
   `Access-Control-Allow-Credentials` when applicable and `Vary: Origin`

### Run the Response-Phase Plugin Chain

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-response-phase`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:

- After the upstream response is classified as buffered, the response-phase hook runs the bound
  transforms that declare the response phase and the response half of the bound guards: the RequestId
  transform propagates or echoes `X-Request-ID` onto the response, and the RequiredHeaders guard checks
  `required_response_headers` against the upstream response.
- A mapped gateway error reaches the error-phase hook instead, and the transforms that declare the
  error phase run against the error context before the pipeline serializes it.
- A response from an upstream that bound no response-phase plugins is passed through by this feature
  with nothing added except the CORS headers of an allowed cross-origin request.

**Error Scenarios**:

- `502` REQUIRED_HEADER_MISSING when the upstream response lacks the first configured required response
  header; the response body is not forwarded and the pipeline maps the rejection.
- `500` when a response-phase or error-phase transform fails; no partial body is emitted to the caller.
- A response classified as streamed is handed to DECOMPOSITION entry 2.6 before the response-phase hook
  runs, so this feature applies no response-phase transform to a streamed exchange and the streamed
  session owns its header handling.

**Steps**:

1. [x] - `p1` - The pipeline classifies the upstream response, or maps the failure it raised, and - `inst-rp-01`
   dispatches into this feature at the response-phase hook
2. [x] - `p1` - **IF** the response was handed to the entry-2.6 streaming path - `inst-rp-02`
   1. [x] - `p1` - Run no response-phase transform: the handoff precedes this hook in the pipeline, and - `inst-rp-03`
      the streamed session behaviour is entry 2.6's
3. [x] - `p1` - **ELSE IF** the upstream call succeeded - `inst-rp-04`
   1. [x] - `p1` - Run the response half of the bound guards with - `inst-rp-05`
      `cpt-cf-oagw-algo-required-headers` against the upstream response headers
   2. [x] - `p1` - **IF** a required response header is missing - `inst-rp-06`
      1. [x] - `p1` - **RETURN** the rejection to the pipeline as `502` with the first missing header - `inst-rp-07`
         named and the upstream body discarded
   3. [x] - `p1` - **ELSE** run the bound transforms that declare the response phase, including the - `inst-rp-08`
      RequestId echo of `cpt-cf-oagw-algo-request-id`, and add the CORS headers of an allowed
      cross-origin request
4. [x] - `p1` - **ELSE** the call failed and the pipeline mapped the error - `inst-rp-09`
   1. [x] - `p1` - Run the bound transforms that declare the error phase against the error context, and - `inst-rp-10`
      add the rate-limit headers to a throttled error response
5. [x] - `p1` - **RETURN** the mutated response header set, or the mutated error context, to the - `inst-rp-11`
   pipeline for the error-source stamping and the response

## 3. Processes / Business Logic (CDSL)

Internal system functions that do not interact with actors directly. These are the stages of the plugin
chain and the policy decisions behind it, in execution order; each is called by a flow above or by
another stage, and each raises the failures the pipeline's `cpt-cf-oagw-algo-proxy-error-mapping`
turns into responses.

### Plugin Chain Resolution and Ordered Execution

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-chain`

**Input**: the ordered plugin binding list the merged configuration carries (upstream bindings before
route bindings, in binding position order, each with its `plugin_ref` and inline `config`), the
effective `auth` configuration, and the runtime registries entry 2.3 constructed.

**Output**: the resolved and ordered chain — one auth plugin, the guard plugins, the request
transforms — or the `503` PluginNotFound outcome to map.

**Steps**:

1. [ ] - `p1` - Take the binding list as it was validated and stored by entry 2.3; no binding is - `inst-alc-01`
   re-validated, re-ordered or re-written here, and no definition is mutated
2. [ ] - `p1` - Split the list by plugin type into the auth, guard and transform sets, keeping each - `inst-alc-02`
   set in binding-position order
3. [ ] - `p1` - **FOR EACH** binding in a set - `inst-alc-03`
   1. [ ] - `p1` - Resolve the `plugin_ref` through the registry of its type: a named GTS identifier - `inst-alc-04`
      against the builtin registry, a UUID against the same registry's instance map
   2. [ ] - `p1` - **IF** no registry of the requested type holds the identifier, or the identifier - `inst-alc-05`
      resolves only in a registry of another type
      1. [ ] - `p1` - **RETURN** `503` PluginNotFound with no partial execution - `inst-alc-06`
4. [ ] - `p1` - Determine the execution order: the auth plugin, then the guards, then the request - `inst-alc-07`
   transforms, then the upstream call owned by the pipeline, then the response and error transforms -

5. [ ] - `p1` - Compose each stage as upstream bindings first, then route bindings, so the effective - `inst-alc-08`
   sequence inside a stage is `[U1, U2, R1, R2]`
6. [ ] - `p1` - Execute each stage in order, passing the plugin's inline `config` pairs to the plugin - `inst-alc-09`
   instance and stopping at the first rejection
7. [ ] - `p1` - **TRY** each plugin invocation inside the request's remaining `proxy_timeout_secs` - `inst-alc-10`
   budget, so a plugin can never extend the request deadline
8. [ ] - `p1` - **CATCH** a plugin that outlives that budget - `inst-alc-11`
   1. [ ] - `p1` - Fail the hook with the timeout outcome, carry no partial mutation forward, and let - `inst-alc-12`
      the pipeline classify the response
9. [ ] - `p1` - Record the executed plugin identifiers and their outcomes on the request context - - `inst-alc-13`

10. [ ] - `p1` - **RETURN** the resolved chain, or the `503` outcome - `inst-alc-14`

### Credential Resolution from the Cred Store

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-cred-resolve`

**Input**: a `cred://` reference from the effective `auth` configuration or from a plugin config pair,
and the calling tenant from the security context the pipeline handed the hook.

**Output**: the resolved secret value as `SecretString`, or the `500` SecretNotFound outcome to map.

**Steps**:

1. [ ] - `p1` - Parse the reference and require the `cred://` scheme and a UUID instance; a reference - `inst-acr-01`
   that is absent, malformed or not a `cred://` URI is a resolution failure
2. [ ] - `p1` - Resolve the reference through the `credstore-sdk` contract - `inst-acr-02`
   `cpt-cf-oagw-contract-cred-store` with `CredStoreClientV1`, scoped to the calling tenant, so a
   reference that names another tenant's credential is indistinguishable from an unknown one -

3. [ ] - `p1` - **TRY** the resolution - `inst-acr-03`
   1. [ ] - `p1` - Call the credstore once per reference per request that needs it; an OAuth2 cache - `inst-acr-04`
      miss resolves its two references inside the same fetch path and no credential is cached separately
      from its token
4. [ ] - `p1` - **CATCH** an unknown reference, a cross-tenant reference, a credstore error or a - `inst-acr-05`
   credstore timeout
   1. [ ] - `p1` - **RETURN** the `500` SecretNotFound outcome with a detail that names the - `inst-acr-06`
      configuration key, never the reference's target, the secret or any part of it
5. [ ] - `p1` - Wrap the resolved value as `SecretString` and hand it to the caller only - `inst-acr-07`
6. [ ] - `p1` - Place no secret value, no resolved credential and no `cred://` target on the request - `inst-acr-08`
   context, the response, any log line or any error body
7. [ ] - `p1` - Drop the value as soon as the injection or the token fetch that consumed it completes - - `inst-acr-09`

8. [ ] - `p1` - **RETURN** the resolved secret, or the `500` outcome - `inst-acr-10`

### Auth Plugin Credential Injection

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-auth-inject`

**Input**: the resolved auth plugin instance, its plugin config pairs from the effective `auth`
configuration, and the request context with its header set and query string.

**Output**: the mutated request header set and query string carrying the injected credential, or the
`401` / `500` outcome to map.

**Steps**:

1. [x] - `p1` - Dispatch on the plugin identifier the registry resolved - `inst-aai-01`
2. [x] - `p1` - **IF** the plugin is `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1` - `inst-aai-02`
   1. [x] - `p1` - Inject nothing and succeed unconditionally - `inst-aai-03`
3. [x] - `p1` - **ELSE IF** the plugin is `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1` - `inst-aai-04`
   1. [x] - `p1` - Read the plugin config: the `cred://` reference for the key, the injection target - `inst-aai-05`
      (header or query) and the header name, which defaults to `X-API-Key`
   2. [x] - `p1` - Resolve the reference with `cpt-cf-oagw-algo-cred-resolve` - `inst-aai-06`
   3. [x] - `p1` - **IF** the target is the query string - `inst-aai-07`
      1. [x] - `p1` - Append `api_key` with the resolved secret to the forwarded query string and add - `inst-aai-08`
         no header
   4. [x] - `p1` - **ELSE** set the configured request header to the resolved secret - `inst-aai-09`
4. [x] - `p1` - **ELSE** the plugin is an OAuth2 client credentials variant - `inst-aai-10`
   1. [x] - `p1` - Read the plugin config: `token_endpoint` or `issuer_url` (mutually exclusive), - `inst-aai-11`
      `client_id_ref`, `client_secret_ref` and the optional space-separated `scopes`
   2. [x] - `p1` - Obtain the token through `cpt-cf-oagw-algo-token-cache` - `inst-aai-12`
   3. [x] - `p1` - Set `Authorization` to `Bearer <token>` on the outbound request - `inst-aai-13`
5. [x] - `p1` - **CATCH** a rejection the plugin raises - `inst-aai-14`
   1. [x] - `p1` - **RETURN** it as the `401` AuthenticationFailed outcome, or the `500` SecretNotFound - `inst-aai-15`
      outcome when the cause was the credential resolution, with no credential material in the detail -

6. [x] - `p1` - Treat the injection as the only write this feature makes to the outbound credential - `inst-aai-16`
   surface: no credential is written to the context, a log line or a response
7. [x] - `p1` - **RETURN** the mutated header set and query string, or the outcome to map - `inst-aai-17`

### OAuth2 Token Cache Lookup and Fill

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-token-cache`

**Input**: the request context (its tenant and subject identifiers from the security context), the
client-auth method of the resolved plugin variant, the plugin config pairs, and the gear-level
`token_cache_ttl_secs` and `token_cache_capacity`.

**Output**: a verified cached token, or a freshly fetched token placed in the cache, or the `401`
outcome to map.

**Steps**:

1. [x] - `p1` - Build the cache key as `subject_tenant_id:subject_id:auth_method_tag:config_hash`, - `inst-atc-01`
   where `config_hash` is a deterministic hash of the plugin's config pairs sorted by key, so two
   upstreams that differ only in `scopes` or in the token endpoint get different entries
2. [x] - `p1` - Look the key up in the plugin's `pingora-memory-cache` instance sized to - `inst-atc-02`
   `token_cache_capacity`
3. [x] - `p1` - **IF** the cache returns an entry - `inst-atc-03`
   1. [x] - `p1` - Verify that the stored `CachedToken.key` equals the lookup key; a mismatch is - `inst-atc-04`
      treated as a miss, never as a hit, so a hash collision cannot serve another tenant's or another
      subject's token
   2. [x] - `p1` - **IF** the key matches - `inst-atc-05`
      1. [x] - `p1` - **RETURN** the cached token with no credstore call and no IdP call - `inst-atc-06`
4. [x] - `p1` - **ELSE** resolve `client_id_ref` and `client_secret_ref` with - `inst-atc-07`
   `cpt-cf-oagw-algo-cred-resolve`
5. [x] - `p1` - Fetch the token once with `toolkit_auth::oauth2::fetch_token`, authenticating the - `inst-atc-08`
   client in the token request body for the Form variant and with `Authorization: Basic` and the
   base64-encoded `client_id:client_secret` for the Basic variant, requesting the configured `scopes`,
   with no background watcher task spawned
6. [x] - `p1` - **TRY** the fetch - `inst-atc-09`
7. [x] - `p1` - **CATCH** a fetch failure, an IdP error status or a response without a usable token - `inst-atc-10`
   1. [x] - `p1` - Cache nothing, drop the credentials, and **RETURN** the `401` outcome; the next - `inst-atc-11`
      request for the same key retries the IdP
8. [x] - `p1` - **ELSE** compute the cache TTL as `min(token_cache_ttl_secs, expires_in - 30s)` - - `inst-atc-12`

9. [x] - `p1` - **IF** the IdP reported an `expires_in` of at most 30 seconds - `inst-atc-13`
   1. [x] - `p1` - Inject the token for this request and cache nothing, because the safety margin - `inst-atc-14`
      leaves no usable lifetime
10. [x] - `p1` - **ELSE** store the `CachedToken` wrapper carrying the original key and the token as - `inst-atc-15`
    `SecretString` under the computed TTL
11. [x] - `p1` - Keep the cache free of any invalidation hook: a revoked or rotated token stays cached - `inst-atc-16`
    until its TTL expires, which is the recorded consequence of having no invalidation mechanism -

12. [x] - `p1` - **RETURN** the token for injection, or the `401` outcome - `inst-atc-17`

### RequiredHeaders Guard Decision

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-required-headers`

**Input**: the guard's plugin config (`required_request_headers`, `required_response_headers`), the
phase being executed, and the header set of the request or of the upstream response.

**Output**: Allow, or the Reject outcome with the phase-specific status and the first missing header
name.

**Steps**:

1. [x] - `p1` - Read the config key for the phase: `required_request_headers` in the request phase, - `inst-arh-01`
   `required_response_headers` in the response phase; the two keys are independent and configuring one
   does not affect the other phase
2. [x] - `p1` - **IF** the key is absent, or its value is blank after trimming - `inst-arh-02`
   1. [x] - `p1` - **RETURN** Allow for that phase as a fail-open no-op, including for an all-blank - `inst-arh-03`
      value such as `", , ,"`
3. [x] - `p1` - Parse the value: split on `,`, trim each entry, lowercase it, and drop empty entries - - `inst-arh-04`

4. [x] - `p1` - **FOR EACH** required header name in the parsed order - `inst-arh-05`
   1. [x] - `p1` - Scan the phase's header set for that name case-insensitively, checking presence only - `inst-arh-06`
      and never reading a value
   2. [x] - `p1` - **IF** the name is absent - `inst-arh-07`
      1. [x] - `p1` - **RETURN** Reject naming only that first missing header, with status `400` and - `inst-arh-08`
         error code REQUIRED_HEADER_MISSING in the request phase and status `502` with the same error
         code in the response phase; the request-phase rejection maps onto the canonical
         `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` type of entry 2.4's "Route match and
         request validation → `400` → ValidationError" row and the response-phase rejection onto
         `gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1`, the type of entry 2.4's "Upstream
         call → `502` → DownstreamError" row, each carrying REQUIRED_HEADER_MISSING in its problem body

5. [x] - `p1` - **RETURN** Allow when every required name is present - `inst-arh-09`

### RequestId Transform

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-request-id`

**Input**: the request context with its inbound header set and correlation identifier, and in the
response phase the upstream response header set.

**Output**: the header set with `X-Request-ID` set on the outbound request and on the response.

**Steps**:

1. [ ] - `p1` - In the request phase, read `X-Request-ID` from the inbound request - `inst-ari-01`
2. [ ] - `p1` - **IF** the header is present with a non-empty value - `inst-ari-02`
   1. [ ] - `p1` - Propagate that value unchanged to the outbound request - `inst-ari-03`
3. [ ] - `p1` - **ELSE** generate a fresh identifier and set it as the outbound `X-Request-ID` - `inst-ari-04`
4. [ ] - `p1` - Record the identifier on the request context, which is the correlation identifier the - `inst-ari-05`
   pipeline already carries and entry 2.7 logs
5. [ ] - `p1` - In the response phase, set `X-Request-ID` on the response to the same identifier the - `inst-ari-06`
   request carried, so a caller can correlate a response with its request
6. [ ] - `p1` - Apply no other mutation: the transform does not rewrite, truncate or validate the value - `inst-ari-07`
   it propagates
7. [ ] - `p1` - **RETURN** the mutated header sets - `inst-ari-08`

### Rate Limit Scope and Effective Cap

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-rate-limit`

**Input**: the effective rate-limit configuration (its `sharing` mode, `sustained` rate and window,
`burst`, `scope`, `strategy`, `cost` and `response_headers`), the enforced-ancestor rate-limit set the
merged configuration carries, the security context, the peer address of the inbound connection and the
matched route identity.

**Output**: the bucket key, the effective limit and capacity for that key, and the per-request cost.

**Steps**:

1. [x] - `p1` - Convert the sustained configuration into a refill rate in tokens per second: - `inst-arl-01`
   `sustained.rate` divided by the duration of `sustained.window` in `second|minute|hour|day` -

2. [x] - `p1` - Take the burst capacity from `burst.capacity` and default it to `sustained.rate` when - `inst-arl-02`
   the field is absent
3. [x] - `p1` - Apply the inheritance rule `min(ancestor.enforced, descendant)`: the effective limit is - `inst-arl-03`
   the descendant's own limit capped by the effective limit of every ancestor in the walked chain that
   is configured with `sharing: enforce`, so shadowing cannot lift an enforced ancestor's bound
4. [x] - `p1` - Leave a `private` ancestor out of the computation and take an `inherit` ancestor's limit - `inst-arl-04`
   as the descendant's when the descendant declares none, so the effective cap is the minimum over the
   enforced set plus the descendant's own bound
5. [x] - `p1` - Resolve the scope key from `scope`: `global` keys one bucket per process, `tenant` keys - `inst-arl-05`
   on the security context's subject tenant identifier, `user` keys on its subject identifier, `ip`
   keys on the peer address of the inbound connection, and `route` keys on the matched upstream and
   route identity
6. [x] - `p1` - **IF** the scope's identity component is unavailable (a request with no peer address for - `inst-arl-06`
   `ip`)
   1. [x] - `p1` - Fall back to the `tenant` scope key and record the fallback on the request context, - `inst-arl-07`
      so a request is never left unaccounted
7. [x] - `p1` - Compose the bucket key as the rate-limit configuration identity, the scope and the scope - `inst-arl-08`
   id, so two upstreams with equal limits do not share a bucket
8. [x] - `p1` - Take the per-request cost from `cost`, defaulting to `1`, as the number of tokens the - `inst-arl-09`
   request consumes
9. [x] - `p1` - **RETURN** the bucket key, the effective limit, the capacity and the cost - `inst-arl-10`

### Token Bucket Acquisition

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-bucket-consume`

**Input**: the bucket key, the effective limit and capacity, the per-request cost, and the configured
strategy.

**Output**: an admission with the remaining tokens, or a rejection with the `Retry-After` and
rate-limit header values, or a queued or degraded disposition.

**Steps**:

1. [ ] - `p1` - Look up the bucket for the key in the process-local bucket table, creating it full at - `inst-abc-01`
   the effective capacity on first use
2. [ ] - `p1` - Refill the bucket: add the elapsed time since the last update multiplied by the refill - `inst-abc-02`
   rate, capped at the capacity, and record the update time
3. [ ] - `p1` - **IF** the bucket holds at least the request's cost - `inst-abc-03`
   1. [ ] - `p1` - Subtract the cost, keep the fractional remainder, and **RETURN** the admission with - `inst-abc-04`
      the remaining tokens floored to a whole number
4. [ ] - `p1` - **ELSE** compute the shortfall: the time until the bucket can satisfy the cost, and the - `inst-abc-05`
   time until the bucket is full
5. [ ] - `p1` - Derive `Retry-After` as the whole seconds until the bucket can satisfy the cost, rounded - `inst-abc-06`
   up, and `X-RateLimit-Reset` as the Unix epoch second at which the bucket returns to full capacity -

6. [ ] - `p1` - **IF** the strategy is `reject` - `inst-abc-07`
   1. [ ] - `p1` - **RETURN** the rejection with `Retry-After`, and with `X-RateLimit-Limit`, - `inst-abc-08`
      `X-RateLimit-Remaining` and `X-RateLimit-Reset` when `response_headers` is true
7. [ ] - `p1` - **ELSE IF** the strategy is `queue` - `inst-abc-09`
   1. [ ] - `p1` - Place the request in the bounded in-process wait for this bucket, bounded by the - `inst-abc-10`
      queue depth and by the request's remaining budget, and wake it when the refill satisfies the cost -

   2. [ ] - `p1` - **IF** a bound is exceeded first - `inst-abc-11`
      1. [ ] - `p1` - Remove the request from the wait and **RETURN** the same rejection as `reject` - - `inst-abc-12`

8. [ ] - `p1` - **ELSE** (`strategy: degrade`) - `inst-abc-13`
   1. [ ] - `p1` - **RETURN** the degraded disposition without consuming tokens, leaving the bucket - `inst-abc-14`
      untouched for the requests that can be satisfied
9. [ ] - `p1` - Keep the check free of I/O and of any lock held across an await, so a rate-limit check - `inst-abc-15`
   stays inside the sub-millisecond budget ADR 0003 states
10. [ ] - `p1` - **RETURN** the disposition of the request - `inst-abc-16`

### CORS Actual-Request Enforcement

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-cors-enforce`

**Input**: the effective `cors` configuration (`enabled`, `allowed_origins`, `allowed_methods`,
`expose_headers`, `allow_credentials`), the request's `Origin` header, method and header set.

**Output**: an allow decision with the CORS headers the response phase must add, or the `403` outcome
to map.

**Steps**:

1. [x] - `p1` - Treat the request as a CORS request only when `cors.enabled` is true on the effective - `inst-ace-01`
   configuration and the request carries an `Origin` header
2. [x] - `p1` - Match the origin exactly against `allowed_origins`, comparing scheme, host and port, so - `inst-ace-02`
   a different port or a different protocol is a different origin, and treating `*` as a match for any
   origin
3. [x] - `p1` - Apply no pattern, suffix or regex origin matching, so a crafted origin cannot match by - `inst-ace-03`
   construction
4. [x] - `p1` - **IF** the origin does not match - `inst-ace-04`
   1. [x] - `p1` - **RETURN** the `403` outcome with problem type - `inst-ace-05`
      `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1`, a detail naming the origin, and
      `Vary: Origin`
5. [x] - `p1` - **ELSE IF** the request method is not in `allowed_methods` - `inst-ace-06`
   1. [x] - `p1` - **RETURN** the `403` outcome with - `inst-ace-07`
      `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`, a detail naming the method, and
      `Vary: Origin`
6. [x] - `p1` - **ELSE** compute the response headers: `Access-Control-Allow-Origin` set to the request - `inst-ace-08`
   origin, or to `*` when the configuration's only allowed origin is the wildcard;
   `Access-Control-Expose-Headers` from `expose_headers`; and `Access-Control-Allow-Credentials: true`
   only when `allow_credentials` is true and the matched origin is an exact origin
7. [x] - `p1` - Add `Vary: Origin` to every response to a request that carried `Origin`, allowed or - `inst-ace-09`
   rejected, so a shared cache cannot serve one origin's response to another
8. [x] - `p1` - Treat the `allow_credentials` with `*` combination as impossible here: entry 2.2 rejects - `inst-ace-10`
   it at management validation, and this feature never evaluates it
9. [x] - `p1` - Handle no preflight: an `OPTIONS` request with `Origin` and - `inst-ace-11`
   `Access-Control-Request-Method` was already answered with the permissive `204` of
   `cpt-cf-oagw-flow-proxy-preflight` and never reaches this decision
10. [x] - `p1` - **RETURN** the allow decision with its headers, or the `403` outcome - `inst-ace-12`

### Response-Phase Plugin Execution

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-response-phase`

**Input**: the upstream response with its header set, or the mapped error context when the call failed,
the resolved response-phase chain, the CORS decision of the request phase, and the rate-limit outcome.

**Output**: the mutated response header set or error context, or the `502` / `500` outcome to map.

**Steps**:

1. [x] - `p1` - **IF** the response was handed to the entry-2.6 streaming path - `inst-arp-01`
   1. [x] - `p1` - **RETURN** without executing any response-phase plugin - `inst-arp-02`
2. [x] - `p1` - **IF** the upstream call succeeded - `inst-arp-03`
   1. [x] - `p1` - Run the response half of the bound guards with - `inst-arp-04`
      `cpt-cf-oagw-algo-required-headers` against the upstream response headers; a missing header is
      **RETURN**ed as the `502` outcome with the upstream body discarded
   2. [x] - `p1` - Run the bound transforms that declare the response phase, in the same - `inst-arp-05`
      upstream-before-route order the request phase used
   3. [x] - `p1` - Add the CORS headers the request-phase decision computed, including `Vary: Origin` - - `inst-arp-06`

3. [x] - `p1` - **ELSE** run the bound transforms that declare the error phase against the mapped error - `inst-arp-07`
   context, in the same order
4. [x] - `p1` - Attach the rate-limit headers to a `429` response when `response_headers` is true, and - `inst-arp-08`
   `Retry-After` on every throttled response
5. [x] - `p1` - **TRY** each transform invocation inside the request's remaining budget - `inst-arp-09`
6. [x] - `p1` - **CATCH** a transform failure or a timeout - `inst-arp-10`
   1. [x] - `p1` - Discard the partial mutation and **RETURN** the failure to the pipeline as a mapped - `inst-arp-11`
      gateway error, never as a partially transformed body
7. [x] - `p1` - Carry no credential material onto the response: a response header set is never a place - `inst-arp-12`
   a resolved secret reaches
8. [x] - `p1` - **RETURN** the mutated response header set or error context, or the outcome to map - `inst-arp-13`

## 4. States (CDSL)

Two entities in this feature have an explicit lifecycle: a cached OAuth2 access token, and the
rate-limit decision for one request. Both are process-local, both are created and consumed inside a
single request's lifetime or a cache TTL, and neither is persisted.

### Cached OAuth2 Access Token State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-token-cache`

**States**: Empty, Cached, Expired

**Initial State**: Empty

**Transitions**:

1. [x] - `p1` - **FROM** Empty **TO** Cached **WHEN** the token fetch succeeds and the computed TTL - `inst-stc-01`
   `min(token_cache_ttl_secs, expires_in - 30s)` is positive, so the `CachedToken` wrapper is stored
   under the request's cache key
2. [x] - `p1` - **FROM** Cached **TO** Expired **WHEN** the stored TTL elapses and the entry is still - `inst-stc-02`
   present, which is the lazy-expiry state the underlying cache keeps it in until the next lookup
   observes it
3. [x] - `p1` - **FROM** Expired **TO** Empty **WHEN** the next lookup observes the elapsed TTL and - `inst-stc-03`
   reclaims the entry, so the following request performs a fresh fetch. This transition and the refill
   below can both fire on the same lookup of an expired entry, so their guards are checked in a fixed
   order — expiry first, then capacity — and the first guard that matches wins: the elapsed TTL is
   always observed as expiry, and capacity is consulted only afterwards, on the refill that follows.
   This transition wins whenever no fresh token is stored under the key
4. [x] - `p1` - **FROM** Expired **TO** Cached **WHEN** a lookup on the expired entry triggers a fresh - `inst-stc-04`
   fetch that succeeds and is stored under the same key. This is the second guard of the ordered pair
   above: it is reached only after the expiry guard has matched, and it wins over the Expired-to-Empty
   path only when that fetch succeeds with a positive computed TTL, so an expired entry is either
   reclaimed or refilled on a lookup, never both
5. [x] - `p1` - **FROM** Cached **TO** Empty **WHEN** the cache evicts the entry under the - `inst-stc-05`
   `token_cache_capacity` bound, which zeroes the `SecretString` on drop
6. [x] - `p1` - **FROM** Empty **TO** Empty **WHEN** the fetch fails, the IdP returns no usable token, - `inst-stc-06`
   or the token's `expires_in` is at most 30 seconds: nothing is stored, and the next request for the
   key retries the IdP
7. [x] - `p1` - **FROM** Cached **TO** Empty **WHEN** a lookup of the request's cache key finds an - `inst-stc-07`
   entry whose stored `CachedToken.key` does not equal that key: the mismatch is treated as a miss and
   the entry is never served, the request continues down the fetch path of the Empty state, and the
   mismatched stale entry is left in place until its own expiry or eviction rather than reclaimed
   here, which is the same no-invalidation limitation every cached entry in this cache is subject to


The transition set above is closed. A cached entry is never shared across cache keys, so there is no
transition that hands a token to another tenant, subject, client-auth method or configuration; an
entry whose stored key does not match the lookup key takes the Cached-to-Empty mismatch transition
above rather than being served, and the stale entry it leaves behind is reclaimed only by its own
expiry or by eviction. No transition exists out of a terminal read (a lookup that returns a Cached
entry leaves the state machine in Cached), no state is persisted across a process restart — a restart
returns the machine to Empty — and no other transition, including a manual invalidation or an
event-driven eviction, exists in this feature.

### Rate Limit Decision State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-rate-limit-decision`

**States**: Pending, Queued, Admitted, Degraded, Rejected

**Initial State**: Pending

**Transitions**:

1. [x] - `p1` - **FROM** Pending **TO** Admitted **WHEN** the bucket can satisfy the request's cost, so - `inst-srl-01`
   the tokens are consumed and the chain continues
2. [x] - `p1` - **FROM** Pending **TO** Queued **WHEN** the bucket cannot satisfy the cost and the - `inst-srl-02`
   configured strategy is `queue`, so the request enters the bounded in-process wait
3. [x] - `p1` - **FROM** Queued **TO** Admitted **WHEN** the refill satisfies the cost inside both the - `inst-srl-03`
   queue-depth bound and the request's remaining budget
4. [x] - `p1` - **FROM** Queued **TO** Rejected **WHEN** a bound is exceeded first, producing the same - `inst-srl-04`
   `429` as the `reject` strategy
5. [x] - `p1` - **FROM** Pending **TO** Rejected **WHEN** the bucket cannot satisfy the cost and the - `inst-srl-05`
   configured strategy is `reject`
6. [x] - `p1` - **FROM** Pending **TO** Degraded **WHEN** the bucket cannot satisfy the cost and the - `inst-srl-06`
   configured strategy is `degrade`, so the request proceeds without consuming tokens

The transition set above is closed and applies per request. Admitted, Degraded and Rejected are
terminal for the rate-limit decision: no transition leaves them, a rejected request never becomes
admitted by a retry inside the same request, and a degraded request is not re-evaluated against the
same bucket. Queued is the only intermediate state, and a request cannot move from Queued to Degraded
or from Queued back to Pending. A restart discards every Pending and Queued decision together with the
buckets, so no decision survives the process that made it, and no other transition — including a
re-evaluation after a configuration change mid-request — exists in this feature.

## 5. Definitions of Done

Specific implementation tasks derived from the flows, algorithms and states above.

### Plugin Chain Execution Order

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-order`

The system **MUST** execute the request-phase plugin chain in the order Auth, then Guards, then
Transform (request), then the upstream call owned by the pipeline, then Transform (response and error),
with upstream-attached plugins before route-attached plugins inside every stage and in binding-position
order within each level, running the auth plugin exactly once per request and stopping the stage at the
first rejection. The system **MUST** run this chain inside the request-phase and response-phase hook
points `cpt-cf-oagw-dod-plugin-hook-points` defines, consuming the merged configuration that
`cpt-cf-oagw-algo-config-merge` produced, and **MUST NOT** register an endpoint, re-declare the
pipeline's ordering or execute any plugin outside those hook points.

**Implements**:
- `cpt-cf-oagw-flow-plugin-chain`
- `cpt-cf-oagw-algo-plugin-chain`

**Constraints**: None

**Principles**: `cpt-cf-oagw-principle-plugin-immutable`

**Touches**:
- API: none — the behaviour of `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`, registered by entry 2.4
- DB: none — the chain reads the merged configuration and writes nothing
- Context: `RequestContext`, `ResponseContext` (entry-2.4 pipeline context, passed through, not owned)
- Entities: `PluginsConfig`, `AuthConfig`

### Plugin Resolution through the Entry-2.3 Registries

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-auth-chain`

The system **MUST** resolve every `plugins.items[].plugin_ref` binding and every `auth.plugin_type`
identifier through the `AuthPluginRegistry`, `GuardPluginRegistry` and transform registry that
`cpt-cf-oagw-flow-plugin-catalog-bootstrap` constructs, treating a definition as immutable and
read-only, and **MUST** return `503` PluginNotFound, with no partial execution and no upstream call,
when an identifier resolves in no registry of its type — including the catalog-only identifiers
`basic`, `bearer`, `timeout`, `cors`, `logging` and `metrics`, and a UUID-backed custom Starlark
definition that has no runtime instance because no interpreter exists (DECOMPOSITION assumption 4).
The system **MUST** bound every plugin invocation by the request's remaining `proxy_timeout_secs`
budget so a plugin can never extend the request deadline, and **MUST NOT** add a separate
plugin-timeout configuration key or a per-plugin circuit breaker.

**Implements**:
- `cpt-cf-oagw-flow-plugin-chain`
- `cpt-cf-oagw-algo-plugin-chain`

**Constraints**: None

**Touches**:
- API: none — `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` (`503` PluginNotFound)
- DB: none — the registries are the in-process structures entry 2.3 builds
- Context: `RequestContext` (entry-2.4 pipeline context, passed through, not owned)
- Entities: `PluginsConfig`

### Built-in Auth Plugin Credential Injection

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-auth-plugins`

The system **MUST** inject credentials for the resolvable set of auth plugins: the no-op plugin
injects nothing and always succeeds; the API key plugin resolves its `cred://` reference and injects
the secret either as the configured request header, `X-API-Key` when the config names none, or as the
`api_key` query parameter; and both OAuth2 client credentials variants resolve `client_id_ref` and
`client_secret_ref`, authenticate to the token endpoint with the client credentials in the request body
for `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1` and as `Authorization: Basic`
with the base64-encoded `client_id:client_secret` for
`gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1`, requesting the configured
space-separated `scopes`, and both inject `Authorization: Bearer <token>` on the proxied request. The
system **MUST** inject nothing when the effective `auth` configuration names no resolvable plugin, and
**MUST** fail the request with `401` AuthenticationFailed when the plugin rejects.

**Implements**:
- `cpt-cf-oagw-flow-auth-injection`
- `cpt-cf-oagw-algo-auth-inject`

**Constraints**: None

**Principles**: `cpt-cf-oagw-principle-cred-isolation`

**Touches**:
- API: none — the outbound request header set and query string of the proxy request
- DB: none
- Context: `RequestContext` (entry-2.4 pipeline context, passed through, not owned)
- Entities: `AuthConfig`

### OAuth2 Token Cache

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-token-cache`

The system **MUST** cache OAuth2 client credentials tokens inside the plugin in a
`pingora-memory-cache` bounded by `token_cache_capacity`, keyed by
`subject_tenant_id:subject_id:auth_method_tag:config_hash` with `config_hash` computed deterministically
over the plugin's config pairs sorted by key, storing a `CachedToken` wrapper that carries the original
key and verifying it on every hit so a hash collision is served as a miss and never as another tenant's
token. The system **MUST** cache each token for `min(token_cache_ttl_secs, expires_in - 30s)`, **MUST
NOT** cache a token whose `expires_in` is at most 30 seconds, **MUST NOT** cache a failed fetch so the
next request retries the IdP, **MUST** obtain the token with the one-shot
`toolkit_auth::oauth2::fetch_token` and spawn no background watcher task, and **MUST** hold the cached
token as `SecretString` so eviction zeroes it. The system **MUST NOT** add an event-driven or manual
cache invalidation: a revoked or rotated token remains cached until its TTL expires.

**Implements**:
- `cpt-cf-oagw-flow-auth-injection`
- `cpt-cf-oagw-algo-token-cache`
- `cpt-cf-oagw-state-token-cache`

**Constraints**: None

**Principles**: `cpt-cf-oagw-principle-cred-isolation`

**Touches**:
- API: none — the token cache has no surface of its own
- DB: none — the cache is process-local memory
- Entities: `AuthConfig`, `CachedToken`

### Credential Isolation on the Request Path

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-cred-isolation`

The system **MUST** resolve every credential from the credstore by `cred://` UUID reference at request
time through the `credstore-sdk` `CredStoreClientV1` contract `cpt-cf-oagw-contract-cred-store`, scoped
to the calling tenant so a reference that names another tenant's credential is indistinguishable from
an unknown one, and **MUST** fail the request with `500` SecretNotFound when a reference cannot be
resolved. The system **MUST NOT** log a resolved secret, place one in an error body, a response header,
the request context entry 2.7 records or any API response, **MUST** name at most the configuration key
in a failure detail, **MUST** drop a resolved value as soon as the injection or the token fetch that
consumed it completes, and **MUST NOT** read secret material from the store snapshot the pipeline hands
it, which carries `cred://` references only.

**Implements**:
- `cpt-cf-oagw-flow-auth-injection`
- `cpt-cf-oagw-algo-cred-resolve`

**Constraints**: None

**Principles**: `cpt-cf-oagw-principle-cred-isolation`

**Touches**:
- API: none — no credential appears on any response surface
- DB: none — no credential is persisted anywhere
- Context: `RequestContext` (entry-2.4 pipeline context, passed through, not owned)
- Entities: `AuthConfig`

### RequiredHeaders Guard

- [ ] `p2` - **ID**: `cpt-cf-oagw-dod-required-headers`

The system **MUST** implement the RequiredHeaders guard as a stateless check that parses
`required_request_headers` and `required_response_headers` independently by splitting on `,`, trimming,
lowercasing and dropping empty entries, that checks each required name case-insensitively for presence
only and never inspects a value, that reports only the first missing header, and that fails open as a
no-op for its phase when the key is absent or blank. A missing request header **MUST** return `400`
with the error code REQUIRED_HEADER_MISSING and a missing response header **MUST** return `502` with
the same error code, both mapped by the pipeline. Each rejection is carried by a canonical row entry
2.4 already declares, and neither adds a row to that table: the request-phase `400` is carried by
`gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`, the GTS type of entry 2.4's "Route match and
request validation → `400` → ValidationError" row, and the response-phase `502` by
`gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1`, the GTS type of entry 2.4's "Upstream call →
`502` → DownstreamError" row; in both cases REQUIRED_HEADER_MISSING is the error code in the problem
body.

**Implements**:
- `cpt-cf-oagw-flow-plugin-chain`
- `cpt-cf-oagw-flow-response-phase`
- `cpt-cf-oagw-algo-required-headers`

**Constraints**: None

**Touches**:
- API: none — `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` (`400` and `502` REQUIRED_HEADER_MISSING)
- DB: none
- Context: `RequestContext`, `ResponseContext` (entry-2.4 pipeline context, passed through, not owned)
- Entities: `PluginsConfig`

### RequestId Transform

- [ ] `p2` - **ID**: `cpt-cf-oagw-dod-request-id`

The system **MUST** implement the RequestId transform for the request and response phases: an inbound
`X-Request-ID` is propagated unchanged to the outbound request, an absent one is generated and set, the
identifier is recorded on the request context as the correlation identifier, and the response carries
the same `X-Request-ID` so a caller can correlate a response with its request. The system **MUST NOT**
rewrite, truncate or validate the propagated value.

**Implements**:
- `cpt-cf-oagw-flow-plugin-chain`
- `cpt-cf-oagw-flow-response-phase`
- `cpt-cf-oagw-algo-request-id`

**Constraints**: None

**Touches**:
- API: none — the `X-Request-ID` header on the proxied request and on the response
- DB: none
- Context: `RequestContext`, `ResponseContext` (entry-2.4 pipeline context, passed through, not owned)
- Entities: `PluginsConfig`

### Token Bucket Rate Limiting

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-rate-limit`

The system **MUST** enforce the effective rate limit with an in-process token bucket per
`cpt-cf-oagw-adr-rate-limiting`: the sustained `rate` and `window` in `second|minute|hour|day` convert
to a refill rate in tokens per second, `burst.capacity` defaults to `sustained.rate` when absent, the
scope is one of `global|tenant|user|ip|route` with `tenant` as the default and a documented key for
each, `cost` is the number of tokens the request consumes and defaults to `1`, and the effective limit
is `min(ancestor.enforced, descendant)` over the enforced-ancestor set the merged configuration carries,
so a descendant cannot raise an enforced ancestor's limit and shadowing cannot lift it. The system
**MUST** keep a check free of I/O and inside the sub-millisecond budget the ADR states, **MUST** keep
the buckets in process memory so a restart resets them, and **MUST NOT** synchronize them across nodes
or read any `rate_limit_sync` configuration.

**Implements**:
- `cpt-cf-oagw-flow-rate-limit-enforcement`
- `cpt-cf-oagw-algo-rate-limit`
- `cpt-cf-oagw-algo-bucket-consume`

**Constraints**: None

**Principles**: `cpt-cf-oagw-principle-plugin-immutable`

**Touches**:
- API: none — the rate limit is enforced inside the request-phase hook of the proxy request
- DB: none — bucket state is process-local
- Entities: `RateLimitConfig`, `TokenBucket`

### 429 Semantics and Rate Limit Response Headers

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-rate-headers`

The system **MUST** reject a request whose bucket cannot satisfy its cost with `429` RateLimitExceeded
carrying `Retry-After` computed from the time the bucket needs to satisfy the cost, and, when
`response_headers` is true, the headers `X-RateLimit-Limit`, `X-RateLimit-Remaining` and
`X-RateLimit-Reset`, with `X-OAGW-Error-Source: gateway` stamped by the pipeline and no upstream call
made. The system **MUST** apply the configured strategy: `reject` as above, `queue` by holding the
request in a bounded in-process wait bounded by the queue depth and the request's remaining budget and
producing the same `429` when a bound is exceeded, and `degrade` by admitting the request without
consuming tokens and recording the degradation on the request context. The system **MUST NOT** hold a
request without a bound, re-evaluate a terminal decision inside the same request, or emit a
rate-limit header on a request that was not throttled by this feature's evaluation.

**Implements**:
- `cpt-cf-oagw-flow-rate-limit-enforcement`
- `cpt-cf-oagw-algo-bucket-consume`
- `cpt-cf-oagw-state-rate-limit-decision`

**Constraints**: None

**Touches**:
- API: none — `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` (`429` RateLimitExceeded with
  `Retry-After` and the `X-RateLimit-*` headers)
- DB: none
- Context: `ResponseContext` (entry-2.4 pipeline context, passed through, not owned)
- Entities: `RateLimitConfig`, `TokenBucket`

### Actual-Request CORS Enforcement

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-cors-enforcement`

The system **MUST** enforce the effective CORS configuration on an actual cross-origin request after
upstream resolution and before forwarding, per `cpt-cf-oagw-adr-cors`: a request is a CORS request only
when `cors.enabled` is true and it carries an `Origin`; the origin is matched exactly and is both port-
and protocol-sensitive with `*` matching any origin and no pattern or regex matching; a disallowed
origin returns `403` with `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` and a
disallowed method returns `403` with
`gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`, both before any upstream call;
`Access-Control-Allow-Origin` echoes the request origin, or is `*` for a wildcard-only configuration;
`Access-Control-Expose-Headers` is set from `expose_headers`;
`Access-Control-Allow-Credentials: true` is set only when `allow_credentials` is true and the origin
matched exactly; and `Vary: Origin` is always included on a response to a request that carried
`Origin`. The system **MUST** treat `allow_credentials` combined with a wildcard origin as impossible
here because entry 2.2 rejects it at management validation, and **MUST NOT** handle a preflight, whose
permissive `204` is `cpt-cf-oagw-flow-proxy-preflight`'s.

**Implements**:
- `cpt-cf-oagw-flow-cors-actual-request`
- `cpt-cf-oagw-algo-cors-enforce`

**Constraints**: None

**Touches**:
- API: none — the CORS response headers and the two `403` types on actual proxy requests
- DB: none
- Context: `RequestContext`, `ResponseContext` (entry-2.4 pipeline context, passed through, not owned)
- Entities: `AuthConfig`

### Test Layering for the Auth Chain and Rate Limiting

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-auth-test-coverage`

The system **MUST** cover this feature with in-crate Rust tests only — unit tests inside `#[cfg(test)]`
modules per layer for the chain order and the upstream-before-route composition, the `503` outcomes for
catalog-only and custom identifiers, the plugin-deadline bound, each auth plugin's injection target and
header, the token cache key, TTL rule, key verification and no-caching rules, the credstore failure
classes, the RequiredHeaders parse and both phase statuses, the RequestId propagation, the bucket
refill and cost arithmetic, the scope keys and the inheritance cap, the three strategies and the
rate-limit headers, and the CORS origin and method decisions — and integration tests under the crate's
`tests/` directory that boot the gear router and drive the proxy endpoint against a stub upstream
listener, a stub credstore and a mock IdP provided by the test harness, asserting the `401`, `400`,
`403`, `429`, `500` and `503` outcomes, the absence of credential material in every log line and error
body, and the rate-limit and CORS headers. The system **MUST NOT** add an e2e suite under
`testing/e2e/gears/oagw/` (DECOMPOSITION assumption 5).

**Implements**:
- `cpt-cf-oagw-flow-plugin-chain`
- `cpt-cf-oagw-flow-auth-injection`
- `cpt-cf-oagw-flow-rate-limit-enforcement`
- `cpt-cf-oagw-flow-cors-actual-request`
- `cpt-cf-oagw-flow-response-phase`

**Constraints**: None

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` (asserted by the integration tests)
- DB: none — the tests assert behaviour, not storage
- Entities: `PluginsConfig`, `AuthConfig`, `RateLimitConfig`, `TokenBucket`

## 6. Acceptance Criteria

- [x] A request whose upstream binds an auth plugin, a guard and a request transform executes them in the order auth, guard, transform, with upstream bindings before route bindings at every stage, and the request reaches the upstream carrying every injected credential and applied transform; an upstream with no resolvable auth plugin configured forwards the request with no injected credential, and an upstream bound to the no-op plugin does the same explicitly.
- [x] A binding naming `basic`, `bearer`, `timeout`, `cors`, `logging` or `metrics`, or a UUID-backed custom Starlark definition, returns `503` PluginNotFound with `X-OAGW-Error-Source: gateway` and no upstream call is made.
- [x] The API key plugin injects the resolved secret into the configured request header, `X-API-Key` by default, and into the `api_key` query parameter when the config selects query injection.
- [x] Both OAuth2 client credentials variants inject `Authorization: Bearer <token>` on the proxied request; the Form variant authenticates to the token endpoint in the request body and the Basic variant with `Authorization: Basic` carrying the base64 encoding of `client_id:client_secret`.
- [x] A second request with the same tenant, subject, client-auth method and configuration is served from the token cache with no credstore call and no IdP call, and a request with a different tenant, subject, client-auth method or configuration never reuses another entry's token.
- [x] A cached token's TTL is `min(token_cache_ttl_secs, expires_in - 30s)`; a token whose `expires_in` is at most 30 seconds is injected for that request and not cached; a failed token fetch is not cached and the next request retries the IdP.
- [x] An unknown, cross-tenant or unreadable `cred://` reference returns `500` SecretNotFound, and an IdP that is unreachable or refuses the grant returns `401` AuthenticationFailed; neither body, nor any log line or request-context field, contains secret material or a resolved credential.
- [x] The chain reads only `cred://` references from the merged configuration the pipeline hands it and never a resolved secret value: the configuration snapshot carries references only, and no step of the chain reads secret material out of it.
- [x] A missing required request header returns `400` with the error code REQUIRED_HEADER_MISSING and only the first missing header named, carried by the canonical `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` type; a missing required response header returns `502` with the same error code and the upstream body discarded, carried by the canonical `gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1` type; an absent or blank configuration no-ops for its phase.
- [x] `X-Request-ID` present on the inbound request is forwarded unchanged and echoed on the response; an absent one is generated, and the response carries the same identifier the request carried.
- [x] A request under a configured rate limit consumes its `cost` from the bucket; a burst up to `burst.capacity` is admitted, and `burst.capacity` defaults to `sustained.rate` when the field is absent.
- [x] A route-level limit stricter than the upstream-level limit governs, and a descendant that declares a higher limit than an `enforce` ancestor is capped at the ancestor's effective limit; shadowing an ancestor does not lift that cap.
- [x] Each of `global`, `tenant`, `user`, `ip` and `route` scopes produces its own bucket, and two upstreams with equal limits do not share one.
- [x] A throttled request under `strategy: reject` returns `429` RateLimitExceeded with `Retry-After`, `X-OAGW-Error-Source: gateway` and no upstream call, and with `X-RateLimit-Limit`, `X-RateLimit-Remaining` and `X-RateLimit-Reset` when `response_headers` is true; with `response_headers` false only `Retry-After` is added.
- [x] Under `strategy: queue` a request is admitted once the refill satisfies its cost inside the bounded wait, and the same `429` is returned when the queue depth or the request's remaining budget is exceeded first.
- [x] Under `strategy: degrade` a request whose bucket is exhausted is admitted without consuming tokens and the degradation is recorded on the request context.
- [x] An actual cross-origin request with a disallowed origin returns `403` with `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1`, and one with a disallowed method returns `403` with `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`, both before any upstream call and both with `Vary: Origin`.
- [x] An allowed cross-origin request is forwarded and its response carries `Access-Control-Allow-Origin` echoing the request origin, the configured `Access-Control-Expose-Headers`, `Access-Control-Allow-Credentials: true` only for an exact origin with `allow_credentials`, and `Vary: Origin`; `https://app.example.com` matches neither `https://app.example.com:8443` nor `http://app.example.com`.
- [x] A request carrying an `Origin` while `cors.enabled` is false is forwarded with no CORS check and no CORS header, and a response-phase hook reached on a streamed exchange applies no response-phase transform to that exchange.
- [x] The rate-limit check adds no I/O and stays inside the sub-millisecond budget of ADR 0003, a restart resets every bucket and every cached token, and no rate-limit counter or token is persisted.
- [x] The in-crate unit and integration tests pass with a stub credstore and a mock IdP provided by the test harness, and no test artifact is added under `testing/e2e/gears/oagw/`.
