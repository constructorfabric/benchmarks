# Feature: Gear Foundation


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Feature-Local Deviations from Shared Baselines](#15-feature-local-deviations-from-shared-baselines)
  - [1.6 Explicit Non-Applicability](#16-explicit-non-applicability)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Gear Registration and Configuration Bootstrap](#gear-registration-and-configuration-bootstrap)
  - [GTS Type Catalogue Provisioning Handshake](#gts-type-catalogue-provisioning-handshake)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Configuration Load and Validation](#configuration-load-and-validation)
  - [Alias and Hostname Normalization](#alias-and-hostname-normalization)
  - [Domain Error to RFC 9457 Response Mapping](#domain-error-to-rfc-9457-response-mapping)
  - [GTS Type Catalogue Provisioning](#gts-type-catalogue-provisioning)
- [4. States (CDSL)](#4-states-cdsl)
  - [Gear Foundation Provisioning State Machine](#gear-foundation-provisioning-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Gear Registration and Router Mount Point](#gear-registration-and-router-mount-point)
  - [Configuration Surface and Validation](#configuration-surface-and-validation)
  - [Domain Model Types and Layering](#domain-model-types-and-layering)
  - [GTS Identifier Catalogue and Provisioning](#gts-identifier-catalogue-and-provisioning)
  - [Canonical Error Type and Mapping](#canonical-error-type-and-mapping)
  - [Colocated Test Placement and Coverage](#colocated-test-placement-and-coverage)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-gear-foundation-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-gear-foundation`
## 1. Feature Context

### 1.1 Overview

This feature turns `gears/system/oagw/oagw` into a registered ToolKit gear and lays down the shared vocabulary the other eight features compile against: the `OagwConfig` surface, the DDD-Light crate skeleton, the `Upstream`/`Route`/`Plugin` domain model, the GTS identifier catalogue with its types-registry provisioning, and one canonical `DomainError` shape mapped to RFC 9457.

### 1.2 Purpose

`gear-foundation` is the root of the feature graph in DECOMPOSITION §3. `control-plane-config` and `plugin-system` branch off it directly, and everything downstream of them is written against the types, the error catalogue, and the router mount point delivered here. Without it no other feature can be written or tested, because there is no registered gear to host them, no domain type to persist, and no error shape to answer with. The feature materializes the existing design only: it introduces no requirement, no endpoint, and no architecture decision.

Deliverables:

- ToolKit gear registration (`gear.rs`, `lib.rs`) and the empty gear-relative Axum router mount point at `/oagw/v1` that later features populate.
- `OagwConfig` carrying `proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy`, `token_cache_ttl_secs`, and `token_cache_capacity`.
- DDD-Light crate layering (`api/rest`, `domain`, `infra`) with domain code free of infrastructure types.
- Domain model types `Upstream`, `Route`, and `Plugin`, their `server`, `auth`, `headers`, `rate_limit`, `cors`, and `plugins` sub-configurations, and the alias and hostname value objects.
- GTS identifier constants for the upstream, route, protocol, error, and plugin base types, provisioned into the types registry during the post-init phase.
- `DomainError` covering the full error catalogue, tagged with its gateway-versus-upstream source, and mapped to `application/problem+json` responses carrying GTS `type` identifiers.
- Colocated tests under `gears/system/oagw/oagw/tests/`.

**Requirements**:

- [ ] `p1` - `cpt-cf-oagw-fr-error-codes`
- [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
- [ ] `p1` - `cpt-cf-oagw-contract-types-registry`

**Principles**:

- `p1` - `cpt-cf-oagw-principle-rfc9457`

**Constraints**:

- `p1` - `cpt-cf-oagw-constraint-toolkit-deploy`
- `p1` - `cpt-cf-oagw-constraint-multi-sql`

**Design Components**:

- `p1` - `cpt-cf-oagw-component-model`
- `p1` - `cpt-cf-oagw-design-layers`
- `p1` - `cpt-cf-oagw-design-drivers`
- `p1` - `cpt-cf-oagw-design-overview`
- `p1` - `cpt-cf-oagw-design-domain-model`
- `p1` - `cpt-cf-oagw-design-dependencies`
- `p1` - `cpt-cf-oagw-tech-dependencies`

**Domain Model Entities**:

- `Upstream`, `Route`, `Plugin` (aggregate shapes from the design domain model)
- `Endpoint`, `ServerConfig`, `AuthConfig`, `HeadersConfig`, `RateLimitConfig`, `CorsConfig`, `PluginsConfig`
- `Alias`, `Hostname`, and `Scheme` value objects, normalized to ASCII lowercase with trailing dots stripped
- `DomainError`, `ErrorSource`, and the GTS error-type catalogue with gateway/upstream source tags

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Authors the `oagw.config` block that gear init consumes, and receives a fail-fast startup failure when a key is unknown, of the wrong type, or out of range. |
| `cpt-cf-oagw-actor-types-registry` | Receives the OAGW base type schemas and instances during the post-init provisioning handshake and answers per entry with success or a typed failure. |

`cpt-cf-oagw-actor-platform-operator` reaches this feature only as the author of the `oagw.config` block the init hook consumes, not through a management API — none exists at this stage — and DECOMPOSITION §1.5 records that participation by listing `gear-foundation` among the features that actor is served by.

`cpt-cf-oagw-actor-cred-store` and `cpt-cf-oagw-actor-upstream-service` do not participate in this feature. No credential material is resolved — `auth.secret_ref` is carried as an opaque `cred://` reference that the value object validates for shape only — and no outbound connection is opened. Both are consumed by `cpt-cf-oagw-feature-plugin-system` and `cpt-cf-oagw-feature-data-plane-proxy` respectively. The tenant administrator and application developer have no surface here either: no management, proxy, or metrics endpoint exists at this stage, so no use case from PRD §8 reaches this feature.

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Dependencies**: None — this feature is the root of the feature graph in DECOMPOSITION §3 and depends on no other `oagw` feature.

Supporting sources this feature stays consistent with:

- [schemas/upstream.v1.schema.json](../schemas/upstream.v1.schema.json) and [schemas/route.v1.schema.json](../schemas/route.v1.schema.json) — the configuration shapes the domain model types mirror, scoped in §5 to the properties each schema declares. Both files are frozen inputs; where this run overrides them, the override is recorded in §1.5.
- [ADR/0001-request-routing.md](../ADR/0001-request-routing.md) (`cpt-cf-oagw-adr-request-routing`) — the path-based routing table that the router mount point exists to serve.
- [ADR/0006-state-management.md](../ADR/0006-state-management.md) (`cpt-cf-oagw-adr-state-management`) — CP and DP state ownership. This feature creates no cache, no rate limiter, and no other runtime state; the L1 caches of that ADR belong to later features.
- [ADR/0007-error-source-distinction.md](../ADR/0007-error-source-distinction.md) (`cpt-cf-oagw-adr-error-source-distinction`) — the gateway/upstream source tag carried by every `DomainError`.
- [ADR/0008-oauth2-client-credentials-auth-plugin.md](../ADR/0008-oauth2-client-credentials-auth-plugin.md) (`cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`) — the token-cache defaults behind `token_cache_ttl_secs` and `token_cache_capacity`.

**Run-level assumptions** — premises this feature relies on that come from the platform runtime rather than from PRD, DESIGN, the ADRs, or DECOMPOSITION:

- Assumption: the platform types-registry gear catalogues entries through a two-phase protocol, a staging phase that accepts registrations without validating them and a ready phase that validates every entry. If the runtime does not implement it, the `Configured → TypeCatalogProvisioned` transition never fires and the gear fails closed.
- Assumption: the runtime's topological ordering of post-init hooks puts the types-registry's ready-flip before the OAGW post-init hook. If the runtime orders them the other way, the `Configured → TypeCatalogProvisioned` transition never fires and the gear fails closed.

### 1.5 Feature-Local Deviations from Shared Baselines

| Deviation | Rationale | Review owner | Validation performed |
|-----------|-----------|--------------|----------------------|
| Tests are colocated at `gears/system/oagw/oagw/tests/` instead of `testing/e2e/gears/oagw/`. | DECOMPOSITION §1.3(3) reserves `testing/e2e/gears/oagw/` for the acceptance suite; every unit and integration test this decomposition produces lives with the crate, so a feature is done when its tests pass in that tree. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| The router mount point is gear-relative `/oagw/v1/...` with no `/api` prefix. | DECOMPOSITION §1.3(1) corrects the `/api/oagw/v1/...` tabulation in PRD §7.1 and DESIGN §3.3: `/api` is an operator gateway prefix, not a path this gear serves. The mount point delivered here is therefore gear-relative, and every later feature registers under it. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| Write-time endpoint `scheme` validation accepts the `http` literal when `oagw.config.allow_http_upstream` is `true`, overriding the `scheme` enum of the frozen `schemas/upstream.v1.schema.json`. | DECOMPOSITION §1.3(2): which literals a configured endpoint may carry and whether a plaintext connection is opened are two questions, and only the second is governed by the flag. `cpt-cf-oagw-constraint-https-only` describes the default posture this flag lifts. The `Scheme` type declared here supplies the single write-time admission predicate; `cpt-cf-oagw-feature-control-plane-config` calls it when validating a write, and `cpt-cf-oagw-feature-data-plane-proxy` re-evaluates the same flag at dial time, keeping the two checks separate against the same constraint. `wt` remains a legal write-time literal even though no feature delivers WebTransport behaviour (DECOMPOSITION §1.3(4)). | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| `DownstreamError`'s `Depends` Retriable cell in DESIGN §3.3 is resolved **non-retriable** for the `Retry-After` gate in this feature. | The six rows DESIGN §3.3 marks `Yes` keep their unconditional retriability, so the write-time catalogue keeps a plain boolean per variant. A 502's retry decision belongs to the caller and to `cpt-cf-oagw-feature-data-plane-proxy`, which owns upstream-failure policy; resolving the cell here keeps that decision out of the type while preserving the PRD's context-dependent intent one layer up. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| `Route.cors` is declared in this feature although the shipped `schemas/route.v1.schema.json` omits the route-level property. | DECOMPOSITION §1.3(5): route-level CORS is configured through the `cors` field exactly as the Route class in DESIGN §3.1 specifies, and the schema is a frozen input this run does not edit. `cpt-cf-oagw-feature-control-plane-config` validates the object against the same shape as the upstream CORS configuration. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| `proxy_timeout_secs` defaults to `30`, a value no supplied document states. | `30` matches the platform REST request deadline applied by the built-in API gateway middleware stack as observed in the workspace, so an omitted key behaves like the platform default rather than like an unbounded request. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| `ssrf_policy.enabled` defaults to `true`, a value no supplied document states. | `true` is the fail-safe posture: SSRF enforcement is on until an operator turns it off, and the graded e2e configuration sets it to `false` explicitly rather than relying on the default. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| Registry-matching rule applied by this feature's provisioning: an identifier already registered with byte-identical content is accepted as success, and an identifier registered with different content that this gear does not own is a per-entry failure. | Idempotent acceptance is what lets a restart re-run provisioning over entries already in the registry without a conflict; refusing content this gear does not own keeps it from reporting readiness against a catalogue entry it did not write. Cited by the §6 criteria on re-registering an entry with byte-identical content and on a per-entry failure preventing readiness. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| The `DomainError` catalogue gains two management-conflict variants, `AliasConflict` and `MatchConflict`, which have no row in the DESIGN §3.3 catalogue. | DESIGN §3.3 tabulates exactly one 409 row, `PluginInUse`, which is a plugin-lifecycle answer, so the 409s the management write path answers for an alias conflict and for a match conflict have no variant to map to. The catalogue is this feature's to own, so the two rows are added here and consumed by the feature that answers them: `AliasConflict` answers 409 with `gts.cf.core.errors.err.v1~cf.oagw.alias.conflict.v1`, and `MatchConflict` answers 409 with `gts.cf.core.errors.err.v1~cf.oagw.match.conflict.v1`, both non-retriable and both following the identifier pattern of the DESIGN rows. DECOMPOSITION §1.3(9) records the extension as a run-level decision, and `cpt-cf-oagw-feature-control-plane-config` names both variants in the answers its §2 flows and §3 routines return. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| `Route` additionally carries `priority` and `enabled`, two attributes of the Route class in DESIGN §3.1 that the shipped `schemas/route.v1.schema.json` omits from its property list. | DECOMPOSITION §1.3 records the shipped schemas as frozen inputs this run does not edit, and its item 5 already establishes the same class of override for the route-level `cors` object: a property DESIGN §3.1 declares that the shipped route schema omits is declared by this feature and validated by `cpt-cf-oagw-feature-control-plane-config`. Both the match-uniqueness invariant (DESIGN §3.6) and the enable/disable semantics of `cpt-cf-oagw-fr-enable-disable` are stated over `priority` and `enabled`, so neither can be dropped from the domain type. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| The two provisioning criteria closed after the code review round, and `tracing-test` was added as a dev-dependency to assert the log side of one of them. | The criteria on resolvability through the types-registry and on a per-entry refusal are the two this run deferred while the data-plane features landed, and they are closed by `tests/provisioning_tests.rs` against a real in-process types-registry client (`TypesRegistryLocalClient` over `InMemoryGtsRepository`, the shape the `ClientHub` supplies at runtime) rather than against the recording fake the other provisioning tests use: provisioning, the ready commit the types-registry gear drives in its own post-init, a byte-identical read-back of every provisioned entry, the 21 error identifiers, and an identical re-registration. `tracing-test` is already a workspace dependency used by `account-management`, so no new crate enters the dependency graph, and it is dev-only. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |

### 1.6 Explicit Non-Applicability

The areas below apply to the gear as a whole but not to this feature. Each is stated here so the omission is a recorded decision rather than a silent gap.

- **Performance**: no request hot path and no runtime endpoint exist in this feature, so no latency or throughput budget is set here. The only performance-relevant outputs it produces are the `proxy_timeout_secs` deadline and the token-cache ceilings (`token_cache_ttl_secs`, `token_cache_capacity`), and both are consumed by later features that do own a hot path.
- **Compliance and privacy**: no PII, no regulated data, and no retention policy are touched by this feature. `auth.secret_ref` is carried as an opaque, shape-validated `cred://` reference and no credential material is resolved, so there is nothing to protect, log, or retain at this layer.
- **Events**: no event is published or consumed by this feature; the audit and metric surface that reports on the gear belongs to `cpt-cf-oagw-feature-observability`.
- **Rollout and rollback**: this feature is not an independent release unit, so it has no rollout or rollback of its own. It is the foundation slice of the single configuration item described in DECOMPOSITION §1.4, and is baselined and released with the gear.
- **Versioning**: every GTS identifier this feature declares is fixed at `.v1` by DESIGN §3.1, so no version-negotiation, aliasing, or migration surface is introduced here.
- **UX**: no actor-facing surface exists in this feature — the router mount point delivered here carries no routes (§1.1, §1.5) and no management, proxy, or metrics endpoint exists at this stage (§1.3).

## 2. Actor Flows (CDSL)

No actor-facing HTTP flow exists in this feature. Every endpoint, handler, and route registration is out of scope (DECOMPOSITION §2.1), so the flows below are limited to the internal actor interactions that do exist: the ToolKit runtime bringing the gear up, and the types-registry provisioning handshake.

**Use cases**: none. Every `cpt-cf-oagw-usecase-*` identifier in PRD §8 is exercised by a later feature; none of them reaches this one, and none is restated here.

### Gear Registration and Configuration Bootstrap

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-gear-init`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:

- `oagw.config` is present and well-formed: the gear registers under the name `oagw`, holds a validated `OagwConfig`, and exposes an empty router mount point at `/oagw/v1`.
- `oagw.config` is absent entirely: the gear registers with the declared defaults for every key.

**Error Scenarios**:

- A key is of the wrong type, out of range, or not part of the configuration surface: init fails, the gear does not register, and startup aborts with the configuration error naming the offending key.

**Steps**:
1. [x] - `p1` - Operator declares the `oagw.config` block with any of `proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy`, `token_cache_ttl_secs`, `token_cache_capacity` - `inst-gear-init-declare-config`
2. [x] - `p1` - ToolKit runtime instantiates the `oagw` gear and invokes its init hook with the `GearCtx` scoped to gear name `oagw` - `inst-gear-init-runtime-call`
3. [x] - `p1` - Resolve the `TypesRegistryClient` from the `ClientHub`; the dependency on the types-registry gear is declared so the runtime guarantees its init has completed - `inst-gear-init-resolve-registry`
4. [x] - `p1` - Load `OagwConfig` through `config_or_default`, which yields the declared defaults when the `oagw.config` section is absent - `inst-gear-init-load-config`
5. [x] - `p1` - **IF** `cpt-cf-oagw-algo-config-load-validate` returns an error - `inst-gear-init-validate`
   1. [x] - `p1` - Abort init with that error; the gear does not register and startup fails fast before any later feature can consume a half-configured gear - `inst-gear-init-abort`
6. [x] - `p1` - **ELSE** - `inst-gear-init-else`
   1. [x] - `p1` - Store the validated configuration on the gear instance and construct the empty gear-relative router mount point at `/oagw/v1`; no endpoint, handler, or route is registered - `inst-gear-init-mount`
7. [x] - `p1` - **RETURN** a registered gear whose readiness stays withheld until the type catalogue is provisioned (`cpt-cf-oagw-state-gear-foundation-lifecycle`) - `inst-gear-init-return`

### GTS Type Catalogue Provisioning Handshake

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-type-provisioning`

**Actor**: `cpt-cf-oagw-actor-types-registry`

**Success Scenarios**:

- Every OAGW base type schema and instance in the catalogue table is present in the registry when startup completes; an entry already registered with byte-identical content is accepted as success rather than a conflict.
- The registry catalogue is in its ready phase when provisioning runs, so every entry is fully validated before it is accepted (§1.4 run-level assumptions).

**Error Scenarios**:

- Any per-entry failure — malformed identifier, schema validation failure, parent type not yet registered, or an identifier already registered with different content that this gear does not own — fails the post-init phase, and the gear never reports readiness.

**Steps**:
1. [x] - `p1` - Types-registry completes its own init and publishes its client; its catalogue is still in the staging phase, where registrations bypass validation - `inst-type-prov-registry-init`
2. [x] - `p1` - ToolKit runtime runs the post-init phase after every gear's init has returned; system gears run first in topological order, so the registry's own post-init has already flipped its catalogue to the ready phase - `inst-type-prov-post-init-phase`
3. [x] - `p1` - `oagw` enumerates its GTS identifier constants and hands the batch to `cpt-cf-oagw-algo-type-catalog-provisioning` - `inst-type-prov-enumerate`
4. [x] - `p1` - **IF** every entry reports success, including entries already registered with identical content - `inst-type-prov-check`
   1. [x] - `p1` - Mark the type catalogue provisioned and allow readiness to be reported - `inst-type-prov-ready`
5. [x] - `p1` - **ELSE** - `inst-type-prov-else`
   1. [x] - `p1` - Log each failing GTS identifier at ERROR, fail the post-init phase, and leave the gear not ready so startup aborts - `inst-type-prov-fail`
6. [x] - `p1` - **RETURN** a provisioned catalogue and a ready gear, or a failed startup - `inst-type-prov-return`

## 3. Processes / Business Logic (CDSL)

Internal routines called by the flows above or by later features. Only the provisioning routine leaves the process, and it does so through the in-process `types_registry` SDK call; nothing here opens an HTTP connection, touches a database, or resolves a secret.

### Configuration Load and Validation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-config-load-validate`

**Input**: the raw `oagw.config` mapping from the deployment configuration, possibly absent, together with the `OagwConfig` defaults.

**Output**: a validated `OagwConfig`, or a configuration error that fails gear init.

The configurable surface, its defaults, and its constraints:

| Key | Type | Default | Constraint |
|-----|------|---------|------------|
| `proxy_timeout_secs` | integer | 30 | At least 1. Outbound request deadline; the graded e2e configuration overrides it to 2. |
| `allow_http_upstream` | boolean | `false` | The only input that admits the `http` endpoint scheme literal at write time. |
| `ssrf_policy.enabled` | boolean | `true` | Fail-safe default; the graded e2e configuration sets it to `false` explicitly. |
| `token_cache_ttl_secs` | integer | 300 | At least 1. Ceiling for a cached access-token TTL (ADR 0008). |
| `token_cache_capacity` | integer | 10000 | At least 1. Maximum token-cache entries (ADR 0008). |

`ssrf_policy.enabled` gates the SSRF enforcement owned by `cpt-cf-oagw-feature-data-plane-proxy` under `cpt-cf-oagw-nfr-ssrf-protection`; this feature only carries and validates the key and evaluates no SSRF rule of its own.

**Steps**:
1. [x] - `p1` - Parse the mapping into `OagwConfig`, rejecting keys that are not part of the surface above (`deny_unknown_fields`, the platform configuration idiom) - `inst-config-parse`
2. [x] - `p1` - Apply the declared default for every absent key - `inst-config-defaults`
3. [x] - `p1` - **FOR EACH** integer key in {`proxy_timeout_secs`, `token_cache_ttl_secs`, `token_cache_capacity`} - `inst-config-int-loop`
   1. [x] - `p1` - Reject the configuration if the value is not an integer of at least 1 - `inst-config-int-check`
4. [x] - `p1` - **IF** `allow_http_upstream` is `true` - `inst-config-http-if`
   1. [x] - `p1` - Record the lifted posture on the configuration so the `Scheme` value object admits the `http` literal at write time; recording it does not by itself authorize a plaintext dial - `inst-config-http-record`
5. [x] - `p1` - **IF** any check failed - `inst-config-fail-if`
   1. [x] - `p1` - **RETURN** the configuration error naming the offending key - `inst-config-fail-return`
6. [x] - `p1` - **RETURN** the validated `OagwConfig` - `inst-config-return`

The 30-second default for `proxy_timeout_secs` matches the platform REST request deadline applied by the built-in API gateway middleware stack, so an omitted key behaves like the platform default rather than like an unbounded request.

### Alias and Hostname Normalization

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-alias-normalize`

**Input**: an alias or endpoint host string as supplied by a caller, optionally carrying a `:port` suffix.

**Output**: a normalized `Alias` or `Hostname` value object, or a validation error.

**Steps**:
1. [x] - `p1` - Trim surrounding whitespace and strip trailing dots; FQDN notation is tolerated on input and never stored - `inst-alias-trim`
2. [x] - `p1` - Normalize to ASCII lowercase and reject any non-ASCII byte rather than transliterating it - `inst-alias-lower`
3. [x] - `p1` - Validate RFC 1123 hostname syntax: at most 253 characters in total, labels of 1 to 63 characters, labels drawn from ASCII alphanumerics and hyphen, no label starting or ending with a hyphen - `inst-alias-rfc1123`
4. [x] - `p1` - **IF** the string carries a `:port` suffix - `inst-alias-port-if`
   1. [x] - `p1` - Validate the port as an integer from 1 to 65535 and keep it in the normalized value; the port participates in alias identity, so `api.openai.com` and `api.openai.com:8443` are distinct aliases - `inst-alias-port-keep`
5. [x] - `p1` - **IF** the value is empty after trimming - `inst-alias-empty-if`
   1. [x] - `p1` - **RETURN** a validation error - `inst-alias-empty-return`
6. [x] - `p1` - **RETURN** the normalized value object - `inst-alias-return`

This routine normalizes and validates only. Alias derivation — single hostname, longest common registrable suffix, rejection of a bare public suffix such as `co.uk` — and alias immutability across updates are delivered by `cpt-cf-oagw-feature-control-plane-config`, which calls this routine on every value it stores or resolves. Normalization lives here so that write-time storage and proxy-time resolution cannot disagree about what an alias looks like.

### Domain Error to RFC 9457 Response Mapping

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-error-mapping`

**Input**: a `DomainError` carrying its `ErrorSource` tag (`gateway` or `upstream`), its optional `ErrorContext` (`upstream_id`, `host`, `path`, `retry_after_seconds`, `trace_id`), and the request URI to use as the problem `instance`.

**Output**: an HTTP response.

`DomainError` declares one variant per error type in the DESIGN §3.3 catalogue (`cpt-cf-oagw-interface-api`) — `RouteError`, `ValidationError`, `MissingTargetHost`, `InvalidTargetHost`, `UnknownTargetHost`, `AuthenticationFailed`, `RouteNotFound`, `PluginInUse`, `PayloadTooLarge`, `RateLimitExceeded`, `SecretNotFound`, `ProtocolError`, `DownstreamError`, `StreamAborted`, `LinkUnavailable`, `CircuitBreakerOpen`, `PluginNotFound`, `ConnectionTimeout`, `RequestTimeout`, `IdleTimeout` — plus the two management-conflict variants added per §1.5, `AliasConflict` and `MatchConflict`. Each variant carries its HTTP status, GTS `type` identifier, and Retriable flag. Every DESIGN row has exactly one variant and every variant has a row: for the twenty DESIGN-named variants that row is the DESIGN §3.3 row, and for the two added variants it is the §1.5 row that records them, because DESIGN §3.3 has no 409 row for a management write conflict.

The Retriable column of that catalogue is not carried into the type as anything but a boolean. Six rows are marked `Yes` in DESIGN §3.3 and are retriable unconditionally: `RateLimitExceeded`, `LinkUnavailable`, `CircuitBreakerOpen`, `ConnectionTimeout`, `RequestTimeout`, `IdleTimeout`. `DownstreamError` is marked `Depends`, and that cell is resolved non-retriable for the `Retry-After` gate in this feature (§1.5): the retry decision for a 502 belongs to the caller and to `cpt-cf-oagw-feature-data-plane-proxy`, which owns upstream-failure policy. The two §1.5-added variants are marked `No`, since neither an alias conflict nor a match conflict is answered differently by an unchanged retry. Every remaining row is marked `No` and is non-retriable.

**Steps**:
1. [x] - `p1` - Resolve the GTS `type` identifier for the variant from the catalogue; the identifier is taken verbatim from the DESIGN §3.3 row and is never synthesized from the variant name - `inst-errmap-type`
2. [x] - `p1` - **IF** the source tag is `gateway` - `inst-errmap-gateway-if`
   1. [x] - `p1` - Emit a response with the row's status code and GTS `type`, `Content-Type: application/problem+json`, and the RFC 9457 fields `type`, `title`, `status`, `detail`, and `instance` - `inst-errmap-problem`
   2. [x] - `p1` - Attach every present `ErrorContext` member to the problem body as an extension field - `inst-errmap-extensions`
   3. [x] - `p1` - Set `X-OAGW-Error-Source: gateway` - `inst-errmap-gateway-header`
3. [x] - `p1` - **ELSE** - `inst-errmap-else`
   1. [x] - `p1` - Pass the upstream response through with its body and content type unmodified and set `X-OAGW-Error-Source: upstream`; the problem-details mapping is not applied to an upstream-sourced failure - `inst-errmap-upstream`
4. [x] - `p1` - **IF** the variant is one of the six catalogue rows DESIGN §3.3 marks `Yes` (`RateLimitExceeded`, `LinkUnavailable`, `CircuitBreakerOpen`, `ConnectionTimeout`, `RequestTimeout`, `IdleTimeout`) and carries `retry_after_seconds` - `inst-errmap-retry-if`
   1. [x] - `p1` - Emit `Retry-After` from `retry_after_seconds` on the response - `inst-errmap-retry-emit`
5. [x] - `p1` - **RETURN** the response - `inst-errmap-return`

`detail` never contains credential material, a `cred://` reference value, or any configuration value (`cpt-cf-oagw-nfr-credential-isolation`). The `trace_id` extension field is populated when a correlation context is available; the correlation identifier itself is supplied by `cpt-cf-oagw-feature-observability`, which owns the audit and metrics surface.

### GTS Type Catalogue Provisioning

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-type-catalog-provisioning`

**Input**: the OAGW GTS identifier constants and a resolved `TypesRegistryClient`.

**Output**: a provisioned catalogue, or the per-entry failures that fail the post-init phase.

The catalogue this feature owns:

| GTS identifier | Kind | Declares |
|----------------|------|----------|
| `gts.cf.core.oagw.upstream.v1~` | type schema | Upstream aggregate, mirroring `schemas/upstream.v1.schema.json` |
| `gts.cf.core.oagw.route.v1~` | type schema | Route aggregate, mirroring `schemas/route.v1.schema.json` |
| `gts.cf.core.oagw.protocol.v1~` | type schema | Protocol base type |
| `gts.cf.core.oagw.auth_plugin.v1~` | type schema | Auth plugin base type |
| `gts.cf.core.oagw.guard_plugin.v1~` | type schema | Guard plugin base type |
| `gts.cf.core.oagw.transform_plugin.v1~` | type schema | Transform plugin base type |
| `gts.cf.core.errors.err.v1~` | type schema | Gateway error base type, the namespace of every problem `type` |
| `gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1` | instance | HTTP protocol value |
| `gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1` | instance | gRPC protocol value |
| `gts.cf.core.errors.err.v1~cf.oagw.{slug}.v1` | instance, 21 distinct identifiers | One per error type in the DESIGN §3.3 catalogue with its slug taken verbatim from that table, plus one each for the two management-conflict variants added per §1.5. There are 21 identifiers for the 22 variants: 19 carry the catalogue's 20 DESIGN rows, because `RouteError` and `ValidationError` share `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`, and the other two are `gts.cf.core.errors.err.v1~cf.oagw.alias.conflict.v1` and `gts.cf.core.errors.err.v1~cf.oagw.match.conflict.v1`, one each for `AliasConflict` and `MatchConflict`. |

Built-in plugin instance identifiers (`gts.cf.core.oagw.{auth,guard,transform}_plugin.v1~cf.core.oagw.{name}.v1`) are deliberately absent: they are registered by `cpt-cf-oagw-feature-plugin-system`, which owns the built-in catalogue and its resolvability rules.

**Steps**:
1. [x] - `p1` - Collect the base type schemas and the instances they own into one batch, ordered so a parent type never follows its children - `inst-catalog-collect`
2. [x] - `p1` - **TRY** - `inst-catalog-try`
   1. [x] - `p1` - Call the registry's batch register; the registry sorts the batch lexicographically by GTS identifier, which additionally guarantees a base type (suffix `~`) precedes its instances - `inst-catalog-register`
3. [x] - `p1` - **CATCH** a catastrophic SDK failure such as an unavailable backend - `inst-catalog-catch`
   1. [x] - `p1` - Fail the post-init phase; do not retry and do not continue with an unprovisioned catalogue - `inst-catalog-catch-handle`
4. [x] - `p1` - **FOR EACH** per-entry result - `inst-catalog-loop`
   1. [x] - `p1` - **IF** the entry succeeded, or the identifier is already registered with byte-identical content, count it as success - `inst-catalog-idempotent`
   2. [x] - `p1` - **ELSE** record the entry as a failure with its GTS identifier and the typed error - `inst-catalog-collect-failure`
5. [x] - `p1` - **IF** any entry failed - `inst-catalog-fail-if`
   1. [x] - `p1` - Log every failing identifier at ERROR and fail the post-init phase; never skip an entry with a warning - `inst-catalog-fail`
6. [x] - `p1` - **RETURN** the provisioned catalogue - `inst-catalog-return`

The batch register call is bounded by the platform SDK's default call timeout. A timeout is classified as the same catastrophic case the CATCH above handles: it fails the post-init phase and is neither retried nor partially re-issued.

Entries registered before a failure remain in the registry. That is the rollback story rather than a defect: registration is idempotent for identical content, so a restart re-runs provisioning and the already-present entries succeed instead of conflicting. No other state is created by this feature, so there is nothing to revert beyond the process itself.

**Operator recovery for a foreign identifier.** When the conflict case fires — an identifier already registered with content this gear does not own — the ERROR log carries the conflicting GTS identifier, the typed error the registry returned, and the gear that owns the existing entry. The operator inspects the existing registry entry against the catalogue row above: an entry left behind by an earlier run whose content no longer matches what the row declares makes the stale registry entry the thing to remove, while an identifier that does not match what this gear is registering makes the catalogue slug in this gear's configuration the thing to correct. Which of the two remediations applies is decided from that comparison, and it is an operator decision: provisioning never overwrites or deletes an entry it does not own. Already-registered entries are never rolled back; a corrected run re-runs provisioning and the entries already present succeed instead of conflicting.

## 4. States (CDSL)

### Gear Foundation Provisioning State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-gear-foundation-lifecycle`

**States**: `Unregistered`, `Configured`, `TypeCatalogProvisioned`, `Ready`, `StartupFailed`

**Initial State**: `Unregistered`

**Transitions**:
1. [x] - `p1` - **FROM** `Unregistered` **TO** `Configured` **WHEN** the init hook completes with a validated `OagwConfig` - `inst-state-init-ok`
2. [x] - `p1` - **FROM** `Unregistered` **TO** `StartupFailed` **WHEN** configuration loading or validation fails - `inst-state-init-fail`
3. [x] - `p1` - **FROM** `Configured` **TO** `TypeCatalogProvisioned` **WHEN** every catalogue entry is registered and the registry catalogue is in its ready phase (§1.4 run-level assumptions) - `inst-state-provisioned`
4. [x] - `p1` - **FROM** `Configured` **TO** `StartupFailed` **WHEN** any catalogue entry fails to register - `inst-state-provision-fail`
5. [x] - `p1` - **FROM** `TypeCatalogProvisioned` **TO** `Ready` **WHEN** the gear reports readiness; `Ready` serves only the router mount point, which carries no routes in this feature - `inst-state-ready`
6. [x] - `p1` - `StartupFailed` is terminal: the runtime aborts startup, so no later feature ever observes a half-initialized gear - `inst-state-terminal`

The ordering constraint behind transition 3 is the registry's own two-phase catalogue. A registry catalogue starts in a staging phase that accepts registrations without validating them, and flips to its ready phase in the registry's post-init hook, after every gear's init has returned. OAGW provisioning therefore runs in the post-init phase, never during init: provisioning during init would write into a staging catalogue and bypass the validation that makes the catalogue trustworthy. The declared dependency on the types-registry gear keeps the runtime's topological order such that the registry's ready-flip precedes the OAGW post-init hook.

## 5. Definitions of Done

### Gear Registration and Router Mount Point

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-registration`

The system **MUST** register `oagw` as a ToolKit gear from `gear.rs` with the public surface exported from `lib.rs`, expose the empty gear-relative router mount point at `/oagw/v1` for later features to populate, and register no endpoint, handler, or route of its own (`cpt-cf-oagw-adr-request-routing`).

**Implements**:

- `cpt-cf-oagw-flow-gear-init`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:

- API: none — the mount point only; no `METHOD /path` is registered by this feature
- DB: none — no persistence; `cpt-cf-oagw-db-schema` is claimed by later features
- DB Table: none
- Entities: `OagwGear`

### Configuration Surface and Validation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-config-surface`

The system **MUST** load `OagwConfig` through the platform configuration provider with exactly the five configurable families tabulated under `cpt-cf-oagw-algo-config-load-validate`, apply the declared defaults when `oagw.config` is absent, reject unknown keys, reject out-of-range values, and fail gear init on any violation. `allow_http_upstream` **MUST** be the only input that lets the `Scheme` value object admit the `http` literal at write time.

**Implements**:

- `cpt-cf-oagw-flow-gear-init`
- `cpt-cf-oagw-algo-config-load-validate`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`, `cpt-cf-oagw-constraint-https-only`

**Touches**:

- API: none
- DB: none
- DB Table: none
- Entities: `OagwConfig`, `SsrfPolicy`, `Scheme`

### Domain Model Types and Layering

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-domain-model-types`

The system **MUST** declare `Upstream`, `Route`, and `Plugin` together with `Endpoint`, `ServerConfig`, `AuthConfig`, `HeadersConfig`, `RateLimitConfig`, `CorsConfig`, and `PluginsConfig` in the domain layer, and **MUST** keep every one of them free of transport and persistence types (`cpt-cf-oagw-design-layers`). The shipped JSON Schemas are mirrored property for property where a schema declares the type: `Upstream` **MUST** carry the properties `schemas/upstream.v1.schema.json` declares, and `Route` **MUST** carry the properties `schemas/route.v1.schema.json` declares plus the properties added per §1.5 — the route-level `cors` object, and the `priority` and `enabled` route attributes of the DESIGN §3.1 Route class. `Plugin` and the sub-configuration types follow the DESIGN §3.1 domain model, for which no schema is shipped. Alias, hostname, and scheme value objects **MUST** be normalized and validated once, here, and reused by every later feature rather than re-derived at each call site.

**Implements**:

- `cpt-cf-oagw-algo-alias-normalize`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:

- API: none
- DB: none
- DB Table: none
- Entities: `Upstream`, `Route`, `Plugin`, `Endpoint`, `ServerConfig`, `AuthConfig`, `HeadersConfig`, `RateLimitConfig`, `CorsConfig`, `PluginsConfig`, `Alias`, `Hostname`, `Scheme`

Domain purity is a compile-time gate, not a review convention. Domain types are marked so that a field whose type comes from `http`, `axum`, `sea_orm`, or `sqlx` fails macro expansion with a message naming the offending field, and the `DE0301` and `DE0308` lint rules reject infrastructure imports in the domain layer. A domain type that needs a transport shape — an HTTP status code, a header map — forces a mapping type in `api/rest` instead of a dependency.

### GTS Identifier Catalogue and Provisioning

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gts-type-catalog`

The system **MUST** declare the GTS identifier constants for the upstream, route, protocol, error, and plugin base types, and **MUST** provision the catalogue through the `types_registry` SDK during the post-init phase, idempotently for identical content and failing closed on any per-entry error (`cpt-cf-oagw-contract-types-registry`). The provisioned error instance set **MUST** include the two management-conflict instances added per the §1.5 row citing DECOMPOSITION §1.3(9), so a 409 answered anywhere in the gear carries a provisioned GTS `type`. Built-in plugin instance identifiers are **NOT** provisioned here; `cpt-cf-oagw-feature-plugin-system` registers them.

**Implements**:

- `cpt-cf-oagw-flow-type-provisioning`
- `cpt-cf-oagw-algo-type-catalog-provisioning`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:

- API: none
- DB: none
- DB Table: none
- Entities: the base type schemas and instances tabulated under `cpt-cf-oagw-algo-type-catalog-provisioning`

### Canonical Error Type and Mapping

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-catalogue`

The system **MUST** declare `DomainError` with one variant per error type in the DESIGN §3.3 catalogue (`cpt-cf-oagw-interface-api`) plus the two management-conflict variants added per §1.5, each carrying its HTTP status, its GTS `type` identifier, its Retriable flag, and its `gateway`/`upstream` source tag, and **MUST** map gateway-sourced errors to `application/problem+json` while leaving upstream-sourced failures as passthrough (`cpt-cf-oagw-principle-rfc9457`, `cpt-cf-oagw-adr-error-source-distinction`). `AliasConflict` and `MatchConflict` **MUST** answer 409 with the GTS `type` identifiers §1.5 records and **MUST NOT** be retriable. Every gateway error answered anywhere in the gear goes through this mapping, so later features add no second error serialization path.

**Implements**:

- `cpt-cf-oagw-algo-error-mapping`

**Constraints**: none from DESIGN §2.2; the governing elements are the principle and the ADR cited above.

**Touches**:

- API: none — the mapping is a domain-to-transport function with no endpoint exposing it in this feature
- DB: none
- DB Table: none
- Entities: `DomainError`, `ErrorSource`, `ErrorContext`

gear-foundation is the single definition point for `Scheme`, `SsrfPolicy`, `OagwGear`, `ErrorSource`, and `ErrorContext` — the five types named in the Entities lines above and listed in DECOMPOSITION §2.1 as the gear's shared vocabulary — and later features consume them and **MUST NOT** redeclare them.

### Colocated Test Placement and Coverage

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-test-placement`

The system **MUST** deliver this feature's unit and integration tests colocated under `gears/system/oagw/oagw/tests/`, covering configuration validation, alias and hostname normalization, the error mapping, the type-catalogue provisioning, and the domain layer's freedom from infrastructure types, and **MUST NOT** add any test under `testing/e2e/gears/oagw/`.

**Implements**:

- `cpt-cf-oagw-algo-config-load-validate`
- `cpt-cf-oagw-algo-alias-normalize`
- `cpt-cf-oagw-algo-error-mapping`
- `cpt-cf-oagw-algo-type-catalog-provisioning`

**Constraints**: none from DESIGN §2.2; this is the DECOMPOSITION §1.3(3) placement deviation recorded in §1.5.

**Touches**:

- API: none
- DB: none
- DB Table: none
- Entities: none — tests only

## 6. Acceptance Criteria

- [x] The `oagw` gear registers with the ToolKit runtime, and `lib.rs` exports the gear type, `OagwConfig`, the domain model types, the GTS identifier constants, and `DomainError`.
- [x] A request to any `/oagw/v1/...` path is answered by no OAGW handler: the mount point exists and carries no routes.
- [x] With no `oagw.config` section at all, gear init succeeds and every configuration key takes its declared default from the table under `cpt-cf-oagw-algo-config-load-validate`.
- [x] `proxy_timeout_secs: 0`, a non-positive `token_cache_capacity`, or an unknown key under `oagw.config` fails gear init and aborts startup.
- [x] `allow_http_upstream: false` makes the `http` scheme literal fail the write-time admission check, and `allow_http_upstream: true` makes it pass, with no other input affecting the outcome.
- [x] Alias and hostname inputs are normalized to ASCII lowercase with trailing dots stripped, and an RFC 1123-invalid hostname is rejected.
- [x] `api.openai.com` and `api.openai.com:8443` normalize to distinct alias values.
- [x] Each of the 22 `DomainError` variants — the catalogue's 20 DESIGN §3.3 rows plus the two §1.5-added management-conflict variants — has one mapping that yields that variant's HTTP status and GTS `type` identifier, and the two added variants both yield 409.
- [x] A gateway-sourced error response carries `Content-Type: application/problem+json`, `X-OAGW-Error-Source: gateway`, and no credential material or configuration value in `detail`.
- [x] An upstream-sourced failure is passed through with `X-OAGW-Error-Source: upstream` and is not rewritten into a problem body.
- [x] The six catalogue rows DESIGN §3.3 marks `Yes` (`RateLimitExceeded`, `LinkUnavailable`, `CircuitBreakerOpen`, `ConnectionTimeout`, `RequestTimeout`, `IdleTimeout`) emit `Retry-After` when they carry `retry_after_seconds`, and every other row — including the `DownstreamError` row resolved non-retriable in §1.5 — does not.
- [x] After startup, every base type schema and instance in the provisioning table is resolvable through the types-registry — the 21 distinct error identifiers covering the 22 variants included, the two §1.5-added management-conflict identifiers among them — and re-registering an entry with byte-identical content does not fail startup.
- [x] A per-entry registration failure prevents the gear from ever reporting readiness, and the failing GTS identifiers are logged.
- [x] No domain type references a transport or persistence type; the domain-purity compile gate and the `DE0301`/`DE0308` lint rules pass.
- [x] Every test for this feature lives under `gears/system/oagw/oagw/tests/`, passes there, and no test is added under `testing/e2e/gears/oagw/`.
- [x] Each of `Upstream` and `Route` carries the properties its shipped JSON Schema declares for that type — no missing, extra, or renamed field — verified against that schema's `properties` set, with `Route` additionally carrying the §1.5-added `cors`, `priority`, and `enabled`.
