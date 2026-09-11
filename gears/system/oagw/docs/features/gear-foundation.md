# Feature: Gear Foundation and Configuration


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Gear Registration and Router Mounting at Server Startup](#gear-registration-and-router-mounting-at-server-startup)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Gear Configuration Resolution](#gear-configuration-resolution)
  - [Shared Configuration-Store Write (Control Plane Path)](#shared-configuration-store-write-control-plane-path)
  - [Shared Configuration-Store Read (Data Plane Path)](#shared-configuration-store-read-data-plane-path)
  - [RFC 9457 Gateway-Error Rendering](#rfc-9457-gateway-error-rendering)
  - [Request Correlation-ID Assignment](#request-correlation-id-assignment)
  - [Structured Audit-Log Scaffold Emission](#structured-audit-log-scaffold-emission)
- [4. States (CDSL)](#4-states-cdsl)
  - [Gear Bootstrap State Machine](#gear-bootstrap-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Gear Registration and Router Mounting](#gear-registration-and-router-mounting)
  - [Typed Gear-Configuration Surface](#typed-gear-configuration-surface)
  - [Shared Configuration-Store Contract](#shared-configuration-store-contract)
  - [RFC 9457 Error Envelope and GTS Error-Type Catalog](#rfc-9457-error-envelope-and-gts-error-type-catalog)
  - [Error-Source Header on Gateway Errors](#error-source-header-on-gateway-errors)
  - [Audit-Log Scaffold and Correlation-ID Plumbing](#audit-log-scaffold-and-correlation-id-plumbing)
  - [Single-Executable ToolKit Deployment Packaging](#single-executable-toolkit-deployment-packaging)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-gear-foundation-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-gear-foundation`
## 1. Feature Context

### 1.1 Overview

This feature establishes the `oagw` gear as a loadable, addressable component of the host ToolKit runtime, exposes its typed gear-configuration surface (`proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy.enabled`), provides the single in-process configuration-store abstraction shared by the Control Plane (write path) and Data Plane (read path), and implements the cross-cutting RFC 9457 error-rendering and `X-OAGW-Error-Source` contract every later feature's error paths depend on. It has no user-facing endpoint of its own; it is the substrate every other feature (2.2–2.9) is built on.

### 1.2 Purpose

OAGW cannot register a router, read its deployment configuration, share state between its Control Plane and Data Plane halves, or report errors consistently until this feature exists. Every later FEATURE (upstream/route/plugin CRUD, proxy forwarding, streaming, CORS, rate limiting, plugin execution) mounts under the router this feature establishes, reads the `OagwConfig` this feature resolves, and renders its gateway-originated errors through the envelope this feature defines.

**Requirements Covered**:

- [ ] `p1` - `cpt-cf-oagw-fr-error-codes`
- [ ] `p1` - `cpt-cf-oagw-nfr-high-availability` — partial: this feature delivers the gear's addressable/loadable bootstrap with the host runtime and the consistent RFC 9457 error envelope that availability signaling depends on; the circuit-breaker half of this NFR is out of scope for this decomposition round (see `DECOMPOSITION.md` §2.1 Out of scope and `DESIGN.md` §4.7 future work)

**Design Principles Covered**:

- `cpt-cf-oagw-principle-rfc9457`
- `cpt-cf-oagw-principle-error-source`

**Design Constraints Covered**:

- `cpt-cf-oagw-constraint-toolkit-deploy`

**Design Components**:

- `cpt-cf-oagw-design-drivers`
- `cpt-cf-oagw-design-layers`
- `cpt-cf-oagw-tech-dependencies`
- `cpt-cf-oagw-design-overview`
- `cpt-cf-oagw-design-dependencies`
- `cpt-cf-oagw-component-model`
- `cpt-cf-oagw-interface-api`
- `cpt-cf-oagw-adr-error-source-distinction`
- `cpt-cf-oagw-adr-state-management`
- `cpt-cf-oagw-adr-data-plane-caching`

**Two overrides carried into this feature, superseding what `PRD.md`/`DESIGN.md` tabulate** (per `DECOMPOSITION.md` §1 Overview corrections 1 and 2):

1. This gear's router is mounted gear-relative; every path is `/oagw/v1/...` with no leading `/api` (the `/api` prefix, when present, is added by the api-gateway host, not by this gear).
2. `cpt-cf-oagw-constraint-https-only` is the *default* posture only. The `allow_http_upstream` flag this feature owns lifts that default; this feature is responsible only for surfacing the flag's resolved value to readers, not for opening or refusing any connection — connection-time enforcement is `cpt-cf-oagw-feature-proxy-core`'s concern.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Starts/deploys the host ToolKit process carrying the `oagw` gear; supplies (or omits) the `gears.oagw.config` YAML stanza; observes gear registration success or startup failure. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **ADRs**: [ADR/0005-data-plane-caching.md](../ADR/0005-data-plane-caching.md), [ADR/0006-state-management.md](../ADR/0006-state-management.md), [ADR/0007-error-source-distinction.md](../ADR/0007-error-source-distinction.md)
- **Dependencies**: None (`cpt-cf-oagw-feature-gear-foundation` has no upstream FEATURE dependency; see `DECOMPOSITION.md` §3 Feature Dependencies)

## 2. Actor Flows (CDSL)

This feature has no proxy or management endpoint of its own, so the only genuine actor-facing flow is server startup / gear registration as observed by the platform operator. Behavior that is not actor-facing (configuration resolution, the config-store contract, error rendering, audit-log scaffolding) is documented in §3 instead of being padded into this section.

**Use cases**: none in `PRD.md` §8 describe this flow directly; it is the bootstrap precondition for `cpt-cf-oagw-usecase-configure-upstream`, `cpt-cf-oagw-usecase-configure-route`, and `cpt-cf-oagw-usecase-proxy-request`, all of which require the gear to already be registered and its router already mounted.

### Gear Registration and Router Mounting at Server Startup

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-gear-bootstrap`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The operator starts the host process with a server YAML that either sets `gears.oagw.config` (e.g., the graded `proxy_timeout_secs: 2, allow_http_upstream: true, ssrf_policy.enabled: false`) or omits the stanza entirely; in both cases the gear registers and its router becomes mountable at `/oagw/v1/...`.

**Error Scenarios**:
- The operator supplies a `gears.oagw.config` stanza containing a key with the wrong type or an out-of-range value (e.g., a non-integer or negative `proxy_timeout_secs`); gear registration fails fast with a descriptive startup error naming the offending key, and the host aborts startup rather than silently substituting a default for a value that was explicitly (and invalidly) supplied.

**Steps**:
1. [x] - `p1` - Operator starts the host ToolKit process with a server YAML that may or may not contain a `gears.oagw.config` stanza - `inst-gear-bootstrap-01`
2. [x] - `p1` - Host ToolKit runtime discovers the `oagw` gear and invokes its registration entry point, passing it the raw `gears.oagw.config` YAML node (or nothing, if absent) - `inst-gear-bootstrap-02`
3. [x] - `p1` - Gear resolves the raw node into a typed `OagwConfig` via `cpt-cf-oagw-algo-config-resolution` - `inst-gear-bootstrap-03`
4. [x] - `p1` - **IF** resolution fails (a present key has the wrong type or an out-of-range value) - `inst-gear-bootstrap-04`
   1. [x] - `p1` - Gear registration aborts with a descriptive error identifying the offending configuration key; the host ToolKit process fails startup rather than mounting a partially-configured gear - `inst-gear-bootstrap-05`
5. [x] - `p1` - **ELSE** - `inst-gear-bootstrap-06`
   1. [x] - `p1` - Gear constructs the single in-process configuration-store handle (per `cpt-cf-oagw-algo-config-store-write` / `cpt-cf-oagw-algo-config-store-read`), seeds it with the resolved `OagwConfig`, and registers its router under the gear-relative mount point `/oagw/v1` with the host ToolKit runtime's route table - `inst-gear-bootstrap-07`
6. [x] - `p1` - **RETURN** the gear is addressable and loadable: it appears in the host runtime's registered-gear set, its router is mounted, and its configuration-store handle is ready for the Control Plane (2.2–2.4) and Data Plane (2.5) to read from and write to - `inst-gear-bootstrap-08`

## 3. Processes / Business Logic (CDSL)

### Gear Configuration Resolution

- [x] `p1` - **ID**: `cpt-cf-oagw-algo-config-resolution`

**Input**: the raw `gears.oagw.config` YAML node from the server configuration, or its absence.

**Output**: a typed `OagwConfig` value with fields `proxy_timeout_secs` (positive integer seconds), `allow_http_upstream` (boolean), `ssrf_policy.enabled` (boolean); or a resolution failure naming the offending key.

Each of the three fields is defaulted **independently** when the stanza — or that specific key within it — is absent; this feature's documented defaults are: `proxy_timeout_secs = 30`, `allow_http_upstream = false` (preserves the `cpt-cf-oagw-constraint-https-only` default posture), `ssrf_policy.enabled = true` (preserves the safe posture that `cpt-cf-oagw-nfr-ssrf-protection` assumes by default). The graded `config/e2e-local.yaml` stanza sets all three explicitly (`2`, `true`, `false`), so resolution against that file must yield exactly those three values with no defaults applied. A key that is present but fails to parse as its documented type, or fails its documented range check (`proxy_timeout_secs` must be a positive integer), is a resolution failure — it is never silently coerced to the default, because a present-but-invalid value is an operator error, not an absence.

**Steps**:
1. [x] - `p1` - Parse the server YAML down to the `gears.oagw.config` node - `inst-config-resolution-01`
2. [x] - `p1` - **IF** the node is absent entirely - `inst-config-resolution-02`
   1. [x] - `p1` - Apply the documented default to all three fields and return the defaulted `OagwConfig` - `inst-config-resolution-03`
3. [x] - `p1` - **ELSE** for each of `proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy.enabled` independently - `inst-config-resolution-04`
   1. [x] - `p1` - **IF** the key is absent from the node - `inst-config-resolution-05`
      1. [x] - `p1` - Apply that key's documented default - `inst-config-resolution-06`
   2. [x] - `p1` - **ELSE TRY** parse the key's value as its documented type and, for `proxy_timeout_secs`, validate it is a positive integer - `inst-config-resolution-07`
      1. [x] - `p1` - **CATCH** type-mismatch or range error - `inst-config-resolution-08`
         1. [x] - `p1` - Fail resolution, naming the offending key and the value that was rejected - `inst-config-resolution-09`
4. [x] - `p1` - **RETURN** the fully-resolved `OagwConfig`, or the resolution failure from step 3.1.1 (propagated to `cpt-cf-oagw-flow-gear-bootstrap` step 4) - `inst-config-resolution-10`

### Shared Configuration-Store Write (Control Plane Path)

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-config-store-write`

**Input**: a configuration value and its key, submitted by a Control-Plane-owned writer (this feature's own bootstrap seeding of `OagwConfig`, and — per `cpt-cf-oagw-adr-state-management`'s CP/DP split — every later Control-Plane write, e.g., 2.2–2.4's persisted-record writes, that needs the same single-writer-role contract this feature establishes).

**Output**: the store's current value for that key is replaced; no partial ("torn") value is ever observable by a concurrent reader.

This algorithm defines the **ownership and concurrency contract**, not the store's internal code structure: exactly one logical writer role (Control Plane) may write; the Data Plane never writes through this path (see `cpt-cf-oagw-algo-config-store-read`); the store is in-process and single-instance for this round (no cross-instance/Redis synchronization — that distribution question belongs to the L1/L2 caching mechanics `DECOMPOSITION.md` §2.1 Out of scope defers to 2.2/2.3/2.5).

**Steps**:
1. [x] - `p1` - Acquire exclusive write access to the store for the target key - `inst-config-store-write-01`
2. [x] - `p1` - Replace the stored value atomically (a concurrent reader observes either the old or the new value in full, never a mix of both) - `inst-config-store-write-02`
3. [x] - `p1` - Release exclusive access - `inst-config-store-write-03`
4. [x] - `p1` - **RETURN** acknowledgement; from this point on, every subsequent read (`cpt-cf-oagw-algo-config-store-read`) within the same process observes the new value (read-after-write visibility, no propagation delay) - `inst-config-store-write-04`

### Shared Configuration-Store Read (Data Plane Path)

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-config-store-read`

**Input**: a configuration key (e.g., the gear-level `OagwConfig`, or — for later features — an upstream/route record key).

**Output**: the store's current value for that key.

**Steps**:
1. [x] - `p1` - Acquire non-exclusive (shared/read) access to the store for the target key, without blocking other concurrent readers - `inst-config-store-read-01`
2. [x] - `p1` - **IF** the key has never been written (the store has not yet been seeded for it) - `inst-config-store-read-02`
   1. [x] - `p1` - Treat this as a bootstrap-ordering defect: `cpt-cf-oagw-flow-gear-bootstrap` seeds `OagwConfig` before the router is mounted, so no in-flight request should ever observe an unseeded key for this feature's own data - `inst-config-store-read-03`
3. [x] - `p1` - **RETURN** the current value for the key - `inst-config-store-read-04`

### RFC 9457 Gateway-Error Rendering

- [x] `p1` - **ID**: `cpt-cf-oagw-algo-error-render`

**Input**: a gateway-originated error carrying one of the error-type identifiers enumerated in `cpt-cf-oagw-dod-error-envelope`'s table, plus whatever request context is available at the point the error is raised (`upstream_id`, `host`, `path`, `retry_after_seconds`, `trace_id` — each independently optional, since not every error type has all five populated, e.g. a management-API `ValidationError` has no `upstream_id`).

**Output**: an HTTP response whose body is `application/problem+json` and whose headers include `X-OAGW-Error-Source: gateway`.

**Steps**:
1. [x] - `p1` - Read the error-type identifier from the input error value - `inst-error-render-01`
2. [x] - `p1` - Resolve the documented `(HTTP status, GTS type identifier, title)` triple for that error-type from the catalog in `cpt-cf-oagw-dod-error-envelope` - `inst-error-render-02`
3. [x] - `p1` - Build the RFC 9457 standard fields: `type` (the resolved GTS identifier), `title` (the resolved title), `status` (the resolved HTTP status), `detail` (an occurrence-specific human-readable explanation), `instance` (the request's URI) - `inst-error-render-03`
4. [x] - `p1` - **FOR EACH** of the extension fields `upstream_id`, `host`, `path`, `retry_after_seconds`, `trace_id` - `inst-error-render-04`
   1. [x] - `p1` - **IF** the field's value is available in the current request context - `inst-error-render-05`
      1. [x] - `p1` - Include it in the response body - `inst-error-render-06`
   2. [x] - `p1` - **ELSE** omit it (its absence never invalidates the RFC 9457 envelope) - `inst-error-render-07`
5. [x] - `p1` - Set the response `Content-Type` header to `application/problem+json` and the HTTP status to the resolved status from step 2 - `inst-error-render-08`
6. [x] - `p1` - Set the `X-OAGW-Error-Source: gateway` response header - `inst-error-render-09`
7. [x] - `p1` - **IF** `retry_after_seconds` is present in the body - `inst-error-render-10`
   1. [x] - `p1` - Also set the `Retry-After` response header to the same value, per the example in `cpt-cf-oagw-adr-error-source-distinction` Appendix A - `inst-error-render-11`
8. [x] - `p1` - **RETURN** the completed HTTP response - `inst-error-render-12`

Rendering the complementary passthrough half — `X-OAGW-Error-Source: upstream` with the upstream's response body forwarded unchanged — is explicitly out of scope for this feature; only `cpt-cf-oagw-feature-proxy-core` (2.5) receives upstream responses to pass through.

### Request Correlation-ID Assignment

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-correlation-id`

**Input**: an inbound request, before any routing/business-logic processing.

**Output**: a request-scoped context carrying a correlation (request) identifier.

**Steps**:
1. [x] - `p1` - **IF** the inbound request already carries a platform-standard correlation-id header - `inst-correlation-id-01`
   1. [x] - `p1` - Adopt that value as the correlation id for this request - `inst-correlation-id-02`
2. [x] - `p1` - **ELSE** generate a new identifier that is unique per request - `inst-correlation-id-03`
3. [x] - `p1` - Attach the identifier to the request-scoped context so that later stages (this feature's own error rendering and audit-log scaffold, and later features' proxy handling, plugin execution, and metrics) can read and reuse it without regenerating it - `inst-correlation-id-04`
4. [x] - `p1` - **RETURN** the request-scoped context carrying the correlation id - `inst-correlation-id-05`

Per-request population of this id onto the proxy hot path's audit-log entries and metrics (i.e., actually calling this algorithm and using its result for a live proxied request) is `cpt-cf-oagw-feature-proxy-core`'s (2.5) concern; this feature only defines and provides the assignment mechanism.

### Structured Audit-Log Scaffold Emission

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-audit-log-emit`

**Input**: a request-scoped context (correlation id from `cpt-cf-oagw-algo-correlation-id`, plus whatever of `tenant_id`, `principal_id`, `host`, `path`, `method`, `status`, `duration_ms` the calling feature has resolved at the time it emits).

**Output**: one structured JSON log line.

**Steps**:
1. [x] - `p1` - Build a JSON object with exactly the documented field set: `timestamp`, `level`, `event`, `request_id`, `tenant_id`, `principal_id`, `host`, `path`, `method`, `status`, `duration_ms` - `inst-audit-log-emit-01`
2. [x] - `p1` - **FOR EACH** field whose value has not been resolved by the calling feature at emission time - `inst-audit-log-emit-02`
   1. [x] - `p1` - Emit the field as an explicit absent/null placeholder rather than a fabricated value - `inst-audit-log-emit-03`
3. [x] - `p1` - Serialize the object as a single JSON line and write it to the structured-logging sink - `inst-audit-log-emit-04`
4. [x] - `p1` - **RETURN** (emission complete) - `inst-audit-log-emit-05`

Per-request field population for a live proxied request (i.e., filling in `host`/`path`/`method`/`status`/`duration_ms`/`tenant_id`/`principal_id` with real values) is `cpt-cf-oagw-feature-proxy-core`'s (2.5) concern; this feature only defines the field set, the serialization shape, and provides the emission mechanism that later features call into.

## 4. States (CDSL)

Of the four domain-model entities this feature owns (Gear configuration, Problem Details error envelope, Error taxonomy/GTS catalog, Shared configuration-store handle), only the gear's own bootstrap sequence has a genuine lifecycle with distinct, testable states; the other three are values or lookup tables with no state transitions of their own, so no additional state machine is defined for them here.

### Gear Bootstrap State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-gear-bootstrap`

**States**: Uninitialized, ConfigResolving, ConfigResolved, Registered, BootstrapFailed

**Initial State**: Uninitialized

**Transitions**:
1. [x] - `p1` - **FROM** Uninitialized **TO** ConfigResolving **WHEN** the host ToolKit runtime invokes the gear's registration entry point - `inst-state-gear-bootstrap-01`
2. [x] - `p1` - **FROM** ConfigResolving **TO** ConfigResolved **WHEN** `cpt-cf-oagw-algo-config-resolution` returns a fully-defaulted, valid `OagwConfig` - `inst-state-gear-bootstrap-02`
3. [x] - `p1` - **FROM** ConfigResolving **TO** BootstrapFailed **WHEN** `cpt-cf-oagw-algo-config-resolution` rejects a present key for a type or range violation - `inst-state-gear-bootstrap-03`
4. [x] - `p1` - **FROM** ConfigResolved **TO** Registered **WHEN** the configuration-store handle is constructed and seeded and the router is mounted at `/oagw/v1` - `inst-state-gear-bootstrap-04`

`BootstrapFailed` is terminal for this gear instance: the host ToolKit process aborts startup rather than retrying or falling back, so no outgoing transition from `BootstrapFailed` is defined.

## 5. Definitions of Done

**Review domains dispositioned as not applicable to this feature**: UX and accessibility — not applicable because this feature exposes no user interface of any kind (no HTML surface, no CLI, no interactive output); its only observable surfaces are the host runtime's registered-gear set, a structured JSON log line, and a machine-readable `application/problem+json` body, none of which has a human-interaction or assistive-technology contract to satisfy. Compliance and privacy — not applicable because this feature processes no personal, sensitive or otherwise regulated data: the only values it handles are three operator-supplied gear-configuration keys and the fixed audit-log field set, which by its own definition carries no request or response bodies and no header values. Data privacy — not applicable for the same reason; there is no subject data to minimise, retain or erase, and no credentials or secrets flow through this feature at all. Performance — not applicable as a graded quality because everything this feature does happens once at gear registration (configuration resolution, store construction, router mounting) or is a constant-time in-process lookup with no I/O; no latency or throughput budget in `PRD.md` is charged to it, and the hot-path latency budget belongs to `cpt-cf-oagw-feature-proxy-core`. Extension points — not applicable because this feature exposes no plugin, filter or hook surface of its own: the error-rendering, correlation-id and audit-log mechanisms are called directly by later features rather than registered into, and the named data-plane hooks that the layered features attach to are owned by `cpt-cf-oagw-feature-proxy-core`. Resilience and recovery — dispositioned rather than waived, and covered by the reliability and rollback statements immediately below: bootstrap failure is fail-fast and terminal, and there is no persisted state to recover.

**Security, reliability, data-integrity, observability, and rollback**: Security — no credentials or secrets flow through this feature (that is `cpt-cf-oagw-feature-plugin-execution`'s concern); the only security-relevant surface is that the audit-log scaffold's fixed field set (§3 `cpt-cf-oagw-algo-audit-log-emit`) never carries request/response bodies or headers, consistent with `DESIGN.md` §4.3. Reliability — a malformed `gears.oagw.config` stanza fails gear startup fast and loud (§4 `cpt-cf-oagw-state-gear-bootstrap`) rather than degrading silently, so a misconfigured gateway never serves traffic under an unvalidated configuration. Data integrity — the configuration-store write contract (`cpt-cf-oagw-algo-config-store-write`) guarantees no torn reads are ever observable, which is the only data-integrity property this feature owns (it persists no records of its own). Observability — this feature's audit-log field set and error-rendering envelope are the mechanisms every later feature's observability depends on; per-request emission/population is explicitly deferred to `cpt-cf-oagw-feature-proxy-core`. Rollback — not applicable: this feature introduces no database schema, no migration, and no persisted state; a failed bootstrap (`BootstrapFailed`) simply prevents the gear from ever mounting its router, leaving no partial state to roll back.

### Gear Registration and Router Mounting

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-registration`

The system **MUST** register the `oagw` gear with the host ToolKit runtime at startup and mount its router at the gear-relative prefix `/oagw/v1`, with no leading `/api` segment contributed by the gear itself.

**Implements**:
- `cpt-cf-oagw-flow-gear-bootstrap`
- `cpt-cf-oagw-state-gear-bootstrap`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- Entities: Gear configuration (`OagwConfig`)

### Typed Gear-Configuration Surface

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-config-surface`

The system **MUST** resolve a typed `OagwConfig` from the `gears.oagw.config` YAML stanza, accepting at minimum `proxy_timeout_secs` (positive integer seconds, default `30`), `allow_http_upstream` (boolean, default `false`), `ssrf_policy.enabled` (boolean, default `true`), `token_cache_ttl_secs` (positive integer seconds, default `300`), and `token_cache_capacity` (positive integer, default `10000`), each defaulted independently when absent, and MUST fail gear registration when a present key fails its type or range check rather than substituting the default for it. Against the graded `config/e2e-local.yaml` stanza, resolution MUST yield exactly `proxy_timeout_secs = 2`, `allow_http_upstream = true`, `ssrf_policy.enabled = false`. An unrecognized key present in the `gears.oagw.config` stanza (one that is not one of the keys this surface documents) MUST NOT fail gear registration; the resolver logs a `tracing::warn!` naming the unrecognized key and otherwise ignores it.

**Implements**:
- `cpt-cf-oagw-algo-config-resolution`

**Touches**:
- Entities: Gear configuration (`OagwConfig`)

### Shared Configuration-Store Contract

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-config-store-contract`

The system **MUST** provide a single in-process configuration-store abstraction, consistent with the Control-Plane/Data-Plane split in `cpt-cf-oagw-adr-state-management`, in which the Control Plane is the sole writer, the Data Plane is a read-only consumer, writes are atomic (no torn reads observable to concurrent readers), and a completed write is immediately visible to subsequent reads within the same process. This feature establishes only the ownership/concurrency contract and the `OagwConfig` value it carries; it does NOT implement the L1/L2 cache population or invalidation mechanics that `cpt-cf-oagw-adr-data-plane-caching` describes for upstream/route/plugin records — those hot-path read/write mechanics belong to `cpt-cf-oagw-feature-upstream-management` (2.2), `cpt-cf-oagw-feature-route-management` (2.3), and `cpt-cf-oagw-feature-proxy-core` (2.5).

**Implements**:
- `cpt-cf-oagw-algo-config-store-write`
- `cpt-cf-oagw-algo-config-store-read`

**Touches**:
- Entities: Shared configuration-store handle

### RFC 9457 Error Envelope and GTS Error-Type Catalog

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-envelope`

The system **MUST** render every gateway-originated error as `application/problem+json` per RFC 9457 (`type`, `title`, `status`, `detail`, `instance` standard fields, plus the `upstream_id`/`host`/`path`/`retry_after_seconds`/`trace_id` extension fields, each populated when available in context and omitted otherwise), using the GTS `type` identifier and HTTP status documented for each error type in `DESIGN.md` §3.3's Error Response Format table:

| Error Type | HTTP | GTS Instance ID |
|---|---|---|
| RouteError | 400 | `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` |
| ValidationError | 400 | `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` |
| MissingTargetHost | 400 | `gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1` |
| InvalidTargetHost | 400 | `gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1` |
| UnknownTargetHost | 400 | `gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1` |
| AuthenticationFailed | 401 | `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1` |
| CorsOriginNotAllowed | 403 | `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` |
| CorsMethodNotAllowed | 403 | `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1` |
| RouteNotFound | 404 | `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1` |
| PluginInUse | 409 | `gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1` |
| PayloadTooLarge | 413 | `gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1` |
| RateLimitExceeded | 429 | `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1` |
| SecretNotFound | 500 | `gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1` |
| ProtocolError | 502 | `gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1` |
| DownstreamError | 502 | `gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1` |
| StreamAborted | 502 | `gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1` |
| LinkUnavailable | 503 | `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1` |
| CircuitBreakerOpen | 503 | `gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1` |
| PluginNotFound | 503 | `gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1` |
| ConnectionTimeout | 504 | `gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1` |
| RequestTimeout | 504 | `gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1` |
| IdleTimeout | 504 | `gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1` |

This feature's rendering engine MUST be able to render every row above (the catalog is a cross-cutting lookup table this feature owns); which later feature actually raises a given error type (e.g., `RateLimitExceeded` is raised by `cpt-cf-oagw-feature-rate-limiting`, 2.8) is out of scope here.

**Accepted residual risk — oagw-level addition beyond DESIGN's table**: `CorsOriginNotAllowed`/`CorsMethodNotAllowed` (403, `cf.oagw.cors.origin_not_allowed.v1` / `cf.oagw.cors.method_not_allowed.v1`, raised by `cpt-cf-oagw-feature-cors-handling`, 2.7) are listed above so this catalog's completeness claim holds, but they do not appear in `DESIGN.md` §3.3's Error Response Format table — that table is a FROZEN INPUT with no 403 row at all. This divergence is accepted the same way the pre-existing `ref-missing-from-kind` finding on `ADR-0009` is accepted: a known, frozen-input gap that downstream (oagw-level) documentation corrects without editing the frozen source.

**Implements**:
- `cpt-cf-oagw-algo-error-render`

**Touches**:
- Entities: Problem Details error envelope, Error taxonomy / GTS error `type` catalog

### Error-Source Header on Gateway Errors

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-source-gateway-header`

The system **MUST** set `X-OAGW-Error-Source: gateway` on every gateway-originated error response rendered by `cpt-cf-oagw-algo-error-render`, per `cpt-cf-oagw-principle-error-source` and `cpt-cf-oagw-adr-error-source-distinction`. Setting `X-OAGW-Error-Source: upstream` and passing upstream error bodies through verbatim is explicitly out of scope for this feature; that half belongs to `cpt-cf-oagw-feature-proxy-core` (2.5).

**Implements**:
- `cpt-cf-oagw-algo-error-render`

**Touches**:
- Entities: Problem Details error envelope

### Audit-Log Scaffold and Correlation-ID Plumbing

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-audit-correlation-scaffold`

The system **MUST** provide a structured JSON audit-log scaffold emitting the field set `timestamp`, `level`, `event`, `request_id`, `tenant_id`, `principal_id`, `host`, `path`, `method`, `status`, `duration_ms` (unresolved fields emitted as explicit absent/null placeholders, never fabricated), and MUST provide request-correlation-ID assignment that later features attach their own per-request values to. Per-request Prometheus metric emission and correlation-ID population on the proxy hot path are explicitly out of scope for this feature; both belong to `cpt-cf-oagw-feature-proxy-core` (2.5).

**Implements**:
- `cpt-cf-oagw-algo-audit-log-emit`
- `cpt-cf-oagw-algo-correlation-id`

**Touches**:
- Entities: Gear configuration (`OagwConfig`)

### Single-Executable ToolKit Deployment Packaging

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-single-executable-packaging`

The system **MUST** package the `oagw` gear so that it deploys as part of a single ToolKit executable, with no separate runtime process required for this feature's behavior, per `cpt-cf-oagw-constraint-toolkit-deploy`.

**Implements**:
- `cpt-cf-oagw-flow-gear-bootstrap`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- Entities: Gear configuration (`OagwConfig`)

## 6. Acceptance Criteria

- [x] Starting the host with `gears.oagw.config` absent entirely registers the gear successfully with `proxy_timeout_secs = 30`, `allow_http_upstream = false`, `ssrf_policy.enabled = true`.
- [x] Starting the host with the graded `config/e2e-local.yaml` stanza (`proxy_timeout_secs: 2`, `allow_http_upstream: true`, `ssrf_policy.enabled: false`) registers the gear with `OagwConfig` fields matching those three values exactly.
- [x] Starting the host with a `gears.oagw.config` stanza where `proxy_timeout_secs` is a non-integer or a negative/zero value fails gear registration with an error identifying `proxy_timeout_secs` as the offending key, and the host does not begin serving traffic.
- [x] Starting the host with a `gears.oagw.config` stanza where `allow_http_upstream` or `ssrf_policy.enabled` is a non-boolean value fails gear registration with an error identifying the offending key.
- [x] Omitting only one of the three keys (leaving the other two present) resolves the omitted key to its own documented default while the two present keys resolve to their supplied values (independent per-key defaulting).
- [x] After successful registration, the gear appears in the host ToolKit runtime's registered-gear set and its router is mounted such that the runtime recognizes `/oagw/v1` as this gear's route prefix.
- [x] A value written to the configuration store via the Control-Plane write path is observable by a Data-Plane read within the same process immediately after the write completes, with no restart or polling delay required.
- [x] Concurrent reads and a write against the configuration store from multiple threads complete without deadlock, panic, or a torn (partially-updated) value being observed by any reader.
- [x] For each row of the error-type table in `cpt-cf-oagw-dod-error-envelope`, rendering that error type produces a response with the documented HTTP status, `Content-Type: application/problem+json`, and a body whose `type` field equals the documented GTS identifier, with `title`, `status`, `detail`, and `instance` all populated.
- [x] Every response rendered by `cpt-cf-oagw-algo-error-render` carries the `X-OAGW-Error-Source: gateway` header.
- [x] A rendered error whose context includes a `retry_after_seconds` value carries that same value in both the response body's `retry_after_seconds` field and the `Retry-After` header; a rendered error without that context omits both without breaking the response's validity as `application/problem+json`.
- [x] The audit-log scaffold emits one syntactically valid, single-line JSON object per invocation containing exactly the fields `timestamp`, `level`, `event`, `request_id`, `tenant_id`, `principal_id`, `host`, `path`, `method`, `status`, `duration_ms`, with unresolved fields present as explicit null placeholders rather than omitted or fabricated.
- [x] A correlation ID generated by `cpt-cf-oagw-algo-correlation-id` is non-empty, unique per invocation absent an inbound correlation-id header, and is reused (not regenerated) when an inbound request already supplies one.
- [x] The gear builds and deploys as part of a single ToolKit executable artifact, with no additional runtime process required for any behavior defined in this feature.
