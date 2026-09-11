# Feature: Rate Limiting

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Proxy Request Within the Effective Rate Limit](#proxy-request-within-the-effective-rate-limit)
  - [Rate Limit Exceeded](#rate-limit-exceeded)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Hierarchical Effective Rate-Limit Composition](#hierarchical-effective-rate-limit-composition)
  - [Rate-Limit Counter-Key Selection](#rate-limit-counter-key-selection)
  - [Token-Bucket Consumption and Replenishment](#token-bucket-consumption-and-replenishment)
  - [Rate-Limit Response-Header Computation](#rate-limit-response-header-computation)
- [4. States (CDSL)](#4-states-cdsl)
  - [Token-Bucket Lifecycle](#token-bucket-lifecycle)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Token-Bucket Algorithm and Dual-Rate Configuration](#token-bucket-algorithm-and-dual-rate-configuration)
  - [Position on the Proxy Path and Per-Instance Ownership](#position-on-the-proxy-path-and-per-instance-ownership)
  - [Scope-Based Counter-Key Selection](#scope-based-counter-key-selection)
  - [Hierarchical Effective-Limit Composition](#hierarchical-effective-limit-composition)
  - [429 Rejection Contract and Strategy Handling](#429-rejection-contract-and-strategy-handling)
  - [Concurrency Safety and Accuracy on the Hot Path](#concurrency-safety-and-accuracy-on-the-hot-path)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p2` - **ID**: `cpt-cf-oagw-featstatus-rate-limiting-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-rate-limiting`
## 1. Feature Context

### 1.1 Overview

This feature enforces upstream- and route-level rate limits on the proxy path (`cpt-cf-oagw-feature-proxy-core`'s `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`) using an in-memory, per-Data-Plane-instance token-bucket limiter with dual-rate (sustained + burst) configuration, a `min()` hierarchical composition of the Upstream/Route/tenant-ancestor chain, and the documented `429`/`X-RateLimit-*`/`Retry-After` response contract.

### 1.2 Purpose

`cpt-cf-oagw-fr-rate-limiting` requires the system to enforce rate limits at upstream and route levels with rate/window/capacity/cost/scope/strategy configuration. `cpt-cf-oagw-usecase-rate-limit-exceeded` requires that a caller who exceeds the configured rate receives a response handled per the configured strategy. This feature is the enforcement half of that requirement: `cpt-cf-oagw-feature-upstream-management` (2.2) and `cpt-cf-oagw-feature-route-management` (2.3) already persist the `rate_limit` field's sharing modes and threshold fields; this feature reads that persisted, resolved configuration — after `cpt-cf-oagw-feature-proxy-core` has already located the Upstream/Route and merged the tenant hierarchy — and turns it into an actual accept/reject decision on every proxied request.

**Requirements Covered**:

- [ ] `p2` - `cpt-cf-oagw-fr-rate-limiting`
- [ ] `p2` - `cpt-cf-oagw-usecase-rate-limit-exceeded`

Citation priority note: `cpt-cf-oagw-fr-rate-limiting` is cited above at `p2`, following `DECOMPOSITION.md` §2.8's own marker for this citation, even though `PRD.md` defines the identifier itself at `p1`. This file follows the DECOMPOSITION round's markers rather than the PRD's original priorities; the drift originates upstream and is not corrected here.

This feature's evaluation sits on the hot path `cpt-cf-oagw-nfr-low-latency` governs (<10ms added overhead at p95); `cpt-cf-oagw-nfr-low-latency` itself is `cpt-cf-oagw-feature-proxy-core`'s Requirements Covered entry, not this feature's, but §3/§5 below state the concurrency-safety and accuracy expectations this feature's evaluation must meet to stay inside that budget.

At the shared post-resolution policy hook `cpt-cf-oagw-feature-proxy-core` exposes, this feature's rate-limit evaluation runs after `cpt-cf-oagw-feature-cors-handling`'s origin/method validation and before `cpt-cf-oagw-feature-plugin-execution`'s Auth/Guard/Transform chain (CORS -> rate limiting -> plugin chain -> forward). Consequently a request that is simultaneously CORS-invalid and over its rate-limit budget receives CORS's `403`, not this feature's `429` — this feature's bucket is never consumed for such a request.

**Design Principles Covered**: None (`DECOMPOSITION.md` §2.8 lists none for this entry)

**Design Constraints Covered**: None (`DECOMPOSITION.md` §2.8 lists none for this entry)

**Design Components**:

- `cpt-cf-oagw-component-model`
- `cpt-cf-oagw-adr-rate-limiting`
- `cpt-cf-oagw-adr-state-management`

**Path-prefix override carried into this feature** (per `DECOMPOSITION.md` §1 Overview correction 1): the proxy path this feature layers onto is gear-relative, `/oagw/v1/proxy/{alias}[/{path_suffix}]`, with no leading `/api` segment.

**Explicit out-of-scope carried from `DECOMPOSITION.md` §2.8**: cross-instance synchronization of rate-limit counters via Redis (ADR-0003's "Hybrid Local + Periodic Sync" distribution mode) is deferred to a future iteration; this feature delivers only the local-only, per-Data-Plane-instance mode ADR-0003 describes as the MVP starting point, per `cpt-cf-oagw-adr-state-management`'s DP-owns-rate-limiters decision. A multi-instance deployment therefore enforces the configured limit independently on each instance rather than against one shared global counter. The upstream/route CRUD endpoints that persist the `rate_limit` field (2.2/2.3) and rate-limit-specific Prometheus metrics beyond proxy-core's base observability hooks are likewise out of scope for this feature.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Sends proxy requests subject to the effective rate limit; receives the `429`/`X-RateLimit-*`/`Retry-After` response contract when the configured limit is exceeded, and receives `X-RateLimit-*` headers on every response while rate limiting is active for the resource it is calling. |
| `cpt-cf-oagw-actor-tenant-admin` | Configures the `rate_limit.sharing`/`sustained`/`burst`/`scope`/`strategy`/`cost` values this feature reads and enforces (the CRUD write path that persists those values belongs to `cpt-cf-oagw-feature-upstream-management`/`cpt-cf-oagw-feature-route-management`, not this feature). |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **ADRs**: [ADR/0003-rate-limiting.md](../ADR/0003-rate-limiting.md), [ADR/0006-state-management.md](../ADR/0006-state-management.md)
- **Dependencies**: `cpt-cf-oagw-feature-proxy-core` (rate-limit evaluation needs the resolved Upstream/Route/tenant context and hierarchical configuration merge that proxy-core produces before this feature can select a counter scope or compute an effective limit; per `DECOMPOSITION.md` §3 Feature Dependencies)

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-oagw-usecase-rate-limit-exceeded` (the exceeded flow below); the within-limit flow below has no dedicated PRD use-case identifier of its own — it is the rate-limited slice of `cpt-cf-oagw-usecase-proxy-request`, the general proxy use case `cpt-cf-oagw-feature-proxy-core` (2.5) owns.

### Proxy Request Within the Effective Rate Limit

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-ratelimit-within-limit`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The app developer's request resolves to an Upstream/Route whose effective rate-limit bucket still has enough tokens for the request's `cost`; the request is forwarded and the response carries `X-RateLimit-Limit`/`X-RateLimit-Remaining`/`X-RateLimit-Reset` reflecting the post-consumption state.
- The resolved Upstream/Route chain declares no `rate_limit` at all; the request is forwarded with no rate-limit headers added.

**Error Scenarios**: None — exhaustion is covered by `cpt-cf-oagw-flow-ratelimit-exceeded`.

**Steps**:
1. [x] - `p1` - App developer sends `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`, already resolved by `cpt-cf-oagw-feature-proxy-core` to a specific Upstream, Route, and tenant, with hierarchical configuration merge complete - `inst-ratelimit-within-limit-01`
2. [x] - `p1` - System computes the effective `rate_limit` configuration via `cpt-cf-oagw-algo-ratelimit-effective-limit` - `inst-ratelimit-within-limit-02`
3. [x] - `p1` - **IF** the composition yields "no rate limit configured" - `inst-ratelimit-within-limit-03`
   1. [x] - `p1` - Skip rate-limit evaluation entirely and proceed directly to `cpt-cf-oagw-feature-proxy-core`'s forwarding step, with no `X-RateLimit-*` headers added - `inst-ratelimit-within-limit-04`
4. [x] - `p1` - **ELSE** - `inst-ratelimit-within-limit-05`
   1. [x] - `p1` - System selects the counter key via `cpt-cf-oagw-algo-ratelimit-scope-key` - `inst-ratelimit-within-limit-06`
   2. [x] - `p1` - System evaluates `cpt-cf-oagw-algo-ratelimit-consume` for that key with the request's `cost` tokens - `inst-ratelimit-within-limit-07`
   3. [x] - `p1` - **IF** the decision is ALLOW - `inst-ratelimit-within-limit-08`
      1. [x] - `p1` - System computes the response headers via `cpt-cf-oagw-algo-ratelimit-headers` and forwards the request to the upstream through `cpt-cf-oagw-feature-proxy-core`'s forwarding step, attaching those headers to the returned response - `inst-ratelimit-within-limit-09`
5. [x] - `p1` - **RETURN** the upstream's response (or the gateway's own response, if no rate limit is configured), carrying `X-RateLimit-*` headers whenever step 4 evaluated a bucket - `inst-ratelimit-within-limit-10`

### Rate Limit Exceeded

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-ratelimit-exceeded`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Preconditions** (per `cpt-cf-oagw-usecase-rate-limit-exceeded`): a `rate_limit` is configured on the resolved upstream or route, and the caller has exhausted the effective bucket for the selected counter key.

**Success Scenarios**:
- `strategy: reject` — the request is rejected with `429` and the documented headers.
- `strategy: queue` — the request is held until a token frees up (within bounded queue capacity) and then admitted.
- `strategy: degrade` — the request is forwarded anyway, marked with `X-RateLimit-Remaining: 0`.

**Error Scenarios**:
- `strategy: queue` with the bounded queue already full — treated identically to `strategy: reject`.

**Steps**:
1. [x] - `p1` - App developer sends a proxy request that resolves to an Upstream/Route whose effective `rate_limit` bucket, for the counter key selected by `cpt-cf-oagw-algo-ratelimit-scope-key`, is already exhausted - `inst-ratelimit-exceeded-01`
2. [x] - `p1` - System evaluates `cpt-cf-oagw-algo-ratelimit-consume`, yielding DENY - `inst-ratelimit-exceeded-02`
3. [x] - `p1` - **IF** the effective `strategy` is `reject` - `inst-ratelimit-exceeded-03`
   1. [x] - `p1` - System computes headers via `cpt-cf-oagw-algo-ratelimit-headers` (`X-RateLimit-Limit`, `X-RateLimit-Remaining: 0`, `X-RateLimit-Reset`, `Retry-After`) - `inst-ratelimit-exceeded-04`
   2. [x] - `p1` - System returns `429` rendered through `cpt-cf-oagw-feature-gear-foundation`'s RFC 9457 envelope with GTS `type` `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1` and `X-OAGW-Error-Source: gateway`, carrying the headers from the previous step; the request is never forwarded to the upstream - `inst-ratelimit-exceeded-05`
4. [x] - `p1` - **ELSE IF** the effective `strategy` is `queue` - `inst-ratelimit-exceeded-06`
   1. [x] - `p1` - **IF** the per-key bounded queue has spare capacity - `inst-ratelimit-exceeded-07`
      1. [x] - `p1` - Hold the request until either a token becomes available (re-evaluate `cpt-cf-oagw-algo-ratelimit-consume`, which now yields ALLOW) or the documented bounded wait elapses - `inst-ratelimit-exceeded-08`
   2. [x] - `p1` - **ELSE** (the bounded queue is already full) - `inst-ratelimit-exceeded-09`
      1. [x] - `p1` - Apply the same `429` contract as step 3 - `inst-ratelimit-exceeded-10`
5. [x] - `p1` - **ELSE** (the effective `strategy` is `degrade`) - `inst-ratelimit-exceeded-11`
   1. [x] - `p1` - Forward the request to the upstream despite exhaustion, attaching `X-RateLimit-Remaining: 0` and the other `X-RateLimit-*` headers to the forwarded response as the reduced-functionality marker, without consuming further tokens beyond what is already exhausted - `inst-ratelimit-exceeded-12`
6. [x] - `p1` - **RETURN** the response produced by whichever branch above was taken - `inst-ratelimit-exceeded-13`

## 3. Processes / Business Logic (CDSL)

### Hierarchical Effective Rate-Limit Composition

- [x] `p1` - **ID**: `cpt-cf-oagw-algo-ratelimit-effective-limit`

**Input**: the resolved Upstream's `rate_limit` (the Upstream selected by `cpt-cf-oagw-feature-proxy-core`'s alias-shadowing walk) and the resolved Route's `rate_limit` (the matched Route under that Upstream), each schema-conformant per the `rate_limit` definition shared by `upstream.v1.schema.json`/`route.v1.schema.json` (`sharing`, `algorithm`, `sustained.rate`/`sustained.window`, `burst.capacity`, `scope`, `strategy`, `cost`); plus, for every ancestor tenant on the alias-shadowing chain between the resolved tenant and the root whose own Upstream/Route (for the same alias/route match) declares a `rate_limit` with `sharing: enforce`, that ancestor's `sustained`/`burst` values — per `cpt-cf-oagw-feature-proxy-core`'s shadowing rule that "ancestor constraints configured with `sharing: enforce` remain active after shadowing."

**Output**: one effective `rate_limit` (`algorithm`, `sustained.rate`, `sustained.window`, `burst.capacity`, `scope`, `strategy`, `cost`) to enforce for this request, or the sentinel "no rate limit configured" when neither the Upstream, the Route, nor any enforcing ancestor declares one.

**Steps**:
1. [x] - `p1` - **IF** neither the resolved Upstream nor the resolved Route declares a `rate_limit` object - `inst-ratelimit-effective-limit-01`
   1. [x] - `p1` - **RETURN** "no rate limit configured" - `inst-ratelimit-effective-limit-02`
2. [x] - `p1` - **ELSE** compose the Upstream-versus-Route pair, using the Upstream's `rate_limit.sharing` as the parent's sharing mode and the Route's `rate_limit` (if any) as the child - `inst-ratelimit-effective-limit-03`
   1. [x] - `p1` - **IF** the Upstream declares no `rate_limit`, or its `sharing` is `private` - `inst-ratelimit-effective-limit-04`
      1. [x] - `p1` - The composed pair is the Route's `rate_limit` exactly (the Upstream does not constrain it); if the Route also declares none, treat this pair as "no rate limit configured" - `inst-ratelimit-effective-limit-05`
   2. [x] - `p1` - **ELSE IF** the Upstream's `sharing` is `inherit` **AND** the Route declares no `rate_limit` of its own - `inst-ratelimit-effective-limit-06`
      1. [x] - `p1` - The composed pair is the Upstream's `rate_limit` exactly - `inst-ratelimit-effective-limit-07`
   3. [x] - `p1` - **ELSE** (the Upstream's `sharing` is `inherit` and the Route specifies its own `rate_limit`, or the Upstream's `sharing` is `enforce` regardless of whether the Route specifies one) - `inst-ratelimit-effective-limit-08`
      1. [x] - `p1` - Normalize both levels' `sustained.rate` to a common per-second rate (`rate / seconds_in(window)`), since `sustained.window` may differ between the two levels - `inst-ratelimit-effective-limit-09`
      2. [x] - `p1` - The composed pair's `sustained` is `min(upstream_rate_per_second, route_rate_per_second)`, and its `burst.capacity` is `min(upstream.burst.capacity, route.burst.capacity)` (each defaulting to that level's own `sustained.rate` first, per the schema's documented default, when that level omits `burst.capacity`); when only one level specifies a `rate_limit` in this branch, treat the missing level as a neutral (non-constraining) value so the result equals the specified level's own value - `inst-ratelimit-effective-limit-10`
      3. [x] - `p1` - The composed pair's `algorithm`, `scope`, `strategy`, `cost` are taken from the Route's `rate_limit` when the Route declares one, else the Upstream's — these fields are categorical selections, not budgets, so they are taken from the single more-specific level rather than min-composed - `inst-ratelimit-effective-limit-11`
3. [x] - `p1` - **FOR EACH** ancestor tenant from the resolved tenant's immediate parent up to the root, in that order - `inst-ratelimit-effective-limit-12`
   1. [x] - `p1` - **IF** that ancestor's own Upstream or Route (for the same alias/route match) declares a `rate_limit` with `sharing: enforce` - `inst-ratelimit-effective-limit-13`
      1. [x] - `p1` - Normalize its `sustained.rate` to per-second and fold it into the running composition: running `sustained` = `min(running_sustained_per_second, ancestor_sustained_per_second)`; running `burst.capacity` = `min(running_burst, ancestor_burst)` - `inst-ratelimit-effective-limit-14`
4. [x] - `p1` - Convert the final per-second `sustained` value back to a whole-number `rate` in the chosen output `window` unit, rounding down so the effective limit is never looser than any contributing level - `inst-ratelimit-effective-limit-15`
5. [x] - `p1` - **RETURN** the fully composed effective `rate_limit`, or the "no rate limit configured" sentinel from step 1 - `inst-ratelimit-effective-limit-16`

### Rate-Limit Counter-Key Selection

- [x] `p1` - **ID**: `cpt-cf-oagw-algo-ratelimit-scope-key`

**Input**: the effective `rate_limit.scope` from `cpt-cf-oagw-algo-ratelimit-effective-limit`, plus the request context `cpt-cf-oagw-feature-proxy-core` has already resolved: the identifying resource (the Route if it was the most-specific contributor per `cpt-cf-oagw-algo-ratelimit-effective-limit` step 2.3, else the Upstream), the resolved `tenant_id`, the caller's `principal_id` (from the SecurityContext extracted at the start of the proxy flow, before Data-Plane processing, per `cpt-cf-oagw-seq-proxy-flow`), and the caller's originating IP address.

**Output**: a counter key identifying exactly one token bucket, in the same `resource_type`/`resource_id`/`scope`/`scope_id` shape ADR-0003's Redis key structure documents (this feature reuses that key composition for its local, non-Redis bucket lookup; the Redis half itself is out of scope, see §1.2).

**Steps**:
1. [x] - `p1` - Set `resource_type`/`resource_id` to the Route and its id if the Route was the most-specific contributor, else the Upstream and its id - `inst-ratelimit-scope-key-01`
2. [x] - `p1` - **IF** `scope` is `global` - `inst-ratelimit-scope-key-02`
   1. [x] - `p1` - The key is `(resource_type, resource_id)` alone — one bucket shared by every caller of that resource - `inst-ratelimit-scope-key-03`
3. [x] - `p1` - **ELSE IF** `scope` is `tenant` - `inst-ratelimit-scope-key-04`
   1. [x] - `p1` - The key is `(resource_type, resource_id, tenant_id)` — one bucket per tenant calling that resource - `inst-ratelimit-scope-key-05`
4. [x] - `p1` - **ELSE IF** `scope` is `user` - `inst-ratelimit-scope-key-06`
   1. [x] - `p1` - The key is `(resource_type, resource_id, principal_id)` — one bucket per authenticated caller identity calling that resource - `inst-ratelimit-scope-key-07`
5. [x] - `p1` - **ELSE IF** `scope` is `ip` - `inst-ratelimit-scope-key-08`
   1. [x] - `p1` - The key is `(resource_type, resource_id, client_ip)` — one bucket per originating IP address calling that resource - `inst-ratelimit-scope-key-09`
6. [x] - `p1` - **ELSE** (`scope` is `route`) - `inst-ratelimit-scope-key-10`
   1. [x] - `p1` - The key is `(resource_type="route", resource_id=matched_route_id)` alone, collapsing tenant/user/ip into a single bucket for that route regardless of which resource contributed the effective configuration - `inst-ratelimit-scope-key-11`
7. [x] - `p1` - **RETURN** the composed key - `inst-ratelimit-scope-key-12`

### Token-Bucket Consumption and Replenishment

- [x] `p1` - **ID**: `cpt-cf-oagw-algo-ratelimit-consume`

**Input**: the counter key from `cpt-cf-oagw-algo-ratelimit-scope-key`; the effective `sustained.rate`/`sustained.window`/`burst.capacity`/`cost` from `cpt-cf-oagw-algo-ratelimit-effective-limit`; the bucket's current state (`tokens`, `last_replenished_at`) per `cpt-cf-oagw-state-ratelimit-bucket`, or "no bucket yet" for a key seen for the first time.

**Output**: an ALLOW or DENY decision, the bucket's updated state, the remaining token count, and a reset timestamp.

**Steps**:
1. [x] - `p1` - **IF** no bucket exists yet for this key - `inst-ratelimit-consume-01`
   1. [x] - `p1` - Create it, seeded to `tokens = burst.capacity`, `last_replenished_at = now` (`cpt-cf-oagw-state-ratelimit-bucket` transition Created→HasTokens) - `inst-ratelimit-consume-02`
2. [x] - `p1` - Compute `elapsed = now - last_replenished_at` - `inst-ratelimit-consume-03`
3. [x] - `p1` - Compute the per-second replenishment rate as `sustained.rate / seconds_in(sustained.window)`, and add `elapsed * rate_per_second` tokens to `tokens`, capped at `burst.capacity` - `inst-ratelimit-consume-04`
4. [x] - `p1` - Set `last_replenished_at = now` - `inst-ratelimit-consume-05`
5. [x] - `p1` - **IF** `tokens >= cost` - `inst-ratelimit-consume-06`
   1. [x] - `p1` - Subtract `cost` from `tokens`; decision = ALLOW - `inst-ratelimit-consume-07`
6. [x] - `p1` - **ELSE** - `inst-ratelimit-consume-08`
   1. [x] - `p1` - Leave `tokens` unchanged (`cost` is not consumed on denial); decision = DENY - `inst-ratelimit-consume-09`
7. [x] - `p1` - Compute `remaining = floor(tokens)` - `inst-ratelimit-consume-10`
8. [x] - `p1` - Compute `reset` as the earliest future time at which `tokens` will again be at least `1` (`now` if `tokens >= 1` already, else `now + (1 - tokens) / rate_per_second`) - `inst-ratelimit-consume-11`
9. [x] - `p1` - Persist the updated bucket state for this key - `inst-ratelimit-consume-12`
10. [x] - `p1` - **RETURN** decision, `remaining`, `reset` - `inst-ratelimit-consume-13`

Steps 1–9 MUST execute as a single atomic unit per key under concurrent evaluation from multiple in-flight requests on the same Data-Plane instance (see `cpt-cf-oagw-dod-ratelimit-concurrency`); no specific data structure or locking primitive is prescribed.

### Rate-Limit Response-Header Computation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-ratelimit-headers`

**Input**: the decision, `remaining`, and `reset` from `cpt-cf-oagw-algo-ratelimit-consume`; the effective `sustained.rate`; the effective `strategy`.

**Output**: the header set to attach to the response ultimately returned to the caller.

**Steps**:
1. [x] - `p1` - Set `X-RateLimit-Limit` to the effective `sustained.rate` - `inst-ratelimit-headers-01`
2. [x] - `p1` - Set `X-RateLimit-Remaining` to `remaining` - `inst-ratelimit-headers-02`
3. [x] - `p1` - Set `X-RateLimit-Reset` to `reset`, expressed as a Unix timestamp per ADR-0003's `More Information` example - `inst-ratelimit-headers-03`
4. [x] - `p1` - **IF** the decision is DENY **AND** the effective `strategy` is `reject` - `inst-ratelimit-headers-04`
   1. [x] - `p1` - Additionally set `Retry-After` to `ceil(reset - now)` seconds - `inst-ratelimit-headers-05`
5. [x] - `p1` - **RETURN** the header set: `X-RateLimit-Limit`/`X-RateLimit-Remaining`/`X-RateLimit-Reset` are attached to every response for which a bucket was evaluated, ALLOW or DENY alike; `Retry-After` is attached only to a `strategy: reject` denial's `429` response - `inst-ratelimit-headers-06`

## 4. States (CDSL)

Of the three Domain Model Entities this feature owns (Token bucket, Rate-limit scope, Rate-limit counter), only the Token bucket has a genuine lifecycle; "scope" is a categorical selection with no state of its own, and "counter" is the bucket's own key/value pair, modeled here alongside it.

### Token-Bucket Lifecycle

- [x] `p2` - **ID**: `cpt-cf-oagw-state-ratelimit-bucket`

**States**: Created, HasTokens, Exhausted

**Initial State**: Created

**Transitions**:
1. [x] - `p1` - **FROM** Created **TO** HasTokens **WHEN** the bucket for a counter key is first seeded, at `tokens = burst.capacity` - `inst-state-ratelimit-bucket-01`
2. [x] - `p1` - **FROM** HasTokens **TO** Exhausted **WHEN** a consumption (`cpt-cf-oagw-algo-ratelimit-consume`) leaves `tokens < 1` - `inst-state-ratelimit-bucket-02`
3. [x] - `p1` - **FROM** Exhausted **TO** HasTokens **WHEN** replenishment (elapsed time × `sustained.rate`/`sustained.window`) brings `tokens` back to `>= 1` - `inst-state-ratelimit-bucket-03`
4. [x] - `p1` - **FROM** HasTokens **TO** HasTokens **WHEN** a consumption leaves `tokens >= 1` (the bucket remains usable) - `inst-state-ratelimit-bucket-04`

`Exhausted` is not terminal: every bucket returns to `HasTokens` once enough time elapses, since `sustained.rate` is always `>= 1` per the schema's `minimum: 1` constraint on that field. A bucket in `Exhausted` still accepts evaluation (yielding DENY, or ALLOW once replenished) without erroring.

## 5. Definitions of Done

**Security, reliability, data-integrity, observability, and rollback**: Security — rate-limit evaluation reads only request/tenant/routing context `cpt-cf-oagw-feature-proxy-core` has already resolved; it introduces no new credential or secret handling (that remains `cpt-cf-oagw-feature-plugin-execution`'s concern), and its `429` body follows the same RFC 9457 envelope as every other gateway error, so no rate-limit-specific path can leak upstream response details. Reliability — this feature never opens a network connection or blocks on an external dependency to reach its allow/deny decision (per ADR-0003's local-only distribution mode for this round), so the check itself cannot become an availability risk; a first-seen counter key is corrected by seeding a fresh bucket (`cpt-cf-oagw-state-ratelimit-bucket` Created→HasTokens), never by failing the request. Data integrity — `cpt-cf-oagw-algo-ratelimit-consume`'s read-modify-write MUST be atomic per key so concurrent requests against the same counter never observe or produce a torn token count; this is the only data-integrity property this feature owns, since it persists no durable record of its own. Observability — `X-RateLimit-Limit`/`Remaining`/`Reset` on every evaluated response and `Retry-After` on every `429` give the caller direct, per-request visibility into remaining budget; the `oagw_rate_limit_exceeded_total{host, path}` counter and `oagw_rate_limit_usage_ratio{host, path}` gauge `DESIGN.md` §4.2 already documents are proxy-core's base observability hooks and are explicitly out of scope for new work here (`DECOMPOSITION.md` §2.8 Out of scope). `429` rejections are also captured by the base audit-log scaffold `cpt-cf-oagw-feature-gear-foundation` defines (its `status` and `duration_ms` fields record the rejection and its timing); this feature adds no rate-limit-specific audit field of its own — `X-RateLimit-*`/`Retry-After` on the response itself remain the rate-limit-specific signal. Rollback — not applicable: this feature introduces no database schema, migration, or persisted state; its in-memory bucket state is per-instance and is discarded on process restart with no rollback procedure required.

**UX/Accessibility**: Not applicable because this feature has no user interface — it is a server-side request-handling behavior observed only through HTTP response headers and status codes.

**Compliance/Data Privacy**: Not applicable because this feature stores no personal or regulated data. The `user`/`ip` counter-key scopes transiently reference the caller's `principal_id` or originating IP address, both already extracted by proxy-core's SecurityContext/request handling; this feature holds them only as the in-memory token-bucket key, discarded on process restart, and never writes either to a durable record of its own.

### Token-Bucket Algorithm and Dual-Rate Configuration

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ratelimit-token-bucket`

The system **MUST** enforce rate limits using an in-memory token-bucket algorithm keyed off the schema's `rate_limit` object (`upstream.v1.schema.json`/`route.v1.schema.json` `rate_limit` definition): tokens replenish at `sustained.rate` per `sustained.window` (`second`|`minute`|`hour`|`day`), the bucket's maximum size is `burst.capacity` (defaulting to `sustained.rate` when the upstream/route omits it, per the schema description), and each admitted request consumes `cost` tokens (default `1`). `algorithm: sliding_window` MUST be accepted as a legal persisted value — its persistence is `cpt-cf-oagw-feature-upstream-management`/`cpt-cf-oagw-feature-route-management`'s concern — but this feature applies token-bucket accounting uniformly regardless of the configured `algorithm` value: `DECOMPOSITION.md` §2.8's Domain Model Entities list only "Token bucket," no sliding-window-specific entity or component is documented for this round, and ADR-0003 frames token bucket as the chosen default with sliding window as an alternative it does not describe an implementation for. This feature MUST NOT silently claim sliding-window accuracy guarantees (no boundary-burst) while applying token-bucket mechanics.

**Implements**:
- `cpt-cf-oagw-algo-ratelimit-consume`
- `cpt-cf-oagw-state-ratelimit-bucket`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: Token bucket

### Position on the Proxy Path and Per-Instance Ownership

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ratelimit-position`

The system **MUST** evaluate the effective rate limit after `cpt-cf-oagw-feature-proxy-core` completes alias/route resolution and hierarchical configuration merge (so the effective `rate_limit` configuration is fully known) and before the request is forwarded to the upstream. Rate-limit counters and bucket state MUST be owned per Data-Plane instance, in-process, consistent with `cpt-cf-oagw-adr-state-management`'s decision that per-instance token buckets are owned by the Data Plane because it already has full request context. No cross-instance synchronization is implemented in this round: `DECOMPOSITION.md` §2.8 Out of scope explicitly defers ADR-0003's "Hybrid Local + Periodic Sync" Redis distribution mode to a future iteration, so a multi-instance deployment enforces the configured limit independently and identically on each instance rather than sharing one global counter.

**Implements**:
- `cpt-cf-oagw-flow-ratelimit-within-limit`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: Rate-limit counter

### Scope-Based Counter-Key Selection

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ratelimit-scope-selection`

The system **MUST** select the rate-limit counter key from the effective `rate_limit.scope` value as follows:

| `scope` | Counter subject |
|---|---|
| `global` | The resource (Upstream or Route) that contributed the effective configuration — one bucket shared by all callers |
| `tenant` (schema default) | The resolved `tenant_id` — one bucket per tenant calling that resource |
| `user` | The caller's authenticated `principal_id` from the SecurityContext extracted at the start of the proxy flow — one bucket per calling identity |
| `ip` | The caller's originating IP address — one bucket per source IP |
| `route` | The matched route's identifier alone — one bucket per route, independent of tenant/user/ip |

**Implements**:
- `cpt-cf-oagw-algo-ratelimit-scope-key`

**Touches**:
- Entities: Rate-limit scope, Rate-limit counter

### Hierarchical Effective-Limit Composition

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ratelimit-hierarchical-budget`

The system **MUST** compute the effective `sustained`/`burst` values as the documented `min()` composition across the tenant chain: `effective_rate = min(selected_upstream_rate, route_rate, all_ancestor_enforced_rates)`, normalizing every contributing level's `sustained.rate` to a common per-second basis before comparison (since `sustained.window` may differ per level) and taking the plain `min()` of `burst.capacity` values directly. The Upstream-versus-Route composition MUST follow the same `sharing`-mode inheritance table used for the tenant-ancestor chain, applied with the Upstream as parent and the Route as child:

| Parent (`Upstream.rate_limit.sharing`) | Child (Route) specifies | Effective limit |
|---|---|---|
| `private` | any (or none) | Child's limit only |
| `inherit` | none | Parent's (Upstream's) limit |
| `inherit` | own limit | `min(parent, child)` |
| `enforce` | any (or none) | `min(parent, child)` — cannot exceed parent |

The categorical fields `algorithm`/`scope`/`strategy`/`cost` are taken from the single most-specific level that declares a `rate_limit` (Route if present, else Upstream) rather than min-composed, since they are selections, not budgets. `budget.mode`/`budget.total`/`budget.overcommit_ratio` and `response_headers`, described in ADR-0003's narrative, are **not** present in the frozen `upstream.v1.schema.json`/`route.v1.schema.json` `rate_limit` definitions (both declare `additionalProperties: false` over exactly `sharing`/`algorithm`/`sustained`/`burst`/`scope`/`strategy`/`cost`); this feature therefore does NOT implement allocated/shared budget modes or overcommit-ratio validation, and does NOT gate `X-RateLimit-*` header emission behind a `response_headers` toggle — it implements only the `min()` composition and the always-on header contract the frozen schemas and `DECOMPOSITION.md` §2.8's Scope actually specify.

**Implements**:
- `cpt-cf-oagw-algo-ratelimit-effective-limit`

**Touches**:
- Entities: Token bucket, Rate-limit scope

### 429 Rejection Contract and Strategy Handling

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ratelimit-rejection-contract`

The system **MUST**, when the effective `strategy` is `reject` (the schema default) and the token bucket denies a request, return `429` with GTS error identifier `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1` (the `RateLimitExceeded` row `cpt-cf-oagw-feature-gear-foundation`'s error catalog already establishes) rendered through that feature's RFC 9457 envelope with `X-OAGW-Error-Source: gateway`, carrying `Retry-After` and `X-RateLimit-Limit`/`X-RateLimit-Remaining`/`X-RateLimit-Reset` response headers, and MUST NOT forward the request to the upstream. `X-RateLimit-Limit`/`Remaining`/`Reset` (but not `Retry-After`) MUST also be attached to every allowed response for which rate limiting was evaluated, per `cpt-cf-oagw-algo-ratelimit-headers`.

`strategy: queue` and `strategy: degrade` MUST be accepted as configuration values and delivered at the basic level `DECOMPOSITION.md` §2.8 Scope and the PRD's alternative flows describe:
- `queue`: a request arriving when tokens are exhausted is held in a bounded per-key queue until either a token becomes available (the request is then admitted) or the queue is already at its documented bounded capacity, in which case the request is rejected with the same `429` contract as `strategy: reject`.
- `degrade`: a request arriving when tokens are exhausted is still forwarded to the upstream (never blocked), with the response carrying `X-RateLimit-Remaining: 0` and the other `X-RateLimit-*` headers as the reduced-functionality marker the PRD's alternative flow describes, without consuming additional tokens beyond what is already exhausted.

**Implements**:
- `cpt-cf-oagw-flow-ratelimit-exceeded`
- `cpt-cf-oagw-algo-ratelimit-headers`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: Rate-limit counter

### Concurrency Safety and Accuracy on the Hot Path

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ratelimit-concurrency`

The system **MUST** make the token-bucket read-modify-write of `cpt-cf-oagw-algo-ratelimit-consume` safe under concurrent evaluation from multiple in-flight requests against the same counter key on the same Data-Plane instance, without a torn read/write ever causing over-admission beyond `burst.capacity` for that key. This check sits on the hot path `cpt-cf-oagw-nfr-low-latency` governs (<10ms added overhead at p95, excluding upstream response time); the concurrency mechanism MUST NOT itself become a source of contention that violates that budget. No specific data structure or locking primitive is prescribed by this feature.

**Implements**:
- `cpt-cf-oagw-algo-ratelimit-consume`

**Touches**:
- Entities: Token bucket, Rate-limit counter

## 6. Acceptance Criteria

- [ ] A sequence of requests against a resource whose effective `sustained.rate`/`burst.capacity` has not yet been exhausted are each forwarded to the upstream, each carrying `X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset` headers, with `X-RateLimit-Remaining` decreasing by `cost` per request.
- [ ] The first request that arrives after the effective bucket is exhausted (`strategy: reject`) is rejected with `429`, a body `type` of `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1`, `X-OAGW-Error-Source: gateway`, and `Retry-After`/`X-RateLimit-Limit`/`X-RateLimit-Remaining: 0`/`X-RateLimit-Reset` headers, and is never forwarded to the upstream.
- [ ] After waiting at least one full `sustained.window` beyond the point of exhaustion, the same counter key admits at least one further request, demonstrating token replenishment.
- [ ] A burst of requests up to `burst.capacity`, issued against a freshly-seeded bucket faster than `sustained.window` would otherwise allow, all succeed, demonstrating burst capacity permitting a short spike above the sustained rate.
- [ ] When both a route and its upstream declare a `rate_limit` with `sharing: enforce` and differing `sustained.rate` values, the effective enforced rate equals the smaller (`min()`) of the two, verified by exhausting the bucket at the smaller configured rate rather than the larger.
- [ ] `rate_limit.scope` values of `tenant`, `user`, `ip`, and `route` each produce independently-exhaustible counters: exhausting the bucket for one subject does not deny a request from a different subject sharing the same resource; `scope: global` produces one bucket shared across all such subjects.
- [ ] A resource whose effective composition resolves to "no rate limit configured" forwards every request without evaluating a bucket and without emitting `X-RateLimit-*` headers.
- [ ] Configuring `algorithm: sliding_window` on an upstream/route still enforces rate limiting (via token-bucket accounting) rather than being silently unenforced or causing an unhandled error.
- [ ] Two Data-Plane instances enforcing the same configured `sustained.rate` each independently admit up to that rate, confirming the counter is per-instance and not shared across instances.
- [ ] Concurrent requests issued in parallel against the same counter key never admit more than `burst.capacity` requests in total when the bucket starts full, verifying atomic consumption under concurrency.
