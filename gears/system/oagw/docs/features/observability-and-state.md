# Feature: Observability and State



<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Metrics Scrape](#metrics-scrape)
  - [Proxy Request Instrumentation](#proxy-request-instrumentation)
  - [Rate-Limit Signal Emission and Per-Instance Limiter State](#rate-limit-signal-emission-and-per-instance-limiter-state)
  - [Circuit-Breaker Metric Surface](#circuit-breaker-metric-surface)
  - [Audit Record Emission](#audit-record-emission)
  - [Trace Identifier Propagation](#trace-identifier-propagation)
  - [Control Plane Configuration Read and L1 Population](#control-plane-configuration-read-and-l1-population)
  - [Control Plane Write-Side Invalidation](#control-plane-write-side-invalidation)
  - [Data Plane Hot-Config Lookup](#data-plane-hot-config-lookup)
  - [Data Plane Hot-Config Flush](#data-plane-hot-config-flush)
  - [Shared Upstream HTTP Client](#shared-upstream-http-client)
  - [Health and Readiness Reporting](#health-and-readiness-reporting)
  - [Deployment Mode Selection](#deployment-mode-selection)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Metric Label Normalization](#metric-label-normalization)
  - [Cache Key Derivation](#cache-key-derivation)
  - [L1 Cache Maintenance](#l1-cache-maintenance)
  - [Write-Side Invalidation and Data Plane Flush](#write-side-invalidation-and-data-plane-flush)
  - [Audit Record Construction](#audit-record-construction)
  - [Deployment Mode Resolution](#deployment-mode-resolution)
- [4. States (CDSL)](#4-states-cdsl)
  - [Cache Entry State Machine](#cache-entry-state-machine)
  - [Health and Readiness Surface State Machine](#health-and-readiness-surface-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Metrics Endpoint Registration](#metrics-endpoint-registration)
  - [Metric Names, Labels, and Histogram Buckets](#metric-names-labels-and-histogram-buckets)
  - [Metric Cardinality Rules](#metric-cardinality-rules)
  - [Request, Latency, and Error Metrics](#request-latency-and-error-metrics)
  - [Rate-Limit Metrics and Per-Instance Limiter State](#rate-limit-metrics-and-per-instance-limiter-state)
  - [Circuit-Breaker Metric Names Only](#circuit-breaker-metric-names-only)
  - [Structured JSON Audit Logging](#structured-json-audit-logging)
  - [Trace Identifier Propagation](#trace-identifier-propagation-1)
  - [Control Plane L1 Configuration Cache](#control-plane-l1-configuration-cache)
  - [Data Plane L1 Hot-Config Cache](#data-plane-l1-hot-config-cache)
  - [Cache Key Families](#cache-key-families)
  - [Shared Upstream HTTP Client](#shared-upstream-http-client-1)
  - [Per-Instance Rate-Limit State Ownership](#per-instance-rate-limit-state-ownership)
  - [Health and Readiness Surface](#health-and-readiness-surface)
  - [Deployment Mode Posture](#deployment-mode-posture)
  - [Automated Unit Test Coverage](#automated-unit-test-coverage)
  - [Automated Integration Test Coverage](#automated-integration-test-coverage)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [x] `p1` - **ID**: `cpt-cf-oagw-featstatus-observability-implemented`

<!-- reference to DECOMPOSITION entry -->
- [x] `p2` - `cpt-cf-oagw-feature-observability-and-operability`

## 1. Feature Context

### 1.1 Overview

Implements the observability surface and the Control Plane / Data Plane state ownership that make the gear operable and fast. This feature registers the Prometheus metric registry and exposes it at `GET /metrics`, emits the structured JSON audit record for every proxy request, propagates trace identifiers into problem+json responses and log records, owns the Control Plane L1 configuration cache with write-side invalidation, owns the Data Plane L1 hot-config cache with an explicit flush triggered on configuration writes, owns the shared upstream HTTP client and the per-instance rate-limit state of the Data Plane, and reports gear health and readiness through the ToolKit `RestApiCapability` healthcheck hook.

### 1.2 Purpose

Without this feature the proxy hot path is invisible and slow: operators cannot see request volume, latency, errors, or rate-limit pressure, and every proxy request would re-read configuration from the store. This feature instruments the Data Plane path delivered by `cpt-cf-oagw-feature-request-proxy` and owns the in-memory state that both planes rely on. It is the last entry of the decomposition: it instruments and caches the whole flow, which is why it depends on `cpt-cf-oagw-feature-request-proxy` and, transitively through it, on `cpt-cf-oagw-feature-gear-foundation`.

The feature realizes the observability slice of `cpt-cf-oagw-component-model` — the metrics recorder and registry in `infra/metrics.rs`, the audit logger emitting structured JSON to stdout, the Control Plane L1 cache with write-side invalidation, the Data Plane L1 hot-config cache with explicit flush, and the health and readiness surface — and it sits in the infrastructure layer recorded in `cpt-cf-oagw-design-layers` so that neither the domain layer nor the transport layer depends on a metric or cache implementation. It inherits the Control Plane / Data Plane ownership split of `cpt-cf-oagw-design-overview`, serves the low-latency driver collected in `cpt-cf-oagw-design-drivers`, and carries the technology surface declared in `cpt-cf-oagw-tech-dependencies` for this entry: an in-process metric registry, an in-process LRU cache pair, and the shared upstream client behind the reverse-proxy engine of `cpt-cf-oagw-design-dependencies`. The mechanism that keeps the layers independent is concrete: the infrastructure layer supplies a caching decorator implementing the domain repository trait, so the domain layer's repository boundary is unaware of the cache, and metric recording is performed in `infra/proxy/` around the domain service call, so the domain layer never depends on a metric implementation.

The caches are in scope here because `cpt-cf-oagw-adr-data-plane-caching` and `cpt-cf-oagw-adr-state-management` place both L1 layers in scope; they supersede the "future consideration" wording of DESIGN §4.1, which this entry supersedes locally as recorded in the decomposition overview. The L2 layer of `cpt-cf-oagw-adr-data-plane-caching` remains out of scope: the decision marks it optional and the graded configuration has no Redis dependency.

**Requirements**:

- [x] `p2` - `cpt-cf-oagw-nfr-observability` - the metric registry at `GET /metrics`, the audit record with correlation identifiers, and trace-identifier propagation; 100% of proxy requests are logged with a correlation identifier and metrics are scrapeable at `/metrics`
- [x] `p1` - `cpt-cf-oagw-nfr-high-availability` - the single-executable deployment posture, the health and readiness surface, and the cache-invalidation and flush behavior that keeps the hot path consistent; the circuit breaker itself is DESIGN §4.7 future work and only its metric names are in scope (graded deviation 9)

The latency budget these caches serve, `cpt-cf-oagw-nfr-low-latency`, is delivered by `cpt-cf-oagw-feature-request-proxy`; this feature supplies the configuration reads that budget assumes, at the sub-microsecond L1 access time `cpt-cf-oagw-adr-data-plane-caching` records as its design target rather than as a figure this entry measures. The request-instrumentation surface also serves `cpt-cf-oagw-usecase-proxy-request`, the rate-limit signal surface serves `cpt-cf-oagw-usecase-rate-limit-exceeded`, and the write-side invalidation and flush surface serves `cpt-cf-oagw-usecase-configure-upstream` and `cpt-cf-oagw-usecase-configure-route`.

**Principles**: none delivered. The decomposition records that this entry introduces no new design principle and only instruments the principles implemented by the proxy and error features. One principle bounds this feature from the outside: `cpt-cf-oagw-principle-no-cache` forbids caching upstream responses, which is why every cache here holds configuration, never a response, and why the shared client and the proxy path never persist a response body.

**Constraints**: none delivered. `cpt-cf-oagw-constraint-toolkit-deploy` (single-executable deployment) is covered by `cpt-cf-oagw-feature-gear-foundation`; this feature follows it by keeping all metric, cache, and rate-limit state in-process so the single executable needs no external state service.

**Sequences**: none owned. The cache lookup and flush steps are steps inside `cpt-cf-oagw-seq-proxy-flow`, which is owned by `cpt-cf-oagw-feature-request-proxy`; this feature supplies the behavior behind those steps and defines no sequence of its own.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Scrapes `GET /metrics` after passing the admin authorization boundary, reads the audit stream from stdout, consults the health and readiness surface, and selects the deployment mode that decides whether only the L1 layers exist |
| `cpt-cf-oagw-actor-tenant-admin` | Performs the configuration writes whose side effects are Control Plane L1 write-side invalidation and the Data Plane L1 flush |
| `cpt-cf-oagw-actor-app-developer` | Generates the proxy traffic that is counted, timed, logged, and rate-limit instrumented, and consumes the trace identifier returned in a problem+json body |
| `cpt-cf-oagw-actor-upstream-service` | Receives the requests the shared HTTP client issues and is the subject of the connection-state and availability signals the registry publishes |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **ADRs**: [ADR 0001](../ADR/0001-request-routing.md) (audit-record field set), [ADR 0003](../ADR/0003-rate-limiting.md) (rate-limit strategy and the `X-RateLimit-*` response headers; it does not own limiter state placement), [ADR 0005](../ADR/0005-data-plane-caching.md) (L1/L2 split, cache keys, invalidation), [ADR 0006](../ADR/0006-state-management.md) (CP/DP state ownership, including the per-instance rate-limiter ownership the limiter flow cites), [ADR 0007](../ADR/0007-error-source-distinction.md) (`trace_id` in problem+json)
- **Dependencies**: `cpt-cf-oagw-feature-request-proxy` (direct; the proxy hot path is instrumented and the Data Plane L1 cache is keyed by its resolutions), `cpt-cf-oagw-feature-gear-foundation` (transitive, via `cpt-cf-oagw-feature-request-proxy`; the caches and the metric registry are wired during gear initialization alongside the services they instrument)

**Applicability**: the requirement domains this entry does not carry are excluded explicitly rather than left unaddressed:

- **Compliance (COMPL-001, COMPL-002)**: not applicable because no regulatory regime applies to the metric, audit, cache, or health surface of this entry, and this entry carries no data-residency, retention-regulation, or licensing obligation; the audit record is written to stdout and its retention is the centralized logging consumer's concern, which is out of scope here.
- **User experience (UX-001, UX-002)**: not applicable because the observability surface is a machine-scraped exposition, a JSON log stream, and a healthcheck result consumed by the platform; there is no human UI journey and no browser UI for this entry to cover.
- **Integration — database and messaging (INT-002, INT-004)**: not applicable because this entry integrates no external database — the store behind the repository boundary is the in-process, config-backed repository of `cpt-cf-oagw-feature-gear-foundation` (graded deviation 5) — and no message bus, because the Data Plane flush notification is a direct in-process call in the graded single-executable configuration.
- **Data — index and join patterns (DATA)**: not applicable because both caches are in-process keyed maps over `CacheKey`, so no query engine, index shape, or join pattern applies; every lookup is a keyed lookup and every invalidation is the enumerable key set derived by `cpt-cf-oagw-algo-observability-and-state-cache-key-derivation`.
- **Performance — batch and N+1 patterns (PERF)**: not applicable because the read path has no N+1 shape: one read serves exactly one `CacheKey` from one cache and issues at most one backing read, and the lock-contention posture of the two hot-path structures is recorded on `cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance` rather than as a separate performance obligation here.

The domains this entry does carry are stated rather than implied. **Audit retention** is owned by the centralized logging consumer and is out of scope here: this entry writes one JSON line per event to stdout and buffers no log record in any cache it owns. **Rollout and rollback** is the cold-start posture of PRD §12: a restarted instance serves from a cold cache and its rate-limit state is empty until repopulated, so there is no persisted limiter state to roll forward or back. **Recovery after restart** is the same posture — both L1 caches repopulate lazily from the repository on first read and the limiters restart empty rather than being restored. **Lock contention** is the per-cache single-mutex posture recorded on `cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance`. The **technical-debt notes** this entry carries are the circuit-breaker metric stubs registered without a breaker implementation (graded deviation 9) and the shared L2 layer of `cpt-cf-oagw-adr-data-plane-caching` that a configuration requesting it is refused as unsupported, both deliberate reservations rather than missing work.

## 2. Actor Flows (CDSL)

**Use cases**: this feature exposes no end-user use case of its own. `cpt-cf-oagw-usecase-proxy-request` supplies the traffic that is instrumented and cached, `cpt-cf-oagw-usecase-rate-limit-exceeded` supplies the rejection events the rate-limit metrics count, and `cpt-cf-oagw-usecase-configure-upstream` and `cpt-cf-oagw-usecase-configure-route` supply the configuration writes whose side effects invalidate and flush the caches.

### Metrics Scrape

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-observability-and-state-metrics-scrape`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A caller in the platform admin principal class requests `GET /metrics` and receives the Prometheus text exposition of every registered metric family, including the request, duration, in-flight, error, circuit-breaker, rate-limit, routing, and upstream-health families of DESIGN §4.2.
- The endpoint is registered by this feature at `/metrics`, deliberately outside the gear-relative `/oagw/v1` prefix that the rest of the gear registers, so the metric surface is addressable as a platform-level scrape target rather than as an OAGW API operation.
- A non-administrator is refused before any metric value is read, with the refusal rendered by `cpt-cf-oagw-feature-error-handling` as `403`.

**Error Scenarios**:
- The request carries no administrator authorization: the scrape is rejected and no metric series is disclosed. A caller that authenticates but is not in the platform admin principal class — including one that holds a valid proxy permission — is refused with `403` rendered by `cpt-cf-oagw-feature-error-handling`, because the check admits only the platform's admin principal class.
- The endpoint is addressed by a path under `/oagw/v1`: no such route exists there, because the metric surface is registered only at the platform-level `/metrics` path.

**Steps**:
1. [x] - `p1` - Register the `/metrics` route on the router contributed through `RestApiCapability`, outside the `/oagw/v1` prefix used by every other OAGW route - `inst-os-scrape-1`
2. [x] - `p1` - Apply the platform admin authorization predicate to the registered route through the same `authz_resolver` handle `cpt-cf-oagw-feature-gear-foundation` delivers that the management API uses, admitting only the platform's admin principal class and no other principal class, as a documented dependency on the platform authorization boundary recorded against `cpt-cf-oagw-constraint-toolkit-deploy` as the deployment posture reference - `inst-os-scrape-2`
3. [x] - `p1` - **IF** the request fails the admin authorization check - `inst-os-scrape-3`
   1. [x] - `p1` - Refuse the request without enumerating, counting, or disclosing any metric series - `inst-os-scrape-4`
4. [x] - `p1` - Render every registered counter, gauge, and histogram family with its declared label set into the Prometheus text exposition format - `inst-os-scrape-5`
5. [x] - `p1` - Serve the exposition with the numeric values current at scrape time and no cached snapshot, so consecutive scrapes observe monotonic counters and current gauges - `inst-os-scrape-6`
6. [x] - `p1` - **RETURN** the exposition body with the response status the framework assigns to a successful handler - `inst-os-scrape-7`

### Proxy Request Instrumentation

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- Every proxy request increments `oagw_requests_total{host, http.request.method, http.route, http.response.status_code}`, observes `oagw_request_duration_seconds{host, http.route, phase}` against the DESIGN §4.2 bucket set, and is reflected in `oagw_requests_in_flight{host}` for the interval it is being processed.
- Every request that ends in a gateway error increments `oagw_errors_total{host, http.route, error_type}` in addition to the request counter.
- Every routed request increments `oagw_routing_target_host_used{upstream_id, endpoint_host}` and `oagw_routing_endpoint_selected{upstream_id, endpoint_host, selection_method}` with `selection_method` one of `explicit_header`, `round_robin`, or `default`.

**Error Scenarios**:
- A request rejected before a route match exists produces no `http.route` label value and no routing metric, and is still counted in `oagw_requests_total` and in `oagw_errors_total` with the route label omitted.
- An upstream that never becomes reachable leaves `oagw_upstream_available{host, endpoint}` at 0 for its series and produces no duration observation for the `upstream_call` phase beyond the failure observation.

**Steps**:
1. [x] - `p1` - Increment `oagw_requests_in_flight{host}` when the proxy handler takes a request and decrement it when the handler returns, including on the abort path of a streaming response - `inst-os-req-1`
2. [x] - `p1` - Observe `oagw_request_duration_seconds{host, http.route, phase}` at each recorded pipeline phase, with `phase` taken from the fixed declared set `route_match`, `plugin_chain_request`, `upstream_call`, `plugin_chain_response`, and `response`, and using the bucket set `[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]` - `inst-os-req-2`
3. [x] - `p1` - Increment `oagw_requests_total{host, http.request.method, http.route, http.response.status_code}` once the response status is known, with the method and status normalized by `cpt-cf-oagw-algo-observability-and-state-metric-label-normalization` - `inst-os-req-3`
4. [x] - `p1` - **IF** the request ends in a gateway error rather than a passthrough response - `inst-os-req-4`
   1. [x] - `p1` - Increment `oagw_errors_total{host, http.route, error_type}` with the `error_type` carried by the gateway error contract owned by the error-handling entry - `inst-os-req-5`
5. [x] - `p1` - **IF** a routing decision selects an endpoint for the request - `inst-os-req-6`
   1. [x] - `p1` - Increment `oagw_routing_target_host_used{upstream_id, endpoint_host}` and `oagw_routing_endpoint_selected{upstream_id, endpoint_host, selection_method}` with `selection_method` taken from `explicit_header`, `round_robin`, and `default` - `inst-os-req-7`
6. [x] - `p1` - Publish `oagw_upstream_available{host, endpoint}` as a 0/1 gauge per configured endpoint and `oagw_upstream_connections{host, state}` as a gauge with `state` one of `idle`, `active`, and `max`, sourced from the connection state of the shared client - `inst-os-req-8`
7. [x] - `p1` - Record every metric on the request path through per-family atomic counters, gauges, and histogram accumulators that take no shared lock on the request path, so metric recording adds no cross-request serialization to the proxy hot path and the only guard the instrumentation takes is the per-cache one recorded on `cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance` - `inst-os-req-8b`
8. [x] - `p1` - **RETURN** the response to the caller with the instrumentation complete for the request - `inst-os-req-9`

### Rate-Limit Signal Emission and Per-Instance Limiter State

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-observability-and-state-rate-limit-signals`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- Every rejected request increments `oagw_rate_limit_exceeded_total{host, path}`, where the `path` label carries the normalized route match pattern (`http.route`) and never the raw request path.
- `oagw_rate_limit_usage_ratio{host, path}` reports the consumed fraction of the effective limit as a value between 0.0 and 1.0 for the same normalized route pattern.
- The limiter state feeding both signals is per-instance state owned by the Data Plane, keyed by the counter scope of the rate-limiting entry, with no distributed coordination.

**Error Scenarios**:
- A request rejected before a route match exists yields no normalized route pattern, so no rate-limit series is produced for it and no raw request path is ever written into the `path` label.
- Two gateway instances report different usage ratios for the same route, because per-instance counters are not globally accurate; this is the documented consequence of `cpt-cf-oagw-adr-state-management`.

**Steps**:
1. [x] - `p1` - Hold the rate-limit counter state in the Data Plane as per-instance in-memory state, keyed by the configured counter scope and the tenant identity of the request, as owned by the Data Plane per `cpt-cf-oagw-adr-state-management` - `inst-os-rl-1`
2. [x] - `p1` - Do not synchronize that state with any other instance and do not persist it, so the state lives and dies with the process - `inst-os-rl-2`
3. [x] - `p1` - **IF** a rate-limit evaluation rejects a request - `inst-os-rl-3`
   1. [x] - `p1` - Increment `oagw_rate_limit_exceeded_total{host, path}` with the `path` label set to the normalized route match pattern of the matched route, never to the raw request path - `inst-os-rl-4`
4. [x] - `p1` - Publish `oagw_rate_limit_usage_ratio{host, path}` from the same per-instance counter state as a gauge in the range 0.0 to 1.0 - `inst-os-rl-5`
5. [x] - `p1` - **IF** no route matched the request - `inst-os-rl-6`
   1. [x] - `p1` - Emit no rate-limit series for the request rather than substituting the raw request path for the missing route pattern - `inst-os-rl-7`
6. [x] - `p1` - **RETURN** the rate-limit decision to the proxy path with the rate-limit signals recorded for the request - `inst-os-rl-8`

### Circuit-Breaker Metric Surface

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-observability-and-state-breaker-metric-surface`

**Actor**: `cpt-cf-oagw-actor-upstream-service`

**Success Scenarios**:
- The metric registry registers `oagw_circuit_breaker_state{host}` and `oagw_circuit_breaker_transitions_total{host, from_state, to_state}` with the label sets DESIGN §4.2 declares, so a future breaker implementation can publish into a scrape surface that already exists.
- The upstream-facing signals `oagw_upstream_available{host, endpoint}` and `oagw_upstream_connections{host, state}` report the reachability and connection state observed through the shared client.

**Error Scenarios**:
- No breaker transition is ever recorded and no `from_state` or `to_state` value is emitted, because breaker state is out of scope per graded deviation 9; the metric families exist unpopulated rather than being absent from the exposition.

**Steps**:
1. [x] - `p1` - Register `oagw_circuit_breaker_state{host}` as a gauge family and `oagw_circuit_breaker_transitions_total{host, from_state, to_state}` as a counter family in the registry at initialization - `inst-os-cb-1`
2. [x] - `p1` - Do not implement breaker state, breaker transitions, or the `CircuitBreakerOpen` error outcome; DESIGN §4.7 lists the circuit breaker as future development and graded deviation 9 confines this entry to the metric names - `inst-os-cb-2`
3. [x] - `p1` - Expose the two breaker families in the `GET /metrics` exposition alongside the other registered families so a future implementation needs no registry change - `inst-os-cb-3`
4. [x] - `p1` - **IF** a future breaker implementation registers a transition - `inst-os-cb-4`
   1. [x] - `p1` - Record it in `oagw_circuit_breaker_transitions_total{host, from_state, to_state}` and update `oagw_circuit_breaker_state{host}` without altering the label vocabulary - `inst-os-cb-5`
5. [x] - `p1` - **RETURN** a registered breaker metric surface with no breaker behavior attached to it - `inst-os-cb-6`

### Audit Record Emission

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-observability-and-state-audit-record`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Every proxy request produces one structured JSON audit record on stdout carrying exactly the field set of `cpt-cf-oagw-adr-request-routing`: `timestamp`, `level`, `event`, `request_id`, `tenant_id`, `principal_id`, `host`, `path`, `method`, `status`, `duration_ms`, `request_size`, `response_size`, and `error_type`.
- Configuration writes and authentication failures produce their own events at the levels DESIGN §4.3 assigns, and the sampling policy applies only to those two non-proxy-request event classes so log volume stays bounded without ever suppressing a proxy-request record; a high-volume route is still a proxy request, so its records are emitted unconditionally with their correlation identifiers.

**Error Scenarios**:
- A record would carry a request or response body, a query parameter, a header outside the allowlist, or any credential material: the field is omitted rather than written, because the record carries no PII and no secrets.
- The audit destination is stdout only: no log file, no remote shipping, and no log record is buffered in a cache layer owned by this feature.

**Steps**:
1. [x] - `p1` - Build the audit record from the request context at the point the response is complete, filling every field of the `cpt-cf-oagw-adr-request-routing` field set - `inst-os-audit-1`
2. [x] - `p1` - Set `level` from the outcome of the request: `INFO` for successful requests and normal operations, `WARN` for rate-limit rejections and retry guidance, `ERROR` for upstream failures, timeouts, and authentication failures, and `DEBUG` for detailed plugin execution, which is disabled in production - `inst-os-audit-2`
3. [x] - `p1` - Set `error_type` to the gateway error type when the request failed at the gateway and to null when the request succeeded - `inst-os-audit-3`
4. [x] - `p1` - Omit request and response bodies, query parameters, and every header except the allowlisted ones, and omit API keys, tokens, and credentials from every field, so no record carries PII or secret material - `inst-os-audit-4`
5. [x] - `p1` - Apply the sampling policy to the non-proxy-request event classes only — configuration changes and authentication failures — so a flood of either cannot exhaust the log sink, and never to the proxy-request class: every proxy-request audit record is emitted unconditionally with its correlation identifier, so the sampling decision never removes a proxy request from the audit stream - `inst-os-audit-5`
6. [x] - `p1` - **IF** a configuration write is performed by the management API - `inst-os-audit-6`
   1. [x] - `p1` - Emit a configuration-change audit record naming the operation and the affected record without echoing any credential-bearing value - `inst-os-audit-7`
7. [x] - `p1` - Serialize the record as one JSON object per line and write it to stdout - `inst-os-audit-7b`
8. [x] - `p1` - **RETURN** the emitted record to the centralized logging consumer - `inst-os-audit-8`

### Trace Identifier Propagation

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-observability-and-state-trace-propagation`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- Every proxy request carries a correlation identifier that is propagated into the `trace_id` extension field of a rendered problem+json body and into the `request_id` field of the audit record, so a caller-reported failure can be joined with the log record and the metric series for the same request.
- 100% of proxy requests are logged with a correlation identifier, including requests rejected before route matching and requests that fail at the upstream; this is the always-emitted proxy-request population, which the sampling policy of the audit flow never applies to.

**Error Scenarios**:
- A request fails at the gateway: the rendered problem+json body carries the same `trace_id` that the audit record for that request carries, so the two are joinable without guessing.
- A passthrough upstream response is returned: no gateway-generated body exists, and the correlation identifier is still present in the audit record for the request.

**Steps**:
1. [x] - `p1` - Obtain the correlation identifier for the request at the start of proxy handling and keep it immutable for the life of the request, minting it once at proxy entry as a server-generated lowercase hyphenated UUID or adopting an inbound `X-Request-ID` that entry 2.6's request-id transform validates, where the request-id transform of `cpt-cf-oagw-feature-plugin-system` is the only writer of the `X-Request-ID` header and writes this correlation identifier rather than minting its own value - `inst-os-trace-1`
2. [x] - `p1` - Propagate the identifier into the `trace_id` extension field of every gateway-rendered problem+json body, alongside the request-context extension fields the error contract declares - `inst-os-trace-2`
3. [x] - `p1` - Propagate the same identifier into the `request_id` field of the audit record emitted for the request - `inst-os-trace-3`
4. [x] - `p1` - **IF** the response is a gateway error - `inst-os-trace-4`
   1. [x] - `p1` - Ensure the `trace_id` in the body and the `request_id` in the audit record are identical for the request - `inst-os-trace-5`
5. [x] - `p1` - **RETURN** the response carrying the correlation identifier wherever the contract permits it - `inst-os-trace-6`

### Control Plane Configuration Read and L1 Population

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-observability-and-state-cp-cache-read`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A configuration read on the Control Plane consults the 10,000-entry L1 LRU first; the sub-microsecond access time of an L1 hit is the design target `cpt-cf-oagw-adr-data-plane-caching` records for the L1 layer, not a figure this entry measures.
- A miss falls through to the configuration store behind the repository boundary, and the resolved value is written back into the L1 cache so the next read hits.
- Caches are populated lazily on read; there is no proactive warming.

**Error Scenarios**:
- The configuration store has no record for the key: the miss is returned as the distinct not-found outcome and nothing is cached, so a nonexistent configuration cannot be pinned in the cache.
- The configuration store read fails behind the repository boundary: the failure propagates as the distinct store-error domain error for entry 2.5 to classify and is never returned as a not-found outcome, and nothing is inserted into the cache, so the next read re-attempts the store.
- The requested key is not derivable as one of the three `CacheKey` families: the read bypasses the cache and goes straight to the repository boundary.

**Steps**:
1. [x] - `p1` - Derive the `CacheKey` for the read with `cpt-cf-oagw-algo-observability-and-state-cache-key-derivation` - `inst-os-cpread-1`
2. [x] - `p1` - Consult the Control Plane L1 cache for the derived key - `inst-os-cpread-2`
3. [x] - `p1` - **IF** the L1 cache holds the key - `inst-os-cpread-3`
   1. [x] - `p1` - Return the cached value without consulting the repository - `inst-os-cpread-4`
4. [x] - `p1` - **IF** the L1 cache does not hold the key - `inst-os-cpread-5`
   1. [x] - `p1` - Read the record through the tenant-scoped repository boundary - `inst-os-cpread-6`
   2. [x] - `p1` - Insert the resolved value into the L1 cache stamped with the store generation observed for the key, evicting the least recently used entry when the cache is at capacity, and return the value as the resolved outcome - `inst-os-cpread-7`
5. [x] - `p1` - **IF** the configuration store holds no record for the key - `inst-os-cpread-8`
   1. [x] - `p1` - Return the distinct not-found outcome and insert nothing, so a negative result is never cached - `inst-os-cpread-9`
6. [x] - `p1` - **IF** the configuration store read fails behind the repository boundary - `inst-os-cpread-9b`
   1. [x] - `p1` - Propagate the failure as the distinct store-error domain error for entry 2.5 to classify, never as a not-found outcome, and insert nothing into the cache - `inst-os-cpread-9c`
7. [x] - `p1` - **RETURN** one of the four outcomes of the read — the cached value on a hit, the resolved value on a miss that was then populated, the distinct not-found outcome, or the distinct store-error domain error - `inst-os-cpread-10`

### Control Plane Write-Side Invalidation

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- Every accepted configuration write flushes the affected Control Plane L1 keys before the write is reported as successful, so a read issued after a successful write never observes the pre-write value from the cache; a read that raced the flush and populated a pre-write value is discarded by the per-key generation check of `cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush`, so it cannot survive the flush either.
- Invalidation is keyed: only the entries derived from the written record are dropped, not the whole cache.

**Error Scenarios**:
- The write is rejected by validation, tenant scoping, or a uniqueness key: no entry is invalidated, because no configuration change occurred.
- The flush of the optional shared L2 layer is not possible because the L2 layer does not exist in the graded configuration; the invalidation path completes without it.

**Steps**:
1. [x] - `p1` - Accept the configuration write through the management surface and apply it to the in-memory, config-backed repository behind the repository boundary - `inst-os-cpinv-1`
2. [x] - `p1` - **IF** the write is rejected - `inst-os-cpinv-2`
   1. [x] - `p1` - Leave the cache untouched and report the rejection without flushing anything - `inst-os-cpinv-3`
3. [x] - `p1` - Derive the set of `CacheKey` values affected by the written record with `cpt-cf-oagw-algo-observability-and-state-cache-key-derivation` - `inst-os-cpinv-4`
4. [x] - `p1` - Remove each affected key from the Control Plane L1 cache, so the next read repopulates it from the repository - `inst-os-cpinv-4b`
5. [x] - `p1` - Flush the optional shared L2 layer for the affected keys when an L2 layer is configured, and skip that step when it is not - `inst-os-cpinv-5`
6. [x] - `p1` - Trigger the Data Plane L1 flush with `cpt-cf-oagw-flow-observability-and-state-dp-cache-flush` before the write is reported as complete - `inst-os-cpinv-6`
7. [x] - `p1` - **RETURN** the successful write outcome, with the Control Plane L1 and the notified Data Plane L1 consistent with the written configuration - `inst-os-cpinv-7`

### Data Plane Hot-Config Lookup

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-observability-and-state-dp-cache-read`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A proxy request consults the 1,000-entry Data Plane L1 hot-config cache for the resolved upstream and route configuration and, on a hit, proceeds without a Control Plane call.
- On a miss, the Data Plane resolves the target through the Control Plane and inserts the resolved pair into its L1 cache for the next request.

**Error Scenarios**:
- The Control Plane cannot resolve the alias or match a route: nothing is inserted into the hot-config cache, so a failed resolution is never served from cache on a later request.
- The hot-config cache has reached its 1,000-entry capacity: the least recently used entry is evicted to make room, and no entry is dropped because of age, because the cache has no TTL.

**Steps**:
1. [x] - `p1` - Derive the `CacheKey` for the requested upstream and route with `cpt-cf-oagw-algo-observability-and-state-cache-key-derivation` - `inst-os-dpread-1`
2. [x] - `p1` - Consult the Data Plane L1 hot-config cache for the derived key - `inst-os-dpread-2`
3. [x] - `p1` - **IF** the hot-config cache holds the key - `inst-os-dpread-3`
   1. [x] - `p1` - Use the cached resolved configuration and continue the proxy pipeline without contacting the Control Plane - `inst-os-dpread-4`
4. [x] - `p1` - **IF** the hot-config cache does not hold the key - `inst-os-dpread-5`
   1. [x] - `p1` - Resolve the target through the Control Plane, including the tenant-hierarchy alias walk and the route match - `inst-os-dpread-6`
   2. [x] - `p1` - Insert the resolved configuration into the hot-config cache under the key the derivation names, recording the Control Plane keys it was derived from and stamping the entry with the generation observed for those keys, evicting the least recently used entry when the cache is at capacity, and continue the proxy pipeline - `inst-os-dpread-7`
5. [x] - `p1` - **IF** the resolution fails - `inst-os-dpread-8`
   1. [x] - `p1` - Report the failure to the proxy path without caching any partial result - `inst-os-dpread-9`
6. [x] - `p1` - **RETURN** the resolved configuration for the proxy pipeline - `inst-os-dpread-10`

### Data Plane Hot-Config Flush

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-observability-and-state-dp-cache-flush`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- A configuration write triggers an explicit flush of the Data Plane L1 hot-config cache, so the hot path does not keep serving a configuration that a just-accepted write replaced.
- The flush is explicit and immediate: the cache has no TTL, so without the flush an invalidated entry would persist until it was evicted by use.

**Error Scenarios**:
- A write is rejected before it is applied: no flush is triggered, because no configuration changed.
- The Data Plane is not running in the same process as the Control Plane write: the flush is still requested through the notification path recorded in `cpt-cf-oagw-adr-state-management`. That shape is out of the graded scope. In the graded single-executable configuration the flush notification is a direct in-process call and no periodic sync exists, and the no-TTL consequence of relying on notification alone in such a multi-process deployment — a stale entry persisting until it is evicted by use — is accepted and recorded as a limitation rather than covered by a TTL or a sync loop here.

**Steps**:
1. [x] - `p1` - Receive the flush request raised by a completed configuration write - `inst-os-dpflush-1`
2. [x] - `p1` - Remove every Data Plane L1 hot-config entry whose recorded dependency set intersects the affected Control Plane key set derived by `cpt-cf-oagw-algo-observability-and-state-cache-key-derivation`, together with any entry keyed directly by an affected key, so the affected set is enumerated from the entries themselves rather than guessed - `inst-os-dpflush-2`
3. [x] - `p1` - **IF** no affected key set can be derived at all, because the written record is not expressible in one of the three `CacheKey` families - `inst-os-dpflush-3`
   1. [x] - `p1` - Clear the whole Data Plane L1 hot-config cache rather than guessing at an incomplete key set; for a written record inside the three families the derivation is always enumerable, so this fallback is unreachable in the graded configuration - `inst-os-dpflush-4`
4. [x] - `p1` - Leave the cache empty rather than repopulating it eagerly; the next request repopulates it lazily through `cpt-cf-oagw-flow-observability-and-state-dp-cache-read` - `inst-os-dpflush-5`
5. [x] - `p1` - **RETURN** a flushed hot-config cache consistent with the written configuration - `inst-os-dpflush-6`

### Shared Upstream HTTP Client

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-observability-and-state-shared-http-client`

**Actor**: `cpt-cf-oagw-actor-upstream-service`

**Success Scenarios**:
- All Data Plane calls to external services are issued through one shared client instance owned by the Data Plane state, so connection pooling, protocol capability, and socket reuse are shared across every proxy request rather than per request.
- The connection state observed through the shared client feeds `oagw_upstream_connections{host, state}`.

**Error Scenarios**:
- An upstream connection fails: the shared client reports the failure to the proxy path and to the instrumentation, and no response is cached, because `cpt-cf-oagw-principle-no-cache` places response caching with the client and the upstream.
- A request-scoped client is requested: the Data Plane does not construct one, because the shared client is the only outbound path this feature owns.

**Steps**:
1. [x] - `p1` - Construct the shared upstream client once during Data Plane initialization and hold it as part of the Data Plane state alongside the hot-config cache and the rate-limiter registry - `inst-os-client-1`
2. [x] - `p1` - Issue every upstream call through the shared client so connection pooling and host reuse are shared across all proxy traffic - `inst-os-client-2`
3. [x] - `p1` - Derive `oagw_upstream_connections{host, state}` from the shared client's per-host connection state with `state` one of `idle`, `active`, and `max` - `inst-os-client-3`
4. [x] - `p1` - **IF** an upstream call fails - `inst-os-client-4`
   1. [x] - `p1` - Report the failure through the proxy error path and update `oagw_upstream_available{host, endpoint}` for the affected endpoint without caching any response - `inst-os-client-5`
5. [x] - `p1` - **IF** a call through an endpoint succeeds - `inst-os-client-5b`
   1. [x] - `p1` - Set `oagw_upstream_available{host, endpoint}` to `1` for that endpoint, so the gauge carries the recovery transition as well as the failure transition and a recovered endpoint is observable on the next scrape - `inst-os-client-5c`
6. [x] - `p1` - Defer the timeout enforcement and the retry posture of every call this client issues to `cpt-cf-oagw-principle-no-retry` and to `cpt-cf-oagw-feature-request-proxy`, which owns `proxy_timeout_secs` enforcement on the proxy path, and consume the configuration keys the shared client reads — `proxy_timeout_secs`, `ssrf_policy`, `allow_http_upstream`, and the request body-size limit — all delivered by `cpt-cf-oagw-feature-gear-foundation` - `inst-os-client-5d`
7. [x] - `p1` - **RETURN** the upstream response or the failure through the proxy pipeline - `inst-os-client-6`

### Health and Readiness Reporting

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-observability-and-state-health-readiness`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The gear reports health and readiness through the ToolKit `RestApiCapability` healthcheck hook, so the platform can determine whether the OAGW gear is alive and ready to serve proxy traffic without a gear-specific endpoint.
- Readiness reflects the state machine `cpt-cf-oagw-state-observability-and-state-health-surface`: the gear is ready when it is initialized with a parsed configuration, a registered metric registry, both L1 caches, and the services it instruments.

**Error Scenarios**:
- Gear initialization failed: the healthcheck hook does not report ready, and no cache or metric surface is advertised as available.
- The gear is alive but a dependency it needs to serve traffic is unavailable: the hook reports alive without reporting ready from the `unhealthy` state of `cpt-cf-oagw-state-observability-and-state-health-surface`, so the platform can route around the instance, and reports ready again once the dependency is available.

**Steps**:
1. [x] - `p1` - Register the healthcheck through the ToolKit `RestApiCapability` healthcheck hook during gear initialization, using the path the framework provides - `inst-os-health-1`
2. [x] - `p1` - Report readiness from the gear state recorded in `cpt-cf-oagw-state-observability-and-state-health-surface`, including the initialization outcome and the availability of the state this feature owns - `inst-os-health-2`
3. [x] - `p1` - **IF** initialization failed - `inst-os-health-3`
   1. [x] - `p1` - Report the gear as not ready so the platform does not route proxy traffic to it - `inst-os-health-4`
4. [x] - `p1` - **IF** the gear is initialized but a state component it owns becomes unavailable, which places the gear in the `unhealthy` state of `cpt-cf-oagw-state-observability-and-state-health-surface`: alive, initialized, and not ready - `inst-os-health-5`
   1. [x] - `p1` - Report alive but not ready from the `unhealthy` state, and identify the unavailable component in the healthcheck result without disclosing configuration values - `inst-os-health-6`
5. [x] - `p1` - **IF** the state component that was unavailable becomes available again while the gear is still initialized - `inst-os-health-6b`
   1. [x] - `p1` - Return the gear to the `ready` state and report ready on the next healthcheck evaluation - `inst-os-health-6c`
6. [x] - `p1` - **RETURN** the health and readiness result to the platform healthcheck consumer - `inst-os-health-7`

### Deployment Mode Selection

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-observability-and-state-deployment-mode`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Single-executable operation runs the Control Plane and the Data Plane in one process with the L1 layers only: the Control Plane 10,000-entry L1 cache and the Data Plane 1,000-entry hot-config cache, with no shared cache layer and no external cache dependency.
- Microservice operation is the documented alternative shape: the same L1 layers plus an optional shared L2 layer that is configured but not implemented in the graded configuration.

**Error Scenarios**:
- A configuration or deployment request asks for the shared L2 layer in the graded configuration: the request is refused as unsupported rather than silently degrading to L1, because no L2 implementation exists in this decomposition.
- The deployment mode is not declared: the single-executable mode is used, matching `cpt-cf-oagw-constraint-toolkit-deploy`.

**Steps**:
1. [x] - `p1` - Determine the deployment mode, which in the graded configuration is fixed to the single-executable mode of `cpt-cf-oagw-constraint-toolkit-deploy` rather than read from `oagw.config`, because this entry adds no configuration key and adding one is out of scope for this decomposition - `inst-os-deploy-1`
2. [x] - `p1` - In the single-executable mode, construct the Control Plane L1 cache, the Data Plane L1 hot-config cache, and no shared cache layer - `inst-os-deploy-2`
3. [x] - `p1` - Size the two constructed caches from the fixed constants this entry carries — 10,000 Control Plane entries and 1,000 Data Plane entries — and the audit sampling rate from its build-time constant, reading none of the three from `oagw.config` or from an environment variable - `inst-os-deploy-2b`
4. [x] - `p1` - **IF** the mode names a shared L2 layer - `inst-os-deploy-3`
   1. [x] - `p1` - Refuse the request as unsupported, because `cpt-cf-oagw-adr-data-plane-caching` makes L2 optional and the graded configuration declares no L2 implementation - `inst-os-deploy-4`
5. [x] - `p1` - Record the selected mode so the invalidation and flush behavior of `cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush` covers exactly the layers that exist - `inst-os-deploy-4b`
6. [x] - `p1` - **RETURN** the selected deployment mode with the L1 layers constructed and no L2 layer constructed - `inst-os-deploy-5`

## 3. Processes / Business Logic (CDSL)

### Metric Label Normalization

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-observability-and-state-metric-label-normalization`

**Input**: one proxy request with its resolved upstream alias, the matched route (if any), the request method, the pipeline phase being recorded, and the final response status or gateway error.
**Output**: the bounded label set for the metric families recorded for that request.

**Steps**:
1. [x] - `p1` - Set the `host` label from the resolved upstream alias, which is the OAGW-specific label DESIGN §4.2 retains - `inst-os-algo-label-1`
2. [x] - `p1` - Set the `http.route` label from the normalized route match pattern of the matched route and never from the raw request path - `inst-os-algo-label-2`
3. [x] - `p1` - Normalize `http.request.method` to a standard HTTP verb, and to `_OTHER` when the method is not one of them - `inst-os-algo-label-3`
4. [x] - `p1` - Record `http.response.status_code` as the numeric status code, so status-class queries such as a 5xx rate are expressed at query time by regex over the numeric code - `inst-os-algo-label-4`
5. [x] - `p1` - Emit no tenant label on any metric family; tenant identity is carried in the audit record, not in the metric label set - `inst-os-algo-label-5`
6. [x] - `p1` - Keep the label-key vocabulary aligned with the OTel HTTP semantic conventions used by the inbound API gateway, so both gateways share dashboards - `inst-os-algo-label-6`
7. [x] - `p1` - Bound every label value to a value derived from configuration or from a fixed enumeration: a route match pattern, a method normalization, a numeric status, a selection method from `explicit_header`, `round_robin`, and `default`, a connection state from `idle`, `active`, and `max`, or an error type from the gateway error contract - `inst-os-algo-label-7`
8. [x] - `p1` - Enumerate the `phase` label of `oagw_request_duration_seconds` from the fixed declared set `route_match`, `plugin_chain_request`, `upstream_call`, `plugin_chain_response`, and `response`, and admit no `phase` value outside that set into the label vocabulary - `inst-os-algo-label-7b`
9. [x] - `p1` - **IF** a label value cannot be derived from configuration or a fixed enumeration, such as a route pattern for a request rejected before route matching - `inst-os-algo-label-8`
   1. [x] - `p1` - Omit the label rather than substituting a per-request value, so no metric series can grow with the request path - `inst-os-algo-label-9`
10. [x] - `p1` - **RETURN** the bounded label set - `inst-os-algo-label-10`

### Cache Key Derivation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-observability-and-state-cache-key-derivation`

**Input**: a configuration lookup or a configuration write, with the tenant identifier, the upstream alias or upstream identifier, the route identifier, the method, the path prefix, or the plugin identifier as applicable.
**Output**: the `CacheKey` value naming the affected configuration, drawn from the three families `cpt-cf-oagw-adr-data-plane-caching` defines, and — for a write — the affected Control Plane key set together with the predicate that selects the affected Data Plane entries.

**Steps**:
1. [x] - `p1` - Derive an upstream key as `upstream:{owner_tenant_id}:{alias}` — the `upstream:{tenant_id}:{alias}` form `cpt-cf-oagw-adr-data-plane-caching` records, with the `tenant_id` component bound to the owning tenant, the tenant that owns the upstream record, and never to the calling tenant, so a descendant resolution that walked the tenant chain to an ancestor record is invalidated when that ancestor record is written - `inst-os-algo-key-1`
2. [x] - `p1` - Derive a route key as `route:{upstream_id}:{method}:{path_prefix}` naming one route configuration under one upstream on the Control Plane, where the owning tenant of that route is the owning tenant of the upstream the route references - `inst-os-algo-key-2`
3. [x] - `p1` - Derive a plugin key as `plugin:{plugin_id}` naming one plugin definition, recorded as a reserved family: in the graded configuration a named plugin is resolved from the in-process plugin registry of DESIGN §3.1 (`cpt-cf-oagw-component-model`) and is never read through the Control Plane cache, so `plugin:{plugin_id}` has no reader in the graded configuration, and the plugin-configuration write owned by `cpt-cf-oagw-feature-plugin-system` is its invalidating trigger and is a no-op here - `inst-os-algo-key-3`
4. [x] - `p1` - Key a Data Plane hot-config entry for a routed request by the route record that matched, as `route:{owner_tenant_id}:{route_id}` where `{owner_tenant_id}` is the owning tenant of that route and `{route_id}` its record identifier, and store in that entry the resolved pair — the route, its owning upstream, and the effective plugin and header configuration derived from them - `inst-os-algo-key-4`
5. [x] - `p1` - Key a Data Plane hot-config entry for a request that resolves only an upstream with no route as `upstream:{owner_tenant_id}:{alias}`, the same form the Control Plane entry for that upstream carries - `inst-os-algo-key-4b`
6. [x] - `p1` - Record on every Data Plane hot-config entry the set of Control Plane keys it was derived from, so a `route:{owner_tenant_id}:{route_id}` entry names the `route:{upstream_id}:{method}:{path_prefix}` key and the `upstream:{owner_tenant_id}:{alias}` key of its owning upstream, and an `upstream:{owner_tenant_id}:{alias}` entry names that upstream key alone - `inst-os-algo-key-4c`
7. [x] - `p1` - Select the affected Data Plane set on invalidation as every hot-config entry whose recorded dependency set intersects the affected Control Plane key set, so the affected set is always enumerable from the entries themselves and no whole-cache invalidation is required to reach a derived configuration - `inst-os-algo-key-5b`
8. [x] - `p1` - Reject or bypass a lookup that cannot be expressed in one of the three families rather than minting an ad-hoc key shape - `inst-os-algo-key-5`
9. [x] - `p1` - Walk an upstream write of upstream `U` owned by tenant `T` with alias `a` through this derivation: the affected Control Plane key is `upstream:T:a`; invalidation removes it, and removes every Data Plane entry whose dependency set contains it — the `route:{owner_tenant_id}:{route_id}` entry of every route under `U`, which records `upstream:T:a` as its owning-upstream dependency, and every `upstream:T:a` entry that resolved `U` without a route. No other entry is removed - `inst-os-algo-key-5c`
10. [x] - `p1` - Walk a route write to route `R` of upstream `U`, method `GET`, path prefix `/v1`, owned by tenant `T`, through this derivation: the affected Control Plane key is `route:{U_id}:GET:/v1`; invalidation removes it, and removes the single Data Plane entry keyed `route:T:{R}`, which records `route:{U_id}:GET:/v1` in its dependency set, leaving the `upstream:T:a` key and every other Data Plane entry in place - `inst-os-algo-key-5d`
11. [x] - `p1` - **RETURN** the derived `CacheKey`, or an indication that no cache key applies, or — for a write — the affected Control Plane key set with the Data Plane dependency predicate - `inst-os-algo-key-6`

### L1 Cache Maintenance

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance`

**Input**: a cache lookup or an insert for a derived `CacheKey`, addressed to either the Control Plane L1 cache or the Data Plane L1 hot-config cache.
**Output**: one of the four read outcomes — the cached value on a hit, the resolved value on a miss that was then populated, the not-found outcome, or the store-error domain error — plus the insertion and eviction decision on a miss.

**Steps**:
1. [x] - `p1` - Maintain the Control Plane L1 cache as a per-instance LRU of 10,000 entries and the Data Plane L1 hot-config cache as a per-instance LRU of 1,000 entries, each guarded by its own single mutex and by no lock shared with the other cache - `inst-os-algo-lru-1`
2. [x] - `p1` - Hold each cache guard only for the lookup plus the LRU order update, never across I/O and never across a configuration-store read, so a backing read executes outside the guard - `inst-os-algo-lru-1b`
3. [x] - `p1` - Apply no TTL to either cache: an entry persists until it is invalidated by a write, flushed explicitly, or evicted by least-recent use - `inst-os-algo-lru-2`
4. [x] - `p1` - Record a hit on lookup and move the entry to the most recently used position without touching the configuration store; that move-to-front update sits on the same critical path as the value read by design and is accepted as the cost of a per-instance LRU per `cpt-cf-oagw-adr-data-plane-caching`, so the L1 hit latency is claimed relative to that guard and not as a lock-free figure - `inst-os-algo-lru-3`
5. [x] - `p1` - Record a miss on lookup and populate the cache lazily from the store or from the Control Plane, with no proactive warming of either cache - `inst-os-algo-lru-4`
6. [x] - `p1` - Stamp every inserted entry with the per-key monotonically increasing generation the configuration store carries, observed when the value was read, and accept the insert only when that generation equals the store's current generation for the key, so a population that raced a flush discards the value and the read is treated as a miss - `inst-os-algo-lru-4b`
7. [x] - `p1` - Evict the least recently used entry when an insert would exceed the configured capacity - `inst-os-algo-lru-5`
8. [x] - `p1` - Never insert a negative result, so a not-found lookup is re-attempted against the configuration store on every request, and insert nothing when the backing read fails, so a store failure leaves the cache as it was - `inst-os-algo-lru-5b`
9. [x] - `p1` - Never insert a response body or a response header set into either cache, because `cpt-cf-oagw-principle-no-cache` reserves caching of responses for the client and the upstream - `inst-os-algo-lru-6`
10. [x] - `p1` - Keep both caches in-process, so the single executable needs no external state store, and bound their aggregate memory by the entry-count cap alone: a cached value is a merged effective configuration whose size is bounded by the validated configuration limits — bounded routes per upstream, bounded plugin chain length, bounded header-rule set, and bounded endpoint pool — so no additional per-entry byte budget is added - `inst-os-algo-lru-7`
11. [x] - `p1` - **RETURN** one of the four outcomes of the operation — the cached value on a hit, the resolved value on a miss that was then populated, the distinct not-found outcome when the store holds no record, or the distinct store-error domain error for entry 2.5 to classify — inserting nothing on either of the last two - `inst-os-algo-lru-8`

### Write-Side Invalidation and Data Plane Flush

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush`

**Input**: an accepted configuration write naming the record it changed.
**Output**: a Control Plane L1 invalidation and a Data Plane L1 flush covering the affected keys, completed before the write is reported as successful.

**Steps**:
1. [x] - `p1` - Apply the configuration write to the configuration store behind the repository boundary, and abort with no cache effect when the write is rejected - `inst-os-algo-inval-1`
2. [x] - `p1` - Derive the affected `CacheKey` set with `cpt-cf-oagw-algo-observability-and-state-cache-key-derivation` - `inst-os-algo-inval-2`
3. [x] - `p1` - Remove the affected keys from the Control Plane L1 cache, in the order `cpt-cf-oagw-adr-data-plane-caching` records: store write, Control Plane L1 flush, optional shared L2 flush, then success - `inst-os-algo-inval-3`
4. [x] - `p1` - Close the race against a reader that misses, reads a pre-write value, and inserts it after the flush: the configuration store carries a per-key monotonically increasing generation, every L1 entry is stamped with the generation observed when it was populated, and an insert is accepted only when the entry's generation equals the store's current generation for that key, so a stale generation discards the value and the read is treated as a miss instead of repopulating the removed key - `inst-os-algo-inval-3b`
5. [x] - `p1` - Skip the shared L2 flush when no L2 layer is constructed, as is the case in the graded configuration - `inst-os-algo-inval-4`
6. [x] - `p1` - Request the Data Plane L1 flush after the Control Plane L1 flush, either as a direct notification in the single-executable mode or through the notification path recorded in `cpt-cf-oagw-adr-state-management` - `inst-os-algo-inval-5`
7. [x] - `p1` - Report the write as complete only after the Control Plane L1 flush and the Data Plane flush request have both been issued - `inst-os-algo-inval-6`
8. [x] - `p1` - **IF** the flush cannot cover the affected key set precisely - `inst-os-algo-inval-7`
   1. [x] - `p1` - Clear the whole Data Plane L1 hot-config cache so no stale entry survives, accepting the temporary repopulation cost; for a written record inside the three `CacheKey` families the derivation of `cpt-cf-oagw-algo-observability-and-state-cache-key-derivation` is always enumerable, so this fallback is unreachable in the graded configuration - `inst-os-algo-inval-8`
9. [x] - `p1` - Emit a configuration-change audit record for the write, naming the operation and the affected record - `inst-os-algo-inval-9`
10. [x] - `p1` - **RETURN** the write outcome with the caches consistent with the written configuration - `inst-os-algo-inval-10`

### Audit Record Construction

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-observability-and-state-audit-record`

**Input**: one completed request with its correlation identifier, request context, response status, timing, sizes, and error type, or one management operation or authentication failure.
**Output**: one structured JSON record on stdout, or no record when the sampling policy suppresses a non-proxy-request source.

**Steps**:
1. [x] - `p1` - Fill the field set `timestamp`, `level`, `event`, `request_id`, `tenant_id`, `principal_id`, `host`, `path`, `method`, `status`, `duration_ms`, `request_size`, `response_size`, `error_type` from the request context - `inst-os-algo-audit-1`
2. [x] - `p1` - Set `event` from the fixed legal set `proxy_request`, `config_change`, and `auth_failure`, admitting no value outside that set, so the event vocabulary is closed and the sampling policy can address each class by name - `inst-os-algo-audit-2`
3. [x] - `p1` - Populate the fourteen fields for a `config_change` event from the management operation rather than from a request: `timestamp`, `level`, `event`, `request_id`, `tenant_id`, `principal_id`, and `status` are populated, `path` carries the management resource path of the affected record as the record's resource identifier, and `host`, `method`, `duration_ms`, `request_size`, and `response_size` are null - `inst-os-algo-audit-2b`
4. [x] - `p1` - Populate the fourteen fields for an `auth_failure` event from the rejected request: `timestamp`, `level`, `event`, `request_id`, `tenant_id`, `principal_id`, `status`, `host`, `path`, `method`, and `error_type` are populated from that request, while `duration_ms`, `request_size`, and `response_size` are null - `inst-os-algo-audit-2c`
5. [x] - `p1` - Carry no `error_message` field and no free-form error text in the structured record: `error_message`, which DESIGN §4.3 names among the failed-request fields, is deliberately not carried, because free-form upstream or internal error text is a leakage channel and `error_type` is the bounded discriminator, and the human-readable message stays in the DEBUG log stream instead - `inst-os-algo-audit-2d`
6. [x] - `p1` - Redact bodies, query parameters, non-allowlisted headers, API keys, tokens, and credential material from every field before serialization, so no record carries PII or secrets - `inst-os-algo-audit-3`
7. [x] - `p1` - Apply the sampling policy to the non-proxy-request classes `config_change` and `auth_failure` only, so log volume stays bounded, and never to `proxy_request`; accounting for the records the sampling policy suppresses belongs to the centralized logging consumer and is out of scope for this entry, because the metric-surface DoD forbids introducing any additional metric family or label key - `inst-os-algo-audit-4`
8. [x] - `p1` - Serialize the record as one JSON object per line and write it to stdout, with no buffering in a cache layer - `inst-os-algo-audit-5`
9. [x] - `p1` - **IF** the record would echo a credential-bearing value - `inst-os-algo-audit-5b`
   1. [x] - `p1` - Drop the value and write null instead, so a log line can never disclose secret material - `inst-os-algo-audit-6`
10. [x] - `p1` - **RETURN** the emitted record or the sampling decision that suppresses it - `inst-os-algo-audit-7`

### Deployment Mode Resolution

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-observability-and-state-deployment-mode`

**Input**: the gear configuration and the deployment shape the platform runs the gear in.
**Output**: the set of cache layers constructed and the notification path used for Data Plane flushes.

**Steps**:
1. [x] - `p1` - Default to the single-executable mode: one process hosting the Control Plane and the Data Plane, the Control Plane L1 cache, the Data Plane L1 hot-config cache, and no shared cache layer - `inst-os-algo-deploy-1`
2. [x] - `p1` - Fix the configuration-derived inputs in the graded configuration rather than reading them from `oagw.config`: the deployment mode is the single-executable mode of `cpt-cf-oagw-constraint-toolkit-deploy`, the two L1 capacities are the fixed constants 10,000 Control Plane entries and 1,000 Data Plane entries, and the audit sampling rate is a build-time constant, and this entry adds no configuration key because adding one is out of scope for this decomposition - `inst-os-algo-deploy-1b`
3. [x] - `p1` - In the single-executable mode, use a direct notification for the Data Plane L1 flush, because both planes share one process - `inst-os-algo-deploy-2`
4. [x] - `p1` - Recognize the microservice mode as the documented alternative shape: the same L1 layers plus an optional shared L2 cache across instances with the five-minute TTL `cpt-cf-oagw-adr-data-plane-caching` records - `inst-os-algo-deploy-3`
5. [x] - `p1` - Refuse to construct an L2 layer in the graded configuration, because the crate declares no shared-cache dependency and the L2 layer is out of scope for this entry - `inst-os-algo-deploy-4`
6. [x] - `p1` - **IF** the microservice mode is selected - `inst-os-algo-deploy-5`
   1. [x] - `p1` - Document the periodic-sync fallback as out of the graded scope: a process that misses a flush notification relies on its own periodic sync, because the hot-config cache has no TTL to expire a stale entry, but no periodic sync exists in the graded single-executable configuration, where the flush notification is a direct in-process call, and the no-TTL consequence of relying on notification alone is accepted and recorded as a limitation of the out-of-scope shape - `inst-os-algo-deploy-6`
7. [x] - `p1` - **RETURN** the constructed layer set and the flush notification path for the selected mode - `inst-os-algo-deploy-7`

## 4. States (CDSL)

### Cache Entry State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-observability-and-state-cache-entry`

**States**: `absent`, `cached`, `invalidated`
**Initial State**: `absent`

**Transitions**:
1. [x] - `p1` - **FROM** `absent` **TO** `cached` **WHEN** a lookup misses and the backing configuration read succeeds, so the resolved value is inserted into the L1 cache - `inst-os-st-cache-1`
2. [x] - `p1` - **FROM** `cached` **TO** `cached` **WHEN** the entry is read again and is moved to the most recently used position, with no expiry, because neither L1 cache has a TTL - `inst-os-st-cache-2`
3. [x] - `p1` - **FROM** `cached` **TO** `invalidated` **WHEN** a configuration write invalidates the derived key on the Control Plane, or a Data Plane flush removes the entry from the hot-config cache - `inst-os-st-cache-3`
4. [x] - `p1` - **FROM** `cached` **TO** `absent` **WHEN** the entry is the least recently used and an insert would exceed the cache capacity of 10,000 entries on the Control Plane or 1,000 entries on the Data Plane - `inst-os-st-cache-4`
5. [x] - `p1` - **FROM** `invalidated` **TO** `absent` **WHEN** the removed entry is released and its key is no longer held by the cache - `inst-os-st-cache-5`

### Health and Readiness Surface State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-observability-and-state-health-surface`

**States**: `uninitialized`, `initializing`, `ready`, `unhealthy`
**Initial State**: `uninitialized`

**Transitions**:
1. [x] - `p1` - **FROM** `uninitialized` **TO** `initializing` **WHEN** the ToolKit runtime starts constructing the gear and the healthcheck hook is registered - `inst-os-st-health-1`
2. [x] - `p1` - **FROM** `initializing` **TO** `ready` **WHEN** the configuration is parsed, the metric registry and both L1 caches are constructed, the shared client exists, and the instrumented services are available - `inst-os-st-health-2`
3. [x] - `p1` - **FROM** `initializing` **TO** `uninitialized` **WHEN** initialization fails, in which case the healthcheck hook does not report ready - `inst-os-st-health-3`
4. [x] - `p1` - **FROM** `ready` **TO** `uninitialized` **WHEN** the platform discards the gear instance, releasing the in-process caches, the shared client, and the metric registry - `inst-os-st-health-4`
5. [x] - `p1` - **FROM** `ready` **TO** `unhealthy` **WHEN** a state component this feature owns becomes unavailable while the gear is initialized, which is the alive-but-not-ready scenario the healthcheck flow reports through `inst-os-health-5` and `inst-os-health-6` - `inst-os-st-health-5`
6. [x] - `p1` - **FROM** `unhealthy` **TO** `ready` **WHEN** the unavailable state component becomes available again while the gear is still initialized, so readiness is restored without a re-initialization - `inst-os-st-health-6`

## 5. Definitions of Done

### Metrics Endpoint Registration

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-observability-and-state-metrics-endpoint`

The system **MUST** register `GET /metrics` and expose the Prometheus text exposition of every registered metric family, and **MUST** register it outside the gear-relative `/oagw/v1` prefix that the rest of the gear uses, so the metric surface is a platform-level scrape target and is not nested under an OAGW management or proxy path. The endpoint **MUST** be admin-only, and the exact check is the platform admin authorization predicate applied through the same `authz_resolver` handle the management API uses, admitting only the platform's admin principal class and no other principal class; this is a documented dependency on the platform authorization boundary recorded against `cpt-cf-oagw-constraint-toolkit-deploy` as the deployment posture reference, and the gate is evaluated against the OAGW permission identifier this entry mints, `gts.cf.core.oagw.proxy.v1~:metrics`, root-tenant scoped (`TenantMode::RootOnly`); DESIGN §3.2 names no permission for the metric surface, so minting this identifier is the recorded delta from that section. A request that fails that check **MUST** disclose no metric series and **MUST** be refused with `403` rendered by `cpt-cf-oagw-feature-error-handling`.

**Implements**:
- `cpt-cf-oagw-flow-observability-and-state-metrics-scrape`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: `GET /metrics`
- Entities: none
- Tests: integration tests in `tests/metrics_endpoint.rs` asserting the route exists at `/metrics`, does not exist under `/oagw/v1/metrics`, refuses an unauthenticated or non-admin caller, refuses with `403` a caller that holds a valid proxy permission but is not in the platform admin principal class, and returns a parseable exposition body

### Metric Names, Labels, and Histogram Buckets

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-observability-and-state-metric-surface`

The system **MUST** register the metric families DESIGN §4.2 declares, with their declared label sets: `oagw_requests_total{host, http.request.method, http.route, http.response.status_code}`, `oagw_request_duration_seconds{host, http.route, phase}` as a histogram, `oagw_requests_in_flight{host}`, `oagw_errors_total{host, http.route, error_type}`, `oagw_circuit_breaker_state{host}`, `oagw_circuit_breaker_transitions_total{host, from_state, to_state}`, `oagw_rate_limit_exceeded_total{host, path}`, `oagw_rate_limit_usage_ratio{host, path}`, `oagw_routing_target_host_used{upstream_id, endpoint_host}`, `oagw_routing_endpoint_selected{upstream_id, endpoint_host, selection_method}`, `oagw_upstream_available{host, endpoint}`, and `oagw_upstream_connections{host, state}`. The request-duration histogram **MUST** use the buckets `[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]` seconds. No additional metric family and no additional label key **MUST** be introduced by this entry.

**Implements**:
- `cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation`
- `cpt-cf-oagw-flow-observability-and-state-metrics-scrape`
- `cpt-cf-oagw-algo-observability-and-state-metric-label-normalization`

**Touches**:
- API: `GET /metrics`
- Entities: none
- Tests: unit tests in `src/infra/metrics_tests.rs` asserting every family name, its label keys, the bucket set, the five declared `phase` values of `oagw_request_duration_seconds`, and the absence of any unregistered family

### Metric Cardinality Rules

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-observability-and-state-metric-cardinality`

The system **MUST** apply the DESIGN §4.2 cardinality rules to every metric record: the `path` label of the rate-limit metrics carries the normalized route match pattern (`http.route`) and never the raw request path, `http.request.method` is normalized to a standard verb or to `_OTHER`, `http.response.status_code` is numeric, and no tenant label is emitted on any metric family. The `host` label **MUST** carry the upstream alias, and the `phase` label **MUST** carry one of the fixed declared values `route_match`, `plugin_chain_request`, `upstream_call`, `plugin_chain_response`, and `response`. Label keys **MUST** follow the OTel HTTP semantic conventions used by the inbound API gateway so both gateways share dashboards, and status-class queries **MUST** be answerable at query time by regex over the numeric status code. The series of the registry **MUST** be bounded by that declared label cardinality — `host` × route × `phase` × `error_type` — which is the declared memory posture of the metric registry rather than an additional byte budget.

**Implements**:
- `cpt-cf-oagw-algo-observability-and-state-metric-label-normalization`
- `cpt-cf-oagw-flow-observability-and-state-rate-limit-signals`

**Touches**:
- API: `GET /metrics`
- Entities: none
- Tests: unit tests in `src/infra/metrics_tests.rs` for method normalization including `_OTHER`, for a raw request path never appearing as a `path` label value, for the numeric status form, for the `phase` value set, and for the absence of a tenant label

### Request, Latency, and Error Metrics

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-observability-and-state-request-metrics`

The system **MUST** record `oagw_requests_total` for every proxy request once its response status is known, observe `oagw_request_duration_seconds` against the declared buckets at the recorded pipeline phases, hold `oagw_requests_in_flight` at the number of requests currently inside the proxy handler including streaming requests, and increment `oagw_errors_total` for every gateway error with the `error_type` carried by the gateway error contract. Routing outcomes **MUST** be counted in `oagw_routing_target_host_used` and `oagw_routing_endpoint_selected` with `selection_method` one of `explicit_header`, `round_robin`, `default`, and upstream reachability and connection state **MUST** be published through `oagw_upstream_available` and `oagw_upstream_connections`. `oagw_upstream_available` **MUST** carry the recovery transition as well as the failure transition: a call through an endpoint sets `oagw_upstream_available{host, endpoint}` to `1`, so an endpoint that recovered after a failure is observable on the next scrape.

**Implements**:
- `cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation`
- `cpt-cf-oagw-flow-observability-and-state-shared-http-client`

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}` (instrumented path; the handler is owned by the request-proxy entry)
- Entities: none
- Tests: integration tests in `tests/metrics_endpoint.rs` driving proxy requests and asserting the resulting counter, histogram, gauge, and routing series, including the `oagw_upstream_available` recovery transition from `0` to `1` after a successful call

### Rate-Limit Metrics and Per-Instance Limiter State

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-observability-and-state-rate-limit-metrics`

The system **MUST** increment `oagw_rate_limit_exceeded_total{host, path}` for every request a rate-limit evaluation rejects, with the `path` label carrying the normalized route match pattern, and **MUST** publish `oagw_rate_limit_usage_ratio{host, path}` as a gauge in the range 0.0 to 1.0. The limiter state behind both metrics **MUST** be per-instance in-memory state owned by the Data Plane and keyed by the counter scope of the rate-limiting entry, **MUST NOT** be synchronized with any other instance, and **MUST NOT** be persisted, per `cpt-cf-oagw-adr-state-management`.

**Implements**:
- `cpt-cf-oagw-flow-observability-and-state-rate-limit-signals`

**Constraints**: none introduced by this entry; the limiter semantics are owned by the rate-limiting entry.

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}` (instrumented path)
- Entities: none
- Tests: unit tests in `src/infra/metrics_tests.rs` for the rejected-request counter and the usage-ratio range, and integration tests in `tests/metrics_endpoint.rs` for the `path` label form

### Circuit-Breaker Metric Names Only

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-observability-and-state-breaker-metrics`

The system **MUST** register `oagw_circuit_breaker_state{host}` and `oagw_circuit_breaker_transitions_total{host, from_state, to_state}` with the label sets DESIGN §4.2 declares, and **MUST NOT** implement circuit-breaker state, transitions, or any fallback behavior, because DESIGN §4.7 defers the circuit breaker to future development and graded deviation 9 confines this entry to the metric names. The availability posture of `cpt-cf-oagw-nfr-high-availability` **MUST** be carried here by the single-executable deployment, the health and readiness surface, and the cache-invalidation and flush behavior instead.

**Implements**:
- `cpt-cf-oagw-flow-observability-and-state-breaker-metric-surface`

**Touches**:
- API: `GET /metrics`
- Entities: none
- Tests: unit tests in `src/infra/metrics_tests.rs` asserting the two breaker families are registered with their label keys and emit no series in the graded configuration

### Structured JSON Audit Logging

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-observability-and-state-audit-log`

The system **MUST** emit one structured JSON audit record per proxy request to stdout with the field set `timestamp`, `level`, `event`, `request_id`, `tenant_id`, `principal_id`, `host`, `path`, `method`, `status`, `duration_ms`, `request_size`, `response_size`, and `error_type`, as `cpt-cf-oagw-adr-request-routing` records it, and **MUST** emit configuration-change records for management writes and records for authentication failures. The `event` field **MUST** carry one of the fixed legal values `proxy_request`, `config_change`, and `auth_failure`, and the fourteen fields **MUST** be populated per class as `cpt-cf-oagw-algo-observability-and-state-audit-record` states: a `config_change` record carries the operation context and the management resource path of the affected record and leaves `host`, `method`, `duration_ms`, `request_size`, and `response_size` null, and an `auth_failure` record carries the rejected request's context and leaves `duration_ms`, `request_size`, and `response_size` null. The record **MUST NOT** carry `error_message` or any free-form error text, because free-form upstream or internal error text is a leakage channel and `error_type` is the bounded discriminator; the human-readable message stays in the DEBUG log stream. Every record **MUST** omit request and response bodies, query parameters, non-allowlisted headers, and all credential material. The sampling policy **MUST** apply to the `config_change` and `auth_failure` classes only and **MUST NOT** apply to the `proxy_request` class, whose records are emitted unconditionally with their correlation identifier, so the 100%-correlation criterion of `cpt-cf-oagw-nfr-observability` is asserted against the always-emitted proxy-request population; accounting for the records sampling suppresses belongs to the centralized logging consumer and is out of scope for this entry. Log levels **MUST** follow DESIGN §4.3.

**Implements**:
- `cpt-cf-oagw-flow-observability-and-state-audit-record`
- `cpt-cf-oagw-algo-observability-and-state-audit-record`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: none
- Entities: none
- Tests: unit tests in `src/infra/audit_tests.rs` for the exact field set, the fixed `event` value set, the per-class field population rules, the level mapping, redaction, the absence of `error_message`, and the unsampled `proxy_request` class, and integration tests in `tests/audit_log_shape.rs` asserting that no credential material or request body appears in any emitted line

### Trace Identifier Propagation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-observability-and-state-trace-propagation`

The system **MUST** propagate one correlation identifier per proxy request into the `trace_id` extension field of every gateway-rendered problem+json body and into the `request_id` field of that request's audit record, so a caller-reported failure, a log line, and a metric series for the same request are joinable. 100% of proxy requests **MUST** be logged with a correlation identifier, including requests rejected before route matching and requests that fail at the upstream; that population is the always-emitted `proxy_request` audit class, to which the sampling policy of `cpt-cf-oagw-dod-observability-and-state-audit-log` never applies.

**Implements**:
- `cpt-cf-oagw-flow-observability-and-state-trace-propagation`

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}` (extension field of the rendered problem+json body)
- Entities: none
- Tests: integration tests in `tests/audit_log_shape.rs` asserting the `trace_id` in a rendered error body equals the `request_id` in the corresponding audit record

### Control Plane L1 Configuration Cache

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-observability-and-state-cp-cache`

The system **MUST** implement the Control Plane L1 configuration cache as a per-instance LRU of 10,000 entries with no TTL, per `cpt-cf-oagw-adr-data-plane-caching`, populated lazily on read with no proactive warming, returning a hit without consulting the repository and a miss falling through to the repository and writing the resolved value back. The read **MUST** return one of four outcomes — the cached value on a hit, the resolved value populated on a miss, the distinct not-found outcome, and the distinct store-error domain error for entry 2.5 to classify — and **MUST** insert nothing on either of the last two, so a store failure is never surfaced as not-found and never cached. Every accepted configuration write **MUST** invalidate the affected keys on the write side before the write is reported as successful, no negative result **MUST** be cached, and an insert **MUST** be accepted only when the per-key generation the entry carries equals the store's current generation for that key, so a population racing a flush cannot insert a pre-write value.

**Implements**:
- `cpt-cf-oagw-flow-observability-and-state-cp-cache-read`
- `cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation`
- `cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance`

**Touches**:
- API: none
- Entities: `CPState`, `CacheKey`
- Tests: unit tests in `src/infra/cp_cache_tests.rs` for hit, miss and repopulation, LRU eviction at 10,000 entries, no-TTL persistence, the four read outcomes including the store-error outcome, and write-side invalidation, for a population racing a flush being unable to insert a pre-write value, and integration tests in `tests/cache_invalidation.rs` for the write-then-read ordering

### Data Plane L1 Hot-Config Cache

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-observability-and-state-dp-cache`

The system **MUST** implement the Data Plane L1 hot-config cache as a per-instance LRU of 1,000 entries with no TTL and explicit invalidation, per `cpt-cf-oagw-adr-state-management`, holding the resolved upstream and route configuration the proxy path reuses. A routed request's entry **MUST** be keyed by the route record it resolved, as `route:{owner_tenant_id}:{route_id}`, and **MUST** store the resolved pair — the route, its owning upstream, and the effective plugin and header configuration derived from them — while a request that resolves only an upstream with no route **MUST** use the `upstream:{owner_tenant_id}:{alias}` key, and every entry **MUST** record the set of Control Plane keys it was derived from. A hit **MUST** proceed without a Control Plane call, a miss **MUST** resolve through the Control Plane and insert the result, and a configuration write **MUST** trigger an explicit flush of the affected entries, selected as every entry whose recorded dependency set intersects the affected Control Plane key set so the affected set is always enumerable; the whole-cache clear **MUST** remain only as the fallback for a written record outside the three `CacheKey` families. An insert **MUST** be accepted only when the per-key generation the entry carries equals the store's current generation for that key. The hot-config cache **MUST** be an optimization layer only: the Control Plane remains authoritative.

**Implements**:
- `cpt-cf-oagw-flow-observability-and-state-dp-cache-read`
- `cpt-cf-oagw-flow-observability-and-state-dp-cache-flush`
- `cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush`

**Touches**:
- API: none
- Entities: `DPState`, `CacheKey`
- Tests: unit tests in `src/infra/dp_cache_tests.rs` for hit, miss, eviction at 1,000 entries, the recorded dependency set, and explicit flush by dependency intersection, and integration tests in `tests/dp_cache_flush.rs` asserting that a configuration write removes the served stale value and leaves an unrelated entry in place

### Cache Key Families

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-observability-and-state-cache-keys`

The system **MUST** derive every cache entry key from the three families `cpt-cf-oagw-adr-data-plane-caching` records — `upstream:{owner_tenant_id}:{alias}`, `route:{upstream_id}:{method}:{path_prefix}` on the Control Plane with the Data Plane routed entry keyed as `route:{owner_tenant_id}:{route_id}`, and `plugin:{plugin_id}` — and **MUST** use one derivation for Control Plane entries, Data Plane entries, and invalidation, so one written record maps to one well-defined set of affected keys: the affected Control Plane keys plus every Data Plane entry whose recorded dependency set intersects them. The `tenant_id` component of the `upstream:` and `route:` families **MUST** be the owning tenant and never the calling tenant, so a descendant resolution that walked the tenant chain to an ancestor record is invalidated when that ancestor record is written. The `plugin:{plugin_id}` family **MUST** be a reserved family with no reader in the graded configuration, because a named plugin is resolved from the in-process plugin registry of DESIGN §3.1 (`cpt-cf-oagw-component-model`) and is never read through the Control Plane cache; the plugin-configuration write owned by `cpt-cf-oagw-feature-plugin-system` is its invalidating trigger and is a no-op in the graded configuration. A lookup that cannot be expressed in one of the three families **MUST** bypass the cache rather than mint a new key shape.

**Implements**:
- `cpt-cf-oagw-algo-observability-and-state-cache-key-derivation`

**Touches**:
- API: none
- Entities: `CacheKey`
- Tests: unit tests in `src/infra/cp_cache_tests.rs` for the three key derivations with the owning-tenant component, for the Data Plane routed-entry key and its recorded dependency set, for the reservation of the `plugin:{plugin_id}` family, and for the bypass behavior on an unrepresentable lookup

### Shared Upstream HTTP Client

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-observability-and-state-shared-http-client`

The system **MUST** construct one shared upstream HTTP client during Data Plane initialization and hold it in the Data Plane state alongside the hot-config cache and the rate-limiter registry, so connection pooling and host reuse are shared by every proxy request, and **MUST NOT** construct a per-request client. The shared client **MUST NOT** cache any response, per `cpt-cf-oagw-principle-no-cache`, and its connection state **MUST** feed `oagw_upstream_connections{host, state}`. The timeout enforcement and the retry posture of the calls it issues **MUST** be the posture deferred to `cpt-cf-oagw-principle-no-retry` and to `cpt-cf-oagw-feature-request-proxy`, which owns `proxy_timeout_secs` enforcement on the proxy path, and the shared client **MUST** consume the configuration keys `proxy_timeout_secs`, `ssrf_policy`, `allow_http_upstream`, and the request body-size limit, all delivered by `cpt-cf-oagw-feature-gear-foundation`; this entry introduces no timeout, retry, TLS, or proxy-configuration key of its own.

**Implements**:
- `cpt-cf-oagw-flow-observability-and-state-shared-http-client`

**Touches**:
- API: none
- Entities: `DPState`
- Tests: unit tests in `src/infra/cp_cache_tests.rs` for single construction and reuse across requests, and integration tests in `tests/metrics_endpoint.rs` for the connection-state series

### Per-Instance Rate-Limit State Ownership

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-observability-and-state-rate-limit-ownership`

The system **MUST** hold rate-limit state as per-instance in-memory state owned by the Data Plane and held in the Data Plane state, keyed by the counter scope of the rate-limiting entry and by tenant identity, per `cpt-cf-oagw-adr-state-management`. The state **MUST NOT** be synchronized across instances, **MUST NOT** be persisted, and **MUST** be released with the process, and the resulting per-instance inaccuracy **MUST** be documented as the accepted posture of this entry rather than as a defect.

**Implements**:
- `cpt-cf-oagw-flow-observability-and-state-rate-limit-signals`

**Touches**:
- API: none
- Entities: `DPState`
- Tests: unit tests in `src/infra/cp_cache_tests.rs` for the registry's presence in the Data Plane state and for per-instance isolation between two registry instances

### Health and Readiness Surface

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-observability-and-state-health-readiness`

The system **MUST** expose gear health and readiness through the ToolKit `RestApiCapability` healthcheck hook, using the path the framework provides rather than registering a gear-specific health route, and the reported readiness **MUST** follow `cpt-cf-oagw-state-observability-and-state-health-surface`: a gear that failed initialization, or that lacks the metric registry, either L1 cache, the shared client, or an instrumented service, **MUST NOT** report ready. An initialized gear whose owned state component becomes unavailable **MUST** be reported from the `unhealthy` state of that machine as alive but not ready, with the unavailable component named in the healthcheck result, and **MUST** return to `ready` when the component becomes available again.

**Implements**:
- `cpt-cf-oagw-flow-observability-and-state-health-readiness`
- `cpt-cf-oagw-state-observability-and-state-health-surface`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: ToolKit `RestApiCapability` healthcheck hook (path provided by the framework)
- Entities: none
- Tests: integration tests in `tests/health_readiness.rs` asserting ready after a successful initialization and not ready after a failed one

### Deployment Mode Posture

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-observability-and-state-deployment-modes`

The system **MUST** operate in the single-executable mode with the L1 layers only and no external shared-cache dependency, and **MUST** document the microservice mode — the same L1 layers plus an optional shared L2 cache across instances — as the alternative shape without implementing it. The L2 Redis layer of `cpt-cf-oagw-adr-data-plane-caching` **MUST** remain out of scope in the graded configuration, and a configuration that requests it **MUST** be refused as unsupported rather than silently degraded. The configuration-derived inputs of this entry **MUST** be the fixed values of the graded configuration and not configuration keys: the deployment mode is fixed to the single-executable mode of `cpt-cf-oagw-constraint-toolkit-deploy`, the two L1 capacities are the fixed constants 10,000 Control Plane entries and 1,000 Data Plane entries, and the audit sampling rate is a build-time constant. This entry **MUST** add no configuration key to `OagwConfig`, because adding configuration keys is out of scope for this decomposition, and the resulting deviation from `cpt-cf-oagw-adr-state-management`, which describes the Data Plane L1 cache as configurable via an environment variable, **MUST** remain recorded here — owner: oagw gear owner (platform-operator review); validation: `src/infra/cp_cache_tests.rs`.

**Implements**:
- `cpt-cf-oagw-flow-observability-and-state-deployment-mode`
- `cpt-cf-oagw-algo-observability-and-state-deployment-mode`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: none
- Entities: `CPState`, `DPState`
- Tests: unit tests in `src/infra/cp_cache_tests.rs` asserting that no L2 layer is constructed and that an L2 request is refused

### Automated Unit Test Coverage

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-observability-and-state-unit-tests`

The system **MUST** ship unit tests as sibling `*_tests.rs` modules inside the `oagw` crate covering the metric family names, label keys, histogram buckets, and the declared `phase` value set; the metric cardinality rules including method normalization and the `path` label form; the audit-record field set, `event` value set, per-class field population, levels, redaction, and sampling including the unsampled `proxy_request` class; the key derivation for all three `CacheKey` families with the owning-tenant component and the Data Plane dependency set; and the Control Plane and Data Plane L1 caches for capacity, no-TTL behavior, hit/miss, the four read outcomes, the per-key generation check, and invalidation, and **MUST NOT** place any test under `testing/e2e/gears/oagw/`, which is out of scope per graded deviation 4.

**Implements**:
- `cpt-cf-oagw-dod-observability-and-state-metric-surface`
- `cpt-cf-oagw-dod-observability-and-state-metric-cardinality`
- `cpt-cf-oagw-dod-observability-and-state-audit-log`
- `cpt-cf-oagw-dod-observability-and-state-cache-keys`
- `cpt-cf-oagw-dod-observability-and-state-cp-cache`
- `cpt-cf-oagw-dod-observability-and-state-dp-cache`

**Touches**:
- API: none
- Entities: `CPState`, `DPState`, `CacheKey`
- Tests: `src/infra/metrics_tests.rs`, `src/infra/audit_tests.rs`, `src/infra/cp_cache_tests.rs`, `src/infra/dp_cache_tests.rs`, `src/infra/cp_cache_tests.rs`

### Automated Integration Test Coverage

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-observability-and-state-integration-tests`

The system **MUST** ship integration-style tests inside the crate's `tests/` directory covering the `GET /metrics` registration and its admin-only enforcement, including the `403` refusal of a caller that holds a valid proxy permission but is not in the platform admin principal class, the audit-record shape emitted for real proxy requests, cache invalidation on configuration writes, the Data Plane flush triggered by a configuration write, the `oagw_upstream_available` recovery transition, and the health and readiness surface, and **MUST NOT** create `testing/e2e/gears/oagw/`.

**Implements**:
- `cpt-cf-oagw-dod-observability-and-state-metrics-endpoint`
- `cpt-cf-oagw-dod-observability-and-state-request-metrics`
- `cpt-cf-oagw-dod-observability-and-state-rate-limit-metrics`
- `cpt-cf-oagw-dod-observability-and-state-trace-propagation`
- `cpt-cf-oagw-dod-observability-and-state-cp-cache`
- `cpt-cf-oagw-dod-observability-and-state-dp-cache`
- `cpt-cf-oagw-dod-observability-and-state-health-readiness`

**Touches**:
- API: `GET /metrics`, `GET /oagw/v1/proxy/{alias}` (instrumented)
- Entities: `CPState`, `DPState`
- Tests: `tests/metrics_endpoint.rs`, `tests/audit_log_shape.rs`, `tests/cache_invalidation.rs`, `tests/dp_cache_flush.rs`, `tests/health_readiness.rs`

## 6. Acceptance Criteria

- [x] `GET /metrics` is registered outside the gear-relative `/oagw/v1` prefix, returns the Prometheus text exposition of every registered family for a caller in the platform admin principal class, and returns `403` rendered by `cpt-cf-oagw-feature-error-handling` with no metric series disclosed to a caller that fails the admin check, including a caller that holds a valid proxy permission (DoD `cpt-cf-oagw-dod-observability-and-state-metrics-endpoint`).
- [x] The registry exposes exactly the twelve families DESIGN §4.2 names, each with its declared label keys, the request-duration histogram uses the buckets `[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]`, and its `phase` label takes one of the five declared values `route_match`, `plugin_chain_request`, `upstream_call`, `plugin_chain_response`, and `response` (DoD `cpt-cf-oagw-dod-observability-and-state-metric-surface`).
- [x] A request with a non-standard method is counted under `http.request.method` `_OTHER`, a `path` label on a rate-limit metric equals the matched route pattern and never the raw request path, the status code is numeric, `phase` stays inside its declared value set, and no metric series carries a tenant label (DoD `cpt-cf-oagw-dod-observability-and-state-metric-cardinality`).
- [x] A proxy request that ends in a gateway error increments `oagw_requests_total`, `oagw_errors_total` with its `error_type`, and returns `oagw_requests_in_flight` to its prior value, including when a streaming response is aborted, and a call through an endpoint sets `oagw_upstream_available{host, endpoint}` to `1` so a recovered endpoint is observable (DoD `cpt-cf-oagw-dod-observability-and-state-request-metrics`).
- [x] A rate-limit rejection increments `oagw_rate_limit_exceeded_total` for the matched route pattern and `oagw_rate_limit_usage_ratio` stays within 0.0 to 1.0 for the same series (DoD `cpt-cf-oagw-dod-observability-and-state-rate-limit-metrics`).
- [x] `oagw_circuit_breaker_state{host}` and `oagw_circuit_breaker_transitions_total{host, from_state, to_state}` are registered and emit no series, and no breaker behavior, fallback, or `CircuitBreakerOpen` outcome is produced by this entry (DoD `cpt-cf-oagw-dod-observability-and-state-breaker-metrics`).
- [x] One proxy request produces exactly one unsampled audit record on stdout with the fourteen declared fields, no `error_message` field, the level DESIGN §4.3 assigns to its outcome, and no request body, query parameter, non-allowlisted header, or credential material anywhere in the line, and a `config_change` or `auth_failure` record carries the field population its class declares (DoD `cpt-cf-oagw-dod-observability-and-state-audit-log`).
- [x] The `trace_id` in a rendered problem+json body equals the `request_id` in the audit record for the same request, and every proxy request including a pre-route rejection is logged with a correlation identifier from the always-emitted, never-sampled `proxy_request` audit class (DoD `cpt-cf-oagw-dod-observability-and-state-trace-propagation`).
- [x] A repeated configuration read is served from the Control Plane L1 cache without a repository call, an accepted configuration write makes the next read observe the written value rather than the cached one, a population racing that flush cannot insert a pre-write value, and a store failure surfaces as the distinct store-error outcome and inserts nothing (DoD `cpt-cf-oagw-dod-observability-and-state-cp-cache`).
- [x] A proxy request after a cached resolution is served without a Control Plane call, and a configuration write removes the served stale value from the Data Plane hot-config cache by dependency intersection, leaving an unrelated entry in place, before the write is reported as complete (DoD `cpt-cf-oagw-dod-observability-and-state-dp-cache`).
- [x] Every cache entry key is one of `upstream:{owner_tenant_id}:{alias}`, the Control Plane `route:{upstream_id}:{method}:{path_prefix}`, the Data Plane routed entry `route:{owner_tenant_id}:{route_id}`, or the reserved `plugin:{plugin_id}` family with no reader in the graded configuration; the `tenant_id` component names the owning tenant and never the calling tenant; a Data Plane entry records the Control Plane keys it was derived from; and a lookup outside those three families bypasses the cache instead of minting a new key shape (DoD `cpt-cf-oagw-dod-observability-and-state-cache-keys`).
- [x] Two consecutive upstream calls reuse one shared client instance, the client caches no response, and `oagw_upstream_connections{host, state}` reflects its connection state (DoD `cpt-cf-oagw-dod-observability-and-state-shared-http-client`).
- [x] Two rate-limiter registry instances in the same process hold independent counters, and neither instance's state is written to any store (DoD `cpt-cf-oagw-dod-observability-and-state-rate-limit-ownership`).
- [x] A successfully initialized gear reports ready through the ToolKit `RestApiCapability` healthcheck hook, a gear whose initialization failed does not, and an initialized gear whose owned state component becomes unavailable reports alive but not ready and returns to ready when that component is available again (DoD `cpt-cf-oagw-dod-observability-and-state-health-readiness`).
- [x] The single-executable mode constructs the Control Plane L1 cache and the Data Plane L1 hot-config cache with no shared cache layer, a configuration that requests the shared L2 layer is refused as unsupported rather than silently degraded, and the deployment mode, the two L1 capacities, and the audit sampling rate are fixed graded values to which this entry adds no configuration key (DoD `cpt-cf-oagw-dod-observability-and-state-deployment-modes`).
- [x] All tests for this feature live inside the `oagw` crate as sibling `*_tests.rs` modules and files under `tests/`, and no `testing/e2e/gears/oagw/` directory is created (DoD `cpt-cf-oagw-dod-observability-and-state-unit-tests`, `cpt-cf-oagw-dod-observability-and-state-integration-tests`).
