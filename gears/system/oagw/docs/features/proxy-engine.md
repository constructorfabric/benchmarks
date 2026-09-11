# Feature: Proxy Engine


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Proxy a Request to an Upstream](#proxy-a-request-to-an-upstream)
  - [Handle a CORS Preflight](#handle-a-cors-preflight)
  - [Distinguish Gateway and Upstream Errors](#distinguish-gateway-and-upstream-errors)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Tenant-Hierarchy Alias Walk and Upstream Selection](#tenant-hierarchy-alias-walk-and-upstream-selection)
  - [Route Matching](#route-matching)
  - [Effective Configuration Merge](#effective-configuration-merge)
  - [Multi-Endpoint Pool Selection and Target-Host Validation](#multi-endpoint-pool-selection-and-target-host-validation)
  - [Request, Body and CORS Validation](#request-body-and-cors-validation)
  - [Header Transformation](#header-transformation)
  - [Upstream Call, Version Negotiation and Timeout](#upstream-call-version-negotiation-and-timeout)
  - [Response Passthrough and Error Mapping](#response-passthrough-and-error-mapping)
  - [Circuit Breaker Evaluation](#circuit-breaker-evaluation)
- [4. States (CDSL)](#4-states-cdsl)
  - [Circuit Breaker State Machine](#circuit-breaker-state-machine)
  - [Proxy Request Context State Machine](#proxy-request-context-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Proxy Endpoint Registration and Permission](#proxy-endpoint-registration-and-permission)
  - [Alias Walk, Shadowing and Enabled Enforcement](#alias-walk-shadowing-and-enabled-enforcement)
  - [Route Matching and Guard Rules](#route-matching-and-guard-rules)
  - [Effective Configuration Merge](#effective-configuration-merge-1)
  - [Multi-Endpoint Pools and Target-Host Selection](#multi-endpoint-pools-and-target-host-selection)
  - [Body Validation, Limits and Smuggling Defence](#body-validation-limits-and-smuggling-defence)
  - [Header Transformation](#header-transformation-1)
  - [Upstream Call, Timeout, Version Negotiation and No-Retry](#upstream-call-timeout-version-negotiation-and-no-retry)
  - [SSRF Posture and Plaintext Gate](#ssrf-posture-and-plaintext-gate)
  - [Circuit Breaker](#circuit-breaker)
  - [CORS Enforcement](#cors-enforcement)
  - [Error Mapping and Error Source Distinction](#error-mapping-and-error-source-distinction)
  - [Plugin Chain Hook Points](#plugin-chain-hook-points)
  - [Streamed Response Handoff](#streamed-response-handoff)
  - [Proxy Hot-Path Latency Budget](#proxy-hot-path-latency-budget)
  - [Test Layering](#test-layering)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-proxy-engine-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-proxy-engine`

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

This feature is the OAGW data plane: the single proxy endpoint
`{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?query]` (gear-relative, DECOMPOSITION assumption 1)
and the one cohesive request pipeline behind it. The pipeline resolves the path alias to an upstream
by walking the tenant hierarchy, matches a route, merges the effective configuration with the
precedence upstream < route < tenant, selects one endpoint from the upstream's pool, validates the
request and its body, transforms the request headers, forwards the request to the upstream over the
crate's existing `toolkit-http`/`pingora-*` client stack under the `proxy_timeout_secs` timeout, and
returns the upstream response as a passthrough — mapping every failure the pipeline can raise onto
the canonical error table with `X-OAGW-Error-Source` on every response. It also owns the request-path
resilience behaviour: per-upstream circuit breaker, adaptive per-host HTTP version negotiation,
HTTP smuggling defence and the SSRF posture of the outbound call. Plugin chain execution (entry 2.5)
and streamed session handling (entry 2.6) hang off hook points this pipeline defines; this feature
defines the hook points, the handoff and the error mapping, not the plugin or streaming behaviour.
The decomposition makes this deliberately the largest package because the request path cannot be
split without losing the pipeline's ordering guarantees.

### 1.2 Purpose

Entry 2.4 turns the control plane's stored configuration into proxied traffic. It comes after entry
2.2 because it resolves an alias to an upstream and matches routes against the store that entry
populates, and it comes before entries 2.5, 2.6 and 2.7 because they all attach to the pipeline this
feature defines: entry 2.5 executes the plugin chain between route match and the upstream call and
maps its failures onto the error table fixed here, entry 2.6 takes over the response at the streamed
handoff point, and entry 2.7 records the audit line and the metrics from the request context this
pipeline produces. This feature owns the request-path half of the decomposition's shared
requirements — request-time resolution of `cpt-cf-oagw-fr-alias-resolution` (entry 2.2 owns
derivation and uniqueness), enforcement of `cpt-cf-oagw-fr-enable-disable` (entry 2.2 owns the CRUD
semantics), the proxy-path mapping of `cpt-cf-oagw-fr-error-codes` (entry 2.6 owns stream errors) and
the body and header validation of `cpt-cf-oagw-nfr-input-validation` (entry 2.2 owns management
request validation). It realizes `cpt-cf-oagw-principle-error-source` and
`cpt-cf-oagw-principle-rfc9457` by classifying every response and returning gateway failures only
through the entry-2.1 mapping layer, `cpt-cf-oagw-principle-no-retry` and
`cpt-cf-oagw-principle-no-cache` by sending exactly one request and never caching a response, and it
implements the request flow of `cpt-cf-oagw-seq-proxy-flow` against the contracts of
`cpt-cf-oagw-interface-api`.

**Requirements**:

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

**Principles**: `cpt-cf-oagw-principle-error-source`, `cpt-cf-oagw-principle-rfc9457`,
`cpt-cf-oagw-principle-no-retry`, `cpt-cf-oagw-principle-no-cache`

**Feature-local deviations and recorded boundaries** (each inherited from the decomposition's
task-level assumptions, from the PRD's own wording, or recorded as a scope boundary this feature
implements against; none is a new decision taken here):

- Gear-relative proxy paths — DECOMPOSITION assumption 1. The endpoint is registered as
  `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` without the `/api` prefix, because the host
  api-gateway nests the gear router under its own `prefix_path`, which is empty in the graded
  configuration. The `/api/oagw/v1/proxy/...` paths in `cpt-cf-oagw-interface-api` and ADR 0001 are
  the absolute form behind an operator gateway and are not what this gear registers. Review owner:
  OAGW component maintainer (`cf-gears-oagw`).
- Plaintext upstream connections are governed by `allow_http_upstream` — DECOMPOSITION assumption 2,
  recorded against `cpt-cf-oagw-constraint-https-only`. An endpoint with `scheme: http` is a legal
  stored target (entry 2.2 accepts it), and the runtime key decides whether a plaintext upstream
  connection is actually attempted: the default `false` fails closed, the graded configuration sets
  `true`. Review owner: OAGW component maintainer, with the security reviewer as second approver.
  Validation: an in-crate test asserts that with the recorded default `allow_http_upstream: false` a
  plaintext endpoint is refused before any connection attempt, and a second test asserts the graded
  configuration admits one.
- `ssrf_policy.enabled` is an operator escape hatch — DECOMPOSITION assumption 7. The key is present
  in the runtime configuration (the graded configuration sets it `false`) and absent from the
  DESIGN's `OagwConfig` surface; setting it to `false` relaxes upstream host validation, while the
  default `true` keeps the DESIGN's SSRF posture unconditional. Two properties stay unconditional in
  both settings: the connect target is always an endpoint taken from the store, and no internal
  gateway header is injected into an outbound request. Review owner: OAGW component maintainer, with
  the security reviewer as second approver. Validation: an in-crate test asserts the default
  `true` enforces host validation unconditionally, and a second test asserts that with `false` the
  outbound request still carries no gateway-internal header and still targets a stored endpoint.
- DNS-resolution validation and IP-pinning rules are out of scope — PRD 4.2 and DESIGN 4.5 both place
  them outside the scope of this design, and no decomposition entry delivers them. The
  DNS-resolution and IP-pinning rules of `cpt-cf-oagw-nfr-ssrf-protection` are therefore not
  implemented here; this feature enforces only the `allow_http_upstream` scheme gate, the
  `ssrf_policy.enabled` escape hatch and the private-address posture (the connect target is always a
  stored endpoint and no gateway-internal header is injected). Review owner: OAGW component
  maintainer.
- Upstream transport arrives through the crate's existing `toolkit-http` and `pingora-*` client stack
  rather than a direct `pingora` gear dependency — DECOMPOSITION assumption 8. No new HTTP client
  dependency is added and no reverse-proxy engine is built here; this feature composes the pipeline
  on top of the client stack the crate already carries. Review owner: OAGW component maintainer.
- No automatic retry of the client request, with connector-level attempts permitted — the PRD wording
  of `cpt-cf-oagw-fr-request-proxy` refines `cpt-cf-oagw-principle-no-retry`: the gateway never
  re-issues the original client request as a whole, while connection or endpoint-level attempts
  performed inside the upstream connector are permitted and are not a re-issue. Review owner: OAGW
  component maintainer. Validation: an in-crate test asserts a failed upstream call surfaces exactly
  one mapped error response and that no second request carrying the original body is issued.
- Disabled shadow stops the alias walk — this feature RECORDS AN AMENDMENT to DECOMPOSITION entry
  2.4's alias-walk wording "the closest enabled upstream wins", which reads as a fall-through to an
  enabled ancestor. The behaviour implemented here is that the closest record holding the alias wins
  and shadows every ancestor record, and a disabled closest record yields `503` with no fall-through
  to an enabled ancestor: that is the behaviour `cpt-cf-oagw-fr-enable-disable` requires, since a
  fall-through would let a descendant's disabled shadow be bypassed silently. The DECOMPOSITION
  sentence is superseded by this reading; the correction is recorded here because the decomposition
  is read-only upstream. Review owner: OAGW component maintainer. Validation: an in-crate test
  asserts a disabled shadowing upstream yields `503` and the ancestor upstream with the same alias is
  not used.
- gRPC has no reachable proxy path — DECOMPOSITION entry 2.2 stores `match.grpc` routes and a `grpc`
  upstream `protocol`, and DESIGN keeps gRPC proxying in Phase 3 with no reachable code path. This
  pipeline matches only HTTP keys (method allowlist and longest path prefix), so a request resolved
  to a `grpc`-protocol upstream finds no HTTP match and returns `404 RouteNotFound`. Review owner:
  OAGW component maintainer. Validation: an in-crate test asserts a request against a `grpc`-protocol
  upstream returns `404` with the route-not-found problem body and issues no upstream call.
- Streamed responses are handed off, not handled — DECOMPOSITION entry 2.4 out of scope. The response
  classification step detects a streamed response (`text/event-stream`, or an upgrade negotiated with
  the upstream) and hands the open upstream exchange to entry 2.6 instead of buffering or
  terminating it; the streamed session behaviour, its lifecycle and its error mapping are entry 2.6's.
  Review owner: OAGW component maintainer. Validation: an in-crate test asserts the classification
  step labels a `text/event-stream` response for the entry-2.6 handoff and that the buffered path is
  not taken for it.

Coverage note: the reference ids cited on the DoDs below that are not carried in the **Requirements**
list above are inherited baselines, not requirements this feature adopts on its own.
`cpt-cf-oagw-constraint-multi-sql` is inherited from dependency entry 2.2, whose logical data model
this feature reads and whose assumption 3 records that it is satisfied at the logical level only;
`cpt-cf-oagw-constraint-toolkit-deploy` is inherited from dependency entry 2.1, which delivers the
gear deployment, the REST wiring and the canonical error mapping every DoD below sits on.
`cpt-cf-oagw-principle-tenant-scope` and `cpt-cf-oagw-principle-cred-isolation` apply through the
shared platform baseline of section 1.4 rather than as feature-local mechanisms: the tenant scoping
is the store-level isolation entry 2.2 owns and this feature exercises through the tenant-scoped
alias walk, and the credential isolation is the entry-2.1/entry-2.5 boundary that keeps this feature
from reading secret material at all. `cpt-cf-oagw-db-schema` is the read-side logical model of the
entry-2.2 store, which this feature only reads and never writes.

**Cross-cutting concerns**:

- Security: the proxy endpoint requires Bearer authentication and the
  `gts.cf.core.oagw.proxy.v1~:invoke` permission of `cpt-cf-oagw-interface-api` for an actual proxy
  request, a detected CORS preflight being dispatched before authentication per ADR 0004; the alias
  walk is tenant-scoped so a caller reaches only its own upstreams and those shared down its chain;
  the SSRF posture validates the connect target, keeps the outbound request free of gateway-internal
  headers and honours `ssrf_policy.enabled`; the smuggling defences reject ambiguous framing before
  the call; and no response ever echoes a resolved credential — credential injection itself is the
  entry-2.5 hook, and this feature never reads secret material. Injection classes beyond the input
  validation above are not applicable here: SQL, XSS and command-injection prevention have no surface
  in a gear that holds no SQL, HTML or shell context, and path-traversal prevention is covered by the
  route-validated path suffix and query, which are forwarded to the upstream verbatim.
- Versioning: the versioning and breaking-change policy of the proxy path is owned by
  `cpt-cf-oagw-interface-proxy-api`, whose recorded policy is a major version bump for a breaking
  change; this feature registers no version of its own and adds no breaking change to that contract.
- Reliability: every upstream call is bounded by `proxy_timeout_secs` and classified into a
  connection, request or idle timeout; the circuit breaker bounds the blast radius of an unhealthy
  upstream; failures are mapped, never swallowed, and the client request is never re-issued. There is
  no persistence to recover: a restart resets the breaker and the round-robin cursors to their
  initial state (in-memory, DECOMPOSITION assumption 3).
- Data integrity: the pipeline is read-only over the configuration snapshots entry 2.2 publishes; a
  request resolves one snapshot for its whole lifetime so alias, route and merged configuration are
  mutually consistent, and a concurrent management write affects only requests that start after the
  snapshot is published.
- Observability: the request context carries the correlation identifier, the selected endpoint, the
  selection method and the error type so entry 2.7 can emit the ADR 0001 audit record and the metric
  families; this feature writes no audit record and registers no metric family of its own
  (DECOMPOSITION assumption 9, entry 2.7).
- Rollback: no persistence and no migration exist, so rollback is the operational act of redeploying
  the previous executable; the only in-gear recovery action is the circuit breaker's own path back to
  a closed state.
- Test layering: coverage is in-crate Rust tests only — unit tests inside `#[cfg(test)]` modules per
  layer for the walk, match, merge, selection, validation, transformation, timeout and breaker logic,
  and integration tests under the crate's `tests/` directory that boot the gear router and drive the
  proxy endpoint against a stub upstream listener provided by the test harness — and the
  `testing/e2e/gears/oagw/` directory is not used (DECOMPOSITION assumption 5).
- Compile-time gate: the proxy endpoint exists in the host executable only when the host feature
  `oagw` is enabled and the crate is linked for inventory registration (entry 2.1's gate); this
  feature adds no gate of its own and no new gear.
- Performance: applicable and owned here. `cpt-cf-oagw-nfr-low-latency` budgets less than 10ms of
  gateway-added latency at p95 excluding the upstream response time, and this pipeline is where that
  budget is spent and measured. The disposition of the budget: alias resolution, route matching,
  configuration merge and header transformation are in-memory operations over the published snapshot
  with no blocking I/O and no allocation on the response path beyond the buffered body the limit
  requires; the endpoint selection keeps a per-upstream round-robin cursor instead of re-scanning the
  pool; the per-host HTTP version cache avoids repeating ALPN negotiation on the hot path; and the
  single upstream call is the only I/O, bounded by `proxy_timeout_secs`. The parts of the budget this
  feature does not own are named: plugin execution and rate-limit evaluation costs belong to the
  entry-2.5 hook, audit logging must stay non-blocking per entry 2.7, and the upstream's own response
  time is excluded from the threshold. The budget is verified by an in-crate measurement test that
  times the pipeline around a stub upstream listener and asserts the p95 of the gateway-added portion.
- Compliance/Privacy: not applicable in this feature — nothing is persisted (assumption 3), no
  personal data is processed, and the pipeline places no request body, query string or header value
  on the response context entry 2.7 logs, so there is no retention, residency or subject-right
  surface here.
- Accessibility: not applicable in this feature — no user-facing interface is authored beyond the
  `application/problem+json` error body contract of entry 2.1, whose machine-readable `type`, `title`
  and `detail` fields are the only surface an accessibility concern could attach to.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Sends proxy requests to `/oagw/v1/proxy/{alias}[/{path_suffix}]`, supplies `X-OAGW-Target-Host` when the upstream pool needs it, and consumes the response contract: passthrough bodies plus `X-OAGW-Error-Source`, and problem+json bodies with GTS `type` identifiers for gateway failures. |
| `cpt-cf-oagw-actor-platform-operator` | Owns the configuration the pipeline resolves and the runtime keys that govern it (`allow_http_upstream`, `ssrf_policy.enabled`, `proxy_timeout_secs`); disables an upstream or route to stop traffic and observes the outcome through entry 2.7. |
| `cpt-cf-oagw-actor-upstream-service` | Receives the transformed request as an opaque HTTP endpoint and returns the response this feature passes through; its failures are classified into the error table rather than interpreted. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md) — `cpt-cf-oagw-seq-proxy-flow` (proxy request flow),
  `cpt-cf-oagw-interface-api` (proxy contract, guard rules, body validation rules, header
  transformation table, error table, error source distinction, HTTP version negotiation, SSRF and
  smuggling posture), `cpt-cf-oagw-component-model` (DataPlaneService, proxy infra)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) — entry 2.4 and assumptions 1 to 9
- **ADRs**: [0001 Request Routing](../ADR/0001-request-routing.md)
  (`cpt-cf-oagw-adr-request-routing` — proxy operations route to the data plane, `X-OAGW-Target-Host`
  behaviour matrix), [0004 CORS](../ADR/0004-cors.md) (`cpt-cf-oagw-adr-cors` — preflight and
  actual-request handling, 403 error types), [0007 Error Source Distinction](../ADR/0007-error-source-distinction.md)
  (`cpt-cf-oagw-adr-error-source-distinction` — header values and target-host error extensions);
  supporting baselines: [0005 Control Plane Caching](../ADR/0005-data-plane-caching.md)
  (`cpt-cf-oagw-adr-data-plane-caching` — no configuration cache is introduced on the proxy path, so
  the only data-plane cache this feature keeps is the per-host HTTP version capability cache) and
  [0006 State Management](../ADR/0006-state-management.md) (`cpt-cf-oagw-adr-state-management` —
  in-process data-plane state ownership, decided there as an L1 configuration cache, a shared HTTP
  client and per-instance rate limiters). That decision does not cover the circuit breaker or the
  round-robin cursors: both are in-process per-instance state owned by this feature as a recorded
  boundary (DECOMPOSITION assumption 3) and are reset on restart.
- **Dependencies**: `cpt-cf-oagw-feature-upstream-route-management` — this feature resolves aliases
  and routes from the store that feature owns, reads the enabled states and sharing modes it
  validates, and returns every failure through the entry-2.1 mapping and header layers both features
  sit on
- **Resolved gear dependencies used here**: `tenant-resolver` (calling tenant and ancestor chain for
  the alias walk and the enforced-constraint sweep), `authz-resolver` (the
  `gts.cf.core.oagw.proxy.v1~:invoke` permission check); `credstore` is not called on this path —
  credential resolution is the entry-2.5 hook — and `types-registry` is not called at request time
- **Platform baselines**: toolkit canonical error contract (`toolkit_canonical_errors::CanonicalError`
  serialized as RFC 9457 `application/problem+json` with GTS `type` identifiers in the
  `gts.cf.core.errors.err.v1~cf.oagw....v1` space) and the `X-OAGW-Error-Source` response header,
  both delivered by the entry-2.1 cross-cutting layer; toolkit Bearer authentication and the proxy
  permission string of `cpt-cf-oagw-interface-api`; the `tenant-resolver` tenant hierarchy; the gear
  configuration keys `oagw.config.proxy_timeout_secs`, `oagw.config.allow_http_upstream` and
  `oagw.config.ssrf_policy.enabled` loaded by entry 2.1 (graded values `2`, `true` and `false`;
  entry-2.1 recorded defaults `30`, `false` and `true`); the crate's existing `toolkit-http` and
  `pingora-*` client stack; the host OpenAPI registry (`OpenApiRegistry` parameter of `register_rest`,
  entry-2.1 baseline) the proxy operations are registered in; the `dashmap`/`parking_lot`/`arc-swap`
  snapshot reads of the entry-2.2 store

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (human or external system) and describe the
end-to-end flow of a use case. Every failure below returns through the entry-2.1 mapping layer, so
each gateway error body is `application/problem+json` with a GTS `type` identifier and every response
carries `X-OAGW-Error-Source`. The plugin chain appears below only as the hook points this pipeline
provides; what runs inside them is entry 2.5's behaviour, and a streamed response leaves the pipeline
at the handoff step without its session behaviour being specified here.

**Use cases**: `cpt-cf-oagw-usecase-proxy-request`

**Referenced, not covered here**:

- `cpt-cf-oagw-fr-plugin-system` and `cpt-cf-oagw-fr-builtin-plugins` — covered by DECOMPOSITION
  entry 2.3 (type catalog, builtin registries, definition CRUD and binding validation) and entry 2.5
  (execution order, credential injection, guard decisions, response-phase transforms). This feature
  provides the hook points in the pipeline and nothing of the plugin behaviour.
- `cpt-cf-oagw-fr-auth-injection` and `cpt-cf-oagw-nfr-credential-isolation` — covered by
  DECOMPOSITION entry 2.5, which resolves `cred://` references and injects credentials at the auth
  hook point this feature provides. This feature never resolves or holds secret material.
- `cpt-cf-oagw-fr-rate-limiting` and `cpt-cf-oagw-usecase-rate-limit-exceeded` — covered by
  DECOMPOSITION entry 2.5, which enforces the token buckets and emits `429` with `Retry-After` at the
  guard hook point; this feature maps that outcome onto the error table and adds nothing to it.
- `cpt-cf-oagw-fr-streaming` and `cpt-cf-oagw-usecase-sse-streaming` — covered by DECOMPOSITION
  entry 2.6, which owns the streamed session lifecycle from the handoff point this feature defines.
- `cpt-cf-oagw-nfr-observability` — covered by DECOMPOSITION entry 2.7, which emits the audit record
  and the metric families from the request context this pipeline produces.
- `cpt-cf-oagw-nfr-high-availability` — covered by DECOMPOSITION entry 2.5 as its requirement row;
  the breaker mechanism that carries its trip threshold is implemented here, and the availability
  target itself is not restated in this artifact.
- `cpt-cf-oagw-nfr-multi-tenancy` — covered by DECOMPOSITION entry 2.2, which owns tenant-scoped
  storage; this feature relies on it for the tenant-scoped alias walk.
- `cpt-cf-oagw-fr-upstream-mgmt`, `cpt-cf-oagw-fr-route-mgmt`,
  `cpt-cf-oagw-interface-management-api`, `cpt-cf-oagw-usecase-configure-upstream` and
  `cpt-cf-oagw-usecase-configure-route` — covered by DECOMPOSITION entry 2.2, which registers the
  management endpoints that produce the configuration this pipeline resolves.
- `cpt-cf-oagw-fr-config-layering` and `cpt-cf-oagw-fr-hierarchical-config` — covered by
  DECOMPOSITION entry 2.2, which enforces the sharing modes and the layering constraints on write;
  this feature applies the recorded precedence upstream < route < tenant at read time.

### Proxy a Request to an Upstream

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-proxy-request`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- An actor sends `{METHOD} /oagw/v1/proxy/{alias}/{path_suffix}?query` for an enabled upstream and an
  enabled route; the request is validated, transformed and forwarded, and the upstream response is
  returned with its body unchanged and `X-OAGW-Error-Source: upstream`.
- A multi-endpoint upstream whose alias is an explicitly configured host name rather than a
  common-suffix derivation distributes the requests over the pool by round-robin; a caller that
  supplies `X-OAGW-Target-Host` reaches that endpoint and bypasses the round-robin.
- A multi-endpoint upstream whose alias is a common-suffix derivation routes to the endpoint named in
  `X-OAGW-Target-Host`.
- A response classified as streamed is handed to the entry-2.6 path with the upstream exchange still
  open, and the client receives it as a stream rather than a buffered body.

**Error Scenarios**:
- `404` when no upstream of the tenant chain carries the alias, when the resolved upstream has no
  matching enabled route, or when the resolved upstream is a `grpc`-protocol upstream with no
  reachable HTTP match.
- `503` when the closest upstream holding the alias is disabled, when the plaintext upstream
  connection is not admitted by `allow_http_upstream`, when the circuit breaker is open, or when the
  upstream link is unavailable.
- `400` for a route guard violation (`path_suffix_mode: disabled` with a suffix present, a query
  parameter outside the allowlist, a malformed header, a `Content-Length` that is not a valid integer
  or does not match the body size, a `Transfer-Encoding` other than `chunked`, ambiguous
  `Content-Length` and `Transfer-Encoding` framing) and for a missing, malformed or unknown
  `X-OAGW-Target-Host`.
- `403` when an actual cross-origin request carries an origin or a method the effective CORS
  configuration does not allow.
- `413` when the declared or observed body exceeds the 100MB hard limit, rejected before buffering.
- `502` and `504` when the upstream call fails: connection, request or idle timeout, an
  unreachable upstream, or a malformed or aborted upstream response.
- `401`, `429`, `500` and `503 PluginNotFound` raised at the entry-2.5 hook points and mapped by this
  pipeline onto the same error table.

**Steps**:
1. [x] - `p1` - Actor sends the proxy request to `/oagw/v1/proxy/{alias}[/{path_suffix}][?query]` - `inst-pe-req-01`
2. [x] - `p1` - Detect the CORS preflight shape at handler level, before authentication and before tenant resolution: method `OPTIONS` with both `Origin` and `Access-Control-Request-Method` present, per ADR 0004 - `inst-pe-req-01b`
3. [x] - `p1` - **IF** the shape matches, dispatch the request to `cpt-cf-oagw-flow-proxy-preflight` and **RETURN** the permissive `204` that flow returns, without requiring a Bearer token, without resolving a tenant and without reading the store snapshot - `inst-pe-req-01c`
4. [x] - `p1` - **ELSE** the request is an actual proxy request: API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` authenticates the Bearer token and requires the `gts.cf.core.oagw.proxy.v1~:invoke` permission of `cpt-cf-oagw-interface-api`; a missing or invalid token is **RETURN**ed as `401` - `inst-pe-req-02`
5. [x] - `p1` - Resolve the calling tenant from the security context and open the request context with the correlation identifier this pipeline carries to the response and to entry 2.7 - `inst-pe-req-03`
6. [x] - `p1` - **IF** the request was dispatched to `cpt-cf-oagw-flow-proxy-preflight` in the steps above - `inst-pe-req-04`
   1. [x] - `p1` - The `204` that flow returned is the response and no further step of this flow runs for it: a preflight reaches neither the alias walk, nor the plugin hook points, nor the upstream call - `inst-pe-req-05`
7. [x] - `p1` - Resolve the alias to an upstream with `cpt-cf-oagw-algo-alias-walk` - `inst-pe-req-06`
6. [x] - `p1` - **IF** no upstream of the chain carries the alias, or the resolved upstream is a `grpc`-protocol or `wt`-scheme upstream - `inst-pe-req-07`
   1. [x] - `p1` - **RETURN** `404` with the route-not-found problem body and `X-OAGW-Error-Source: gateway` - `inst-pe-req-08`
7. [x] - `p1` - **ELSE IF** the closest upstream holding the alias is disabled - `inst-pe-req-09`
   1. [x] - `p1` - **RETURN** `503` with the link-unavailable problem body; the walk does not fall through to an ancestor target - `inst-pe-req-10`
8. [x] - `p1` - Match the route with `cpt-cf-oagw-algo-route-match`; no enabled route matching method and path is **RETURN**ed as `404`, and a guard violation on a matched route is **RETURN**ed as `400` - `inst-pe-req-11`
9. [x] - `p1` - Merge the effective configuration with `cpt-cf-oagw-algo-config-merge` over the published store snapshot, including the enforced ancestor constraints collected across the shadowed chain - `inst-pe-req-12`
10. [x] - `p1` - [Hook point, entry 2.5] Run the request-phase plugin chain at its pipeline position — after the effective configuration merge of `cpt-cf-oagw-algo-config-merge` and before endpoint selection; the execution order inside the hook is owned by DECOMPOSITION entry 2.5 and is not restated here — with this pipeline supplying the merged configuration and consuming the outcome - `inst-pe-req-13`
11. [x] - `p1` - **IF** a hook point rejects the request (`401` AuthenticationFailed, `400` guard validation, `429` RateLimitExceeded, `503` PluginNotFound, `500` SecretNotFound) - `inst-pe-req-14`
   1. [x] - `p1` - Map the outcome through `cpt-cf-oagw-algo-proxy-error-mapping` and **RETURN** it; no upstream call is made - `inst-pe-req-15`
12. [x] - `p1` - Select the target endpoint with `cpt-cf-oagw-algo-endpoint-selection`; a missing, malformed or unknown `X-OAGW-Target-Host` is **RETURN**ed as `400` with the ADR 0007 extension fields - `inst-pe-req-16`
13. [x] - `p1` - Validate the request and its body with `cpt-cf-oagw-algo-request-validation`; a framing or limit violation is **RETURN**ed as `400` or `413` before any upstream call - `inst-pe-req-17`
14. [x] - `p1` - Transform the request headers with `cpt-cf-oagw-algo-header-transform` - `inst-pe-req-18`
15. [x] - `p1` - **IF** the request carries a WebSocket upgrade (`Upgrade: websocket` with `Connection: Upgrade`) - `inst-pe-req-18b`
   1. [x] - `p1` - Hand the request-side context to DECOMPOSITION entry 2.6 before the upstream call — the selected endpoint, the transformed header set and the request context — and stop this flow's responsibility there: that entry validates the upgrade header set, re-injects `Upgrade`, `Connection` and the `Sec-WebSocket-*` headers, dials the selected endpoint, relays the `101 Switching Protocols` response verbatim and owns the bidirectional frame relay and the stream error mapping; the circuit-breaker evaluation and the response-classification steps of this pipeline do not run for an upgrade exchange - `inst-pe-req-18b-1`
16. [x] - `p1` - Evaluate the circuit breaker with `cpt-cf-oagw-algo-circuit-breaker`; an open breaker is **RETURN**ed as `503` CircuitBreakerOpen without contacting the upstream - `inst-pe-req-19`
17. [x] - `p1` - Call the upstream with `cpt-cf-oagw-algo-upstream-call` under the `proxy_timeout_secs` timeout - `inst-pe-req-20`
18. [x] - `p1` - **IF** the call fails, or the upstream response is unusable - `inst-pe-req-21`
   1. [x] - `p1` - Record the failure on the breaker, map it through `cpt-cf-oagw-algo-proxy-error-mapping` and **RETURN** the mapped gateway error - `inst-pe-req-22`
19. [x] - `p1` - **ELSE** - `inst-pe-req-23`
   1. [x] - `p1` - Record the success on the breaker and classify the response - `inst-pe-req-24`
   2. [x] - `p1` - **IF** the response is streamed (`text/event-stream` only — an upgrade request was already handed off on the request side in the steps above and never reaches this classification) - `inst-pe-req-25`
      1. [x] - `p1` - Hand the open upstream exchange to DECOMPOSITION entry 2.6 at its hook point without buffering the body, and stop this flow's responsibility there - `inst-pe-req-26`
   3. [x] - `p1` - **ELSE** - `inst-pe-req-27`
      1. [x] - `p1` - Apply the response-side header transformations of `cpt-cf-oagw-algo-header-transform` and pass the body through unchanged - `inst-pe-req-28`
20. [x] - `p1` - [Hook point, entry 2.5] Run the response-phase plugin chain (response or error phase) with this pipeline supplying the response or the mapped error - `inst-pe-req-29`
21. [x] - `p1` - Stamp `X-OAGW-Error-Source` through the entry-2.1 header layer (`upstream` for a passthrough response, `gateway` for a mapped error) and close the request context with the outcome entry 2.7 records - `inst-pe-req-30`
22. [x] - `p1` - **RETURN** the upstream response as a passthrough, or the mapped gateway error as described above - `inst-pe-req-31`

### Handle a CORS Preflight

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-proxy-preflight`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A browser sends `OPTIONS /oagw/v1/proxy/{alias}/{path_suffix}` with `Origin` and
  `Access-Control-Request-Method` and receives `204 No Content` with the requested origin, method and
  headers echoed, `Access-Control-Max-Age: 86400` and the `Vary` header set, per ADR 0004.
- The preflight is answered locally with no Bearer authentication, no upstream resolution, no tenant
  context and no plugin or rate-limit execution, so it succeeds even when the upstream is unreachable.

**Error Scenarios**:
- A request that is not a preflight (missing `Origin` or missing
  `Access-Control-Request-Method`) is not classified as one and continues in
  `cpt-cf-oagw-flow-proxy-request` from authentication, where the usual authentication, resolution and
  guard rules apply; the alias walk runs only after authentication.
- An actual cross-origin request that follows an accepted preflight is still rejected with `403` when
  its origin or method is not in the effective CORS configuration, because origin enforcement happens
  on the actual request.

**Steps**:
1. [x] - `p1` - Actor sends the preflight request - `inst-pe-pre-01`
2. [x] - `p1` - Detect the preflight shape: method `OPTIONS` with both `Origin` and `Access-Control-Request-Method` present; `cpt-cf-oagw-flow-proxy-request` performs this detection at handler level, before authentication, and dispatches the request here - `inst-pe-pre-02`
3. [x] - `p1` - **IF** the shape does not match - `inst-pe-pre-03`
   1. [x] - `p1` - Continue in `cpt-cf-oagw-flow-proxy-request` from authentication; the request is not treated as a preflight and reaches the alias walk only after authentication - `inst-pe-pre-04`
4. [x] - `p1` - **ELSE** - `inst-pe-pre-05`
   1. [x] - `p1` - Answer locally: echo the request's `Origin`, `Access-Control-Request-Method` and `Access-Control-Request-Headers` into the matching `Access-Control-Allow-*` response headers - `inst-pe-pre-06`
   2. [x] - `p1` - Set `Access-Control-Max-Age: 86400` and `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers` - `inst-pe-pre-07`
   3. [x] - `p1` - Resolve no upstream, run no plugin hook, read no tenant context and require no Bearer token - `inst-pe-pre-08`
5. [x] - `p1` - Stamp `X-OAGW-Error-Source: gateway` through the entry-2.1 header layer, because the response is generated by gear code - `inst-pe-pre-09`
6. [x] - `p1` - **RETURN** `204 No Content` with no body - `inst-pe-pre-10`

### Distinguish Gateway and Upstream Errors

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-proxy-error-source`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A failure raised inside the pipeline (resolution, match, merge, selection, validation,
  transformation, timeout, breaker) returns an `application/problem+json` body carrying the GTS
  `type` identifier, the standard problem fields and the OAGW extension fields, with
  `X-OAGW-Error-Source: gateway`.
- An upstream response with an error status is passed through with its status, headers and body
  unchanged and carries `X-OAGW-Error-Source: upstream`, so the caller can tell the two origins apart
  without parsing the body.

**Error Scenarios**:
- An intermediary strips `X-OAGW-Error-Source`: the caller falls back to inspecting the response
  structure (problem+json with a GTS `type` identifier indicates a gateway error), the fallback ADR
  0007 records for that case.
- A gateway-originated failure is serialized with a content type other than
  `application/problem+json`, or with a `type` value outside the GTS identifier table fixed by
  `cpt-cf-oagw-interface-api`: the mapping layer is bypassed and the acceptance criterion fails.

**Steps**:
1. [x] - `p1` - Actor sends a proxy request that ends in a failure - `inst-pe-err-01`
2. [x] - `p1` - Classify the origin of the failure inside `cpt-cf-oagw-algo-proxy-error-mapping` - `inst-pe-err-02`
3. [x] - `p1` - **IF** the failure originated inside the gateway - `inst-pe-err-03`
   1. [x] - `p1` - Resolve the HTTP status and the GTS `type` identifier from the error table and build the canonical problem document through the entry-2.1 mapping layer, attaching the extension fields the request context provides (`upstream_id`, `host`, `path`, `retry_after_seconds`, `trace_id`, and `alias`, `valid_hosts`, `invalid_value` for target-host errors) - `inst-pe-err-04`
   2. [x] - `p1` - Classify the response error source as `gateway` - `inst-pe-err-05`
4. [x] - `p1` - **ELSE** (the upstream produced a response with an error status) - `inst-pe-err-06`
   1. [x] - `p1` - Pass the status, headers and body through unchanged, add no gateway-generated body, and classify the response error source as `upstream` - `inst-pe-err-07`
5. [x] - `p1` - Apply the error-source header layer to the outgoing response without overwriting a value already set by the producing path - `inst-pe-err-08`
6. [x] - `p1` - **RETURN** the response with `X-OAGW-Error-Source` present on success, gateway error and upstream passthrough alike - `inst-pe-err-09`

## 3. Processes / Business Logic (CDSL)

Internal system functions and procedures that do not interact with actors directly. These are the
stages of the proxy pipeline in execution order; each is called by `cpt-cf-oagw-flow-proxy-request`
or by another stage below, and each raises the failures that `cpt-cf-oagw-algo-proxy-error-mapping` turns
into responses.

### Tenant-Hierarchy Alias Walk and Upstream Selection

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-alias-walk`

**Input**: the path `{alias}`, the calling tenant and its ancestor chain from `tenant-resolver`, and
the published configuration snapshot of the entry-2.2 store.

**Output**: the selected upstream with its whole tenant chain, or the `404` / `503` outcome to map.

**Steps**:
1. [x] - `p1` - Normalize `{alias}` to ASCII lowercase and strip one trailing dot, so resolution is case-insensitive and matches the entry-2.2 normalization - `inst-pe-aw-01`
2. [x] - `p1` - Read one published snapshot and keep it for the whole request, so the alias, route and merged configuration cannot be resolved from different generations - `inst-pe-aw-02`
3. [x] - `p1` - Walk the tenant chain from the calling tenant towards the root - `inst-pe-aw-03`
4. [x] - `p1` - **FOR EACH** tenant in the chain - `inst-pe-aw-04`
   1. [x] - `p1` - Look up `(tenant_id, alias)` in that tenant's own upstreams - `inst-pe-aw-05`
   2. [x] - `p1` - **IF** a record exists, stop the walk: this is the closest match and it shadows every ancestor record with the same alias - `inst-pe-aw-06`
   3. [x] - `p1` - **ELSE** continue to the parent tenant - `inst-pe-aw-07`
5. [x] - `p1` - **IF** no tenant in the chain holds the alias - `inst-pe-aw-08`
   1. [x] - `p1` - **RETURN** not found, mapped to `404` RouteNotFound - `inst-pe-aw-09`
6. [x] - `p1` - **ELSE IF** the closest match is disabled - `inst-pe-aw-10`
   1. [x] - `p1` - **RETURN** the disabled outcome mapped to `503` LinkUnavailable, without falling through to an ancestor target (recorded interpretation in section 1.2) - `inst-pe-aw-11`
7. [x] - `p1` - **ELSE** - `inst-pe-aw-12`
   1. [x] - `p1` - **RETURN** the selected enabled upstream together with the ancestor chain walked, for the merge and the enforced-constraint sweep - `inst-pe-aw-13`
8. [x] - `p1` - Treat the walk as the only place a request can reach an ancestor resource: a descendant never addresses an ancestor upstream directly, it inherits it through this walk - `inst-pe-aw-14`

### Route Matching

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-route-match`

**Input**: the selected upstream and its ancestor chain, the request method, the request path with
its optional path suffix, and the request query parameters.

**Output**: the matched route with the path and query to forward, or the `404` / `400` outcome to map.

**Steps**:
1. [x] - `p1` - Build the candidate set from the enabled routes of the selected upstream plus the enabled routes inherited through the ancestor chain, and give descendant routes priority over ancestor routes on the same match key - `inst-pe-rm-01`
2. [x] - `p1` - Exclude every disabled route from the candidate set, per the request-path half of `cpt-cf-oagw-fr-enable-disable` - `inst-pe-rm-02`
3. [x] - `p1` - Require the request method to be in the route's `match.http.methods` allowlist - `inst-pe-rm-03`
4. [x] - `p1` - Select the longest `match.http.path` prefix that matches the request path; break a tie by the higher route `priority` - `inst-pe-rm-04`
5. [x] - `p1` - **IF** no candidate matches method and path, or the selected upstream's `protocol` is the gRPC protocol identifier and therefore has no HTTP match key - `inst-pe-rm-05`
   1. [x] - `p1` - **RETURN** not found, mapped to `404` RouteNotFound - `inst-pe-rm-06`
6. [x] - `p1` - **ELSE** - `inst-pe-rm-07`
   1. [x] - `p1` - Apply `path_suffix_mode`: `disabled` fails with `400` when a suffix is present; `append` forwards `match.http.path` followed by the request's path suffix - `inst-pe-rm-08`
   2. [x] - `p1` - Validate the query parameters against `match.http.query_allowlist`; any parameter outside the allowlist fails with `400` - `inst-pe-rm-09`
7. [x] - `p1` - Record the matched route on the request context with the normalized route pattern, which is the label entry 2.7 reports as `http.route` - `inst-pe-rm-10`
8. [x] - `p1` - **RETURN** the matched route and the forward path and query - `inst-pe-rm-11`

### Effective Configuration Merge

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-config-merge`

**Input**: the selected upstream, the matched route, the tenant chain walked for the alias, and the
sharing modes and enforced constraints stored with them.

**Output**: the effective configuration for this request (`EffectiveUpstream`): the header rules, the
CORS configuration, the rate-limit bound, the plugin binding list and the ancestor enforced
constraint set.

**Steps**:
1. [x] - `p1` - Start from the upstream configuration as the base layer - `inst-pe-cm-01`
2. [x] - `p1` - Apply the route's overrides, then the tenant-level overrides, so the precedence is upstream < route < tenant per `cpt-cf-oagw-fr-config-layering` - `inst-pe-cm-02`
3. [x] - `p1` - Honour the sharing modes recorded on write: a field marked `enforce` keeps the ancestor value, a field marked `inherit` accepts the descendant override, and a field marked `private` contributes only at its owning level - `inst-pe-cm-03`
4. [x] - `p1` - Compute the effective rate limit as `min(all enforced ancestor limits, route limit, tenant limit)`, so the stricter bound always wins - `inst-pe-cm-04`
5. [x] - `p1` - Merge CORS by unioning `allowed_origins` when the ancestor mode is `inherit` and by keeping the ancestor set when it is `enforce` - `inst-pe-cm-05`
6. [x] - `p1` - Produce the plugin binding list as the concatenation of the ancestor chain's bindings and the descendant's, upstream bindings before route bindings; what executes against that list is entry 2.5's behaviour - `inst-pe-cm-06`
7. [x] - `p1` - Collect the enforced constraints of every ancestor in the walked chain, including those shadowed by the selected upstream, so shadowing cannot lift an enforced bound - `inst-pe-cm-07`
8. [x] - `p1` - Keep the auth configuration as stored — `cred://` references with their sharing mode — and resolve no credential here; resolution and injection are the entry-2.5 auth hook - `inst-pe-cm-08`
9. [x] - `p1` - Apply tag semantics as add-only union for the discovery record; tags play no part in matching or forwarding - `inst-pe-cm-09`
10. [x] - `p1` - Never place secret material on the effective configuration or the request context - `inst-pe-cm-10`
11. [x] - `p1` - **RETURN** the effective configuration for the selection, validation, transformation and call stages - `inst-pe-cm-11`

### Multi-Endpoint Pool Selection and Target-Host Validation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-endpoint-selection`

**Input**: the selected upstream's endpoint pool (all endpoints sharing `protocol`, `scheme` and
`port` per the entry-2.2 pool rule), the `X-OAGW-Target-Host` request header when present, the
upstream `alias`, and the per-upstream round-robin cursor.

**Output**: the selected endpoint, or the `400` target-host outcome to map.

**Steps**:
1. [x] - `p1` - Read `X-OAGW-Target-Host` once and treat it as a routing header only; it is never forwarded to the upstream - `inst-pe-es-01`
2. [x] - `p1` - **IF** the pool holds exactly one endpoint - `inst-pe-es-02`
   1. [x] - `p1` - Select that endpoint; the header is optional, and a value that is present is still validated - `inst-pe-es-03`
3. [x] - `p1` - **ELSE IF** a target-host value is present - `inst-pe-es-04`
   1. [x] - `p1` - Validate the value as a hostname or an IP literal with no port, no path and no special characters; a malformed value is **RETURN**ed as `400` InvalidTargetHost with the `invalid_value` extension - `inst-pe-es-05`
   2. [x] - `p1` - Compare the value against the pool's endpoint hosts case-insensitively with trailing dots stripped; a value matching no endpoint is **RETURN**ed as `400` UnknownTargetHost with `invalid_value` and `valid_hosts` - `inst-pe-es-06`
   3. [x] - `p1` - **ELSE** select the named endpoint and bypass the round-robin cursor - `inst-pe-es-07`
4. [x] - `p1` - **ELSE** (multi-endpoint pool, no header) - `inst-pe-es-08`
   1. [x] - `p1` - **IF** the upstream `alias` is the PSL-validated common domain suffix derived from the pool's endpoint hosts — a hostname-derived alias, not an explicitly configured one — so no endpoint host of the pool is named by the alias and the caller must name the endpoint it wants - `inst-pe-es-09`
      1. [x] - `p1` - **RETURN** the missing-header outcome mapped to `400` MissingTargetHost with `alias` and `valid_hosts`, naming the endpoints the caller may choose - `inst-pe-es-10`
   2. [x] - `p1` - **ELSE** select the next endpoint by advancing the per-upstream round-robin cursor - `inst-pe-es-11`
5. [x] - `p1` - Record the selection method (`explicit_header`, `round_robin` or `default`) on the request context for entry 2.7's routing metric - `inst-pe-es-12`
6. [x] - `p1` - Strip `X-OAGW-Target-Host` from the outbound header set after selection - `inst-pe-es-13`
7. [x] - `p1` - **RETURN** the selected endpoint - `inst-pe-es-14`

### Request, Body and CORS Validation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-request-validation`

**Input**: the inbound request (method, path, query, headers, body), the matched route and the
effective CORS configuration.

**Output**: a validated request with its body buffered up to the limit, or the `400` / `403` / `413`
outcome to map.

**Steps**:
1. [x] - `p1` - Parse the headers strictly: reject a header name or value containing CR or LF, a header name that is not a valid token, and a header section that cannot be parsed without recovery - `inst-pe-bv-01`
2. [x] - `p1` - Validate the framing: reject a `Content-Length` that is not a valid integer, a request carrying both `Content-Length` and `Transfer-Encoding`, duplicate `Content-Length` values that disagree, and a `Transfer-Encoding` whose value is not `chunked` - `inst-pe-bv-02`
3. [x] - `p1` - Reject a declared or observed body above the 100MB hard limit of `cpt-cf-oagw-constraint-body-limit` with `413` before buffering any of it - `inst-pe-bv-03`
4. [x] - `p1` - Buffer the body up to the limit and compare its actual size with `Content-Length`; a mismatch is **RETURN**ed as `400` - `inst-pe-bv-04`
5. [x] - `p1` - Enforce the CORS configuration on an actual cross-origin request — one carrying an `Origin` header with `cors.enabled` — by rejecting a disallowed origin with `403` and `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` and a disallowed method with `403` and `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1` - `inst-pe-bv-05`
6. [x] - `p1` - Always include `Vary: Origin` on a response to a request that carried `Origin`, per ADR 0004 - `inst-pe-bv-06`
7. [x] - `p1` - Reject any well-known header that cannot be validated, set or adjusted for forwarding with `400`, per the DESIGN well-known header rule - `inst-pe-bv-07`
8. [x] - `p1` - Record the request size on the request context for entry 2.7, without recording any header or body content - `inst-pe-bv-08`
9. [x] - `p1` - **RETURN** the validated request and its buffered body - `inst-pe-bv-09`

### Header Transformation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-header-transform`

**Input**: the validated request with its header set, the effective `headers` configuration
(`request.set`, `request.add`, `request.remove`, `request.passthrough`,
`request.passthrough_allowlist`, `response.set`, `response.add`, `response.remove`), the selected
endpoint and the negotiated HTTP version.

**Output**: the outbound request header set, and the response header set after the upstream call.

**Steps**:
1. [x] - `p1` - Apply the configured `request.remove` list to the inbound headers - `inst-pe-ht-01`
2. [x] - `p1` - Apply the passthrough policy: `none` forwards no inbound header, `allowlist` forwards only the names in `request.passthrough_allowlist`, `all` forwards every inbound header that survives the steps below - `inst-pe-ht-02`
3. [x] - `p1` - Strip the hop-by-hop headers `Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding` and `Upgrade`, together with any header the `Connection` header names - `inst-pe-ht-03`
4. [x] - `p1` - Strip `X-OAGW-Target-Host` after endpoint selection, on both HTTP/1.1 and HTTP/2 - `inst-pe-ht-04`
5. [x] - `p1` - Rewrite the request authority to the selected endpoint: the `Host` header on HTTP/1.1 and the `:authority` pseudo-header on HTTP/2, in both cases derived from the endpoint host and port and never from a client-supplied value - `inst-pe-ht-05`
6. [x] - `p1` - Inject no gateway-internal header into the outbound request: no tenant identifier, no security context, no internal routing or correlation header, in both SSRF postures - `inst-pe-ht-06`
7. [x] - `p1` - Apply `request.set` (overwrite) and then `request.add` (append), and recompute `Content-Length` from the buffered body so the forwarded framing is consistent - `inst-pe-ht-07`
8. [x] - `p1` - **FOR EACH** header applied - `inst-pe-ht-08`
   1. [x] - `p1` - Validate the resulting name and value as a well-formed header field; a transformation that would produce an invalid header fails with `400` - `inst-pe-ht-09`
9. [x] - `p1` - On the response side, strip the same hop-by-hop set from the upstream response, apply `response.set` (overwrite), `response.add` (append) and `response.remove`, and pass every other upstream response header through unchanged - `inst-pe-ht-10`
10. [x] - `p1` - **RETURN** the outbound request header set, and the response header set when the response side runs - `inst-pe-ht-11`

### Upstream Call, Version Negotiation and Timeout

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-upstream-call`

**Input**: the transformed request, the selected endpoint, the effective configuration, and the gear
configuration keys `oagw.config.proxy_timeout_secs`, `oagw.config.allow_http_upstream` and
`oagw.config.ssrf_policy.enabled`.

**Output**: the upstream response, or the failure that `cpt-cf-oagw-algo-proxy-error-mapping` turns into a
gateway error.

**Steps**:
1. [x] - `p1` - **IF** the selected endpoint's `scheme` is `http` and `allow_http_upstream` is `false` - `inst-pe-uc-01`
   1. [x] - `p1` - Refuse the call with `503` LinkUnavailable before any connection attempt; the default posture stays HTTPS-only and the graded configuration opts in explicitly (DECOMPOSITION assumption 2) - `inst-pe-uc-02`
2. [x] - `p1` - Validate the connect target against the SSRF posture: the target is the selected endpoint taken from the store, never a client-supplied host; with `ssrf_policy.enabled: true` the host validation is unconditional, and with `false` the operator has relaxed it while the target and the no-internal-header rule still hold - `inst-pe-uc-03`
3. [x] - `p1` - Consult the per-host HTTP version cache - `inst-pe-uc-04`
4. [x] - `p1` - **IF** the host has no cached capability - `inst-pe-uc-05`
   1. [x] - `p1` - Attempt HTTP/2 through ALPN during the TLS handshake; cache the negotiated capability for the host with a one-hour TTL, and fall back to HTTP/1.1 when the attempt fails - `inst-pe-uc-06`
5. [x] - `p1` - **ELSE** - `inst-pe-uc-07`
   1. [x] - `p1` - Use the cached capability for this host; an `http` scheme endpoint uses HTTP/1.1, since no TLS handshake and therefore no ALPN negotiation exists - `inst-pe-uc-08`
6. [x] - `p1` - Send the request over the crate's existing `toolkit-http`/`pingora-*` client stack with no new client dependency, applying the `oagw.config.proxy_timeout_secs` bound as the connection, request and idle timeout - `inst-pe-uc-09`
7. [x] - `p1` - Classify an expiry as connection, request or idle timeout, and an unreachable or reset connection as a downstream failure - `inst-pe-uc-10`
8. [x] - `p1` - Send exactly one request: never re-issue the original client request as a whole, and treat connection or endpoint-level attempts inside the client stack as connector behaviour and not as a retry - `inst-pe-uc-11`
9. [x] - `p1` - Read the response without caching it, per `cpt-cf-oagw-principle-no-cache`; no response body, header or status is retained beyond the request - `inst-pe-uc-12`
10. [x] - `p1` - **IF** the response indicates a streamed body or an upgraded protocol - `inst-pe-uc-13`
   1. [x] - `p1` - Return the open exchange to the caller for the entry-2.6 handoff instead of reading the body to completion - `inst-pe-uc-14`
11. [x] - `p1` - **RETURN** the upstream response, or the classified failure - `inst-pe-uc-15`

### Response Passthrough and Error Mapping

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-error-mapping`

**Input**: the upstream response, or the failure raised by any pipeline stage or entry-2.5 hook
point, together with the request context (`upstream_id`, `alias`, `host`, `path`, selection method,
correlation identifier) and the breaker outcome.

**Output**: the client response with its status, headers, body and `X-OAGW-Error-Source`.

**Steps**:
1. [x] - `p1` - **IF** an upstream response is available - `inst-pe-em-01`
   1. [x] - `p1` - Pass the status, headers and body through after the response-side header transformations, add no gateway-generated body, and classify the error source as `upstream` for every status, including error statuses - `inst-pe-em-02`
2. [x] - `p1` - **ELSE** - `inst-pe-em-03`
   1. [x] - `p1` - Resolve the HTTP status and the GTS `type` identifier from the table below and build the canonical `application/problem+json` document through the entry-2.1 mapping layer, classifying the error source as `gateway` - `inst-pe-em-04`
   2. [x] - `p1` - Attach the extension fields the request context provides (`upstream_id`, `host`, `path`, `retry_after_seconds`, `trace_id`), the ADR 0007 fields `alias`, `valid_hosts` and `invalid_value` on the target-host errors, and omit the ones that do not apply - `inst-pe-em-05`
3. [x] - `p1` - Stamp `X-OAGW-Error-Source` on the response through the entry-2.1 header layer, on success as well as on failure - `inst-pe-em-06`
4. [x] - `p1` - Never include credential material, a resolved secret value, a request body or a header value in the problem document - `inst-pe-em-07`
5. [x] - `p1` - Record the error type on the request context for entry 2.7 and for the breaker's failure count - `inst-pe-em-08`
6. [x] - `p1` - **RETURN** the client response - `inst-pe-em-09`

**Error table** (the request-path half of `cpt-cf-oagw-fr-error-codes`; every `type` identifier is in
the `gts.cf.core.errors.err.v1~cf.oagw....v1` space and every row is a gateway error with
`X-OAGW-Error-Source: gateway`):

| Pipeline stage | HTTP | Error | GTS `type` identifier | Retriable |
|---|---|---|---|---|
| Alias walk | 404 | RouteNotFound | `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1` | No |
| Alias walk | 503 | LinkUnavailable | `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1` | Yes |
| Route match | 404 | RouteNotFound | `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1` | No |
| Route match and request validation | 400 | ValidationError | `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` | No |
| Endpoint selection | 400 | MissingTargetHost | `gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1` | No |
| Endpoint selection | 400 | InvalidTargetHost | `gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1` | No |
| Endpoint selection | 400 | UnknownTargetHost | `gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1` | No |
| CORS enforcement | 403 | CORS Origin Not Allowed | `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` | No |
| CORS enforcement | 403 | CORS Method Not Allowed | `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1` | No |
| Body limit | 413 | PayloadTooLarge | `gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1` | No |
| Auth hook point (entry 2.5) | 401 | AuthenticationFailed | `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1` | No |
| Guard hook point (entry 2.5) | 429 | RateLimitExceeded | `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1` | Yes |
| Plugin resolution (entry 2.5) | 503 | PluginNotFound | `gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1` | No |
| Credential hook point (entry 2.5) | 500 | SecretNotFound | `gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1` | No |
| Circuit breaker | 503 | CircuitBreakerOpen | `gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1` | Yes |
| Upstream call | 502 | DownstreamError | `gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1` | Depends |
| Upstream call | 502 | ProtocolError | `gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1` | No |
| Upstream call | 504 | ConnectionTimeout | `gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1` | Yes |
| Upstream call | 504 | RequestTimeout | `gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1` | Yes |
| Upstream call | 504 | IdleTimeout | `gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1` | Yes |

### Circuit Breaker Evaluation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-circuit-breaker`

**Input**: the selected upstream (keyed by its endpoint host), the outcome of the last upstream call
for that key, and the current time.

**Output**: the decision to admit the call or to reject it with `503` CircuitBreakerOpen, and the
recorded outcome for the state machine in section 4.

**Steps**:
1. [x] - `p1` - Keep one breaker per upstream endpoint host in process-local state; the breaker is core gateway behaviour and not a plugin - `inst-pe-cb-01`
2. [x] - `p1` - **IF** the breaker for that host is open - `inst-pe-cb-02`
   1. [x] - `p1` - Reject the call before any connection attempt with `503` CircuitBreakerOpen, and record no failure on the breaker for the rejected request - `inst-pe-cb-03`
3. [x] - `p1` - **ELSE** - `inst-pe-cb-04`
   1. [x] - `p1` - Admit the call, then record its outcome in the 30-second sliding window kept for that host - `inst-pe-cb-05`
4. [x] - `p1` - Trip the breaker to open when the fifth failure within the window is recorded, so no more than five failed requests are needed to stop traffic to an unhealthy upstream - `inst-pe-cb-06`
5. [x] - `p1` - Count as failures the mapped gateway errors of the upstream-call stage: connection, request and idle timeouts, downstream and protocol errors; do not count upstream error responses passed through to the caller, gateway validation failures raised before the call, or rejections this breaker produced - `inst-pe-cb-07`
6. [x] - `p1` - Admit a single probe request when the open state's cool-down elapses, and let its outcome drive the transition back to closed or to open - `inst-pe-cb-08`
7. [x] - `p1` - Reset the window when the breaker returns to closed - `inst-pe-cb-09`
8. [x] - `p1` - Record every state transition with its host, from-state and to-state for entry 2.7's `oagw_circuit_breaker_transitions_total`, and emit no metric here - `inst-pe-cb-10`
9. [x] - `p1` - **RETURN** the admission decision and the recorded outcome - `inst-pe-cb-11`

## 4. States (CDSL)

Optional: Include when entities have explicit lifecycle states.

Two lifecycles belong to this feature: the per-upstream circuit breaker that entry 2.4's scope names
as an entity, and the request context that the pipeline carries from ingress to response. The
resources the pipeline reads have no lifecycle here — the upstream and route lifecycles are entry
2.2's state machines, and this feature only observes their stored `enabled` values.

### Circuit Breaker State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-circuit-breaker`

**States**: `closed`, `open`, `half_open`

**Initial State**: `closed`

**Transitions**:
1. [x] - `p1` - **FROM** `closed` **TO** `open` **WHEN** the fifth failed upstream call within the 30-second sliding window is recorded for that endpoint host - `inst-pe-scb-01`
2. [x] - `p1` - **FROM** `open` **TO** `half_open` **WHEN** the cool-down elapses and the next request is admitted as the single probe - `inst-pe-scb-02`
3. [x] - `p1` - **FROM** `half_open` **TO** `closed` **WHEN** the probe request completes successfully, and the window is reset - `inst-pe-scb-03`
4. [x] - `p1` - **FROM** `half_open` **TO** `open` **WHEN** the probe request fails, restarting the cool-down - `inst-pe-scb-04`

**Closed transition set**: the transitions above are the only ones possible. A request rejected by an
`open` breaker causes no transition and records no failure, a `closed` breaker stays `closed` on a
successful call or on fewer than five failures in the window, and no state is re-entered on its own
or skipped. The machine is per endpoint host and process-local (in-memory, DECOMPOSITION assumption
3), so a host process restart returns every breaker to `closed` and drops its window.

### Proxy Request Context State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-request-lifecycle`

**States**: `received`, `resolved`, `matched`, `validated`, `forwarded`, `responded`, `handed_off`,
`failed`

**Initial State**: `received`

**Transitions**:
1. [x] - `p1` - **FROM** `received` **TO** `resolved` **WHEN** the alias walk selects an enabled upstream - `inst-pe-srl-01`
2. [x] - `p1` - **FROM** `resolved` **TO** `matched` **WHEN** a route matches method and path and its guard rules pass - `inst-pe-srl-02`
3. [x] - `p1` - **FROM** `matched` **TO** `validated` **WHEN** the request, body and CORS validation pass and the entry-2.5 request-phase hook points do not reject - `inst-pe-srl-03`
4. [x] - `p1` - **FROM** `validated` **TO** `forwarded` **WHEN** the endpoint is selected, the headers are transformed, the breaker admits the call and the upstream call is sent - `inst-pe-srl-04`
5. [x] - `p1` - **FROM** `forwarded` **TO** `responded` **WHEN** a complete upstream response is received and passed through - `inst-pe-srl-05`
6. [x] - `p1` - **FROM** `forwarded` **TO** `handed_off` **WHEN** the response is classified as streamed and the open exchange is handed to DECOMPOSITION entry 2.6 - `inst-pe-srl-06`
7. [x] - `p1` - **FROM** `received`, `resolved`, `matched`, `validated` or `forwarded` **TO** `failed` **WHEN** any stage or hook point raises a failure that `cpt-cf-oagw-algo-proxy-error-mapping` turns into a gateway error - `inst-pe-srl-07`

**Closed transition set**: the transitions above are the only ones possible. The order is fixed and
no state is skipped or re-entered; a request that reaches `responded`, `handed_off` or `failed` is
finished and its context is closed with the outcome entry 2.7 records. The state machine describes
the context's progress only — it introduces no persisted state, and a `failed` request has no
further transition. A request dispatched as a CORS preflight is answered by
`cpt-cf-oagw-flow-proxy-preflight` from `received`, before authentication and tenant resolution, and
enters none of these transitions.

## 5. Definitions of Done

Specific implementation tasks derived from flows/algorithms above.

### Proxy Endpoint Registration and Permission

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-endpoint`

The system **MUST** register the proxy path `{METHOD} /oagw/v1/proxy/{alias}` and
`{METHOD} /oagw/v1/proxy/{alias}/{path_suffix}` under the gear mount root for the methods the route
model admits (`GET`, `POST`, `PUT`, `DELETE`, `PATCH`) plus `OPTIONS` for the preflight path, with no
`/api` prefix (DECOMPOSITION assumption 1), **MUST** authenticate each actual proxy call with the
toolkit Bearer surface and require the `gts.cf.core.oagw.proxy.v1~:invoke` permission of
`cpt-cf-oagw-interface-api` — a request detected as a CORS preflight is dispatched before
authentication, per ADR 0004 — **MUST** return every gateway failure through the entry-2.1 mapping
layer as `application/problem+json` with `X-OAGW-Error-Source` set, **MUST** pass an upstream
response through with `X-OAGW-Error-Source: upstream`, and **MUST** register the proxy operations in
the host OpenAPI registry (platform baseline, section 1.4). The system **MUST NOT** register any
management endpoint here — those are entry 2.2's.

**Implements**:
- `cpt-cf-oagw-flow-proxy-request`
- `cpt-cf-oagw-flow-proxy-preflight`
- `cpt-cf-oagw-flow-proxy-error-source`
- `cpt-cf-oagw-state-request-lifecycle`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Principles**: `cpt-cf-oagw-principle-error-source`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`, `{METHOD} /oagw/v1/proxy/{alias}/{path_suffix}`
- DB: none — this feature persists nothing; the store it reads is entry 2.2 (DECOMPOSITION assumption 3)
- Entities: `RequestContext`, `ResponseContext`

### Alias Walk, Shadowing and Enabled Enforcement

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-alias-walk`

The system **MUST** resolve the path alias exactly as `cpt-cf-oagw-algo-alias-walk` specifies —
lowercase normalization with trailing dots stripped, a descendant-to-root walk over one published
snapshot, the closest match winning and shadowing every ancestor record with the same alias, an
enabled record selected as the routing target and a disabled closest match rejected with `503`
LinkUnavailable and no fall-through to an ancestor target — and **MUST** reach an ancestor upstream
only through this walk, never by direct addressing.

**Implements**:
- `cpt-cf-oagw-flow-proxy-request`
- `cpt-cf-oagw-algo-alias-walk`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Principles**: `cpt-cf-oagw-principle-tenant-scope`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}` (alias path segment)
- DB: `cpt-cf-oagw-db-schema` (tenant-hierarchy read of the upstream logical table)
- Entities: `EffectiveUpstream`, `RequestContext`

### Route Matching and Guard Rules

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-matching`

The system **MUST** match routes exactly as `cpt-cf-oagw-algo-route-match` specifies — enabled
routes of the selected upstream plus enabled routes inherited through the ancestor chain with
descendant priority, the method allowlist, the longest path prefix with the higher `priority`
breaking a tie, `path_suffix_mode` `disabled` rejecting a suffix with `400` and `append` forwarding
match path plus suffix, and the query allowlist rejecting an unknown parameter with `400` — **MUST**
return `404` RouteNotFound when no enabled route matches, and **MUST** leave a `grpc`-protocol
upstream without a reachable HTTP match, returning `404`.

**Implements**:
- `cpt-cf-oagw-flow-proxy-request`
- `cpt-cf-oagw-algo-route-match`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path_suffix}` (suffix and query handling)
- DB: `cpt-cf-oagw-db-schema` (route and HTTP match logical tables)
- Entities: `MatchedRoute`, `RequestContext`

### Effective Configuration Merge

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-config-merge`

The system **MUST** merge the effective configuration exactly as `cpt-cf-oagw-algo-config-merge`
specifies, applying the precedence upstream < route < tenant of `cpt-cf-oagw-fr-config-layering`,
honouring the stored sharing modes (`enforce` pinning the ancestor value, `inherit` accepting the
descendant override, `private` contributing at its owning level), computing the effective rate limit
as `min(all enforced ancestor limits, route limit, tenant limit)`, unioning CORS origins under
`inherit` and pinning them under `enforce`, concatenating ancestor and descendant plugin bindings
with upstream bindings before route bindings, and collecting the enforced constraints of every
ancestor in the walked chain so shadowing cannot lift an enforced bound. The merged configuration
**MUST** carry `cred://` references only and **MUST NOT** contain secret material.

**Implements**:
- `cpt-cf-oagw-flow-proxy-request`
- `cpt-cf-oagw-algo-config-merge`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: none — the merge produces no endpoint
- DB: `cpt-cf-oagw-db-schema` (read-side application of the sharing modes recorded on write)
- Entities: `EffectiveUpstream`, `RequestContext`

### Multi-Endpoint Pools and Target-Host Selection

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-endpoint-pool`

The system **MUST** select an endpoint exactly as `cpt-cf-oagw-algo-endpoint-selection` specifies —
a single-endpoint pool selected directly with the header optional but validated when present, a
multi-endpoint pool with a valid `X-OAGW-Target-Host` routed to that endpoint and bypassing
round-robin, a multi-endpoint pool with an explicit alias that is not a common-suffix derivation and
no header distributed by a per-upstream round-robin cursor, and a multi-endpoint pool whose alias is
a common-suffix derivation requiring the
header and returning `400` MissingTargetHost with `alias` and `valid_hosts` when it is absent — and
**MUST** return `400` InvalidTargetHost for a malformed value and `400` UnknownTargetHost for a value
matching no endpoint, both carrying the ADR 0007 extension fields. The header **MUST** be stripped
from the outbound request after selection and the selection method (`explicit_header`,
`round_robin`, `default`) **MUST** be recorded on the request context.

**Implements**:
- `cpt-cf-oagw-flow-proxy-request`
- `cpt-cf-oagw-algo-endpoint-selection`

**Constraints**: `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}` (`X-OAGW-Target-Host` request header, `400` target-host errors)
- DB: none
- Entities: `EffectiveUpstream`, `RequestContext`

### Body Validation, Limits and Smuggling Defence

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-body-validation`

The system **MUST** validate every proxy request exactly as `cpt-cf-oagw-algo-request-validation`
specifies: strict header parsing with `400` for a header name or value containing CR or LF or a
non-token name; `400` for a `Content-Length` that is not a valid integer, for a request carrying both
`Content-Length` and `Transfer-Encoding`, for disagreeing duplicate `Content-Length` values and for a
`Transfer-Encoding` other than `chunked`; `413` for a declared or observed body above the 100MB hard
limit of `cpt-cf-oagw-constraint-body-limit`, rejected before buffering; and `400` for a body whose
actual size does not match its `Content-Length`. The system **MUST** reject an unparseable header
section without recovery and **MUST** apply these checks before any upstream call.

**Implements**:
- `cpt-cf-oagw-flow-proxy-request`
- `cpt-cf-oagw-algo-request-validation`

**Constraints**: `cpt-cf-oagw-constraint-body-limit`

**Principles**: `cpt-cf-oagw-principle-rfc9457`

**Touches**:
- API: every proxy request's validation outcome (`400`, `403`, `413` problem bodies)
- DB: none
- Entities: `RequestContext`, `ErrorContext`

### Header Transformation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-header-transform`

The system **MUST** transform request and response headers exactly as
`cpt-cf-oagw-algo-header-transform` specifies: the configured `request.remove` list, the passthrough
policy `none|allowlist|all` with `request.passthrough_allowlist` and the recorded default `none`, the
hop-by-hop stripping of `Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`,
`Trailer`, `Transfer-Encoding` and `Upgrade` together with the headers the `Connection` header names,
the rewrite of `Host` on HTTP/1.1 and `:authority` on HTTP/2 to the selected endpoint, the strip of
`X-OAGW-Target-Host` after selection, `request.set` then `request.add` with `Content-Length`
recomputed from the buffered body, and on the response side the same hop-by-hop strip plus
`response.set`, `response.add` and `response.remove` with every other upstream header passed through.
The system **MUST NOT** inject a gateway-internal header into an outbound request and **MUST**
produce a `400` when a transformation would yield an invalid header field.

**Implements**:
- `cpt-cf-oagw-flow-proxy-request`
- `cpt-cf-oagw-algo-header-transform`

**Constraints**: `cpt-cf-oagw-constraint-no-direct-internet`

**Principles**: `cpt-cf-oagw-principle-cred-isolation`

**Touches**:
- API: request and response headers of `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- DB: none
- Entities: `RequestContext`, `ResponseContext`

### Upstream Call, Timeout, Version Negotiation and No-Retry

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-call`

The system **MUST** call the upstream exactly as `cpt-cf-oagw-algo-upstream-call` specifies over the
crate's existing `toolkit-http`/`pingora-*` client stack with no new client dependency (DECOMPOSITION
assumption 8), **MUST** bound the call with `oagw.config.proxy_timeout_secs` and classify an expiry
as connection, request or idle timeout, **MUST** negotiate the HTTP version adaptively per host —
attempting HTTP/2 through ALPN on the first TLS handshake, caching the negotiated capability per host
with a one-hour TTL, falling back to HTTP/1.1 on failure and using HTTP/1.1 for an `http` scheme
endpoint — **MUST** send exactly one request and never re-issue the original client request as a
whole, and **MUST NOT** cache any upstream response, header or body beyond the request.

**Implements**:
- `cpt-cf-oagw-flow-proxy-request`
- `cpt-cf-oagw-algo-upstream-call`

**Constraints**: `cpt-cf-oagw-constraint-no-direct-internet`

**Principles**: `cpt-cf-oagw-principle-no-retry`, `cpt-cf-oagw-principle-no-cache`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` (outbound call and `502`/`504` outcomes)
- Config: `oagw.config.proxy_timeout_secs`, `oagw.config.allow_http_upstream`, `oagw.config.ssrf_policy.enabled`
- Entities: `RequestContext`, `ResponseContext`

### SSRF Posture and Plaintext Gate

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ssrf-posture`

The system **MUST** enforce the SSRF posture of `cpt-cf-oagw-nfr-ssrf-protection` on the request
path: the connect target is always the selected endpoint taken from the store and never a
client-supplied host, no internal gateway header is injected into an outbound request, the request
path and query are validated against the matched route, and the well-known internal headers are
stripped or validated. The system **MUST** honour `oagw.config.ssrf_policy.enabled` as the operator
escape hatch of DECOMPOSITION assumption 7 — host validation unconditional at the default `true`,
relaxed at `false` with the target and no-internal-header rules still holding — and **MUST** refuse a
plaintext upstream connection with `503` LinkUnavailable when the endpoint scheme is `http` and
`oagw.config.allow_http_upstream` is `false`, the default posture of
`cpt-cf-oagw-constraint-https-only` failing closed.

**Implements**:
- `cpt-cf-oagw-flow-proxy-request`
- `cpt-cf-oagw-algo-upstream-call`
- `cpt-cf-oagw-algo-header-transform`

**Constraints**: `cpt-cf-oagw-constraint-https-only`, `cpt-cf-oagw-constraint-no-direct-internet`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` (outbound request construction, `503` refusal)
- Config: `oagw.config.allow_http_upstream`, `oagw.config.ssrf_policy.enabled`
- Entities: `RequestContext`

### Circuit Breaker

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-circuit-breaker`

The system **MUST** implement the per-endpoint-host circuit breaker exactly as
`cpt-cf-oagw-algo-circuit-breaker` and the state machine `cpt-cf-oagw-state-circuit-breaker`
specify: a 30-second sliding window per host, a trip to `open` when the fifth failure within the
window is recorded, rejection of further calls with `503` CircuitBreakerOpen before any connection
attempt, a single probe on the cool-down elapsing, and the probe's outcome driving the transition
back to `closed` or to `open`. The system **MUST** count only upstream-call failures (timeouts,
downstream and protocol errors), **MUST** keep the state process-local and reset it on restart, and
**MUST** record every transition for entry 2.7 without emitting a metric itself. The breaker is core
gateway behaviour and **MUST NOT** be implemented as a plugin.

**Implements**:
- `cpt-cf-oagw-flow-proxy-request`
- `cpt-cf-oagw-algo-circuit-breaker`
- `cpt-cf-oagw-state-circuit-breaker`

**Constraints**: None

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` (`503` CircuitBreakerOpen)
- DB: none — breaker state is process-local (DECOMPOSITION assumption 3)
- Entities: `CircuitBreaker`, `ErrorContext`

### CORS Enforcement

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-cors`

The system **MUST** handle CORS per ADR 0004: an `OPTIONS` request carrying `Origin` and
`Access-Control-Request-Method` returns a permissive `204` with the requested origin, method and
headers echoed, `Access-Control-Max-Age: 86400` and the `Vary` header set, with no Bearer
authentication, no upstream resolution, no tenant context and no plugin or rate-limit execution; an
actual cross-origin request
is checked after upstream resolution and before forwarding, a disallowed origin returning `403` with
`gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` and a disallowed method returning
`403` with `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`; the effective CORS
configuration is the merged one of `cpt-cf-oagw-algo-config-merge`; and `Vary: Origin` is always
included on a response to a request that carried `Origin`.

**Implements**:
- `cpt-cf-oagw-flow-proxy-preflight`
- `cpt-cf-oagw-flow-proxy-request`
- `cpt-cf-oagw-algo-request-validation`

**Constraints**: None

**Touches**:
- API: `OPTIONS /oagw/v1/proxy/{alias}[/{path_suffix}]`, CORS response headers on actual requests
- DB: none
- Entities: `RequestContext`, `ResponseContext`

### Error Mapping and Error Source Distinction

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-mapping`

The system **MUST** map every pipeline failure through `cpt-cf-oagw-algo-proxy-error-mapping` onto the
error table of that algorithm — the statuses and GTS `type` identifiers of
`cpt-cf-oagw-interface-api` for `400` Validation, the three `400` target-host errors, `401`
AuthenticationFailed, `404` RouteNotFound, `413` PayloadTooLarge, `429` RateLimitExceeded, `500`
SecretNotFound, `502` DownstreamError and ProtocolError, `503` CircuitBreakerOpen, LinkUnavailable
and PluginNotFound, the three `504` timeouts, and the two ADR 0004 `403` CORS errors — building each
gateway error as `application/problem+json` through the entry-2.1 mapping layer with the standard
problem fields, the OAGW extension fields and the ADR 0007 fields `alias`, `valid_hosts` and
`invalid_value` on the target-host errors. The system **MUST** classify every response with
`X-OAGW-Error-Source` — `gateway` for a mapped error and `upstream` for a passthrough response,
including a passthrough error — and **MUST NOT** emit a gateway error in any other body format or
include credential material in any error body.

**Implements**:
- `cpt-cf-oagw-flow-proxy-request`
- `cpt-cf-oagw-flow-proxy-error-source`
- `cpt-cf-oagw-algo-proxy-error-mapping`

**Constraints**: None

**Principles**: `cpt-cf-oagw-principle-rfc9457`, `cpt-cf-oagw-principle-error-source`

**Touches**:
- API: every proxy response (`application/problem+json` for gateway errors, passthrough otherwise)
- DB: none
- Entities: `ErrorContext`, `ResponseContext`

### Plugin Chain Hook Points

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-hook-points`

The system **MUST** provide the pipeline hook points entry 2.5 executes — the request phase after the
effective configuration merge and before endpoint selection, and the response phase after the
response is classified or the error is mapped — **MUST** hand each hook the effective configuration,
the merged plugin binding list ordered upstream bindings before route bindings, and the request or
response context, **MUST** consume the hook outcome by mapping a rejection onto this feature's error
table (`401`, `400`, `429`, `500`, `503`) and continuing the pipeline on success, and **MUST NOT**
implement any plugin behaviour itself: no credential resolution or injection, no guard decision, no
request or response mutation, no rate-limit accounting and no plugin timeout — all of that is
DECOMPOSITION entry 2.5's, built on the registries of entry 2.3.

**Implements**:
- `cpt-cf-oagw-flow-proxy-request`
- `cpt-cf-oagw-algo-config-merge`
- `cpt-cf-oagw-algo-proxy-error-mapping`

**Constraints**: None

**Touches**:
- API: none — the hook points expose no endpoint
- DB: none
- Entities: `RequestContext`, `ResponseContext`, `ErrorContext`

### Streamed Response Handoff

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-streaming-handoff`

The system **MUST** classify the upstream response before reading it to completion and **MUST** hand
an open exchange to DECOMPOSITION entry 2.6 when the response indicates a streamed body
(`text/event-stream`) or a negotiated upgrade, leaving the streamed session behaviour, its lifecycle
and its error mapping to that entry. That handoff takes two forms, and this DoD states both: on the
response side, a response classified as `text/event-stream` is handed over as an open exchange whose
body is unread and whose response head already carries `X-OAGW-Error-Source`; on the request side, a
request carrying a WebSocket upgrade (`Upgrade: websocket` with `Connection: Upgrade`) is handed over
after this pipeline's header transformation and endpoint selection and before this pipeline's upstream
call, so the upgrade dial, the `101 Switching Protocols` relay and the bidirectional frame relay are
entry 2.6's and this pipeline evaluates neither the circuit breaker nor the response classification
for an upgrade exchange. The system **MUST NOT** buffer a response it has classified as
streamed, **MUST** apply the entry-2.1 header layer to the handoff so `X-OAGW-Error-Source` is
present on streamed responses too, and **MUST** ensure the body-limit checks of
`cpt-cf-oagw-dod-body-validation` do not consume a streamed body.

**Implements**:
- `cpt-cf-oagw-flow-proxy-request`
- `cpt-cf-oagw-algo-upstream-call`
- `cpt-cf-oagw-state-request-lifecycle`

**Constraints**: `cpt-cf-oagw-constraint-body-limit`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` (streamed responses)
- DB: none
- Entities: `ResponseContext`, `RequestContext`

### Proxy Hot-Path Latency Budget

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-latency-budget`

The system **MUST** keep the gateway-added latency of a proxy request below 10ms at p95 excluding the
upstream response time, per `cpt-cf-oagw-nfr-low-latency`, by resolving the alias, matching the
route, merging the configuration, selecting the endpoint and transforming the headers as in-memory
operations over one published snapshot with no blocking I/O on those steps, by advancing a per-upstream
round-robin cursor instead of re-scanning the pool, by serving the HTTP version from the per-host
cache instead of renegotiating on the hot path, and by keeping the upstream call the only I/O on the
path. The system **MUST NOT** perform a blocking or synchronous write on the request path, **MUST**
record the measurements needed to verify the budget for entry 2.7 rather than emitting metrics
itself, and **MUST** leave the plugin-execution and rate-limit costs to the entry-2.5 hook and the
non-blocking logging requirement to entry 2.7.

**Implements**:
- `cpt-cf-oagw-flow-proxy-request`
- `cpt-cf-oagw-algo-alias-walk`
- `cpt-cf-oagw-algo-route-match`
- `cpt-cf-oagw-algo-config-merge`
- `cpt-cf-oagw-algo-endpoint-selection`
- `cpt-cf-oagw-algo-header-transform`

**Constraints**: None

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` (latency of the whole pipeline)
- DB: none
- Entities: `RequestContext`

### Test Layering

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-proxy-test-coverage`

The system **MUST** cover this feature with in-crate Rust tests only — unit tests inside
`#[cfg(test)]` modules per layer for the alias walk and its disabled-shadow rule, route matching, the
configuration merge, endpoint selection and the three target-host errors, body and framing
validation, header transformation, the version negotiation cache, the timeout classification, the
breaker transitions and the error mapping — and integration tests under the crate's `tests/`
directory that boot the gear router and drive the proxy endpoint against a stub upstream listener
provided by the test harness, asserting the passthrough contract, `X-OAGW-Error-Source` on success,
gateway error and upstream error, the preflight `204`, the streamed handoff, the `403` CORS
rejections and the no-retry behaviour. The system **MUST NOT** add an e2e suite under
`testing/e2e/gears/oagw/` (DECOMPOSITION assumption 5).

**Implements**:
- `cpt-cf-oagw-flow-proxy-request`
- `cpt-cf-oagw-flow-proxy-preflight`
- `cpt-cf-oagw-flow-proxy-error-source`
- `cpt-cf-oagw-algo-endpoint-selection`
- `cpt-cf-oagw-algo-circuit-breaker`
- `cpt-cf-oagw-state-circuit-breaker`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` (asserted by the integration tests)
- DB: none — the DoD asserts behaviour, not storage
- Entities: `RequestContext`, `ResponseContext`, `EffectiveUpstream`, `MatchedRoute`, `CircuitBreaker`

## 6. Acceptance Criteria

- [x] A proxy request to an enabled upstream and an enabled route returns the upstream response with its status, headers and body unchanged and `X-OAGW-Error-Source: upstream`.
- [x] `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` is served with no `/api` prefix, and an unauthenticated call or one without `gts.cf.core.oagw.proxy.v1~:invoke` is rejected with `401` and a problem+json body.
- [x] An alias held only by an ancestor tenant resolves through the hierarchy walk; a disabled closest match returns `503` LinkUnavailable and the ancestor upstream with the same alias is not used.
- [x] A disabled route is excluded from matching; a request whose method and path match no enabled route returns `404` RouteNotFound.
- [x] `path_suffix_mode: disabled` with a suffix present returns `400`; a query parameter outside the route's allowlist returns `400`; `path_suffix_mode: append` forwards the match path followed by the suffix.
- [x] The effective configuration applies the precedence upstream < route < tenant, keeps an `enforce` ancestor value, computes the rate limit as the minimum across enforced ancestors, route and tenant, and cannot be lifted by shadowing.
- [x] A multi-endpoint pool with an explicit alias and no target-host header distributes requests by round-robin; a valid `X-OAGW-Target-Host` selects that endpoint and bypasses the cursor.
- [x] A multi-endpoint pool whose alias is a common-suffix derivation returns `400` MissingTargetHost with `alias` and `valid_hosts` when the header is absent, `400` InvalidTargetHost with `invalid_value` for a malformed value, and `400` UnknownTargetHost with `invalid_value` and `valid_hosts` for a value matching no endpoint; `X-OAGW-Target-Host` is never forwarded upstream.
- [x] A `Content-Length` that is not a valid integer, a body whose size does not match its `Content-Length`, a `Transfer-Encoding` other than `chunked`, a request carrying both `Content-Length` and `Transfer-Encoding`, and a header value containing CR or LF all return `400` before any upstream call.
- [x] A body above the 100MB hard limit returns `413` before it is buffered.
- [x] The outbound request carries the `Host` (or HTTP/2 `:authority`) of the selected endpoint, no hop-by-hop header, no `X-OAGW-Target-Host` and no gateway-internal header; the configured `request.set`, `request.add`, `request.remove` and passthrough policy are applied.
- [x] The upstream response receives the configured `response.set`, `response.add` and `response.remove` transformations, loses its hop-by-hop headers and is otherwise passed through unchanged.
- [x] An `OPTIONS` preflight with `Origin` and `Access-Control-Request-Method` returns `204` with the echoed CORS headers and `Access-Control-Max-Age: 86400` without a Bearer token, without resolving a tenant and without resolving an upstream; an actual request with a disallowed origin or method returns `403` with the corresponding ADR 0004 type; `Vary: Origin` is always present on such responses.
- [x] With the recorded default `allow_http_upstream: false`, an `http`-scheme endpoint is refused with `503` before any connection attempt; with the graded configuration the plaintext connection is attempted.
- [x] With the default `ssrf_policy.enabled: true`, upstream host validation is unconditional; with `false`, the connect target is still a stored endpoint and the outbound request still carries no gateway-internal header.
- [x] The fifth failed upstream call within 30 seconds for one endpoint host trips the breaker to `open`, subsequent requests receive `503` CircuitBreakerOpen without contacting the upstream, the cool-down admits one probe, and the probe's outcome closes the breaker or reopens it.
- [x] A connection, request or idle timeout returns `504` with the matching GTS `type` identifier, bounded by `oagw.config.proxy_timeout_secs`.
- [x] A failed upstream call produces exactly one mapped error response and no second request carrying the original body is issued; no upstream response is cached between requests.
- [x] Every gateway error is `application/problem+json` with the GTS `type` identifier, `title`, `status`, `detail` and `instance`, the OAGW extension fields when the request context provides them, and `X-OAGW-Error-Source: gateway`; no error body or log-bound context contains credential material.
- [x] A `text/event-stream` response is handed to the entry-2.6 path without buffering, with `X-OAGW-Error-Source` present on the streamed response.
- [x] The entry-2.5 hook points receive the merged configuration and the ordered plugin binding list, a hook rejection is mapped onto the error table, and no plugin behaviour is implemented in this feature.
- [x] The in-crate unit and integration tests pass with the host feature set used by the graded configuration, the latency measurement asserts the sub-10ms p95 gateway-added budget, and no test artifact is added under `testing/e2e/gears/oagw/`.
- [x] The proxy operations are registered in the host OpenAPI registry (`OpenApiRegistry` parameter of `register_rest`, platform baseline in section 1.4) and appear in the document that registry emits.
