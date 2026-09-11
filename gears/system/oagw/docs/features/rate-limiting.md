# Feature: Rate Limiting


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Proxy Request Rate Limit Check](#proxy-request-rate-limit-check)
  - [Rate-Limit Configuration Surface](#rate-limit-configuration-surface)
  - [Counter Scoping and Tenant Isolation](#counter-scoping-and-tenant-isolation)
  - [Rate-Limit Usage Observation](#rate-limit-usage-observation)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Dual-Rate Token Bucket](#dual-rate-token-bucket)
  - [Sliding Window Alternative](#sliding-window-alternative)
  - [Effective Limit Resolution](#effective-limit-resolution)
  - [Counter Key Derivation](#counter-key-derivation)
  - [Strategy Dispatch and Rejection Response](#strategy-dispatch-and-rejection-response)
  - [Usage Ratio Observation](#usage-ratio-observation)
- [4. States (CDSL)](#4-states-cdsl)
  - [Rate Limit Counter State Machine](#rate-limit-counter-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Dual-Rate Configuration Surface](#dual-rate-configuration-surface)
  - [Hierarchical min() Inheritance](#hierarchical-min-inheritance)
  - [Token Bucket with Refill on Read](#token-bucket-with-refill-on-read)
  - [Sliding Window Alternative](#sliding-window-alternative-1)
  - [Counter Scopes and Per-Tenant Isolation](#counter-scopes-and-per-tenant-isolation)
  - [Per-Instance In-Memory State](#per-instance-in-memory-state)
  - [Reject Enforcement with 429 and Retry-After](#reject-enforcement-with-429-and-retry-after)
  - [Reject-Only Execution Boundary](#reject-only-execution-boundary)
  - [Enforcement Point on the Proxy Path](#enforcement-point-on-the-proxy-path)
  - [Hot-Path Cost and Registry Bounds](#hot-path-cost-and-registry-bounds)
  - [Payload Limit Interaction](#payload-limit-interaction)
  - [Rate-Limit Metrics Surface](#rate-limit-metrics-surface)
  - [Automated Unit Test Coverage](#automated-unit-test-coverage)
  - [Automated Integration Test Coverage](#automated-integration-test-coverage)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [x] `p1` - **ID**: `cpt-cf-oagw-featstatus-rate-limiting-implemented`

<!-- reference to DECOMPOSITION entry -->
- [x] `p2` - `cpt-cf-oagw-feature-rate-limiting`

## 1. Feature Context

### 1.1 Overview

Implements the dual-rate rate limiter that sits on the proxy hot path: a sustained rate over a human-readable window plus a burst capacity, evaluated against per-instance in-memory counters keyed by a configurable scope, inherited across the tenant hierarchy so a descendant can only be stricter, and enforced with the `reject` strategy that answers `429` with `Retry-After` and the `X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset` response headers. The feature owns the rate-limit decision on the proxy path and the rate-limit metrics; it introduces no new endpoint.

### 1.2 Purpose

Rate limiting protects external service agreements and prevents cost overruns. It must do so without adding a control-plane hop or a network round trip per request, which is why the decision is taken in the Data Plane against in-memory counters and why the check runs on the proxy hot path as a step inside `cpt-cf-oagw-seq-proxy-flow`, ahead of the upstream call. This feature realizes the decision recorded in `cpt-cf-oagw-adr-rate-limiting` (token bucket by default, sliding window as the optional algorithm, dual-rate configuration, `min()` inheritance) and the rate-limit slice of `cpt-cf-oagw-component-model`: the `RateLimiterRegistry` owned by the Data Plane, the token-bucket and sliding-window implementations, the decision integration point in the proxy path, and the `429` response construction with `Retry-After` and `X-RateLimit-*` headers.

Enforcement is owned by this feature. The proxy handler, request-surface validation, body-limit enforcement, and the response pipeline it renders through are owned by `cpt-cf-oagw-feature-request-proxy`; the shared `application/problem+json` rendering of the rejected response is owned by entry 2.5. This feature supplies the decision, the status code, the retry guidance, and the quota headers.

Delivered by this feature:

- `p1` - `cpt-cf-oagw-fr-rate-limiting`
- [ ] `p2` - `cpt-cf-oagw-usecase-rate-limit-exceeded` - covered here for the `reject` outcome and the 429/`Retry-After` mapping; `queue` and `degrade` execution are excluded per graded deviation 8

**Principles**: `cpt-cf-oagw-principle-tenant-scope` - rate-limit counters are keyed by tenant scope, so a tenant can never consume another tenant's budget, except the operator-controlled `global` scope, which is a documented deployment-level exception (see `cpt-cf-oagw-flow-rate-limiting-counter-scope`).

**Constraints**: none. Entry 2.7 introduces no DESIGN constraint; the low-latency budget this feature serves is `cpt-cf-oagw-nfr-low-latency`, which is delivered by `cpt-cf-oagw-feature-request-proxy` and honored here by keeping the check in-memory and allocation-free on the hot path. The metric surface this feature feeds serves `cpt-cf-oagw-nfr-observability`, whose recorder and `/metrics` endpoint are owned by entry 2.9.

**Applicability.** Not applicable because, for each requirement class outside this record: there is no external integration and no message surface, because the counters are in-process and in-memory only per `inst-rl-scp-4`, so no broker, queue, or external store is integrated and no message contract is produced or consumed (integration requirements INT-003, INT-004, and INT-005 do not apply). There is no cache integration owned here, because the Data Plane L1 hot-config cache is delivered by entry 2.9 and this feature only reads the configuration that cache serves. There is no accessibility surface, because the feature is consumed only through the HTTP proxy API and adds no user interface of its own (UX-002 does not apply). There is no compliance or data-residency obligation carried by this record, because a counter holds no request payload and no credential material, and no identifier beyond the tenant, principal, peer-address, and route components the counter key already names (COMPL-001 and COMPL-002 do not apply).

**Graded-configuration boundary.** Two graded deviations from the supplied PRD and ADR text and one documented field-set delta govern this feature and are restated where they bind:

- Graded deviation 8 - the graded configuration exercises the `reject` strategy only. The `queue` and `degrade` strategy values and the budget-allocation modes (`unlimited` | `allocated` | `shared` with `overcommit_ratio`) remain legal configuration surface and are parsed, validated, and stored, but they are not executed. The counter-scope enum additionally admits `route`, which the PRD enumeration in `cpt-cf-oagw-fr-rate-limiting` does not list.
- Graded deviation 3 - automated test coverage is in scope: unit tests as sibling `*_tests.rs` modules in the `oagw` crate and integration-style tests in the crate's `tests/` directory. Per graded deviation 4 no test is placed under `testing/e2e/gears/oagw/`.
- Documented field-set delta - the `rate_limit` block admitted by this feature carries `response_headers` and the budget-allocation fields `budget.mode`, `budget.total`, and `budget.overcommit_ratio` beyond the canonical `rate_limit` shape of `schemas/upstream.v1.schema.json` and `schemas/route.v1.schema.json`, both of which set `additionalProperties` to false and neither of which declares those keys. The delta is a documented deviation owned by this feature, with the ADR 0003 field table and its schema-constraints block as the authority for the admitted names, types, and defaults, and the two schema artifacts are not extended by this feature record: they stay read-only inputs. The exception set is exactly those four fields and nothing else, so every other key a `rate_limit` block carries is rejected at the configuration boundary.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Sets the system-level and upstream-level limits, owns the decision to spend instance-wide budget through the `global` scope, and reads the rate-limit metrics to detect abusive consumers |
| `cpt-cf-oagw-actor-tenant-admin` | Sets stricter limits for the tenant hierarchy within the sharing modes, is the subject of the enforced ancestor caps, and can never reach another tenant's counters |
| `cpt-cf-oagw-actor-app-developer` | Consumes the proxy path and receives `429` with `Retry-After` and the `X-RateLimit-*` headers when the configured budget is exhausted |
| `cpt-cf-oagw-actor-upstream-service` | The protected external service whose agreement and capacity the configured budget defends |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **ADR**: [ADR 0003 - Rate Limiting](../ADR/0003-rate-limiting.md)
- **Schemas**: [schemas/upstream.v1.schema.json](../schemas/upstream.v1.schema.json), [schemas/route.v1.schema.json](../schemas/route.v1.schema.json)
- **Dependencies**: requires `cpt-cf-oagw-feature-request-proxy` - the rate check runs on the proxy hot path and its `429` response must be rendered through the proxy response pipeline. It consumes the configuration model, the merge engine, and the domain types delivered by `cpt-cf-oagw-feature-gear-foundation`. This feature is independent of `cpt-cf-oagw-feature-error-handling` and `cpt-cf-oagw-feature-cors`; all three can be developed in parallel once `cpt-cf-oagw-feature-request-proxy` exists.

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-oagw-usecase-rate-limit-exceeded`. The `reject` alternative flow of `cpt-cf-oagw-usecase-proxy-request` ("Rate limit exceeded: Return 429 with Retry-After header") is realized by the proxy request rate-limit check below; that use case itself is owned by `cpt-cf-oagw-feature-request-proxy`.

### Proxy Request Rate Limit Check

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-rate-limiting-proxy-check`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A proxy request whose cost fits the remaining budget consumes `cost` tokens from the counter for the configured scope and proceeds on the proxy path toward the upstream call.
- When `response_headers` is true (the default) the allowed response carries `X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset` for the counter that was consulted.
- No rate limit is configured on the resolved upstream or route: the request proceeds with no counter consulted and no quota headers added.

**Error Scenarios**:
- The remaining budget is below `cost`: the configured strategy is applied. Under the graded configuration the only executable strategy is `reject`, so the request is answered `429` with `Retry-After` and the three `X-RateLimit-*` headers, and the upstream is never called.
- A `queue` or `degrade` strategy is configured: the values are legal configuration surface but are not executed per graded deviation 8, so the `reject` outcome is applied and no request is ever queued or served with reduced functionality.
- The counter state for a key is absent: it is created lazily at full capacity, so the first request for a key is never rejected for missing state.
- The request body size is declared by `Content-Length` and exceeds the payload limit: the `413` is returned before the rate-limit step runs, because that declared size is validated on the proxy path ahead of this flow, so no counter is consulted, created, or consumed.
- The request body length is undeclared, because the body is chunked or otherwise lengthless: the rate-limit decision is taken before buffering, so the charge stands, and when the payload limit is exceeded mid-buffer the `413` is emitted and the counter is not restored. That no-refund outcome is the documented deviation for undeclared-length bodies.

**Steps**:
1. [x] - `p1` - Receive the resolved request context from the proxy path after the upstream and route are resolved, after the request surface has been validated so a body size declared by `Content-Length` has already been checked against the payload limit, and after the effective configuration is merged, and before the upstream call is issued - `inst-rl-chk-1`
2. [x] - `p1` - Resolve the effective rate-limit configuration for the request, composed from the upstream-level and route-level `rate_limit` fields and the enforced ancestor limits - `inst-rl-chk-2`
3. [x] - `p1` - **IF** no `rate_limit` configuration is present for the resolved upstream or route - `inst-rl-chk-3`
   1. [x] - `p1` - Continue the proxy path without consulting a counter, without observing a usage ratio, and without adding quota headers - `inst-rl-chk-4`
4. [x] - `p1` - Compute the counter key for the configured scope from the request context - `inst-rl-chk-5`
5. [x] - `p1` - Attempt to acquire `cost` tokens from the counter for that key, refilling on read under `token_bucket` or admitting against the trailing window under `sliding_window` - `inst-rl-chk-6`
6. [x] - `p1` - **IF** the acquisition succeeds - `inst-rl-chk-7`
   1. [x] - `p1` - Observe the post-decision usage ratio for the counter and attach the three `X-RateLimit-*` headers to the response when `response_headers` is true - `inst-rl-chk-8`
   2. [x] - `p1` - Continue the proxy path toward the upstream call - `inst-rl-chk-9`
7. [x] - `p1` - **IF** the acquisition fails - `inst-rl-chk-10`
   1. [x] - `p1` - Apply the configured strategy, where `reject` is the only executable value and a configured `queue` or `degrade` value resolves to the `reject` outcome per graded deviation 8 - `inst-rl-chk-11`
   2. [x] - `p1` - Build the rejected response: status `429`, `Retry-After` in seconds, and `X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset` for the counter that refused the request - `inst-rl-chk-12`
   3. [x] - `p1` - Increment the exceeded counter and observe the usage ratio for the refused bucket - `inst-rl-chk-13`
   4. [x] - `p1` - Return the rejected response through the proxy response pipeline, so the shared `application/problem+json` body and the gateway error-source header are rendered by the pipeline owned by entries 2.4 and 2.5, and the upstream is never called - `inst-rl-chk-14`
8. [x] - `p1` - **RETURN** the decision outcome: the request proceeds on the proxy path, or the `429` response is returned to the caller with its retry guidance and quota headers - `inst-rl-chk-15`

### Rate-Limit Configuration Surface

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-rate-limiting-config-surface`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A `rate_limit` block on an upstream record or on a route record parses with the dual-rate fields, the sharing mode, the algorithm, the scope, the strategy, the cost, and the response-header switch, and every absent field receives its declared default.
- `burst.capacity` omitted defaults to `sustained.rate`; `cost` omitted defaults to `1`; `scope` omitted defaults to `tenant`; `algorithm` omitted defaults to `token_bucket`; `strategy` omitted defaults to `reject`; `sustained.window` omitted defaults to `second`; `sharing` omitted defaults to `private`.

**Error Scenarios**:
- `sustained.rate` is absent or below 1, `burst.capacity` is below 1, or `cost` is below 1: the record is rejected with a validation error naming the offending field.
- `algorithm`, `sustained.window`, `scope`, `strategy`, or `sharing` carries a value outside its enumeration: the record is rejected.
- The block carries a key outside the documented field set, which is the field set parsed by `inst-rl-cfg-2` plus the documented deviation fields `response_headers`, `budget.mode`, `budget.total`, and `budget.overcommit_ratio`: the record is rejected, so a misspelled control cannot silently disable enforcement.

**Steps**:
1. [x] - `p1` - Accept a `rate_limit` block on an upstream record and on a route record, so the same dual-rate shape is legal at both levels - `inst-rl-cfg-1`
2. [x] - `p1` - Parse the field set `sharing`, `algorithm`, `sustained.rate`, `sustained.window`, `burst.capacity`, `scope`, `strategy`, `cost`, `response_headers`, `budget.mode`, `budget.total`, and `budget.overcommit_ratio`, where `response_headers` and the three budget fields are the documented field-set delta of the graded-configuration boundary, validated by this feature rather than by the upstream and route schemas - `inst-rl-cfg-2`
3. [x] - `p1` - Admit the enumeration values `sharing` in `private | inherit | enforce`, `algorithm` in `token_bucket | sliding_window`, `sustained.window` in `second | minute | hour | day`, `scope` in `global | tenant | user | ip | route`, and `strategy` in `reject | queue | degrade` - `inst-rl-cfg-3`
4. [x] - `p1` - Admit the budget-allocation surface `budget.mode` in `unlimited | allocated | shared` with `budget.total` and `budget.overcommit_ratio` as legal configuration values that are parsed, validated, and stored but not executed, per graded deviation 8, and validate the admitted budget values: `budget.total` is an integer of at least 1, `budget.overcommit_ratio` is a number in the range 1.0 to 2.0 inclusive whose default is 1.0, and `budget.total` is required whenever `budget.mode` is `allocated` or `shared` - `inst-rl-cfg-4`
5. [x] - `p1` - Apply the declared defaults so an omitted `burst.capacity` becomes `sustained.rate` and the remaining omitted fields become `cost` 1, `scope` `tenant`, `algorithm` `token_bucket`, `strategy` `reject`, `sustained.window` `second`, `sharing` `private`, and `response_headers` true - `inst-rl-cfg-5`
6. [x] - `p1` - **IF** a numeric field violates its minimum of 1, `budget.overcommit_ratio` falls outside the range 1.0 to 2.0, `budget.total` is absent while `budget.mode` is `allocated` or `shared`, or an enumeration field carries an unknown value - `inst-rl-cfg-6`
   1. [x] - `p1` - Reject the record with a validation error naming the offending field and store nothing - `inst-rl-cfg-7`
7. [x] - `p1` - **RETURN** the stored `RateLimitConfig` values on the upstream and route records, ready to be composed by the effective-limit resolution - `inst-rl-cfg-8`

### Counter Scoping and Tenant Isolation

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-rate-limiting-counter-scope`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- Every counter is keyed by the configured scope and by the requesting tenant, so two tenants never share a counter under the `tenant`, `user`, `ip`, or `route` scope and one tenant can never consume another tenant's budget.
- The `route` scope keys the counter by the matched route identity, so a route-level limit is enforced per route independently of the upstream-level budget, per graded deviation 8.
- The `global` scope keys a single instance-wide counter with no tenant component, which is an operator-controlled deployment setting rather than a tenant-facing one.

**Error Scenarios**:
- The scope discriminator is unavailable for a request, such as no authenticated principal for the `user` scope or no downstream peer address for the `ip` scope: the counter falls back to the tenant discriminator rather than collapsing unrelated callers into one bucket.
- The owning upstream or route is deleted: its counters are dropped, so no stale budget survives a resource deletion.

**Steps**:
1. [x] - `p1` - Receive the scope from the effective rate-limit configuration and the request context for the resolved upstream, route, and caller - `inst-rl-scp-1`
2. [x] - `p1` - Build the counter key with the tenant identifier as its leading component for every scope other than `global`, followed by the scope discriminator: nothing further for `global`, the caller's tenant for `tenant`, the authenticated principal for `user`, the downstream peer address for `ip`, and the matched route identity for `route` - `inst-rl-scp-2`
3. [x] - `p1` - Key the counter by exactly three components - the owning resource identity, the tenant identifier, and the scope discriminator - so an upstream and its routes never contend for one counter, and carry the window state inside the counter rather than in the key, so successive windows are phases of one counter and not separate counters - `inst-rl-scp-3`
4. [x] - `p1` - Hold every counter in memory inside the `RateLimiterRegistry` owned by the Data Plane instance, with no cross-instance coordination and no persisted counter state, per `cpt-cf-oagw-adr-state-management`, and bound the registry explicitly: it holds at most 10,000 counters, mirroring the 10,000-entry L1 LRU posture recorded in `cpt-cf-oagw-adr-state-management`, and a counter whose key has not been consulted for 15 minutes is evicted, so the key space cannot grow without limit under an unbounded set of callers; an evicted key is dropped without being logged and without emitting any metric observation - `inst-rl-scp-4`
5. [x] - `p1` - Take the per-key lock of the `RateLimiterRegistry` as the unit of synchronization for the counter it hands out, so the check-and-deduct of one counter is atomic, two concurrent acquires of the same key are fully serialized, the total deduction equals the number of successful acquisitions multiplied by `cost`, and the counter is never over-deducted - `inst-rl-scp-10`
6. [x] - `p1` - **IF** the scope discriminator is absent from the request context - `inst-rl-scp-5`
   1. [x] - `p1` - Fall back to the tenant discriminator so unrelated callers are never merged into a single bucket - `inst-rl-scp-6`
7. [x] - `p1` - **IF** the owning upstream or route is deleted, or its effective limit changes so the stored state no longer matches the configured capacity - `inst-rl-scp-7`
   1. [x] - `p1` - Drop the counters keyed under that resource identity so no stale budget survives the configuration change - `inst-rl-scp-8`
8. [x] - `p1` - **RETURN** the counter for the computed key, isolated per tenant and per owning resource - `inst-rl-scp-9`

### Rate-Limit Usage Observation

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-rate-limiting-usage-observation`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Every refused request increments `oagw_rate_limit_exceeded_total` exactly once, labeled with the upstream alias and the normalized route match pattern.
- The `oagw_rate_limit_usage_ratio` gauge reports the consumed fraction of the bucket for the counter that served the decision, in the range 0.0 to 1.0, after every decision taken. The series for one `host` and `path` is a sample of the most recently decided counter for that host and path, not a per-counter gauge.

**Error Scenarios**:
- The metric recorder is unavailable: the rate-limit decision is unaffected and the request is still allowed or refused on its merits, because enforcement never depends on observability.

**Steps**:
1. [x] - `p1` - Emit the rate-limit metric observations through the metrics recorder owned by entry 2.9, which owns the registry and the admin-only `/metrics` endpoint - `inst-rl-obs-1`
2. [x] - `p1` - Increment `oagw_rate_limit_exceeded_total` once per refused request, labeled with `host` carrying the upstream alias and `path` carrying the normalized route match pattern, never the raw request path - `inst-rl-obs-2`
3. [x] - `p1` - Set `oagw_rate_limit_usage_ratio` to the consumed fraction of the bucket for the counter that served the decision, labeled with the same `host` and `path` labels and clamped to the range 0.0 to 1.0, so the series for one `host` and `path` is a sample of the most recently decided counter for that host and path rather than a per-counter gauge, and is safe to read as a per-upstream and per-route pressure signal and not as a per-consumer quota - `inst-rl-obs-3`
4. [x] - `p1` - Omit any tenant label from both metrics and derive `path` from the matched route pattern, so cardinality stays bounded per the DESIGN metric rules - `inst-rl-obs-4`
5. [x] - `p1` - Carry a refusal into the shared structured audit record by supplying the rate-limit outcome and the retry guidance to the pipeline that emits it, so the record is emitted by `cpt-cf-oagw-feature-request-proxy` and `cpt-cf-oagw-feature-observability-and-operability` and carries `error_type` set to the rate-limit GTS type `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1`, `status` 429, `request_id`, `tenant_id`, and `principal_id` - `inst-rl-obs-8`
6. [x] - `p1` - **IF** the metric recorder is unavailable - `inst-rl-obs-5`
   1. [x] - `p1` - Skip the observation and leave the allow or refuse decision unchanged - `inst-rl-obs-6`
7. [x] - `p1` - **RETURN** the emitted rate-limit metric series for the request - `inst-rl-obs-7`

## 3. Processes / Business Logic (CDSL)

### Dual-Rate Token Bucket

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-rate-limiting-token-bucket`

**Input**: one counter for a computed key, the effective sustained rate and window, the effective burst capacity, the request `cost`, and the instant of the decision, which is read from the injectable monotonic clock.
**Output**: an allow decision with the tokens remaining, or a refuse decision with the retry guidance.

**Steps**:
1. [x] - `p1` - Refill on read: advance the stored token count by the elapsed time measured on the injectable monotonic clock multiplied by the refill rate derived from `sustained.rate` per `sustained.window`, and cap the result at the burst capacity - `inst-rl-tb-1`
2. [x] - `p1` - Compare the available tokens against the request `cost` - `inst-rl-tb-2`
3. [x] - `p1` - **IF** the available tokens are at least `cost` - `inst-rl-tb-3`
   1. [x] - `p1` - Deduct `cost` tokens, persist the new balance and the read instant on the counter, and report the allow decision with the remaining tokens - `inst-rl-tb-4`
4. [x] - `p1` - **IF** the available tokens are below `cost` - `inst-rl-tb-5`
   1. [x] - `p1` - Consume nothing, persist the refilled balance and the read instant so the elapsed time is not double-counted, and report the refuse decision with the shortfall and the time to restore `cost` tokens - `inst-rl-tb-6`
5. [x] - `p1` - Treat the burst capacity as the maximum immediately spendable amount, so a quiescent caller may spend up to `burst.capacity` in one burst and then is limited to the sustained refill rate - `inst-rl-tb-7`
6. [x] - `p1` - Take the per-key lock of the `RateLimiterRegistry` as the unit of synchronization, so the refill, the comparison, and the deduction of one counter are one atomic check-and-deduct: two concurrent acquires of the same key are fully serialized, the total deduction equals the number of successful acquisitions multiplied by `cost`, and the bucket is never over-deducted - `inst-rl-tb-9`
7. [x] - `p1` - **RETURN** the decision with the balance, the effective limit, and the reset instant, which under `token_bucket` is the instant at which the balance returns to the full burst capacity under refill - `inst-rl-tb-8`

### Sliding Window Alternative

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-rate-limiting-sliding-window`

**Input**: one counter for a computed key, the effective sustained rate and window, the request `cost`, and the instant of the decision, which is read from the injectable monotonic clock.
**Output**: an allow decision with the usage remaining in the trailing window, or a refuse decision with the window reset instant.

**Steps**:
1. [x] - `p1` - Drop the recorded consumption that falls outside the trailing `sustained.window` ending at the decision instant, with the window bounds measured on the injectable monotonic clock - `inst-rl-sw-1`
2. [x] - `p1` - Sum the cost already admitted inside the trailing window - `inst-rl-sw-2`
3. [x] - `p1` - **IF** the summed consumption plus the request `cost` is at most the sustained rate - `inst-rl-sw-3`
   1. [x] - `p1` - Record the request `cost` at the decision instant and report the allow decision with the remaining window budget - `inst-rl-sw-4`
4. [x] - `p1` - **IF** the summed consumption plus the request `cost` exceeds the sustained rate - `inst-rl-sw-5`
   1. [x] - `p1` - Record nothing and report the refuse decision with the reset instant of the algorithm, which is the instant at which the oldest admitted consumption leaves the trailing window - `inst-rl-sw-6`
5. [x] - `p1` - Apply no burst allowance, because the sliding window enforces the sustained rate over the trailing window without a boundary burst - `inst-rl-sw-7`
6. [x] - `p1` - Take the same per-key lock of the `RateLimiterRegistry` as the unit of synchronization, so the trailing-window read, the admission test, and the recording of `cost` are one atomic check-and-deduct and the window is never over-admitted - `inst-rl-sw-9`
7. [x] - `p1` - **RETURN** the decision with the window usage and the window reset instant - `inst-rl-sw-8`

### Effective Limit Resolution

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-rate-limiting-effective-limit`

**Input**: the `rate_limit` configuration of the matched route, of the selected upstream, and of every tenant layer in the hierarchy from root to leaf, each with its sharing mode.
**Output**: one effective dual-rate limit for the request.

**Steps**:
1. [x] - `p1` - Collect every `rate_limit` layer that applies to the request: the selected upstream, the matched route, and the tenant chain reached during alias resolution - `inst-rl-eff-1`
2. [x] - `p1` - Compose the layers through the configuration merge engine delivered by `cpt-cf-oagw-feature-gear-foundation`, which applies `min(ancestor, descendant)` to rate limits so the stricter value always wins - `inst-rl-eff-2`
3. [x] - `p1` - Retain every enforced ancestor limit across alias shadowing, so a descendant that shadows an ancestor alias cannot escape the ancestor's enforced budget - `inst-rl-eff-3`
4. [x] - `p1` - Compute the effective sustained rate as the minimum sustained rate over the contributing layers that are visible to the requester, honoring the sharing modes so a `private` ancestor layer contributes nothing and an `enforce` ancestor layer always contributes - `inst-rl-eff-4`
5. [x] - `p1` - Compute the effective burst capacity as the minimum burst capacity over the same contributing layers, and default it to the effective sustained rate when no layer specifies one - `inst-rl-eff-5`
6. [x] - `p1` - Take the algorithm, scope, strategy, cost, and response-header switch from the most specific contributing layer that specifies each, so a route-level `cost` or `scope` override applies to that route only - `inst-rl-eff-6`
7. [x] - `p1` - Convert the effective sustained rate and window into one refill rate expressed per second, so `token_bucket` and `sliding_window` consume the same effective limit - `inst-rl-eff-7`
8. [x] - `p1` - Leave the rate limiter inactive for a request whose resolved configuration carries no `rate_limit` on any contributing layer - `inst-rl-eff-8`
9. [x] - `p1` - **RETURN** the one effective dual-rate limit for the request - `inst-rl-eff-9`

### Counter Key Derivation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-rate-limiting-counter-key`

**Input**: the effective scope, the resolved upstream and route identities, the caller's tenant identifier, the authenticated principal, and the downstream peer address.
**Output**: one counter key.

**Steps**:
1. [x] - `p1` - Start the key with the owning resource identity, so upstream-level and route-level counters for the same caller are distinct - `inst-rl-key-1`
2. [x] - `p1` - **IF** the effective scope is not `global` - `inst-rl-key-2`
   1. [x] - `p1` - Place the caller's tenant identifier immediately after the resource identity, so no two tenants share a counter under the `tenant`, `user`, `ip`, or `route` scope - `inst-rl-key-3`
3. [x] - `p1` - Append the scope discriminator: none for `global`, the caller's tenant for `tenant`, the authenticated principal for `user`, the downstream peer address for `ip`, and the matched route identity for `route` - `inst-rl-key-4`
4. [x] - `p1` - **IF** the discriminator for the configured scope is absent from the request context - `inst-rl-key-5`
   1. [x] - `p1` - Use the caller's tenant identifier as the discriminator so unrelated callers are never merged - `inst-rl-key-6`
5. [x] - `p1` - Complete the key at the scope discriminator and append nothing further, so the counter identity is exactly the owning resource identity, the tenant, and the scope discriminator, and the window state is carried inside the counter rather than encoded in the key - `inst-rl-key-7`
6. [x] - `p1` - Take the tenant identifier and the authenticated principal only from the authenticated security context that the proxy path established, and take the `ip` discriminator only from the connection peer address and never from a client-supplied forwarding header, so no client-supplied header or query component contributes to the counter key - `inst-rl-key-9`
7. [x] - `p1` - **RETURN** the counter key, which is stable for repeated identical requests and never contains credential material - `inst-rl-key-8`

### Strategy Dispatch and Rejection Response

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-rate-limiting-strategy-dispatch`

**Input**: the effective strategy, the refused decision carrying the shortfall, the reset instant, and the window-release instant under `sliding_window`, and the effective limit and remaining balance.
**Output**: the response to return to the caller.

**Steps**:
1. [x] - `p1` - Dispatch on the effective strategy value `reject`, `queue`, or `degrade` - `inst-rl-str-1`
2. [x] - `p1` - Execute only the `reject` branch: the `queue` and `degrade` branches are legal configuration surface that is not executed, per graded deviation 8 - `inst-rl-str-2`
3. [x] - `p1` - **IF** the effective strategy is `queue` or `degrade` - `inst-rl-str-3`
   1. [x] - `p1` - Resolve the outcome to `reject`, so no request is ever queued for later execution and no request is ever served with reduced functionality - `inst-rl-str-4`
4. [x] - `p1` - Compute `Retry-After` per algorithm: under `token_bucket`, the whole number of seconds until the counter can satisfy the request `cost`, computed from the shortfall and the refill rate and floored at one second; under `sliding_window`, the number of seconds until the refused decision's window-release instant, at which the trailing window has released enough admitted consumption to admit `cost` - `inst-rl-str-5`
5. [x] - `p1` - Set `X-RateLimit-Limit` to the effective limit for the counter, `X-RateLimit-Remaining` to the whole number of tokens left after the refused request, and `X-RateLimit-Reset` to the reset instant of the refusing counter, which is the instant at which the balance returns to the full burst capacity under refill for `token_bucket` and the instant at which the oldest admitted consumption leaves the trailing window for `sliding_window`, rendered as an epoch second by the single wall-clock mapping of the clock contract - `inst-rl-str-6`
6. [x] - `p1` - Attach the three `X-RateLimit-*` headers and `Retry-After` to the rejected response whenever `response_headers` is true, which is the default - `inst-rl-str-7`
7. [x] - `p1` - Report the status `429`, the GTS type `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1`, the `retry_after_seconds` extension value, and the retriable classification from the DESIGN error table, and leave the `application/problem+json` body rendering and the gateway error-source header to the shared response pipeline - `inst-rl-str-8`
8. [x] - `p1` - **RETURN** the rejection response for the proxy response pipeline to render - `inst-rl-str-9`

### Usage Ratio Observation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-rate-limiting-usage-ratio`

**Input**: the counter that served a decision, its effective capacity, and its consumption after the decision.
**Output**: one gauge observation.

**Steps**:
1. [x] - `p1` - Compute the consumed fraction as the consumption after the decision divided by the effective capacity, clamped to the range 0.0 to 1.0 - `inst-rl-ratio-1`
2. [x] - `p1` - Label the observation with `host` carrying the upstream alias and `path` carrying the normalized route match pattern, never the raw request path, and with no tenant label - `inst-rl-ratio-2`
3. [x] - `p1` - Emit the observation after an allow decision and after a refuse decision, so the gauge reports the bucket that actually served the request and one `host` and `path` series is a sample of the most recently decided counter for that host and path rather than a per-counter gauge - `inst-rl-ratio-3`
4. [x] - `p1` - **RETURN** the observation for the metrics recorder owned by entry 2.9 - `inst-rl-ratio-4`

## 4. States (CDSL)

### Rate Limit Counter State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-rate-limiting-counter`

**States**: `absent`, `active`, `depleted`, `released`
**Initial State**: `absent`
**Transitions**:
1. [x] - `p1` - **FROM** `absent` **TO** `active` **WHEN** the first request for a computed counter key arrives and the counter is created lazily at the effective burst capacity, which is the `token_bucket` capacity, while under `sliding_window` the counter is created with zero recorded consumption and no burst capacity applies - `inst-rl-st-cnt-1`
2. [x] - `p1` - **FROM** `active` **TO** `active` **WHEN** the active predicate of the algorithm keeps holding after a decision: under `token_bucket`, a refill-on-read advances the balance and an acquisition of `cost` tokens succeeds; under `sliding_window`, a consumption is admitted inside the trailing window - `inst-rl-st-cnt-2`
3. [x] - `p1` - **FROM** `active` **TO** `depleted` **WHEN** the depleted predicate of the algorithm holds: under `token_bucket`, the available balance is below the request `cost`; under `sliding_window`, the trailing-window consumption plus the request `cost` exceeds the sustained rate over the effective window - `inst-rl-st-cnt-3`
4. [x] - `p1` - **FROM** `depleted` **TO** `active` **WHEN** the active predicate of the algorithm holds again: under `token_bucket`, a refill-on-read restores at least `cost` tokens; under `sliding_window`, enough admitted consumption has left the trailing window for the summed consumption plus `cost` to stay within the sustained rate - `inst-rl-st-cnt-4`
5. [x] - `p1` - **FROM** `depleted` **TO** `depleted` **WHEN** a refill-on-read advances the balance and the acquisition of `cost` tokens still fails, or, under `sliding_window`, the trailing window still cannot admit `cost` - `inst-rl-st-cnt-8`
6. [x] - `p1` - **FROM** `active` **TO** `released` **WHEN** the owning upstream or route is deleted, or its effective limit changes so the stored state no longer matches the configured capacity - `inst-rl-st-cnt-5`
7. [x] - `p1` - **FROM** `depleted` **TO** `released` **WHEN** the owning upstream or route is deleted before the balance recovers - `inst-rl-st-cnt-6`
8. [x] - `p1` - **FROM** `released` **TO** `absent` **WHEN** the in-memory entry is dropped and the next request for the key starts from a fresh bucket - `inst-rl-st-cnt-7`

The `active` and `depleted` predicates are per algorithm: under `token_bucket` a counter is `active` while its balance covers the request `cost` and `depleted` while it does not, and under `sliding_window` a counter is `active` while the trailing-window consumption plus the request `cost` stays within the sustained rate over the effective window and `depleted` while it does not. `burst.capacity` participates only in the `token_bucket` predicates.

## 5. Definitions of Done

### Dual-Rate Configuration Surface

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-rate-limiting-dual-rate-config`

The system **MUST** accept a `rate_limit` block on upstream records and on route records carrying `sharing` (`private` default, `inherit`, `enforce`), `algorithm` (`token_bucket` default, `sliding_window`), `sustained.rate` with `sustained.window` (`second` | `minute` | `hour` | `day`, default `second`), `burst.capacity` defaulting to `sustained.rate`, `scope`, `strategy`, `cost` defaulting to 1, and `response_headers` defaulting to true, matching the shapes of the upstream and route schemas. The system **MUST** also parse, validate, and store `budget.mode` in `unlimited | allocated | shared`, `budget.total`, and `budget.overcommit_ratio` as configuration surface that is not executed, per graded deviation 8, where `budget.total` is an integer of at least 1, `budget.overcommit_ratio` is in the range 1.0 to 2.0 inclusive and defaults to 1.0, and `budget.total` is required whenever `budget.mode` is `allocated` or `shared`. A `sustained.rate`, `burst.capacity`, or `cost` below 1, a `budget.total` below 1, an `overcommit_ratio` outside its range, an omitted `budget.total` while `budget.mode` is `allocated` or `shared`, an unknown enumeration value, and an unknown key outside the exception set `response_headers`, `budget.mode`, `budget.total`, and `budget.overcommit_ratio` **MUST** be rejected with a validation error naming the offending field; those four keys are admitted beyond the canonical schema shape as the documented field-set delta recorded in the graded-configuration boundary, and the schema artifacts are not extended by this record.

**Implements**:
- `cpt-cf-oagw-flow-rate-limiting-config-surface`

**Constraints**: none

**Touches**:
- API: none - the block is carried on the upstream and route payloads owned by entries 2.2 and 2.3
- Entities: `RateLimitConfig`
- Tests: unit tests in a sibling `*_tests.rs` module of the domain layer for every default, every enumeration boundary, and every rejection case, including a `budget.total` below 1, an `overcommit_ratio` below 1.0 and above 2.0, an omitted `budget.total` while `budget.mode` is `allocated` or `shared`, and an accepted `response_headers` and budget field set beyond the canonical schema shape

### Hierarchical min() Inheritance

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-rate-limiting-hierarchical-inheritance`

The system **MUST** compose the effective rate limit for a request as `min(ancestor.enforced, descendant)` over the upstream-level and route-level `rate_limit` configuration and the tenant-chain layers, so a descendant can only be stricter, an ancestor layer with `sharing: private` contributes nothing to a descendant, and an ancestor limit configured with `sharing: enforce` is retained across alias shadowing. The system **MUST NOT** allow alias shadowing to select a routing target that escapes an enforced ancestor budget.

**Implements**:
- `cpt-cf-oagw-algo-rate-limiting-effective-limit`
- `cpt-cf-oagw-flow-rate-limiting-config-surface`

**Constraints**: none

**Touches**:
- API: none
- Entities: `RateLimitConfig`, `Upstream`, `Route`
- Tests: unit tests in a sibling `*_tests.rs` module covering a three-level hierarchy, the `private`, `inherit`, and `enforce` sharing modes, and enforcement across shadowing

### Token Bucket with Refill on Read

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-rate-limiting-token-bucket`

The system **MUST** implement the `token_bucket` algorithm with refill-on-read semantics: the balance is advanced by elapsed time multiplied by the refill rate derived from `sustained.rate` per `sustained.window`, capped at `burst.capacity`, and a request is allowed only when the balance covers its `cost`, which is deducted atomically. A quiescent caller **MUST** be able to spend up to `burst.capacity` immediately and then be limited to the sustained refill rate, and a refused request **MUST NOT** consume any token. The decision instant **MUST** come from an injectable monotonic clock, every elapsed-time computation of the algorithm **MUST** use that clock, and the epoch-second `X-RateLimit-Reset` value **MUST** be derived by a single stated wall-clock mapping that pairs one wall-clock reading with the decision's monotonic reading and adds the monotonic distance to the reset instant, so no elapsed-time arithmetic ever reads the wall clock.

**Implements**:
- `cpt-cf-oagw-algo-rate-limiting-token-bucket`
- `cpt-cf-oagw-flow-rate-limiting-proxy-check`

**Constraints**: none

**Touches**:
- API: none
- Entities: `TokenBucket`
- Tests: unit tests in a sibling `*_tests.rs` module for refill-on-read with an injected monotonic clock, burst up to capacity, sustained-rate exhaustion at an injected instant, weighted `cost` deduction, the no-consumption-on-refuse invariant, and the single wall-clock mapping that renders the monotonic reset instant as the epoch-second `X-RateLimit-Reset`

### Sliding Window Alternative

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-rate-limiting-sliding-window`

The system **MUST** implement the `sliding_window` algorithm as an alternative to the token bucket: consumption admitted inside the trailing `sustained.window` is summed and a request is admitted only when that sum plus its `cost` does not exceed the sustained rate, with no boundary burst. The same counter key, scope, cost, and `min()` inheritance **MUST** apply to both algorithms, so switching `algorithm` changes only the admission shape and not the effective limit.

**Implements**:
- `cpt-cf-oagw-algo-rate-limiting-sliding-window`
- `cpt-cf-oagw-algo-rate-limiting-effective-limit`

**Constraints**: none

**Touches**:
- API: none
- Entities: `TokenBucket`
- Tests: unit tests in a sibling `*_tests.rs` module for window expiry driven by an injected monotonic clock, exact-rate admission, refusal at the boundary with the window-release instant, and the absence of a window-boundary burst

### Counter Scopes and Per-Tenant Isolation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-rate-limiting-counter-scopes`

The system **MUST** key counters by the configured scope over the full enumeration `global`, `tenant`, `user`, `ip`, and `route` - the `route` value being admitted per graded deviation 8 beyond the PRD enumeration - and **MUST** embed the caller's tenant identifier as the leading component of every key whose scope is not `global`, so two tenants never share a counter and one tenant can never consume another tenant's budget. A missing scope discriminator **MUST** fall back to the tenant discriminator rather than merging unrelated callers, and the counters of a deleted upstream or route **MUST** be dropped. The tenant identifier and the authenticated principal in a key **MUST** come only from the authenticated security context owned by the proxy path, `cpt-cf-oagw-feature-request-proxy`; the `ip` discriminator **MUST** be the connection peer address and **MUST NOT** be a client-supplied forwarding header; and no client-supplied header or query component **MUST** contribute to the derived key.

**Implements**:
- `cpt-cf-oagw-flow-rate-limiting-counter-scope`
- `cpt-cf-oagw-algo-rate-limiting-counter-key`
- `cpt-cf-oagw-state-rate-limiting-counter`

**Constraints**: none

This realizes `cpt-cf-oagw-principle-tenant-scope` for the counter space: the scoping of stored configuration records is delivered by `cpt-cf-oagw-feature-gear-foundation`, and the scoping of the in-memory counter space is delivered here.

**Touches**:
- API: none
- Entities: `TokenBucket`
- Tests: unit tests in a sibling `*_tests.rs` module for each of the five scopes, the tenant-prefix invariant across two tenants, the discriminator fallback, counter release on resource deletion, and a derived key that is unchanged when a client-supplied forwarding header or query component varies

### Per-Instance In-Memory State

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-rate-limiting-per-instance-state`

The system **MUST** hold every rate-limit counter in memory inside the `RateLimiterRegistry` owned by the Data Plane instance, per `cpt-cf-oagw-adr-state-management`, with no distributed synchronization, no Redis-backed counter, and no persisted counter state. The `rate_limit_sync` block described by `cpt-cf-oagw-adr-rate-limiting` **MUST NOT** be implemented, and a restart **MUST** start every counter from a fresh bucket, accepting the brief cold-start burst that the PRD risk table records for rate-limit state loss. The registry **MUST** be bounded: it holds at most 10,000 counters, mirroring the 10,000-entry L1 LRU posture recorded in `cpt-cf-oagw-adr-state-management`, and a counter whose key has not been consulted for 15 minutes is evicted, so the key space stays bounded while the set of distinct callers does not; an evicted key is dropped without being logged and without emitting any metric observation. Check-and-deduct on one counter **MUST** be atomic under the registry's per-key lock, so two concurrent acquires of the same key are fully serialized, the total deduction equals the number of successful acquisitions multiplied by `cost`, and the counter is never over-deducted; the key representation on the hot path is one composite key allocation per request.

**Implements**:
- `cpt-cf-oagw-flow-rate-limiting-counter-scope`
- `cpt-cf-oagw-state-rate-limiting-counter`

**Constraints**: none

**Touches**:
- API: none
- Entities: `TokenBucket`
- Tests: unit tests in a sibling `*_tests.rs` module asserting that the registry owns no external dependency, that a recreated registry starts at full capacity, that the registry size stays bounded at 10,000 counters while more distinct peer addresses than the bound are exercised and that a key idle for 15 minutes is evicted without being logged or observed, and that concurrent acquires of one key never over-deduct the counter

### Reject Enforcement with 429 and Retry-After

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-rate-limiting-reject-response`

The system **MUST** answer a refused request with status `429`, a `Retry-After` value of at least one second derived per algorithm from the shortfall and the refill rate under `token_bucket` and from the window-release instant under `sliding_window`, and the `X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset` headers whenever `response_headers` is true. The refused response **MUST** be returned through the proxy response pipeline so the shared `application/problem+json` body, the GTS type `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1`, the `retry_after_seconds` extension field, and the gateway error-source header are rendered by the pipeline owned by entries 2.4 and 2.5, and the upstream **MUST NOT** be called. A refusal **MUST** also be carried into the shared structured audit record with `error_type` set to the rate-limit GTS type, `status` 429, `request_id`, `tenant_id`, and `principal_id`; that record is emitted by the pipeline owned by `cpt-cf-oagw-feature-request-proxy` and `cpt-cf-oagw-feature-observability-and-operability`, with this feature supplying the outcome and the retry guidance.

**Implements**:
- `cpt-cf-oagw-flow-rate-limiting-proxy-check`
- `cpt-cf-oagw-algo-rate-limiting-strategy-dispatch`

**Constraints**: none

**Touches**:
- API: none - the `429` response and the `X-RateLimit-*` headers are added to `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`, which introduces no new endpoint
- Entities: `RateLimitConfig`
- Tests: integration tests in the crate's `tests/` directory asserting the status, the four headers, the problem+json body shape, that no upstream call is issued for a refused request, and that the audit record for the refusal carries the rate-limit `error_type`, `status` 429, `request_id`, `tenant_id`, and `principal_id`

### Reject-Only Execution Boundary

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-rate-limiting-reject-only-execution`

The system **MUST** execute only the `reject` strategy. The `queue` and `degrade` strategy values and the budget-allocation modes `unlimited`, `allocated`, and `shared` with `overcommit_ratio` **MUST** remain legal configuration surface that parses, validates, and stores without error, and **MUST NOT** be executed: a configuration carrying them resolves to the `reject` outcome at enforcement time, so no request is ever queued for later execution and no request is ever served with reduced functionality. This boundary is graded deviation 8 and **MUST** remain recorded here.

**Implements**:
- `cpt-cf-oagw-algo-rate-limiting-strategy-dispatch`
- `cpt-cf-oagw-flow-rate-limiting-config-surface`

**Constraints**: none

**Touches**:
- API: none
- Entities: `RateLimitConfig`
- Tests: unit tests in a sibling `*_tests.rs` module asserting that `queue`, `degrade`, and every budget mode are accepted at the configuration boundary and that each resolves to a `429` refusal at enforcement time

### Enforcement Point on the Proxy Path

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-rate-limiting-enforcement-point`

The system **MUST** own the rate-limit decision and evaluate it on the proxy hot path after the upstream and route are resolved and the effective configuration is merged, and before the upstream call is issued, as a step inside `cpt-cf-oagw-seq-proxy-flow`, which is owned by `cpt-cf-oagw-feature-request-proxy`. The check **MUST** be served from in-memory state with no control-plane call per request, and **MUST NOT** alter the plugin execution order or the request-validation behavior owned by entries 2.4 and 2.6. A request whose resolved configuration carries no `rate_limit` **MUST** proceed with no counter consulted.

**Implements**:
- `cpt-cf-oagw-flow-rate-limiting-proxy-check`
- `cpt-cf-oagw-algo-rate-limiting-effective-limit`

**Constraints**: none

**Touches**:
- API: none - the decision is a step of `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`, which is owned by entry 2.4
- Entities: `RateLimitConfig`, `TokenBucket`
- Tests: integration tests in the crate's `tests/` directory asserting that an unconfigured upstream or route is never rate-limited and that a configured one is refused before any upstream call

### Hot-Path Cost and Registry Bounds

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-rate-limiting-hot-path-cost`

The system **MUST** keep the per-check cost of one rate-limit decision within the less-than-1ms rate-check budget that `cpt-cf-oagw-adr-rate-limiting` records and that the request-routing decision `cpt-cf-oagw-adr-request-routing` serves by keeping the check on the Data Plane, and **MUST** assert that ceiling with a Criterion-based benchmark test that measures the check alone rather than a proxied request. The hot path **MUST** allocate one composite counter key per request and no further key-sized allocation, and the `RateLimiterRegistry` **MUST** stay inside the bound of at most 10,000 counters with a 15-minute idle-key eviction recorded in `cpt-cf-oagw-dod-rate-limiting-per-instance-state`. The end-to-end latency assertion of the proxy path **MUST NOT** be made here: the less-than-10ms p95 budget is owned by `cpt-cf-oagw-feature-request-proxy` under `cpt-cf-oagw-nfr-low-latency`.

**Implements**:
- `cpt-cf-oagw-flow-rate-limiting-proxy-check`
- `cpt-cf-oagw-flow-rate-limiting-counter-scope`

**Constraints**: none

**Touches**:
- API: none - the cost is observable only as a step of `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`, which is owned by entry 2.4
- Entities: `TokenBucket`, `RateLimiterRegistry`
- Tests: a Criterion-based benchmark test asserting that one rate-limit decision with the injected monotonic clock completes under the less-than-1ms ceiling, and unit tests in a sibling `*_tests.rs` module asserting the single composite key allocation per request and the bounded registry

### Payload Limit Interaction

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-rate-limiting-payload-interaction`

The system **MUST** keep the payload-limit outcome and the rate-limit outcome mutually exclusive for a single request whose size is declared by `Content-Length`, which is validated on the proxy path before the rate-limit step: such a request refused on payload size **MUST** be answered `413` without consulting, creating, or consuming any rate-limit counter, so an oversized request never spends another caller's or the same caller's budget. Conversely, a request refused by the rate limiter **MUST** be answered `429` and **MUST NOT** be reclassified as `413` or as any other payload outcome, and **MUST NOT** reach the body-buffering stage. For a chunked or otherwise undeclared-length body the mutual exclusion cannot hold, and the documented deviation applies: the rate-limit decision is taken before buffering, the bytes are consumed, and when the payload limit is exceeded mid-buffer the `413` is emitted and the counter is **NOT** restored, so no refund is issued. The boundary drawn by `cpt-cf-oagw-constraint-body-limit` is acknowledged here; the enforcement of that boundary is owned by `cpt-cf-oagw-feature-request-proxy`.

**Implements**:
- `cpt-cf-oagw-flow-rate-limiting-proxy-check`
- `cpt-cf-oagw-state-rate-limiting-counter`

**Constraints**: none

**Touches**:
- API: none - the interaction is observable on `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `TokenBucket`
- Tests: integration tests in the crate's `tests/` directory asserting that an oversized request with a declared `Content-Length` yields `413` with an unchanged counter balance, that an oversized chunked request yields `413` with the charge retained and no refund, and that a refused request is never answered `413`

### Rate-Limit Metrics Surface

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-rate-limiting-metrics`

The system **MUST** feed the two rate-limit metrics defined by the DESIGN metric table: `oagw_rate_limit_exceeded_total{host, path}` incremented exactly once per refused request, and `oagw_rate_limit_usage_ratio{host, path}` set to the consumed fraction of the served bucket in the range 0.0 to 1.0 after every decision. The `oagw_rate_limit_usage_ratio{host, path}` series is a sample of the most recently decided counter for that `host` and `path`, not a per-counter gauge, because the label set carries no counter identity: it is safe to read as a per-upstream and per-route pressure signal and not as a per-consumer quota. The `host` label **MUST** carry the upstream alias and the `path` label **MUST** carry the normalized route match pattern, never the raw request path, and **MUST NOT** carry a tenant label, per the DESIGN cardinality rules. Emission **MUST** go through the metrics recorder owned by entry 2.9, and an unavailable recorder **MUST NOT** change any allow or refuse decision.

**Implements**:
- `cpt-cf-oagw-flow-rate-limiting-usage-observation`
- `cpt-cf-oagw-algo-rate-limiting-usage-ratio`

**Constraints**: none

**Touches**:
- API: none - the series are exposed at `GET /metrics`, which is owned by entry 2.9
- Entities: `TokenBucket`
- Tests: unit tests in a sibling `*_tests.rs` module for the counter increment and the ratio bounds, and integration tests in the crate's `tests/` directory for the label values, including the normalized route pattern and the absence of a tenant label

### Automated Unit Test Coverage

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-rate-limiting-unit-tests`

The system **MUST** ship unit tests as sibling `*_tests.rs` modules inside the `oagw` crate covering bucket refill, burst behavior up to capacity, weighted `cost` deduction, hierarchical `min()` inheritance across all three sharing modes and across alias shadowing, counter key derivation for every scope in `global | tenant | user | ip | route`, per-tenant counter isolation, sliding-window expiry, the reject-only execution boundary, the payload-limit interaction, the computed quota headers, the budget-allocation rejection cases of `cpt-cf-oagw-dod-rate-limiting-dual-rate-config`, a registry size that stays bounded while more distinct peer addresses than the bound are exercised, concurrent acquires of one key that never over-deduct the counter, and a derived counter key that does not change when a client-supplied header varies, and **MUST NOT** place any test under `testing/e2e/gears/oagw/`, which is out of scope per graded deviation 4.

**Implements**:
- `cpt-cf-oagw-dod-rate-limiting-dual-rate-config`
- `cpt-cf-oagw-dod-rate-limiting-hierarchical-inheritance`
- `cpt-cf-oagw-dod-rate-limiting-token-bucket`
- `cpt-cf-oagw-dod-rate-limiting-sliding-window`
- `cpt-cf-oagw-dod-rate-limiting-counter-scopes`
- `cpt-cf-oagw-dod-rate-limiting-reject-only-execution`

**Constraints**: none

**Touches**:
- API: none
- Entities: `RateLimitConfig`, `TokenBucket`
- Tests: sibling `*_tests.rs` modules of the domain and infrastructure layers covering the configuration surface, both algorithms, inheritance, scope keying, header computation, the budget rejection cases, the bounded registry, and the key provenance

### Automated Integration Test Coverage

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-rate-limiting-integration-tests`

The system **MUST** ship integration-style tests inside the crate's `tests/` directory covering the full proxy-path behavior: a request within budget is forwarded, a request that exhausts the bucket is refused with `429`, `Retry-After`, and the three `X-RateLimit-*` headers, the refused request reaches no upstream, an unconfigured upstream or route is never limited, two tenants do not share a counter under any scoped key, an enforced ancestor limit survives alias shadowing, an oversized request with a declared `Content-Length` is answered `413` without consuming budget, and an oversized chunked request is answered `413` with its charge retained. The suite **MUST NOT** create `testing/e2e/gears/oagw/`.

**Implements**:
- `cpt-cf-oagw-dod-rate-limiting-reject-response`
- `cpt-cf-oagw-dod-rate-limiting-enforcement-point`
- `cpt-cf-oagw-dod-rate-limiting-payload-interaction`
- `cpt-cf-oagw-dod-rate-limiting-metrics`

**Constraints**: none

**Touches**:
- API: enforcement verification - `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `RateLimitConfig`, `TokenBucket`
- Tests: `tests/rate_limiting.rs` and `tests/rate_limiting_isolation.rs`

## 6. Acceptance Criteria

- [ ] **Two criteria remain deliberately unticked** and are not claimed by this run: the Criterion benchmark of `cpt-cf-oagw-dod-rate-limiting-hot-path-cost` (the crate manifest declares no `criterion` dependency, so the less-than-1ms assertion is unmeasured), and the counter release of `cpt-cf-oagw-dod-rate-limiting-counter-scopes` on a management-plane delete (the `RateLimiterRegistry::release_resource` boundary exists and is unit-tested, but the Control Plane delete operations do not invoke it — see `RVW-130` in `REVIEW-FINDINGS.md`).
- [x] A `rate_limit` block on an upstream record and on a route record parses with every documented field, and an omitted `burst.capacity` becomes `sustained.rate`, an omitted `cost` becomes 1, an omitted `scope` becomes `tenant`, an omitted `algorithm` becomes `token_bucket`, an omitted `strategy` becomes `reject`, an omitted `sustained.window` becomes `second`, an omitted `sharing` becomes `private`, and an omitted `response_headers` becomes true (DoD `cpt-cf-oagw-dod-rate-limiting-dual-rate-config`).
- [x] A `sustained.rate`, `burst.capacity`, or `cost` below 1, a `budget.total` below 1, an `overcommit_ratio` outside 1.0 to 2.0, an omitted `budget.total` while `budget.mode` is `allocated` or `shared`, a value outside any enumeration, or an unknown key outside the exception set `response_headers`, `budget.mode`, `budget.total`, and `budget.overcommit_ratio` is rejected with a validation error naming the offending field (DoD `cpt-cf-oagw-dod-rate-limiting-dual-rate-config`).
- [x] A three-layer hierarchy produces the effective limit `min(system, partner, tenant)` for both the sustained rate and the burst capacity, an ancestor layer with `sharing: private` contributes nothing to a descendant, and an ancestor layer with `sharing: enforce` is retained when a descendant shadows the ancestor alias (DoD `cpt-cf-oagw-dod-rate-limiting-hierarchical-inheritance`).
- [x] A quiescent caller spends up to `burst.capacity` immediately and is then limited to the sustained refill rate, tokens refill on read from an injected monotonic clock, a request is refused without consuming any token when the balance is below its `cost`, and a `cost` above 1 deducts its full weight (DoD `cpt-cf-oagw-dod-rate-limiting-token-bucket`).
- [x] Two concurrent acquires of the same key are fully serialized, so the total deduction equals the number of successful acquisitions multiplied by `cost` and the counter is never over-deducted, and the unit test driving concurrent acquires of one key asserts that invariant (DoD `cpt-cf-oagw-dod-rate-limiting-per-instance-state`).
- [x] The `RateLimiterRegistry` holds at most 10,000 counters, a key idle for 15 minutes is evicted without being logged or observed, and the unit test exercising more distinct peer addresses than the bound shows the registry size stays bounded (DoD `cpt-cf-oagw-dod-rate-limiting-per-instance-state`).
- [x] Under `sliding_window` a request is admitted only when the trailing-window consumption plus its `cost` does not exceed the sustained rate, and no burst occurs at a window boundary (DoD `cpt-cf-oagw-dod-rate-limiting-sliding-window`).
- [x] Counters keyed under `global`, `tenant`, `user`, `ip`, and `route` are distinct for the same caller, the caller's tenant identifier leads every key whose scope is not `global`, two tenants never share a counter, and a missing scope discriminator falls back to the tenant discriminator (DoD `cpt-cf-oagw-dod-rate-limiting-counter-scopes`).
- [x] Varying a client-supplied forwarding header or query component does not change the derived counter key, the tenant identifier and the principal in a key come only from the authenticated security context of the proxy path, and the `ip` discriminator is the connection peer address and never a forwarding header (DoD `cpt-cf-oagw-dod-rate-limiting-counter-scopes`).
- [ ] Deleting an upstream or a route releases its counters, and changing an effective limit re-derives the bucket instead of honoring stale state (DoD `cpt-cf-oagw-dod-rate-limiting-counter-scopes`).
- [x] Every counter lives in the Data Plane instance's memory, no Redis-backed or persisted counter exists, and a restart starts every bucket fresh (DoD `cpt-cf-oagw-dod-rate-limiting-per-instance-state`).
- [x] A request that exhausts its bucket is answered `429` with `Retry-After` of at least one second and with `X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset`, rendered as an `application/problem+json` gateway error, and no upstream call is issued for it (DoD `cpt-cf-oagw-dod-rate-limiting-reject-response`).
- [x] A refusal is carried into the shared structured audit record with `error_type` set to the rate-limit GTS type, `status` 429, `request_id`, `tenant_id`, and `principal_id`, emitted by the pipeline owned by `cpt-cf-oagw-feature-request-proxy` and `cpt-cf-oagw-feature-observability-and-operability` with this feature supplying the outcome and the retry guidance (DoD `cpt-cf-oagw-dod-rate-limiting-reject-response`).
- [x] `queue`, `degrade`, and the budget modes `unlimited`, `allocated`, and `shared` with `overcommit_ratio` are accepted at the configuration boundary, and each resolves to the `reject` outcome at enforcement time, so no request is ever queued or degraded (DoD `cpt-cf-oagw-dod-rate-limiting-reject-only-execution`).
- [x] An upstream or route with no `rate_limit` configuration is never rate-limited, and a configured one is evaluated after resolution and before the upstream call (DoD `cpt-cf-oagw-dod-rate-limiting-enforcement-point`).
- [ ] One rate-limit decision on the hot path stays within the less-than-1ms rate-check ceiling under a Criterion-based benchmark, the hot path allocates one composite counter key per request, and the end-to-end less-than-10ms p95 assertion is not made by this feature (DoD `cpt-cf-oagw-dod-rate-limiting-hot-path-cost`).
- [x] A request whose size is declared by `Content-Length` and refused on payload size is answered `413` with its counter balance unchanged and no counter consulted, created, or consumed, an oversized chunked request is answered `413` with its charge retained and no refund, and a rate-limited request is never answered `413` (DoD `cpt-cf-oagw-dod-rate-limiting-payload-interaction`).
- [x] Every refused request increments `oagw_rate_limit_exceeded_total{host, path}` exactly once and every decision sets `oagw_rate_limit_usage_ratio{host, path}` within 0.0 to 1.0, with `host` the upstream alias and `path` the normalized route match pattern, never the raw request path, and no tenant label, and the usage-ratio series is read as a sample of the most recently decided counter for that host and path rather than as a per-consumer quota (DoD `cpt-cf-oagw-dod-rate-limiting-metrics`).
- [x] All tests for this feature live inside the `oagw` crate as sibling `*_tests.rs` modules and files under `tests/`, and no `testing/e2e/gears/oagw/` directory is created (DoD `cpt-cf-oagw-dod-rate-limiting-unit-tests`, `cpt-cf-oagw-dod-rate-limiting-integration-tests`).
