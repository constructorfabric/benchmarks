# Feature: Observability


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Record the Audit Line for a Proxied Request](#record-the-audit-line-for-a-proxied-request)
  - [Serve the Registered Metric Families on the Host Surface](#serve-the-registered-metric-families-on-the-host-surface)
  - [Propagate the Correlation Identifier End to End](#propagate-the-correlation-identifier-end-to-end)
  - [Observe Routing, Rate-Limit and Circuit-Breaker State](#observe-routing-rate-limit-and-circuit-breaker-state)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Build and Emit the Audit Line](#build-and-emit-the-audit-line)
  - [Emit and Normalize the Metric Series](#emit-and-normalize-the-metric-series)
  - [Propagate and Record the Correlation Identifier](#propagate-and-record-the-correlation-identifier)
  - [Sample and Buffer the Log Emission](#sample-and-buffer-the-log-emission)
- [4. States (CDSL)](#4-states-cdsl)
  - [Metric Registry State Machine](#metric-registry-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Audit Line for Every Proxied Request](#audit-line-for-every-proxied-request)
  - [Audit Field Redaction, Levels and Logged-Event Boundary](#audit-field-redaction-levels-and-logged-event-boundary)
  - [Metric Families, Types, Label Sets and Buckets](#metric-families-types-label-sets-and-buckets)
  - [Metric Cardinality and Label Vocabulary](#metric-cardinality-and-label-vocabulary)
  - [Routing and Rate-Limit Series](#routing-and-rate-limit-series)
  - [Circuit-Breaker, Upstream Health and In-Flight Series](#circuit-breaker-upstream-health-and-in-flight-series)
  - [Correlation Identifier Propagation](#correlation-identifier-propagation)
  - [Non-Blocking Emission on the Proxy Hot Path](#non-blocking-emission-on-the-proxy-hot-path)
  - [Test Layering for Observability](#test-layering-for-observability)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-observability-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-observability`

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
  communicated. Skip for simple features to avoid documentation overhead.
=============================================================================
-->
## 1. Feature Context

### 1.1 Overview

This is the narrowest package of the decomposition. It attaches audit logging and Prometheus metrics
to the proxy pipeline DECOMPOSITION entry 2.4 builds, and it owns exactly four things: the shape of
the audit line a closed request context is turned into, the metric families with their names, types,
label sets and cardinality rules, the end-to-end propagation of the correlation identifier, and the
non-blocking character of that emission. The emitters are the sibling features' instrumented steps —
entry 2.4's pipeline records the request outcome, the selected endpoint with its selection method,
the in-flight count and the breaker transitions, entry 2.5 records the rate-limit decision and the
degradation flag, entry 2.6 records the stream close reason, direction and byte counts — so this
feature specifies what is recorded and how, and never re-states the behaviour that produces the
values. It registers no endpoint of its own: the `GET /metrics` surface is host-provided
(DECOMPOSITION assumption 9) and OAGW registers its metric families on it. It writes nothing to the
store, owns no domain entity, adds no new metric name outside the DESIGN §4.2 roster and adds no
distributed tracing backend.

### 1.2 Purpose

Entry 2.7 exists so an operator can answer, from two artefacts they already operate — the centralized
log store the stdout JSON stream feeds and the host metrics surface the host already scrapes — what
outbound traffic the gateway carried, what it rejected, what failed and how long it took, per
`cpt-cf-oagw-nfr-observability`: 100% of proxy requests logged with a correlation ID, metrics
scraped at the `/metrics` endpoint the host provides. It comes after entry 2.4 because a request
context has to be opened at `inst-pe-req-03` and closed with the outcome at `inst-pe-req-30` before
anything can be recorded from it, and it depends on no other entry: it is the last package of the
decomposition, it adds no requirement of its own and it re-states none of the behaviour that produces
the values it records.

**Requirements**:

- [ ] `p2` - `cpt-cf-oagw-nfr-observability`
- [ ] `p1` - `cpt-cf-oagw-nfr-low-latency`

**Design components**: `cpt-cf-oagw-component-model`

**Principles**: None — DECOMPOSITION entry 2.7 covers no design principle, and this feature
introduces none.

**Constraints**: None — DECOMPOSITION entry 2.7 covers no design constraint, and this feature
introduces none.

**Requirement split this feature implements against** (recorded because the decomposition splits both
requirements across entries):

- `cpt-cf-oagw-nfr-low-latency` is covered here ONLY as the non-blocking-logging constraint on the
  proxy hot path: no audit line and no metric update may ever be written synchronously on the request
  path. The latency budget itself — the sub-10ms p95 gateway-added threshold, its measurement and its
  verification — is owned by DECOMPOSITION entry 2.4
  (`cpt-cf-oagw-dod-latency-budget`), which already names the non-blocking logging requirement as a
  part of the budget it does not own. This feature owns the emission cost, not the budget.
- `cpt-cf-oagw-nfr-observability` is covered here in full: the audit line for every proxy request,
  the correlation identifier's end-to-end propagation and every metric family of DESIGN §4.2 that the
  sibling emitters feed.

**Feature-local readings and recorded boundaries** (each is a reading of DESIGN §4.2, DESIGN §4.3,
ADR 0001 or a decomposition assumption, not a new decision):

- `host` is the upstream alias, not the wire host — DESIGN §4.2 states that `host` remains an
  OAGW-specific label carrying the upstream alias, and ADR 0001's example shows a target host name.
  The DESIGN reading is implemented: `host` on every metric label set and in the audit line's `host`
  key is the resolved upstream alias, the same value the entry-2.4 alias walk resolves. Review owner:
  OAGW component maintainer.
- The `phase` label of `oagw_request_duration_seconds` has two values, `gateway_added` and
  `upstream`. DESIGN §4.2 names the label without fixing its vocabulary; the two values are the split
  `cpt-cf-oagw-nfr-low-latency` and `cpt-cf-oagw-dod-latency-budget` already measure — the
  gateway-added portion and the upstream call — so the histogram verifies the budget without a new
  dimension. Review owner: OAGW component maintainer.
- The `path` label of `oagw_rate_limit_exceeded_total` and `oagw_rate_limit_usage_ratio` is the
  route's configured match path, not the raw request path. DESIGN §4.2's cardinality rule states that
  a raw request path must not be a label; the two rate-limit families name a `path` label, so the
  value is the bounded, configuration-derived match path the entry-2.5 rate limit is keyed on and
  never the request path suffix or query. Review owner: OAGW component maintainer.
- `error_type` carries the GTS `type` identifier of the canonical error — the closed vocabulary of
  `cpt-cf-oagw-algo-proxy-error-mapping`'s error table in the
  `gts.cf.core.errors.err.v1~cf.oagw....v1` space, supplied by
  `toolkit_canonical_errors::CanonicalError`. The same value is the `error_type` label of
  `oagw_errors_total`, so a log line and a metric series can be joined on it without a mapping table.
  Review owner: OAGW component maintainer.
- A CORS preflight answered locally by `cpt-cf-oagw-flow-proxy-preflight` produces no audit line and
  no request metric. It is answered before authentication and tenant resolution, enters none of the
  transitions of `cpt-cf-oagw-state-request-lifecycle` and carries no upstream, tenant or host, so it
  is not a proxy request for `cpt-cf-oagw-nfr-observability`'s 100% threshold. Review owner: OAGW
  component maintainer.
- Audit lines for management configuration changes and for authentication failures are not emitted
  here. DESIGN §4.3 lists both among the logged events, but the operations that produce them belong
  to the management plane of DECOMPOSITION entry 2.2 (upstream, route and plugin CRUD, enable and
  disable) and to the auth hook outcome that plane's callers raise. This feature fixes the shape, the
  level vocabulary and the redaction rule those lines follow and owns nothing else about them.
  Review owner: OAGW component maintainer.
- A gateway-rejected client-input class is recorded at `WARN`, not at `ERROR` — DESIGN §4.3 names
  `WARN` for a rate-limit rejection and an open breaker and `ERROR` for upstream failures, timeouts and
  authentication failures, and says nothing about the classes the gateway raises before it calls an
  upstream. A `400` ValidationError, a `404` RouteNotFound and a `413` PayloadTooLarge are
  caller-attributable: the request the caller sent is the fault, no gear component and no upstream
  misbehaved, and `ERROR` stays reserved for gear and upstream faults. `401` AuthenticationFailed keeps
  the `ERROR` DESIGN §4.3 assigns it, and the `409` PluginInUse class is a management-plane class whose
  line entry 2.2 emits under the level vocabulary this feature fixes. Review owner: OAGW component
  maintainer. Validation: an in-crate test asserts the level recorded for every gateway error status of
  the `cpt-cf-oagw-interface-api` error table, including `WARN` for a `400`, a `404` and a `413` and
  `ERROR` for a `401`.
- Successful `proxy_request` lines are sampled at a fixed 1-in-100 rate and failure lines are never
  sampled — DESIGN §4.3 gives "e.g., sample 1/100 for high-volume routes" as an example of
  rate-limiting high-frequency log volume, not as a configuration key, and the gear configuration set
  DECOMPOSITION entry 2.1 loads (`proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy.enabled`,
  `token_cache_ttl_secs`, `token_cache_capacity`) contains no sampling key, so a per-route threshold
  would have nothing to be configured from and no per-route volume measurement to be compared against.
  The reading implemented here is that the rate is a constant of this feature: every successful
  `proxy_request` line is offered at 1 in 100 uniformly, on every route, with an in-process counter, and
  a `WARN` or `ERROR` line is never sampled away. This feature reads none of the entry-2.1 keys and owns
  no sampling key of its own. Review owner: OAGW component maintainer. Validation: an in-crate test
  drives 100 successful `proxy_request` lines through `cpt-cf-oagw-algo-log-sampling` and asserts
  exactly one is emitted, repeats the rate on a second route and asserts no route name enters the
  decision, and asserts a `429` line and a `502` line are both emitted unsampled.

Coverage note: the reference ids cited on the DoDs below that are not carried in the **Requirements**
list above are inherited baselines, not requirements this feature adopts on its own.

**Cross-cutting concerns**:

- Security: no PII and no secret material reaches a log line or a metric label. The audit line never
  carries a request or response body, a query string or a header value — the correlation identifier is
  the only header-derived value on it, and it is carried as a value the caller already supplied, never
  re-derived from an `Authorization`, `Cookie` or credential header. No API key, token, credential or
  resolved `cred://` value is logged under any level, including DEBUG. No metric label carries a
  tenant identifier, a subject, a peer address or a header value. The scraping surface's own
  authentication and the admin-only posture DESIGN §4.2 records are host behaviour, not this gear's.
- Reliability: emission can never become a request-failure mode. A log write that fails, a metrics
  collector that rejects an update and a bounded channel that is full are absorbed in this feature —
  the line or update is dropped, the drop is counted in-process and the request continues with its
  original status, body and headers. Metric state is process-local and is lost on restart
  (DECOMPOSITION assumption 3): every counter starts again from zero and every gauge from its initial
  value, which is a documented limitation and not a defect to be mitigated here.
- Data integrity: the audit line is built from one closed request context, so the values it carries
  are the values the pipeline recorded and are never re-read from a mutable source; a metric update is
  an atomic increment or observation on a registered collector, never a read-modify-write of an
  aggregate. Nothing is persisted (assumption 3), so there is no write path to make durable.
- Observability: this IS the observability feature. It owns the audit line for proxy requests, the
  metric families, their label sets and cardinality rules, the correlation identifier's propagation
  and the non-blocking emission. It does NOT own a `/metrics` endpoint, a distributed tracing backend,
  span export, a trace identifier format or any log shipping — the stdout stream and the host metrics
  surface are the last thing this feature produces, and ingestion, retention and dashboarding are
  operator concern. `trace_id` in a problem body remains the extension field
  `cpt-cf-oagw-algo-proxy-error-mapping` attaches and no tracing pipeline is built for it here.
- Rollback: no persistence and no migration exist, so rollback is the operational act of redeploying
  the previous executable; the only in-gear action is that a restart empties the in-memory counters
  and gauges, so the first scrape after a rollback starts from empty series.
- Test layering: coverage is in-crate Rust tests only — unit tests inside `#[cfg(test)]` modules that
  assert the emitted JSON keys and their order, the level and redaction rules, the label normalization
  and the sampling arithmetic, and integration tests under the crate's `tests/` directory that boot
  the gear router, drive a proxied request against a stub upstream listener and assert the audit line
  and the metric series that result. The `testing/e2e/gears/oagw/` directory is not used
  (DECOMPOSITION assumption 5) and no e2e suite is added.
- Compile-time gate: this feature adds no gear, no host feature flag and no gate of its own; its code
  is linked when the crate is linked for inventory registration, and the families it registers appear
  on the host metrics surface only when the host feature `oagw` is enabled (the entry-2.1 gate).
- Performance: applicable and owned here. Emission is off the hot path by construction — the audit
  line is handed to a bounded in-process channel and the metric update is an atomic operation on a
  registered collector, so no synchronous I/O, no lock held across a write and no allocation beyond
  the line itself sits between the pipeline's response step and the client. The sub-10ms p95
  gateway-added budget is entry 2.4's; this feature's contribution to it is that recording an outcome
  costs no blocking call, which is the part of `cpt-cf-oagw-nfr-low-latency` this feature owns.
- Compliance/Privacy: applicable and owned here, as the redaction rule above. The audit line carries
  identifiers the security context already holds (tenant, principal, request id) and the upstream
  alias, path and method of the request; it carries no personal data beyond those identifiers, no body
  and no header value. Retention, residency and log-store access are operator concerns for the
  centralized log store the stream feeds, not gear behaviour, and nothing is persisted in the gear.
- Accessibility: not applicable in this feature — no user-facing interface is authored. The two
  artefacts are machine-readable by design: a JSON log line and a Prometheus exposition document, and
  no human-facing text is added by this feature.
- Versioning: the metric names, label keys, label vocabularies and the audit key set declared here are
  the compatibility surface operators build dashboards on. They are stable within this delivery, the
  label-key vocabulary is kept aligned with the inbound API Gateway so both gateways share dashboards,
  and this feature adds no version of its own and no breaking change to any contract it references.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Reads the metric series on the host-provided `GET /metrics` surface and the audit lines in the centralized log store to triage routing, rate-limit, circuit-breaker and latency behaviour; owns the log store, the scrape configuration and the sampling posture this feature exposes. |
| `cpt-cf-oagw-actor-app-developer` | Sends the proxy request whose outcome this feature records, supplies `X-Request-ID` when its client correlates requests, and receives the response carrying the same identifier; it produces the traffic this feature describes and never reads it. |
| `cpt-cf-oagw-actor-upstream-service` | Receives the forwarded request and returns the response whose status, sizes and durations this feature records; it never receives an audit field, a metric label value or a correlation identifier it did not already send. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md) — `cpt-cf-oagw-component-model` (the gear structure and the
  internal services the emission layer hangs off), `cpt-cf-oagw-interface-api` (the error table whose
  GTS `type` identifiers are the `error_type` vocabulary, and the `trace_id` extension field this
  feature does not build a pipeline for), `cpt-cf-oagw-seq-proxy-flow` (the request flow whose steps
  are the emitters)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) — entry 2.7 and assumptions 1 to 9, of
  which assumption 3 (in-memory state, so counters and gauges reset on restart) and assumption 9
  (`/metrics` is host-provided) shape this feature directly
- **ADRs**: [0001 Request Routing](../ADR/0001-request-routing.md)
  (`cpt-cf-oagw-adr-request-routing` — the audit log JSON format this feature implements, its
  `proxy_request` event and its 14 keys); supporting baseline:
  [0006 State Management](../ADR/0006-state-management.md) (`cpt-cf-oagw-adr-state-management` —
  in-process data-plane state ownership, which is why the metric registry is per-instance state)
- **Dependencies**: `cpt-cf-oagw-feature-proxy-engine` — this feature reads the request context that
  feature opens at `inst-pe-req-03` and closes at `inst-pe-req-30`, the selection method
  `inst-pe-es-12` records on it, the in-flight count, the breaker transitions of
  `cpt-cf-oagw-state-circuit-breaker` and the error types `cpt-cf-oagw-algo-proxy-error-mapping`
  records at `inst-pe-em-08`; it writes nothing back into that pipeline
- **Sibling emitters referenced, not owned**: `cpt-cf-oagw-feature-auth-plugins-and-rate-limiting`
  records the rate-limit decision, its scope, the degradation flag, the resolved auth method tag and
  the guard outcome on the request context (entry 2.5) and registers no metric family of its own;
  `cpt-cf-oagw-feature-streaming-sse-websocket` records the stream outcome (`closed`, `aborted`,
  `timed_out`), the direction, the byte counts and the error type per stream (entry 2.6);
  `cpt-cf-oagw-feature-gear-foundation` emits the startup log lines and the readiness report this
  feature adds no families to
- **Platform baselines**: the host-provided metrics surface that already serves `GET /metrics`
  (DECOMPOSITION assumption 9) and on which this gear registers its families; the crate's existing
  `tracing` structured-logging stack writing JSON documents to stdout, ingested by a centralized
  logging system such as ELK or Loki (DESIGN §4.3); `toolkit_canonical_errors::CanonicalError` as the
  supplier of `error_type` and `error_message`; the `dashmap`/`parking_lot`/`arc-swap` primitives
  already present in the crate's `Cargo.toml` for the in-memory collectors and the bounded emission
  channel; the gear configuration keys loaded by entry 2.1 (`proxy_timeout_secs`, `allow_http_upstream`,
  `ssrf_policy.enabled`, `token_cache_ttl_secs`, `token_cache_capacity`), none of which this feature
  reads and no sampling key of its own, the sampling rate being a constant of this feature

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor and describe the end-to-end flow of a use case. The
request whose outcome is recorded below is handled by DECOMPOSITION entry 2.4's pipeline, and every
value this feature reads arrives on the request context that pipeline closes; this feature runs after
the response is committed and can change nothing the client receives.

**Use cases**: `cpt-cf-oagw-usecase-proxy-request`

**Referenced, not covered here**:

- `cpt-cf-oagw-usecase-proxy-request` — covered by DECOMPOSITION entry 2.4, which owns the proxy
  request path whose outcome this feature records.
- The proxy request itself, its pipeline stages, its circuit breaker and its error mapping are covered
  by DECOMPOSITION entry 2.4; the flows below start where that pipeline closes a request context.
- The rate-limit decision, the degradation flag, the guard outcomes and the `X-Request-ID` propagation
  of `cpt-cf-oagw-algo-request-id` are covered by DECOMPOSITION entry 2.5, which populates the request
  context and emits nothing itself.
- The stream outcome, its close reason and its byte counts are covered by DECOMPOSITION entry 2.6,
  which records them per stream for this feature to write.
- The management configuration change events (upstream, route and plugin create, replace, delete,
  enable and disable) and the authentication-failure events are covered by DECOMPOSITION entry 2.2's
  management plane, which owns the operations that produce them; this feature fixes the shape, the
  level and the redaction rule those lines follow and emits none of them.
- The startup log lines and the readiness report are covered by DECOMPOSITION entry 2.1
  (`cpt-cf-oagw-dod-startup-observability`); no metric family is registered by that entry.

### Record the Audit Line for a Proxied Request

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-request-audit`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- An actor sends `{METHOD} /oagw/v1/proxy/{alias}/{path_suffix}` for an enabled upstream and route; the
  pipeline completes the request, closes its context with the outcome, and exactly one structured JSON
  audit line with `event` `proxy_request` and the fourteen keys of ADR 0001 is written to stdout with
  `error_type` `null` and level `INFO`.
- An upstream failure (`502`, `504`) produces the same fourteen keys with the failing GTS `type`
  identifier in `error_type`, an added `error_message`, and level `ERROR`; the response the client
  receives is byte-for-byte the response the entry-2.4 pipeline already committed.
- A rate-limited request (`429` RateLimitExceeded) and a request rejected by an open breaker (`503`
  CircuitBreakerOpen) are recorded at level `WARN` with the same key set and no upstream call having
  been made.
- A streamed exchange is recorded once, at its close, with the duration and the byte counts entry 2.6
  recorded and the close reason (`closed`, `aborted`, `timed_out`) folded into the outcome.

**Error Scenarios**:
- A request that resolves no upstream (`404`) is recorded with `host` `null` rather than with the
  requested alias, because no upstream alias was resolved and a requested alias is not a host value.
- A request whose context carries no principal (rejected before authentication) is recorded with
  `principal_id` `null`; no key is ever omitted from the line.
- An emission that cannot complete — a serialization failure or a bounded channel at capacity — drops
  the line, counts the drop in-process and leaves the request's response untouched.

**Steps**:
1. [x] - `p1` - Actor sends the proxy request; the entry-2.4 pipeline handles it end to end and this - `inst-ob-audit-01`
   feature is not invoked until the response is committed
2. [x] - `p1` - The pipeline opens the request context with the correlation identifier - `inst-ob-audit-02`
   (`inst-pe-req-03`) and closes it with the outcome (`inst-pe-req-30`); this feature is called at that
   close with the completed context and no other input
3. [x] - `p1` - Build the audit record with `cpt-cf-oagw-algo-audit-line`: read the context fields, - `inst-ob-audit-03`
   resolve `host` to the resolved upstream alias and `path` to the proxied path without its query
   string, and read no request body, no response body, no query string and no header value -

4. [x] - `p1` - **IF** the closed outcome is a gateway failure - `inst-ob-audit-04`
   1. [x] - `p1` - Set `error_type` from the `toolkit_canonical_errors::CanonicalError` the - `inst-ob-audit-05`
      entry-2.4 mapping produced, add `error_message` from the same error's redacted detail, and set
      the level to `WARN` for a rate-limit rejection, an open breaker or a gateway-rejected
      client-input class (`400` ValidationError, `404` RouteNotFound, `413` PayloadTooLarge) and to
      `ERROR` for an upstream failure, a timeout or a credential failure
5. [x] - `p1` - **ELSE** - `inst-ob-audit-06`
   1. [x] - `p1` - Emit `error_type` as JSON `null`, add no `error_message`, and set the level to - `inst-ob-audit-07`
      `INFO`
6. [x] - `p1` - Hand the same closed request outcome to `cpt-cf-oagw-algo-metric-emit` so the request - `inst-ob-audit-07a`
   families are updated from the context that produced the line: `oagw_requests_total` is incremented
   and `oagw_request_duration_seconds` observed for every closed request, and `oagw_errors_total` is
   incremented when the outcome is a gateway failure
7. [x] - `p1` - Hand the record to `cpt-cf-oagw-algo-log-sampling` for its class's sampling decision - `inst-ob-audit-08`
   and to the non-blocking writer; never perform a synchronous or blocking write on the request path -

8. [x] - `p1` - **IF** the writer cannot accept or serialize the line - `inst-ob-audit-09`
   1. [x] - `p1` - Drop the line, count the drop in-process and return control to the pipeline with no - `inst-ob-audit-10`
      error raised; the client response is never retried, altered or delayed by an emission failure -

9. [x] - `p1` - **RETURN** control to the pipeline; the response the client receives is unchanged - - `inst-ob-audit-11`


### Serve the Registered Metric Families on the Host Surface

- [x] `p2` - **ID**: `cpt-cf-oagw-flow-metrics-observation`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- An operator scrapes `GET /metrics` on the host surface and finds every family of DESIGN §4.2 this
  gear registers, with the exact names, types and label keys of `cpt-cf-oagw-algo-metric-emit`.
- A family with no series yet — no request served since the process started — appears with its type
  and help metadata and zero series rather than being absent, so a dashboard does not lose a series
  across a restart.
- The histogram family presents the twelve DESIGN §4.2 buckets, so a latency percentile computed in
  the operator's dashboard is comparable across deployments.

**Error Scenarios**:
- The host surface is unavailable or rejects a family at registration; the gear logs the failure at
  `ERROR`, serves traffic with no metric emission and never fails a request because of it.
- A scrape arrives before the gear finished registering; the operator sees a partial document and the
  next scrape is complete, with no partial family left registered.

**Steps**:
1. [x] - `p1` - Actor requests `GET /metrics` on the host-provided metrics surface - `inst-ob-met-01`
2. [x] - `p1` - The host authenticates and authorizes the scrape and serves the endpoint; this gear - `inst-ob-met-02`
   registers no endpoint of its own and answers no scrape itself (DECOMPOSITION assumption 9) -

3. [x] - `p1` - The families registered during gear initialization - `inst-ob-met-03`
   (`cpt-cf-oagw-state-metric-registry`) are collected into the exposition document with the names,
   types and label keys of `cpt-cf-oagw-algo-metric-emit`
4. [x] - `p1` - **FOR EACH** registered family in the document - `inst-ob-met-04`
   1. [x] - `p1` - Expose the family's type and help metadata and its current series; a counter carries - `inst-ob-met-05`
      its accumulated value, a gauge its current value and the histogram its twelve buckets with their
      cumulative counts
5. [x] - `p1` - **IF** a family has no series since the process started - `inst-ob-met-06`
   1. [x] - `p1` - Expose the family with its metadata and zero series rather than omitting it, so a - `inst-ob-met-07`
      series lost to a restart is visible as an empty family and not as a vanished name
6. [x] - `p1` - **ELSE** carry the accumulated series; no OAGW code runs on the scrape path beyond the - `inst-ob-met-08`
   registered collectors
7. [x] - `p1` - **RETURN** the exposition document to the host, which returns it to the actor - - `inst-ob-met-09`


### Propagate the Correlation Identifier End to End

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-correlation-propagation`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- An actor sends a proxy request with `X-Request-ID` set; the identifier reaches the outbound request,
  the response, and the `request_id` key of the audit line, unchanged.
- An actor sends a proxy request with no `X-Request-ID`; the correlation identifier the entry-2.4
  pipeline generated at `inst-pe-req-03` is the value recorded on the audit line, and the outbound
  `X-Request-ID` (when the entry-2.5 RequestId transform is bound) is the same identifier.
- A gateway failure returns an `application/problem+json` body whose `trace_id` extension field and
  whose audit line carry the same identifier, so a caller, a log line and a metric-sourced alert can be
  tied to one request.

**Error Scenarios**:
- A request that reaches no upstream still carries the identifier on the audit line and in the problem
  body: the identifier is opened at `inst-pe-req-03`, before resolution, and is never absent from a
  recorded request.
- An inbound `X-Request-ID` this feature finds on the request context is recorded as it arrived and is
  never rewritten, truncated or validated here; the propagation and any rewrite of the header is the
  entry-2.5 RequestId transform's behaviour, and no correlation or routing header is injected into an
  outbound request by this feature.

**Steps**:
1. [x] - `p1` - Actor sends the proxy request, optionally carrying `X-Request-ID` - `inst-ob-corr-01`
2. [x] - `p1` - The entry-2.4 pipeline opens the request context with the correlation identifier - `inst-ob-corr-02`
   (`inst-pe-req-03`); the entry-2.5 RequestId transform (`cpt-cf-oagw-algo-request-id`) reads the
   inbound header, propagates or generates the value, and records it on the request context
   (`inst-ari-05`)
3. [x] - `p1` - Read the correlation identifier from the request context and carry it into the audit - `inst-ob-corr-03`
   record as `request_id`; read no header value from the request to obtain it
4. [x] - `p1` - Carry the identifier through the entry-2.5 transform to the outbound request and to the - `inst-ob-corr-04`
   response (`inst-ari-03`, `inst-ari-06`) without injecting or stripping any header here; the
   outbound header set is that transform's output, not this feature's
5. [x] - `p1` - **IF** the outcome is a gateway failure - `inst-ob-corr-05`
   1. [x] - `p1` - Confirm the identifier is the same value the problem body's `trace_id` extension - `inst-ob-corr-06`
      field carries, so a client-reported `trace_id` resolves to one audit line
6. [x] - `p1` - **ELSE** carry the identifier on the audit line only; a successful passthrough response - `inst-ob-corr-07`
   carries no gateway-generated identifier beyond the `X-Request-ID` the bound transform set -

7. [x] - `p1` - Apply the allowlist rule of `cpt-cf-oagw-algo-audit-line`: the correlation identifier is - `inst-ob-corr-08`
   the only header-derived value on any log line this feature emits
8. [x] - `p1` - **RETURN** the identifier to the caller's record: one identifier per request, on the - `inst-ob-corr-09`
   request context, on the outbound request when propagated, on the response when propagated, and on
   the audit line

### Observe Routing, Rate-Limit and Circuit-Breaker State

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-runtime-state-observation`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- An operator reads `oagw_routing_endpoint_selected{upstream_id, endpoint_host, selection_method}` and
  sees the distribution of endpoint selections across `explicit_header`, `round_robin` and `default`,
  matching the selection method the entry-2.4 pipeline recorded at `inst-pe-es-12`.
- An operator reads `oagw_rate_limit_exceeded_total{host, path}` and
  `oagw_rate_limit_usage_ratio{host, path}` and sees which route is being throttled and how close its
  bucket is to exhaustion, including the requests a `degrade` strategy admitted.
- An operator reads `oagw_circuit_breaker_state{host}` and
  `oagw_circuit_breaker_transitions_total{host, from_state, to_state}` and sees the current state of
  every per-host breaker and the count of each transition it has taken, matching
  `cpt-cf-oagw-state-circuit-breaker`.
- An operator reads `oagw_upstream_available{host, endpoint}` and
  `oagw_upstream_connections{host, state}` and sees which endpoints are up and how the connections of
  each host are distributed across `idle`, `active` and `max`.

**Error Scenarios**:
- A breaker that has never left `closed` exposes a state gauge of its initial value and no transition
  series, and this is distinguishable from a breaker that is absent because the upstream has no
  configuration.
- A restart empties every counter and gauge; the operator sees the series reset to their initial
  values rather than disappear, and no historical rate can be reconstructed from the gear
  (DECOMPOSITION assumption 3).

**Steps**:
1. [x] - `p1` - Actor reads the routing, rate-limit, breaker, upstream-health and in-flight series from - `inst-ob-obs-01`
   the exposition document of `cpt-cf-oagw-flow-metrics-observation`
2. [x] - `p1` - The routing series are emitted by the entry-2.4 endpoint-selection step - `inst-ob-obs-02`
   (`inst-pe-es-12`), which records the selection method on the request context; this feature owns the
   family name, the label keys and the `explicit_header`|`round_robin`|`default` vocabulary, not the
   selection
3. [x] - `p1` - The rate-limit series are emitted by the entry-2.5 rate-limit step from the decision and - `inst-ob-obs-02b`
   the degradation flag it records on the request context; this feature owns the family names, the
   label keys and the 0.0 to 1.0 bound of the usage ratio
4. [x] - `p1` - The breaker series are emitted by the entry-2.4 breaker on each transition of - `inst-ob-obs-03`
   `cpt-cf-oagw-state-circuit-breaker`, which records every transition without emitting a metric
   itself; this feature owns the gauge, the transition counter and its `host`, `from_state` and
   `to_state` labels
5. [x] - `p1` - The in-flight series is emitted from the request-context lifecycle itself: the - `inst-ob-obs-03a`
   entry-2.4 pipeline opens the request context at `inst-pe-req-03` and closes it at `inst-pe-req-30`,
   and each of those two steps hands an in-flight change to `cpt-cf-oagw-algo-metric-emit`, which
   increments `oagw_requests_in_flight` on the open and decrements it on the close; this feature owns
   the family name and its `host` label, not the open or the close
6. [x] - `p1` - **FOR EACH** recorded value, normalize the label set with - `inst-ob-obs-04`
   `cpt-cf-oagw-algo-metric-emit` before the update reaches a collector
7. [x] - `p1` - **IF** a value arrives with a label value outside its recorded vocabulary - `inst-ob-obs-05`
   1. [x] - `p1` - Map it onto the recorded fallback for that family (an unrecognized method to - `inst-ob-obs-06`
      `_OTHER`, an unrecognized state to the family's documented closed set) and never invent a new
      label value at runtime
8. [x] - `p1` - **ELSE** apply the update to the collector atomically and return - `inst-ob-obs-07`
9. [x] - `p1` - **RETURN**; the series are visible on the next scrape of - `inst-ob-obs-08`
   `cpt-cf-oagw-flow-metrics-observation`

## 3. Processes / Business Logic (CDSL)

Internal system functions that do not interact with actors directly. Each is called from a flow above
or from a sibling feature's instrumented step, and none of them performs I/O on the request path.

### Build and Emit the Audit Line

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-audit-line`

**Input**: the closed request context — the correlation identifier, the tenant and principal
identifiers, the resolved upstream alias, the proxied path without its query string, the method, the
status, the duration in milliseconds, the request and response sizes, the error type and message when
the outcome is a failure, and the stream close reason, direction and byte counts when the response was
streamed.

**Output**: exactly one structured JSON document written to stdout, carrying the fourteen keys of ADR
0001 in order — `timestamp`, `level`, `event` (`proxy_request`), `request_id`, `tenant_id`,
`principal_id`, `host`, `path`, `method`, `status`, `duration_ms`, `request_size`, `response_size`,
`error_type` — plus `error_message` when the outcome is a failure, and nothing else.

**Steps**:
1. [x] - `p1` - Read the context fields and resolve each of the fourteen keys; `timestamp` is the - `inst-ob-aline-01`
   emission instant in UTC with millisecond precision, `event` is the literal `proxy_request`, and
   `status` is the numeric status the client received
2. [x] - `p1` - Resolve `host` to the resolved upstream alias, the same value the `host` metric label - `inst-ob-aline-02`
   carries; when no upstream was resolved, set it to JSON `null` and never to the requested alias,
   path or authority
3. [x] - `p1` - Resolve `path` to the proxied path with its query string removed, and `method` to the - `inst-ob-aline-02a`
   request method as received; derive `request_size` and `response_size` from the byte counts the
   context records, and `duration_ms` from the request's own start-to-close interval
4. [x] - `p1` - **IF** the response was streamed and handed to DECOMPOSITION entry 2.6 - `inst-ob-aline-03`
   1. [x] - `p1` - Fold the recorded close reason (`closed`, `aborted`, `timed_out`), direction and byte - `inst-ob-aline-04`
      counts into `response_size` and, when the close was an abort or a timeout, into `error_type`;
      emit one line at the close, never one per chunk
5. [x] - `p1` - **IF** the outcome is a failure - `inst-ob-aline-05`
   1. [x] - `p1` - Set `error_type` to the GTS `type` identifier of the - `inst-ob-aline-05a`
      `toolkit_canonical_errors::CanonicalError` and `error_message` to its redacted `detail`, which is
      the value the client already received in the problem body
   2. [x] - `p1` - Set `level` to `WARN` when the failure is a rate-limit rejection, an open circuit - `inst-ob-aline-06`
      breaker or a gateway-rejected client-input class (`400` ValidationError, `404` RouteNotFound,
      `413` PayloadTooLarge), and to `ERROR` for an upstream failure, a timeout or a credential
      failure
6. [x] - `p1` - **ELSE** set `error_type` to JSON `null`, add no `error_message`, and set `level` to - `inst-ob-aline-07`
   `INFO`
7. [x] - `p1` - Apply the redaction rule: no request body, no response body, no query string, no header - `inst-ob-aline-08`
   value other than the correlation identifier, and no secret, API key, token or resolved credential
   value is placed in any field
8. [x] - `p1` - **TRY** to serialize the document as a single JSON line - `inst-ob-aline-09`
   1. [x] - `p1` - Offer the serialized line to the bounded writer of `cpt-cf-oagw-algo-log-sampling` - `inst-ob-aline-10`
      without blocking
9. [x] - `p1` - **CATCH** a serialization or writer failure - `inst-ob-aline-11`
   1. [x] - `p1` - Drop the line, count the drop in-process, and raise no error to the caller; the - `inst-ob-aline-12`
      request's response is already committed and is never retried or altered
10. [x] - `p1` - **RETURN** the emission result - `inst-ob-aline-13`

**Edge handling**: a CORS preflight answered by `cpt-cf-oagw-flow-proxy-preflight` is not a proxy
request and produces no line; a context field that is unknown at emission time is emitted as JSON
`null` and never omitted; a streamed exchange produces one line, emitted at its close; a request that
produces more than one response (no such path exists in the pipeline) would produce one line, and the
algorithm holds no state between calls.

### Emit and Normalize the Metric Series

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-metric-emit`

**Input**: the event to record — a completed request outcome, an in-flight change, a breaker
transition, a rate-limit decision or usage ratio, a routing selection, an upstream health change or a
connection-state change — together with the request context or the component state it came from.

**Output**: an atomic update to exactly one registered collector of `cpt-cf-oagw-state-metric-registry`,
with a label set drawn from the recorded vocabulary and no new label key or value.

**Steps**:
1. [x] - `p1` - Select the family from the roster and normalize each label value: `http.request.method` - `inst-ob-aemit-01`
   to a standard verb or `_OTHER`, `http.response.status_code` to the numeric upstream status for a
   request that reached an upstream and to the numeric gateway status the client received for a request
   on which no upstream call was made, so the label agrees with the audit line's `status` field,
   `http.route` to the normalized route match pattern the entry-2.4 route match produced and never to
   the raw request path, and `host` to the resolved upstream alias
2. [x] - `p1` - Normalize the component-specific labels: `upstream_id` and `endpoint_host` for the - `inst-ob-aemit-02`
   routing families, `selection_method` to `explicit_header`, `round_robin` or `default`, `state` to
   `idle`, `active` or `max`, `from_state` and `to_state` to `closed`, `open` or `half_open`, and
   `endpoint` to the stored endpoint identifier
3. [x] - `p1` - Apply the cardinality rules: add no tenant label, no subject, no peer address and no - `inst-ob-aemit-03`
   request identifier to any label set, and derive the `path` label of the two rate-limit families from
   the route's configured match path
4. [x] - `p1` - **IF** the family is the request-duration histogram - `inst-ob-aemit-03a`
   1. [x] - `p1` - Observe the duration in seconds into the twelve DESIGN §4.2 buckets - `inst-ob-aemit-04`
      `[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]`, with `phase`
      `gateway_added` for the gateway-added portion and `upstream` for the upstream call
5. [x] - `p1` - **IF** the family is a gauge with a recorded range - `inst-ob-aemit-05`
   1. [x] - `p1` - Clamp `oagw_rate_limit_usage_ratio` into 0.0 to 1.0 and set - `inst-ob-aemit-06`
      `oagw_upstream_available` to `0` or `1`, so no update can place a value outside its documented
      range
6. [x] - `p1` - **ELSE IF** the family is a counter - `inst-ob-aemit-07`
   1. [x] - `p1` - Increment `oagw_requests_total`, `oagw_errors_total`, - `inst-ob-aemit-07a`
      `oagw_rate_limit_exceeded_total`, `oagw_routing_target_host_used`,
      `oagw_routing_endpoint_selected` or `oagw_circuit_breaker_transitions_total` by one, and never
      decrement a counter
7. [x] - `p1` - **ELSE** set `oagw_requests_in_flight`, `oagw_circuit_breaker_state`, - `inst-ob-aemit-08`
   `oagw_upstream_available` or `oagw_upstream_connections` to its current value
8. [x] - `p1` - **TRY** to apply the update to the registered collector - `inst-ob-aemit-09`
   1. [x] - `p1` - Perform the update as an atomic operation on the collector, with no lock held across - `inst-ob-aemit-10`
      any I/O and no allocation on the request path beyond the label set
9. [x] - `p1` - **CATCH** an unregistered family, a rejected label set or a collector failure - `inst-ob-aemit-11`
   1. [x] - `p1` - Drop the update, count the drop in-process and raise no error to the emitting step; - `inst-ob-aemit-12`
      a metric that cannot be recorded never fails a request
10. [x] - `p1` - **RETURN** the update result - `inst-ob-aemit-13`

**Error handling**: an unknown family, a label key that is not in the family's recorded set and a
label value outside the recorded vocabulary are all dropped rather than materialized, because an
unbounded label set is the one failure mode of this feature that degrades the host process rather than
a single request.

### Propagate and Record the Correlation Identifier

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-correlation-propagate`

**Input**: the request context with the correlation identifier the entry-2.4 pipeline opened at
`inst-pe-req-03`, the inbound `X-Request-ID` when the entry-2.5 RequestId transform recorded one, and
the outcome being recorded.

**Output**: the correlation identifier placed on the audit line's `request_id`, on the problem body's
`trace_id` extension field for a gateway failure, and carried unchanged on the outbound request and
the response when the entry-2.5 transform is bound.

**Steps**:
1. [x] - `p1` - Take the correlation identifier from the request context; never read a header value to - `inst-ob-acorr-01`
   obtain it and never generate one here
2. [x] - `p1` - **IF** the entry-2.5 RequestId transform recorded an identifier on the context - `inst-ob-acorr-02`
   (`inst-ari-05`)
   1. [x] - `p1` - Treat that identifier as the correlation identifier for this request, so the audit - `inst-ob-acorr-03`
      line, the outbound header and the response header agree
3. [x] - `p1` - **ELSE** use the identifier the pipeline generated at `inst-pe-req-03`, which is present - `ob-acorr-04`
   on every request including one rejected before authentication
4. [x] - `p1` - Place the identifier on the audit record as `request_id`, and on a gateway failure - `inst-ob-acorr-04a`
   confirm it is the value carried by the problem body's `trace_id` extension field
5. [x] - `p1` - Leave the outbound header set to the entry-2.5 transform: this algorithm injects no - `inst-ob-acorr-05`
   correlation, routing or tenant header into an outbound request and strips none, per the entry-2.4
   no-internal-header rule
6. [x] - `p1` - Add no second identifier: the correlation identifier is the only request identifier this - `inst-ob-acorr-06`
   feature records, and no metric label carries it (cardinality rule)
7. [x] - `p1` - **RETURN** the identifier as recorded - `inst-ob-acorr-07`

**Edge handling**: an identifier that arrives empty or absent is replaced by the context's own
identifier and is never logged as an empty string; an identifier that contains characters outside a
header-safe set is recorded as it arrived and is never interpreted, truncated or escaped into another
field.

### Sample and Buffer the Log Emission

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-log-sampling`

**Input**: the serialized audit line, its event class (`proxy_request` success, `proxy_request`
failure, circuit-breaker event, configuration change, authentication failure) and the current in-process
sampling state.

**Output**: either the line handed to the bounded in-process writer, or the line dropped with the drop
counted.

**Steps**:
1. [x] - `p1` - Classify the line's event class from the record; the class decides whether the line is - `inst-ob-asamp-01`
   sampled and the level recorded on it, and never a rate, which is fixed
2. [x] - `p1` - **IF** the class is a failure, a circuit-breaker event or an authentication failure - - `inst-ob-asamp-02`

   1. [x] - `p1` - Never sample the line away, but rate-limit the authentication-failure class so a - `inst-ob-asamp-03`
      flooding caller cannot grow the log volume without bound (DESIGN §4.3)
3. [x] - `p1` - **ELSE IF** the class is a successful `proxy_request` line - `inst-ob-asamp-03a`
   1. [x] - `p1` - Sample the line at the fixed rate of 1 in 100, applied uniformly to every route with - `inst-ob-asamp-04a`
      an in-process counter, so no route's volume is measured, no route is named in the decision and no
      configuration key is read to obtain the rate
4. [x] - `p1` - **ELSE** emit the line unsampled - `inst-ob-asamp-04`
5. [x] - `p1` - **TRY** to offer the line to the bounded channel the writer drains - `inst-ob-asamp-05`
   1. [x] - `p1` - Offer without waiting: the offer is a non-blocking attempt that returns immediately - `inst-ob-asamp-06`
      whether or not the channel had capacity, and no caller ever awaits a log write
6. [x] - `p1` - **CATCH** a channel at capacity - `inst-ob-asamp-07`
   1. [x] - `p1` - Drop the line, count the drop in-process and report the count at `DEBUG`; no metric - `inst-ob-asamp-08`
      family beyond the DESIGN §4.2 roster is added for it, and no request is slowed or failed -

7. [x] - `p1` - **RETURN** the emission result - `inst-ob-asamp-09`

**Edge handling**: the sampling decision is deterministic per request and holds no per-request state;
the rate is the fixed 1-in-100 reading DESIGN §4.3 gives as its example and is not a configuration key
of this feature or of the entry-2.1 config set, no per-route threshold exists and no route's volume is
measured, a failure, a circuit-breaker event and an authentication-failure line are never sampled away,
and a line whose class cannot be determined is emitted unsampled rather than dropped.

## 4. States (CDSL)

One entity of this feature has an explicit lifecycle: the metric registry this gear holds in memory.
The breaker states, the request lifecycle and the stream lifecycle are recorded here but owned
elsewhere, and their transitions are not restated as this feature's.

### Metric Registry State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-metric-registry`

**States**: `unregistered`, `registering`, `registered`

**Initial State**: `unregistered`

**Transitions**:
1. [x] - `p1` - **FROM** `unregistered` **TO** `registering` **WHEN** the gear's initialization begins - `inst-ob-mreg-01`
   registering its metric families on the host-provided metrics surface
2. [x] - `p1` - **FROM** `registering` **TO** `registered` **WHEN** every family of DESIGN §4.2 has been - `inst-ob-mreg-02`
   registered with its name, type, label keys and histogram buckets
3. [x] - `p1` - **FROM** `registering` **TO** `unregistered` **WHEN** the host surface is unavailable or - `inst-ob-mreg-02a`
   rejects a family; the failure is logged at `ERROR`, no series is emitted and traffic continues to be
   served
4. [x] - `p1` - **FROM** `registered` **TO** `unregistered` **WHEN** the host process tears the gear - `inst-ob-mreg-03`
   down; the in-memory counters and gauges are dropped with it, so the next start begins from empty
   series (DECOMPOSITION assumption 3)

**Closed transition set**: the transitions above are the only ones possible. The machine is
process-local and holds no persisted state; a `registered` registry re-entering `unregistered` on
teardown is the documented reset-on-restart limitation and not a failure, and no family is registered
or unregistered per request. A registration that fails partway leaves no partial family behind: the
registry returns to `unregistered` and emits nothing, rather than exposing a family whose label set
differs from the recorded one.

## 5. Definitions of Done

Specific implementation tasks derived from flows/algorithms above.

### Audit Line for Every Proxied Request

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-audit-line`

The system **MUST** emit exactly one structured JSON audit line for every proxied request, with `event`
set to the literal `proxy_request` and with the fourteen keys of ADR 0001 — `timestamp`, `level`,
`event`, `request_id`, `tenant_id`, `principal_id`, `host`, `path`, `method`, `status`,
`duration_ms`, `request_size`, `response_size`, `error_type` — present on every line in that order,
with `error_type` JSON `null` on a successful request and with the GTS `type` identifier of the
`toolkit_canonical_errors::CanonicalError` plus an added `error_message` on a failure. The system
**MUST** set `host` to the resolved upstream alias, `path` to the proxied path without its query
string, `duration_ms` to the request's own start-to-close interval and the two size fields to the byte
counts the context records, **MUST** emit a streamed exchange once at its close with the byte counts
entry 2.6 recorded, **MUST** emit a context field that is unknown as JSON `null` rather than omitting
the key, and **MUST NOT** emit a line for a CORS preflight answered by
`cpt-cf-oagw-flow-proxy-preflight`.

**Implements**:
- `cpt-cf-oagw-flow-request-audit`
- `cpt-cf-oagw-algo-audit-line`

**Constraints**: None

**Touches**:
- API: none — this feature registers no endpoint; `GET /metrics` is host-provided
- DB: none — nothing is persisted (DECOMPOSITION assumption 3)
- Entities: none — this feature owns no domain entity; it reads the request context entry 2.4 owns

### Audit Field Redaction, Levels and Logged-Event Boundary

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-audit-fields`

The system **MUST** keep every log line free of request and response bodies, query strings and header
values, with the correlation identifier as the only header-derived value on any line, and **MUST NOT**
log an API key, a token, a credential or a resolved `cred://` value at any level, in any field, on any
code path. The system **MUST** apply the DESIGN §4.3 level vocabulary — `INFO` for a successful
request and a normal operation, `WARN` for a rate-limit rejection, an open circuit breaker, a
gateway-rejected client-input class (`400` ValidationError, `404` RouteNotFound, `413`
PayloadTooLarge) and retry guidance, `ERROR` for an upstream failure, a timeout and an authentication
failure, `DEBUG` for detailed plugin execution and disabled in production — and **MUST** leave the
emission of the configuration-change and authentication-failure lines to the management plane of
DECOMPOSITION entry 2.2, which owns the operations that produce them, while this feature fixes the
shape, the level vocabulary and the redaction rule those lines follow.

**Implements**:
- `cpt-cf-oagw-flow-request-audit`
- `cpt-cf-oagw-flow-correlation-propagation`
- `cpt-cf-oagw-algo-audit-line`
- `cpt-cf-oagw-algo-log-sampling`

**Constraints**: None

**Touches**:
- API: none
- DB: none
- Entities: none

### Metric Families, Types, Label Sets and Buckets

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-metric-families`

The system **MUST** register every family of DESIGN §4.2 on the host-provided metrics surface, with
the exact names, types and label keys: `oagw_requests_total{host, http.request.method, http.route,
http.response.status_code}` as a counter, `oagw_request_duration_seconds{host, http.route, phase}` as
a histogram, `oagw_requests_in_flight{host}` as a gauge, `oagw_errors_total{host, http.route,
error_type}` as a counter, `oagw_circuit_breaker_state{host}` as a gauge,
`oagw_rate_limit_exceeded_total{host, path}` as a counter,
`oagw_circuit_breaker_transitions_total{host, from_state, to_state}` as a counter,
`oagw_rate_limit_usage_ratio{host, path}` as a gauge bounded to 0.0 to 1.0,
`oagw_routing_target_host_used{upstream_id, endpoint_host}` as a counter,
`oagw_routing_endpoint_selected{upstream_id, endpoint_host, selection_method}` as a counter,
`oagw_upstream_available{host, endpoint}` as a gauge valued `0` or `1` and
`oagw_upstream_connections{host, state}` as a gauge with `state` in `idle`, `active` and `max`. The
system **MUST** present the request-duration histogram with the twelve DESIGN §4.2 buckets
`[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]`, **MUST** expose a family
with no series since process start with its metadata and zero series rather than omitting it, **MUST**
add no family outside this roster, and **MUST NOT** register, expose or advertise a `/metrics`
endpoint of its own.

**Implements**:
- `cpt-cf-oagw-flow-metrics-observation`
- `cpt-cf-oagw-flow-request-audit`
- `cpt-cf-oagw-algo-metric-emit`
- `cpt-cf-oagw-state-metric-registry`

**Constraints**: None

**Touches**:
- API: `GET /metrics` (host-provided; families registered on it, endpoint not owned by this gear)
- DB: none
- Entities: none

### Metric Cardinality and Label Vocabulary

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-metric-cardinality`

The system **MUST** keep every label set bounded: no tenant label, no subject, no peer address and no
request identifier on any family; `http.route` set to the normalized route match pattern and never to
the raw request path; `http.request.method` normalized to a standard verb or `_OTHER`;
`http.response.status_code` set to the numeric upstream status per the OTel HTTP semantic conventions
and to the numeric gateway status the client received when no upstream call was made, so the label
agrees with the audit line's `status` field;
status-class questions such as a 5xx rate answered at query time by a regular expression over the
numeric code rather than by a status-class label; and the label-key vocabulary kept aligned with the
inbound API Gateway so both gateways share dashboards. The system **MUST** set `host` to the
OAGW-specific upstream-alias value on every family that carries it, **MUST** set the `path` label of
the two rate-limit families to the route's configured match path, and **MUST NOT** introduce a label
key or a label value at runtime that is not in the recorded vocabulary.

**Implements**:
- `cpt-cf-oagw-flow-metrics-observation`
- `cpt-cf-oagw-flow-runtime-state-observation`
- `cpt-cf-oagw-algo-metric-emit`

**Constraints**: None

**Touches**:
- API: none
- DB: none
- Entities: none

### Routing and Rate-Limit Series

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-routing-metrics`

The system **MUST** own the routing and rate-limit series and their vocabularies:
`oagw_routing_endpoint_selected{upstream_id, endpoint_host, selection_method}` with `selection_method`
restricted to `explicit_header`, `round_robin` and `default`, matching the method the entry-2.4
endpoint-selection step records on the request context at `inst-pe-es-12`;
`oagw_routing_target_host_used{upstream_id, endpoint_host}` tracking the `X-OAGW-Target-Host` usage;
`oagw_rate_limit_exceeded_total{host, path}` for the rejections entry 2.5 records, including the
requests a `degrade` strategy admitted and flagged; and `oagw_rate_limit_usage_ratio{host, path}` for
the bucket's distance from exhaustion. The system **MUST** leave the emission to the sibling steps
that produce the values and **MUST NOT** re-derive a selection, a rate-limit decision or a degradation
flag itself.

**Implements**:
- `cpt-cf-oagw-flow-runtime-state-observation`
- `cpt-cf-oagw-algo-metric-emit`

**Constraints**: None

**Touches**:
- API: none
- DB: none
- Entities: none

### Circuit-Breaker, Upstream Health and In-Flight Series

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-breaker-metrics`

The system **MUST** own the resilience series: `oagw_circuit_breaker_state{host}` as the gauge of the
current per-host state, `oagw_circuit_breaker_transitions_total{host, from_state, to_state}` counting
each transition of `cpt-cf-oagw-state-circuit-breaker` with `from_state` and `to_state` drawn from
`closed`, `open` and `half_open`, `oagw_upstream_available{host, endpoint}` valued `0` for down and
`1` for up, `oagw_upstream_connections{host, state}` with `state` in `idle`, `active` and `max`, and
`oagw_requests_in_flight{host}` as the gauge of requests the pipeline currently holds, incremented when
the pipeline opens the request context at `inst-pe-req-03` and decremented when it closes it at
`inst-pe-req-30`, so a gateway-rejected request opens and closes the same context. The system
**MUST** emit the transition counter once per transition and never per rejected request, **MUST**
record every transition the breaker takes including the probe's outcome, and **MUST** leave the
transition itself, the trip threshold and the cool-down to DECOMPOSITION entry 2.4.

**Implements**:
- `cpt-cf-oagw-flow-runtime-state-observation`
- `cpt-cf-oagw-algo-metric-emit`

**Constraints**: None

**Touches**:
- API: none
- DB: none
- Entities: none

### Correlation Identifier Propagation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-correlation-ids`

The system **MUST** propagate the correlation identifier end to end: the identifier is present on every
recorded request, including one rejected before authentication, it is the `request_id` key of the audit
line, it agrees with the `trace_id` extension field of a gateway error body, and it is the value
carried to the outbound request and the response by the entry-2.5 RequestId transform
(`cpt-cf-oagw-algo-request-id`) when that binding is present. The system **MUST** read the identifier
from the request context and **MUST NOT** generate one, rewrite one, inject a correlation or routing
header into an outbound request, or place the identifier on any metric label.

**Implements**:
- `cpt-cf-oagw-flow-correlation-propagation`
- `cpt-cf-oagw-algo-correlation-propagate`
- `cpt-cf-oagw-algo-audit-line`

**Constraints**: None

**Touches**:
- API: the `request_id` key of the audit line and the `trace_id` extension field of a gateway error body
- DB: none
- Entities: none

### Non-Blocking Emission on the Proxy Hot Path

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-non-blocking-logging`

The system **MUST** keep audit logging and metric emission off the proxy hot path: the audit line is
handed to a bounded in-process channel that a separate drain writes to stdout, the offer never waits,
and no synchronous or blocking write, no file or socket I/O and no lock held across a write occurs on
the request path; the metric update is an atomic operation on a registered collector with no I/O. The
system **MUST** sample successful `proxy_request` lines at the fixed rate of 1 in 100, applied uniformly
to every route and obtained from no configuration key and no per-route volume measurement, **MUST**
never sample a `WARN` or `ERROR` line away, **MUST** rate-limit the
authentication-failure class rather than growing the log volume without bound, **MUST** drop a line or
an update it cannot emit, count the drop in-process and continue, and **MUST** satisfy the
non-blocking half of `cpt-cf-oagw-nfr-low-latency` this feature owns while leaving the latency budget
itself to `cpt-cf-oagw-dod-latency-budget`.

**Implements**:
- `cpt-cf-oagw-flow-request-audit`
- `cpt-cf-oagw-flow-correlation-propagation`
- `cpt-cf-oagw-algo-log-sampling`
- `cpt-cf-oagw-algo-metric-emit`

**Constraints**: None

**Touches**:
- API: none
- DB: none
- Entities: none

### Test Layering for Observability

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-observability-test-coverage`

The system **MUST** cover this feature with in-crate Rust tests only: unit tests inside `#[cfg(test)]`
modules asserting that a completed request produces a line with exactly the fourteen keys in order and
`error_type` `null`, that a failure adds `error_message` and the GTS `type` identifier, that no body,
query string, header value or secret appears in any emitted line at any level, that the level
vocabulary of DESIGN §4.3 is applied, that each family is registered with its recorded name, type and
label keys, that the histogram presents the twelve recorded buckets, that the cardinality rules hold
(no tenant label, normalized `http.route`, normalized method, numeric status), and that the sampling
arithmetic and the bounded-writer drop behave as `cpt-cf-oagw-algo-log-sampling` specifies; and
integration tests under the crate's `tests/` directory that boot the gear router, drive a proxied
request against a stub upstream listener and assert the emitted JSON keys and the resulting metric
series. The system **MUST NOT** add any artifact under `testing/e2e/gears/oagw/`
(DECOMPOSITION assumption 5).

**Implements**:
- `cpt-cf-oagw-flow-request-audit`
- `cpt-cf-oagw-flow-metrics-observation`
- `cpt-cf-oagw-algo-audit-line`
- `cpt-cf-oagw-algo-metric-emit`

**Constraints**: None

**Touches**:
- API: none
- DB: none
- Entities: none

## 6. Acceptance Criteria

- [x] Every proxied request produces exactly one audit line with `event` `proxy_request` and all
  fourteen keys of ADR 0001 present in order, with `error_type` `null` on success; no key is ever
  omitted and no key beyond `error_message` on a failure is ever added.
- [x] A level is recorded for every gateway error status of the `cpt-cf-oagw-interface-api` error
  table: a `400` RouteError, ValidationError, MissingTargetHost, InvalidTargetHost or UnknownTargetHost,
  a `404` RouteNotFound, a `413` PayloadTooLarge, a `429` RateLimitExceeded and a `503`
  CircuitBreakerOpen are recorded at `WARN`; a `409` PluginInUse is a management-plane class whose line
  entry 2.2 emits and is recorded at `WARN` under the same vocabulary; a `401` AuthenticationFailed, a
  `500` SecretNotFound, a `502` ProtocolError, DownstreamError or StreamAborted, a `503` LinkUnavailable
  or PluginNotFound and a `504` ConnectionTimeout, RequestTimeout or IdleTimeout are recorded at
  `ERROR`; a successful passthrough is recorded at `INFO`.
- [x] A failed request's `error_type` is the GTS `type` identifier of the
  `toolkit_canonical_errors::CanonicalError` the entry-2.4 mapping produced, and the added
  `error_message` is the same redacted `detail` the client received in the problem body.
- [x] `host` is the resolved upstream alias and equals the value of the `host` metric label for the
  same request; a request that resolves no upstream records `host` as JSON `null` and never records a
  requested alias, path or authority in its place.
- [x] `path` is the proxied path with its query string removed; no query parameter value, request body
  byte, response body byte or header value other than the correlation identifier appears in any emitted
  line at any level, including `DEBUG`.
- [x] A request rejected before authentication is recorded with every key present, `principal_id` JSON
  `null` where unknown, and with the correlation identifier the pipeline opened at `inst-pe-req-03` as
  its `request_id`.
- [x] A CORS preflight answered by `cpt-cf-oagw-flow-proxy-preflight` produces no audit line and no
  request metric series update.
- [x] A streamed exchange produces exactly one audit line, emitted at its close, whose `duration_ms`,
  `response_size` and `error_type` reflect the close reason (`closed`, `aborted`, `timed_out`), the
  direction and the byte counts entry 2.6 recorded; no line is emitted per chunk.
- [x] The `request_id` of the audit line equals the correlation identifier carried on the request
  context, equals the `X-Request-ID` propagated to the outbound request and the response when the
  entry-2.5 RequestId transform is bound, and equals the `trace_id` extension field of a gateway error
  body for the same request.
- [x] A scrape of `GET /metrics` returns all twelve families of DESIGN §4.2 with their recorded names,
  types and label keys, and a family with no series since process start appears with its metadata and
  zero series rather than being absent.
- [x] `oagw_request_duration_seconds` presents exactly the twelve buckets
  `[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]` and its `phase` label takes
  only the values `gateway_added` and `upstream`.
- [x] `oagw_requests_total` increments by exactly one for a successfully proxied request and by exactly
  one for a gateway-rejected request, each increment taken from the outcome the entry-2.4 pipeline
  closes at `inst-pe-req-30` and handed to `cpt-cf-oagw-algo-metric-emit` by
  `cpt-cf-oagw-flow-request-audit`.
- [x] `oagw_errors_total` is left unchanged by a successfully proxied request and increments by exactly
  one for a gateway-rejected request, its `error_type` label being the GTS `type` identifier the audit
  line carries for the same request.
- [x] `oagw_request_duration_seconds` records one `gateway_added` observation for a successfully
  proxied request and one for a gateway-rejected request, the rejected request adding no `upstream`
  observation because the pipeline made no upstream call.
- [x] `oagw_requests_in_flight` reads one higher while a request is open between `inst-pe-req-03` and
  `inst-pe-req-30` and returns to its previous value once the request closes, for a successfully
  proxied request and for a gateway-rejected request alike, since a rejected request opens and closes
  the same context.
- [x] No metric family carries a tenant identifier, a subject, a peer address or a request identifier
  as a label value; `http.route` is the normalized route match pattern and never the raw request path;
  `http.request.method` is a standard verb or `_OTHER`; `http.response.status_code` is the numeric
  upstream status for a request that reached an upstream and the numeric gateway status the client
  received for a request on which no upstream call was made, agreeing with the audit line's `status`
  field for the same request.
- [x] `oagw_routing_endpoint_selected{upstream_id, endpoint_host, selection_method}` takes
  `selection_method` only from `explicit_header`, `round_robin` and `default`, and its counts agree
  with the selection method the entry-2.4 pipeline records at `inst-pe-es-12` for the same requests.
- [x] `oagw_circuit_breaker_state{host}` reports the current state of each per-host breaker and
  `oagw_circuit_breaker_transitions_total{host, from_state, to_state}` increments exactly once for each
  transition of `cpt-cf-oagw-state-circuit-breaker`, with `from_state` and `to_state` drawn from
  `closed`, `open` and `half_open` and no increment for a request a breaker rejects.
- [x] `oagw_rate_limit_usage_ratio{host, path}` stays within 0.0 to 1.0 for every observed route and
  `oagw_rate_limit_exceeded_total{host, path}` increments for a throttled request and for a request a
  `degrade` strategy admitted; the `path` label value is the route's configured match path.
- [x] A successful `proxy_request` line is sampled at the fixed rate of 1 in 100, applied uniformly to
  every route with no configuration key read and no route's volume measured to obtain the rate, and a
  `WARN` or `ERROR` line — a rate-limit rejection, an open breaker, a gateway-rejected client-input
  class, an upstream failure, a timeout, a credential failure or an authentication failure — is never
  sampled away.
- [x] A request that completes while a log line is being emitted is never delayed by it: no synchronous
  write, no file or socket I/O and no lock held across a write occurs on the request path, a full
  bounded channel drops the line and counts the drop, and the client's status, body and headers are
  unchanged by any emission outcome.
- [x] A process restart returns every counter to zero and every gauge to its initial value, and no
  family disappears from the next scrape (in-memory state, DECOMPOSITION assumption 3).
- [x] The in-crate unit and integration tests assert the emitted JSON keys and the metric series listed
  above, run inside the `cf-gears-oagw` crate's own test targets, and no test artifact is added under
  `testing/e2e/gears/oagw/`.
