---
title: "OAGW Feature Specification - Outbound API Gateway"
system: "cpt-cf-oagw"
kind: "FEATURE"
version: "1.0.0"
status: "drafted"
changelog:
  - "1.0.0: Initial authoring of the nine decomposed OAGW features from DECOMPOSITION.md (gear-foundation p1, domain-model-repositories p2, control-plane-api p3, plugin-system p3, error-semantics p4, rate-limiting p4, cors-handling p4, data-plane-proxy p5, observability-audit p5)."
---

# Feature: OAGW Gear Foundation & Registration


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Boot the OAGW Gear](#boot-the-oagw-gear)
  - [Route Inbound Requests Between Planes](#route-inbound-requests-between-planes)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Load Gear Configuration](#load-gear-configuration)
  - [Register REST and OpenAPI Surface](#register-rest-and-openapi-surface)
- [4. States (CDSL)](#4-states-cdsl)
  - [OAGW Gear Lifecycle](#oagw-gear-lifecycle)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Gear Registration](#gear-registration)
  - [Configuration Module](#configuration-module)
  - [Module Skeleton and Plane Separation](#module-skeleton-and-plane-separation)
  - [REST and OpenAPI Registration](#rest-and-openapi-registration)
  - [Plane Routing](#plane-routing)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Additional Context (optional)](#7-additional-context-optional)
- [8. Feature Context](#8-feature-context)
  - [8.1 Overview](#81-overview)
  - [8.2 Purpose](#82-purpose)
  - [8.3 Actors](#83-actors)
  - [8.4 References](#84-references)
- [9. Actor Flows (CDSL)](#9-actor-flows-cdsl)
  - [Persist and Read Tenant-Scoped Configuration Records](#persist-and-read-tenant-scoped-configuration-records)
  - [Resolve Effective Configuration Across the Tenant Hierarchy](#resolve-effective-configuration-across-the-tenant-hierarchy)
- [10. Processes / Business Logic (CDSL)](#10-processes--business-logic-cdsl)
  - [Validate Upstream Endpoint at the Configuration Boundary](#validate-upstream-endpoint-at-the-configuration-boundary)
  - [Merge Hierarchical Configuration by Sharing Mode](#merge-hierarchical-configuration-by-sharing-mode)
  - [Apply Tenant Scoping to Repository Operations](#apply-tenant-scoping-to-repository-operations)
- [11. States (CDSL)](#11-states-cdsl)
- [12. Definitions of Done](#12-definitions-of-done)
  - [Domain Entity Model](#domain-entity-model)
  - [Repository Traits and In-Memory Storage](#repository-traits-and-in-memory-storage)
  - [Hierarchical Configuration Merge](#hierarchical-configuration-merge)
  - [Tenant-Scoped Repository Reads and Writes](#tenant-scoped-repository-reads-and-writes)
  - [Configuration-Boundary SSRF Guard](#configuration-boundary-ssrf-guard)
  - [Reserved gRPC Match Shape](#reserved-grpc-match-shape)
- [13. Acceptance Criteria](#13-acceptance-criteria)
- [14. Additional Context (optional)](#14-additional-context-optional)
- [15. Feature Context](#15-feature-context)
  - [15.1 Overview](#151-overview)
  - [15.2 Purpose](#152-purpose)
  - [15.3 Actors](#153-actors)
  - [15.4 References](#154-references)
- [16. Actor Flows (CDSL)](#16-actor-flows-cdsl)
  - [Manage Upstreams](#manage-upstreams)
  - [Manage Routes](#manage-routes)
  - [Manage Plugins](#manage-plugins)
- [17. Processes / Business Logic (CDSL)](#17-processes--business-logic-cdsl)
  - [Derive Upstream Alias](#derive-upstream-alias)
  - [Validate Alias Uniqueness and Immutability](#validate-alias-uniqueness-and-immutability)
  - [Validate Route Match-Rule Uniqueness](#validate-route-match-rule-uniqueness)
  - [Authorize Management Operation via GTS Permissions](#authorize-management-operation-via-gts-permissions)
  - [Determine Target-Host Routing Mode for Multi-Endpoint Upstreams](#determine-target-host-routing-mode-for-multi-endpoint-upstreams)
  - [Validate Management DTOs](#validate-management-dtos)
  - [Apply OData List Query Parameters](#apply-odata-list-query-parameters)
- [18. States (CDSL)](#18-states-cdsl)
- [19. Definitions of Done](#19-definitions-of-done)
  - [Upstream CRUD](#upstream-crud)
  - [Route CRUD with Sub-Resources](#route-crud-with-sub-resources)
  - [Plugin CRUD and Source Retrieval](#plugin-crud-and-source-retrieval)
  - [Alias Derivation and Enforcement](#alias-derivation-and-enforcement)
  - [GTS Permission Enforcement](#gts-permission-enforcement)
  - [Tenant-Scoped Visibility and Ancestor 404](#tenant-scoped-visibility-and-ancestor-404)
  - [OData List Semantics](#odata-list-semantics)
- [20. Acceptance Criteria](#20-acceptance-criteria)
- [21. Additional Context (optional)](#21-additional-context-optional)
- [22. Feature Context](#22-feature-context)
  - [22.1 Overview](#221-overview)
  - [22.2 Purpose](#222-purpose)
  - [22.3 Actors](#223-actors)
  - [22.4 References](#224-references)
- [23. Actor Flows (CDSL)](#23-actor-flows-cdsl)
  - [Create, Bind, and Inspect a Custom Plugin](#create-bind-and-inspect-a-custom-plugin)
  - [Authenticate a Proxy Request via an Auth Plugin](#authenticate-a-proxy-request-via-an-auth-plugin)
- [24. Processes / Business Logic (CDSL)](#24-processes--business-logic-cdsl)
  - [Resolve Plugin by GTS Identifier](#resolve-plugin-by-gts-identifier)
  - [Execute Plugin Chain in Deterministic Order](#execute-plugin-chain-in-deterministic-order)
  - [Resolve OAuth2 Token with Internal Cache](#resolve-oauth2-token-with-internal-cache)
  - [Enforce Required Headers Guard](#enforce-required-headers-guard)
  - [Resolve cred:// Secret Reference](#resolve-cred-secret-reference)
  - [Execute Custom Starlark Plugin in a Sandbox](#execute-custom-starlark-plugin-in-a-sandbox)
  - [Garbage-Collect Expired Custom Plugins](#garbage-collect-expired-custom-plugins)
- [25. States (CDSL)](#25-states-cdsl)
  - [Custom Plugin Lifecycle](#custom-plugin-lifecycle)
- [26. Definitions of Done](#26-definitions-of-done)
  - [Plugin Trait Contracts and Registries](#plugin-trait-contracts-and-registries)
  - [Built-in Plugins](#built-in-plugins)
  - [Catalog-Only GTS Identifiers](#catalog-only-gts-identifiers)
  - [Deterministic Execution Order](#deterministic-execution-order)
  - [Plugin Identification and Binding Conflict Detection](#plugin-identification-and-binding-conflict-detection)
  - [OAuth2 Client-Credentials Token Cache](#oauth2-client-credentials-token-cache)
  - [Credential Isolation](#credential-isolation)
  - [Immutable Sandboxed Custom Plugins](#immutable-sandboxed-custom-plugins)
- [27. Acceptance Criteria](#27-acceptance-criteria)
- [28. Additional Context (optional)](#28-additional-context-optional)
- [29. Feature Context](#29-feature-context)
  - [29.1 Overview](#291-overview)
  - [29.2 Purpose](#292-purpose)
  - [29.3 Actors](#293-actors)
  - [29.4 References](#294-references)
- [30. Actor Flows (CDSL)](#30-actor-flows-cdsl)
  - [Consume a Gateway Error Response](#consume-a-gateway-error-response)
  - [Consume an Upstream Error Passthrough](#consume-an-upstream-error-passthrough)
- [31. Processes / Business Logic (CDSL)](#31-processes--business-logic-cdsl)
  - [Build an RFC 9457 Problem+JSON Instance](#build-an-rfc-9457-problemjson-instance)
  - [Classify Error Retriability](#classify-error-retriability)
  - [Attach X-OAGW-Error-Source Headers](#attach-x-oagw-error-source-headers)
  - [Propagate request_id into Error Envelopes](#propagate-request_id-into-error-envelopes)
- [32. States (CDSL)](#32-states-cdsl)
- [33. Definitions of Done](#33-definitions-of-done)
  - [Central RFC 9457 Error Envelope](#central-rfc-9457-error-envelope)
  - [GTS Instance Catalog](#gts-instance-catalog)
  - [Retriability Classification](#retriability-classification)
  - [Error-Source Header Contract](#error-source-header-contract)
  - [Correlation Propagation](#correlation-propagation)
- [34. Acceptance Criteria](#34-acceptance-criteria)
- [35. Additional Context (optional)](#35-additional-context-optional)
- [36. Feature Context](#36-feature-context)
  - [36.1 Overview](#361-overview)
  - [36.2 Purpose](#362-purpose)
  - [36.3 Actors](#363-actors)
  - [36.4 References](#364-references)
- [37. Actor Flows (CDSL)](#37-actor-flows-cdsl)
  - [Rate-Limit Exceeded with the Reject Strategy](#rate-limit-exceeded-with-the-reject-strategy)
  - [Rate-Limit Exceeded with Queue or Degrade Strategy](#rate-limit-exceeded-with-queue-or-degrade-strategy)
- [38. Processes / Business Logic (CDSL)](#38-processes--business-logic-cdsl)
  - [Consume Tokens from a Dual-Rate Bucket](#consume-tokens-from-a-dual-rate-bucket)
  - [Compute Effective Rate via Hierarchical min()](#compute-effective-rate-via-hierarchical-min)
  - [Emit the 429 RateLimitExceeded Rejection](#emit-the-429-ratelimitexceeded-rejection)
  - [Resolve Configuration via the Control-Plane Cache](#resolve-configuration-via-the-control-plane-cache)
- [39. States (CDSL)](#39-states-cdsl)
- [40. Definitions of Done](#40-definitions-of-done)
  - [Dual-Rate Token-Bucket Limiter](#dual-rate-token-bucket-limiter)
  - [Hierarchical min() Inheritance](#hierarchical-min-inheritance)
  - [Reject, Queue, and Degrade Strategies](#reject-queue-and-degrade-strategies)
  - [429 Rejection Headers](#429-rejection-headers)
  - [Control-Plane Configuration Caching](#control-plane-configuration-caching)
- [41. Acceptance Criteria](#41-acceptance-criteria)
- [42. Additional Context (optional)](#42-additional-context-optional)
- [43. Feature Context](#43-feature-context)
  - [43.1 Overview](#431-overview)
  - [43.2 Purpose](#432-purpose)
  - [43.3 Actors](#433-actors)
  - [43.4 References](#434-references)
- [44. Actor Flows (CDSL)](#44-actor-flows-cdsl)
  - [Handle a CORS Preflight Request](#handle-a-cors-preflight-request)
  - [Handle an Actual Cross-Origin Request](#handle-an-actual-cross-origin-request)
- [45. Processes / Business Logic (CDSL)](#45-processes--business-logic-cdsl)
  - [Detect a CORS Preflight](#detect-a-cors-preflight)
  - [Validate Origin by Exact Match](#validate-origin-by-exact-match)
  - [Validate Method Against Allowed Methods](#validate-method-against-allowed-methods)
  - [Merge CORS Configuration Across the Hierarchy](#merge-cors-configuration-across-the-hierarchy)
- [46. States (CDSL)](#46-states-cdsl)
- [47. Definitions of Done](#47-definitions-of-done)
  - [Permissive Preflight Fast Path](#permissive-preflight-fast-path)
  - [Actual-Request Enforcement](#actual-request-enforcement)
  - [CORS Configuration and Secure Defaults](#cors-configuration-and-secure-defaults)
- [48. Acceptance Criteria](#48-acceptance-criteria)
- [49. Additional Context (optional)](#49-additional-context-optional)
- [50. Feature Context](#50-feature-context)
  - [50.1 Overview](#501-overview)
  - [50.2 Purpose](#502-purpose)
  - [50.3 Actors](#503-actors)
  - [50.4 References](#504-references)
- [51. Actor Flows (CDSL)](#51-actor-flows-cdsl)
  - [Execute a Proxy Request](#execute-a-proxy-request)
  - [Proxy a Request Targeting a Disabled Resource](#proxy-a-request-targeting-a-disabled-resource)
  - [Route to a Specific Endpoint via X-OAGW-Target-Host](#route-to-a-specific-endpoint-via-x-oagw-target-host)
- [52. Processes / Business Logic (CDSL)](#52-processes--business-logic-cdsl)
  - [Resolve Upstream by Alias Across the Tenant Hierarchy](#resolve-upstream-by-alias-across-the-tenant-hierarchy)
  - [Match Route by Method Allowlist and Longest Path Prefix](#match-route-by-method-allowlist-and-longest-path-prefix)
  - [Apply the Effective Configuration Merge](#apply-the-effective-configuration-merge)
  - [Apply Target-Host Selection and the Behavior Matrix](#apply-target-host-selection-and-the-behavior-matrix)
  - [Apply Header Transforms and Hop-by-Hop Stripping](#apply-header-transforms-and-hop-by-hop-stripping)
  - [Enforce SSRF on the Outbound Surface](#enforce-ssrf-on-the-outbound-surface)
  - [Proxy Bodies with Streaming Passthrough](#proxy-bodies-with-streaming-passthrough)
  - [Enforce the Circuit Breaker](#enforce-the-circuit-breaker)
  - [Attribute Response Source](#attribute-response-source)
- [53. States (CDSL)](#53-states-cdsl)
  - [Circuit Breaker State Machine](#circuit-breaker-state-machine)
- [54. Definitions of Done](#54-definitions-of-done)
  - [Alias Resolution and Shadowing](#alias-resolution-and-shadowing)
  - [Enable/Disable Semantics](#enabledisable-semantics)
  - [Route Matching](#route-matching)
  - [Effective Configuration Application](#effective-configuration-application)
  - [Plugin Chain Execution](#plugin-chain-execution)
  - [Target-Host Selection Matrix](#target-host-selection-matrix)
  - [Header Matrix and Hop-by-Hop Stripping](#header-matrix-and-hop-by-hop-stripping)
  - [Streaming, Timeout, and No-Retry Policy](#streaming-timeout-and-no-retry-policy)
  - [Outbound SSRF, Payload, and Smuggling Enforcement](#outbound-ssrf-payload-and-smuggling-enforcement)
  - [Circuit Breaker](#circuit-breaker)
  - [Error-Source Attribution](#error-source-attribution)
- [55. Acceptance Criteria](#55-acceptance-criteria)
- [56. Additional Context (optional)](#56-additional-context-optional)
- [57. Feature Context](#57-feature-context)
  - [57.1 Overview](#571-overview)
  - [57.2 Purpose](#572-purpose)
  - [57.3 Actors](#573-actors)
  - [57.4 References](#574-references)
- [58. Actor Flows (CDSL)](#58-actor-flows-cdsl)
  - [Collect Metrics Over the Admin Surface](#collect-metrics-over-the-admin-surface)
  - [Correlate an Event Across Phases](#correlate-an-event-across-phases)
- [59. Processes / Business Logic (CDSL)](#59-processes--business-logic-cdsl)
  - [Record Per-Request Metrics](#record-per-request-metrics)
  - [Record Rate-Limit Observations](#record-rate-limit-observations)
  - [Record Routing and Upstream Observations](#record-routing-and-upstream-observations)
  - [Emit the Audit Log Entry](#emit-the-audit-log-entry)
  - [Bound Label Cardinality](#bound-label-cardinality)
- [60. States (CDSL)](#60-states-cdsl)
  - [Circuit Breaker Metric State](#circuit-breaker-metric-state)
- [61. Definitions of Done](#61-definitions-of-done)
  - [Metrics Vocabulary and Exposition](#metrics-vocabulary-and-exposition)
  - [Audit Log](#audit-log)
  - [Request-ID Correlation](#request-id-correlation)
  - [Error-Source Attribution in Observations](#error-source-attribution-in-observations)
- [62. Acceptance Criteria](#62-acceptance-criteria)
- [63. Additional Context (optional)](#63-additional-context-optional)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-gear-foundation`
## 1. Feature Context

- [ ] `p1` - `cpt-cf-oagw-feature-gear-foundation`

### 1.1 Overview

Register the `oagw` gear with the ToolKit host and establish the crate skeleton, gear configuration module, Control Plane / Data Plane separation, and REST/OpenAPI wiring that every other feature builds on. This is the zero-AR dependency root of the OAGW decomposition.

### 1.2 Purpose

This feature provides the enabling scaffolding: the gear registration (dependencies `credstore`, `types_registry`, `tenant_resolver`, `authz_resolver`, capabilities `[rest, stateful]`, lifecycle entry `init`/`serve`), the `gears.oagw.config` module with its documented defaults, the DDD-Light module layout, and the path-based routing between planes defined by `cpt-cf-oagw-adr-request-routing`. It addresses `cpt-cf-oagw-component-model` and `cpt-cf-oagw-constraint-toolkit-deploy`.

**Requirements**: None — gear registration and configuration scaffolding has no directly traceable PRD FR/NFR; requirement coverage is provided transitively by every dependent feature this foundation enables.

**Principles**: None

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Deploys and starts the gear inside the ToolKit host and observes successful registration and plane routing |

### 1.4 References

- **PRD**: [PRD.md](./PRD.md)
- **Design**: [DESIGN.md](./DESIGN.md)
- **ADRs**: `cpt-cf-oagw-adr-request-routing` (path-based routing rules), `cpt-cf-oagw-adr-state-management` (single-deployment component model)
- **Parent feature**: `cpt-cf-oagw-feature-gear-foundation`
- **Dependencies**: None
- **Related design elements**: `cpt-cf-oagw-component-model`, `cpt-cf-oagw-constraint-toolkit-deploy`, `cpt-cf-oagw-tech-dependencies`, `cpt-cf-oagw-topology-deployment`

## 2. Actor Flows (CDSL)

### Boot the OAGW Gear

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-gear-foundation-boot`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The gear boots with all capability and dependency declarations honored and starts serving.

**Error Scenarios**:
- A required SDK client cannot be acquired, or the gear configuration is invalid, so initialization fails and the gear does not serve.

**Steps**:
1. [ ] - `p1` - Platform operator deploys the oagw gear as a single executable inside the ToolKit host behind the platform api-gateway (which applies the `/api/oagw/v1/...` prefix) - `inst-gf-boot-deploy`
2. [ ] - `p1` - Host loads the `gears.oagw.config` YAML; the gear resolves it via `config_or_default`, applying documented defaults for absent keys - `inst-gf-boot-config`
3. [ ] - `p1` - Gear initialization acquires the in-process SDK clients for `credstore`, `types_registry`, `tenant_resolver`, and `authz_resolver` through the GearCtx client hub - `inst-gf-boot-deps`
4. [ ] - `p1` - Gear registers its REST surface via `RestApiCapability::register_rest` using unprefixed paths and registers OpenAPI schemas via utoipa - `inst-gf-boot-rest`
5. [ ] - `p1` - Gear lifecycle entry `init` completes and `serve` starts; the gear begins accepting inbound requests - `inst-gf-boot-serve`
6. [ ] - `p1` - **IF** a required SDK client is unavailable **OR** the gear configuration is malformed **THEN** initialization fails with an explicit error and the gear does not enter `serve` - `inst-gf-boot-fail`
7. [ ] - `p1` - **RETURN** serving state - `inst-gf-boot-return`

### Route Inbound Requests Between Planes

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-gear-foundation-plane-routing`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Every inbound request is dispatched to the correct service boundary by its path prefix.

**Error Scenarios**:
- A path that matches no registered surface returns a not-found outcome.

**Steps**:
1. [ ] - `p1` - Inbound HTTP request arrives at the API handler after the api-gateway prefix is applied - `inst-gf-route-ingress`
2. [ ] - `p1` - **IF** path begins with `/upstreams`, `/routes`, or `/plugins` **THEN** dispatch to the Control Plane service boundary - `inst-gf-route-cp`
3. [ ] - `p1` - **ELSE IF** path begins with `/proxy` **THEN** dispatch to the Data Plane service boundary - `inst-gf-route-dp`
4. [ ] - `p1` - **ELSE RETURN** not-found outcome (no handler registered for the path) - `inst-gf-route-404`
5. [ ] - `p1` - **RETURN** the plane's response to the caller - `inst-gf-route-return`

## 3. Processes / Business Logic (CDSL)

### Load Gear Configuration

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-gear-foundation-load-config`

**Input**: Raw `gears.oagw.config` YAML (may be absent)

**Output**: Resolved `OagwConfig` with all documented default values applied

**Steps**:
1. [ ] - `p1` - Read the `gears.oagw.config` YAML block if present - `inst-gf-cfg-read`
2. [ ] - `p1` - **IF** `proxy_timeout_secs` is absent **THEN** apply the default `2` - `inst-gf-cfg-timeout`
3. [ ] - `p1` - **IF** `allow_http_upstream` is absent **THEN** apply the default `false` - `inst-gf-cfg-http`
4. [ ] - `p1` - **IF** `ssrf_policy.enabled` is absent **THEN** apply the default `true` - `inst-gf-cfg-ssrf`
5. [ ] - `p1` - **IF** `token_cache_ttl_secs` is absent **THEN** apply the default `300` - `inst-gf-cfg-token-ttl`
6. [ ] - `p1` - **IF** `token_cache_capacity` is absent **THEN** apply the default `10000` - `inst-gf-cfg-token-cap`
7. [ ] - `p1` - **RETURN** the resolved `OagwConfig` as the single source of runtime knob defaults - `inst-gf-cfg-return`

### Register REST and OpenAPI Surface

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-gear-foundation-register-surface`

**Input**: The set of REST handler routes defined by dependent features

**Output**: A registered axum router with utoipa-generated OpenAPI documentation

**Steps**:
1. [ ] - `p1` - Register all unprefixed route paths with `RestApiCapability::register_rest` using `OperationBuilder` route registration - `inst-gf-reg-routes`
2. [ ] - `p1` - Register DTO schemas and endpoint documentation with utoipa - `inst-gf-reg-openapi`
3. [ ] - `p1` - **IF** registration fails (duplicate path or schema conflict) **THEN** fail gear initialization - `inst-gf-reg-fail`
4. [ ] - `p1` - **RETURN** the registered REST router - `inst-gf-reg-return`

## 4. States (CDSL)

### OAGW Gear Lifecycle

- [ ] `p1` - **ID**: `cpt-cf-oagw-state-gear-foundation-lifecycle`

**States**: REGISTERED, INITIALIZED, SERVING, STOPPED

**Initial State**: REGISTERED

**Transitions**:
1. [ ] - `p1` - **FROM** REGISTERED **TO** INITIALIZED **WHEN** the Gear lifecycle `init` completes (SDK clients acquired, configuration resolved, REST surface registered) - `inst-gf-state-init`
2. [ ] - `p1` - **FROM** INITIALIZED **TO** SERVING **WHEN** the `serve` entry starts accepting inbound requests - `inst-gf-state-serve`
3. [ ] - `p1` - **FROM** SERVING **TO** STOPPED **WHEN** the ToolKit host shuts the gear down - `inst-gf-state-stop`
4. [ ] - `p1` - **FROM** REGISTERED **TO** STOPPED **WHEN** initialization fails (dependency unavailable or invalid configuration) - `inst-gf-state-init-fail`

## 5. Definitions of Done

### Gear Registration

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-register`

The system **MUST** register the `oagw` gear with the ToolKit host declaring dependencies `credstore`, `types_registry`, `tenant_resolver`, `authz_resolver`, capabilities `[rest, stateful]`, and a Gear-trait lifecycle with `init`/`serve` entries, deployable as a single executable per the ToolKit constraint.

**Implements**:
- `cpt-cf-oagw-flow-gear-foundation-boot`
- `cpt-cf-oagw-state-gear-foundation-lifecycle`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: none (registration scaffolding)
- DB: none
- Entities: `Gear` (ToolKit host integration), `OagwConfig`

### Configuration Module

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-config`

The system **MUST** provide a gear configuration module resolving `gears.oagw.config` YAML via `config_or_default` with defaults `proxy_timeout_secs=2`, `allow_http_upstream=false`, `ssrf_policy.enabled=true`, `token_cache_ttl_secs=300`, `token_cache_capacity=10000`, and expose the resolved `OagwConfig` to all dependent features.

**Implements**:
- `cpt-cf-oagw-algo-gear-foundation-load-config`
- `cpt-cf-oagw-flow-gear-foundation-boot`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: none
- DB: none
- Entities: `OagwConfig`

### Module Skeleton and Plane Separation

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-skeleton`

The system **MUST** establish the DDD-Light module skeleton (`api/rest`, `domain`, `infra`) inside the single `oagw` crate with the Control Plane (`ControlPlaneService`) and Data Plane (`DataPlaneService`) kept as separate domain-service boundaries within one deployment unit.

**Implements**:
- `cpt-cf-oagw-flow-gear-foundation-plane-routing`
- `cpt-cf-oagw-state-gear-foundation-lifecycle`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: none
- DB: none
- Entities: `ControlPlaneService`, `DataPlaneService`

### REST and OpenAPI Registration

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-rest-openapi`

The system **MUST** register REST handlers with unprefixed paths through `RestApiCapability::register_rest` (the platform api-gateway applies the `/api/oagw/v1/...` prefix) and produce OpenAPI documentation via utoipa for all registered endpoints.

**Implements**:
- `cpt-cf-oagw-algo-gear-foundation-register-surface`
- `cpt-cf-oagw-flow-gear-foundation-boot`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: all management and proxy endpoints registered by dependent features (prefixed by the api-gateway)
- DB: none
- Entities: `RestApiCapability` (wiring)

### Plane Routing

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-plane-routing`

The system **MUST** route inbound requests by path prefix: `/upstreams/*`, `/routes/*`, `/plugins/*` to the Control Plane and `/proxy/*` to the Data Plane, returning a not-found outcome for any unmatched prefix.

**Implements**:
- `cpt-cf-oagw-flow-gear-foundation-plane-routing`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: `* /upstreams/*`, `* /routes/*`, `* /plugins/*`, `* /proxy/*` (dispatch classification)
- DB: none
- Entities: none

## 6. Acceptance Criteria

- [ ] Deploying and starting the gear results in the declared capabilities `[rest, stateful]` and the four SDK dependencies (`credstore`, `types_registry`, `tenant_resolver`, `authz_resolver`) being declared in the gear registration
- [ ] Starting the gear with no `gears.oagw.config` block applies all five documented defaults (`proxy_timeout_secs=2`, `allow_http_upstream=false`, `ssrf_policy.enabled=true`, `token_cache_ttl_secs=300`, `token_cache_capacity=10000`)
- [ ] An inbound request on `/api/oagw/v1/upstreams/*`, `/api/oagw/v1/routes/*`, or `/api/oagw/v1/plugins/*` is dispatched to the Control Plane boundary, while `/api/oagw/v1/proxy/*` is dispatched to the Data Plane boundary
- [ ] An inbound request matching no registered prefix returns a not-found outcome
- [ ] The crate exposes `api/rest`, `domain`, and `infra` modules with distinct `ControlPlaneService` and `DataPlaneService` boundaries
- [ ] OpenAPI documentation is generated for every registered unprefixed endpoint
- [ ] The gear transitions through the Gear Lifecycle states REGISTERED, INITIALIZED, SERVING on a successful boot and does not serve when initialization fails

## 7. Additional Context (optional)

Automated tests for this gear live in the crate's own test modules (unit and integration tests under the `oagw` crate), not under `testing/e2e/gears/oagw/`, which is reserved for the acceptance suite. The acceptance suite may additionally exercise boot/registration behavior end-to-end.


# Feature: OAGW Domain Model & Repositories

- [ ] `p2` - **ID**: `cpt-cf-oagw-featstatus-domain-model-repositories`

## 8. Feature Context

- [ ] `p2` - `cpt-cf-oagw-feature-domain-model-repositories`

### 8.1 Overview

Establish the domain entities, repository contracts, in-memory storage, tenant scoping, and hierarchical configuration-merge semantics that both the Control Plane and the Data Plane consume, along with the SSRF guard applied to upstream endpoint URLs at the configuration boundary.

### 8.2 Purpose

This feature delivers the domain layer (`cpt-cf-oagw-component-domain-layer`) and the storage half of the infrastructure layer (`cpt-cf-oagw-component-infra-layer`): entities (`Upstream`, `Route`, `RouteMatch`, `RouteMethod`, `ServerConfig`/`Endpoint`, `HostEntry`/`TargetHost`, `Plugin`/`PluginConfig`, vault-aware `cred://` secret references), repository traits with in-memory DashMap implementations mirroring the `cpt-cf-oagw-db-schema` tables, and the per-field sharing-mode merge semantics required by `cpt-cf-oagw-fr-config-layering` and `cpt-cf-oagw-fr-hierarchical-config`.

**Requirements**: `cpt-cf-oagw-fr-config-layering`, `cpt-cf-oagw-fr-hierarchical-config`

**Principles**: `cpt-cf-oagw-principle-tenant-scope`

### 8.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Triggers configuration writes that flow through the repository traits and observes effective configuration resolved across the tenant hierarchy |

### 8.4 References

- **PRD**: [PRD.md](./PRD.md)
- **Design**: [DESIGN.md](./DESIGN.md)
- **ADRs**: `cpt-cf-oagw-adr-data-plane-caching` (configuration merge feeds cache resolution), `cpt-cf-oagw-adr-state-management` (repository-backed config state)
- **Parent feature**: `cpt-cf-oagw-feature-domain-model-repositories`
- **Dependencies**: `cpt-cf-oagw-feature-gear-foundation`
- **Related design elements**: `cpt-cf-oagw-component-domain-layer`, `cpt-cf-oagw-component-infra-layer`, `cpt-cf-oagw-db-schema`, `cpt-cf-oagw-dbtable-upstream`, `cpt-cf-oagw-dbtable-route`, `cpt-cf-oagw-dbtable-route-http-match`, `cpt-cf-oagw-dbtable-route-grpc-match`, `cpt-cf-oagw-dbtable-route-method`, `cpt-cf-oagw-dbtable-upstream-tag`, `cpt-cf-oagw-dbtable-route-tag`, `cpt-cf-oagw-constraint-multi-sql`, `cpt-cf-oagw-constraint-in-memory-storage`

## 9. Actor Flows (CDSL)

### Persist and Read Tenant-Scoped Configuration Records

- [ ] `p2` - **ID**: `cpt-cf-oagw-flow-domain-model-repositories-persist`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A validated configuration write is persisted through the repository traits with a server-generated UUID and is later read back scoped to the owning tenant.

**Error Scenarios**:
- An upstream endpoint URL fails the configuration-boundary SSRF guard and the write is rejected.

**Steps**:
1. [ ] - `p2` - A validated upstream/route/plugin configuration write reaches the domain repository boundary - `inst-dm-persist-arrive`
2. [ ] - `p2` - **IF** the write contains upstream endpoint URLs **THEN** run the SSRF guard at the configuration boundary (scheme allowlist, RFC 1123 hostname validation) - `inst-dm-persist-ssrf`
3. [ ] - `p2` - **IF** SSRF validation fails **THEN** reject the write with a validation outcome and do not persist - `inst-dm-persist-ssrf-reject`
4. [ ] - `p2` - DB: INSERT into `oagw_upstream` / `oagw_route` / `oagw_plugin` (server-generated UUID `id`, `tenant_id`, alias, config blobs) through the matching repository trait - `inst-dm-persist-insert`
5. [ ] - `p2` - DB: INSERT dependent rows (`oagw_route_http_match`, `oagw_route_method`, tag and plugin binding rows) with cascade-safe keys - `inst-dm-persist-deps`
6. [ ] - `p2` - DB: SELECT the stored record by `(tenant_id, id)` through the repository trait to confirm tenant-scoped read-back - `inst-dm-persist-read`
7. [ ] - `p2` - **RETURN** the stored entity with its generated identifier - `inst-dm-persist-return`

### Resolve Effective Configuration Across the Tenant Hierarchy

- [ ] `p2` - **ID**: `cpt-cf-oagw-flow-domain-model-repositories-effective`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The effective configuration for a downstream tenant reflects the per-field sharing-mode merge of all ancestors down to the tenant.

**Error Scenarios**:
- An ancestor-enforced field cannot be overridden by the descendant (the merge honors `enforce`).

**Steps**:
1. [ ] - `p2` - Operator requests the effective configuration for a target tenant path - `inst-dm-eff-request`
2. [ ] - `p2` - Walk the tenant chain from the target tenant toward the root collecting per-field configurations and sharing modes - `inst-dm-eff-walk`
3. [ ] - `p2` - Apply the per-field merge rules (auth override-or-force, rate `min()`, plugin concatenation, CORS union-or-force, tag union) - `inst-dm-eff-merge`
4. [ ] - `p2` - **IF** a field is `enforce` at an ancestor **THEN** the descendant contribution to that field is ignored - `inst-dm-eff-enforce`
5. [ ] - `p2` - **RETURN** the merged effective configuration - `inst-dm-eff-return`

## 10. Processes / Business Logic (CDSL)

### Validate Upstream Endpoint at the Configuration Boundary

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-domain-model-repositories-ssrf-validate`

**Input**: An upstream endpoint URL (scheme, host, port)

**Output**: Valid or rejected with a validation outcome

**Steps**:
1. [ ] - `p2` - **IF** scheme is not in the allowlist (`https` by default; plaintext `http` only when `allow_http_upstream` is enabled) **THEN** reject - `inst-dm-ssrf-scheme`
2. [ ] - `p2` - **IF** hostname does not conform to RFC 1123 hostname rules **THEN** reject - `inst-dm-ssrf-host`
3. [ ] - `p2` - **RETURN** valid - `inst-dm-ssrf-return`

### Merge Hierarchical Configuration by Sharing Mode

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-domain-model-repositories-merge`

**Input**: Tenant-chain configurations with per-field sharing modes (`private`, `inherit`, `enforce`)

**Output**: Effective merged configuration

**Steps**:
1. [ ] - `p2` - **FOR EACH** field category (auth, rate limits, plugins, CORS, tags) apply its sharing-mode rule - `inst-dm-merge-loop`
2. [ ] - `p2` - Auth: **IF** `inherit` **AND** the descendant specifies credentials **THEN** descendant overrides; **IF** `enforce` **THEN** the ancestor value is forced - `inst-dm-merge-auth`
3. [ ] - `p2` - Rate limits: effective rate is `min(ancestor, descendant)` across the hierarchy (descendants can only be stricter) - `inst-dm-merge-rates`
4. [ ] - `p2` - Plugins: concatenate ancestor bindings before descendant bindings (enforced plugins cannot be removed) - `inst-dm-merge-plugins`
5. [ ] - `p2` - CORS: **IF** `inherit` **THEN** union descendant origins with ancestor origins; **IF** `enforce` **THEN** the ancestor set is forced - `inst-dm-merge-cors`
6. [ ] - `p2` - Tags: add-only union (descendants may add tags but cannot remove inherited tags) - `inst-dm-merge-tags`
7. [ ] - `p2` - **RETURN** the merged effective configuration - `inst-dm-merge-return`

### Apply Tenant Scoping to Repository Operations

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-domain-model-repositories-tenant-scope`

**Input**: Repository operation request with a `SecurityContext` tenant

**Output**: Tenant-scoped result or not-found outcome

**Steps**:
1. [ ] - `p2` - Bind the calling tenant id from the `SecurityContext` propagated by `tenant_resolver` - `inst-dm-scope-bind`
2. [ ] - `p2` - DB: SELECT/INSERT/UPDATE/DELETE records filtered by the tenant id in the repository trait implementation - `inst-dm-scope-filter`
3. [ ] - `p2` - **IF** the record belongs to an ancestor tenant **THEN** it is not visible through the management surface (not-found outcome) - `inst-dm-scope-ancestor`
4. [ ] - `p2` - **RETURN** the scoped result - `inst-dm-scope-return`

## 11. States (CDSL)

Not applicable — the domain entities in this feature (`Upstream`, `Route`, `ServerConfig`/`Endpoint`, `HostEntry`/`TargetHost`, `Plugin`) carry no lifecycle state machine here: the upstream/route `enabled` flag is a boolean policy attribute honored during resolution (enforced by the Data-Plane Proxy feature), and the `Plugin` lifecycle (immutability and garbage collection) is owned by the Plugin System feature. Repository state is in-memory per `cpt-cf-oagw-constraint-in-memory-storage`.

## 12. Definitions of Done

### Domain Entity Model

- [ ] `p2` - **ID**: `cpt-cf-oagw-dod-domain-model-repositories-entities`

The system **MUST** implement the domain entities `Upstream`, `Route`, `RouteMatch`, `RouteMethod`, `ServerConfig`/`Endpoint` (pool sharing identical protocol/scheme/port), `HostEntry`/`TargetHost`, `Plugin`/`PluginConfig`, vault-aware `cred://` secret references, and per-field sharing-mode configuration, keyed by UUID (`tenant_id`, `upstream_id`, `plugin_uuid`) identifiers.

**Implements**:
- `cpt-cf-oagw-flow-domain-model-repositories-persist`
- `cpt-cf-oagw-algo-domain-model-repositories-merge`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`, `cpt-cf-oagw-constraint-in-memory-storage`

**Touches**:
- API: none (domain layer)
- DB: `oagw_upstream`, `oagw_route`, `oagw_route_http_match`, `oagw_route_method`, `oagw_plugin`
- Entities: `Upstream`, `Route`, `RouteMatch`, `RouteMethod`, `ServerConfig`, `Endpoint`, `HostEntry`, `TargetHost`, `Plugin`, `PluginConfig`

### Repository Traits and In-Memory Storage

- [ ] `p2` - **ID**: `cpt-cf-oagw-dod-domain-model-repositories-repo-traits`

The system **MUST** define the `UpstreamRepository`, `RouteRepository`, and `PluginRepository` traits and provide in-memory DashMap implementations whose shapes mirror the `oagw_*` relational tables (server-generated UUIDs, `(tenant_id, alias)` uniqueness, nullable `plugin_uuid` for named plugins), so a future SeaORM/`toolkit-db` swap satisfies the same traits.

**Implements**:
- `cpt-cf-oagw-flow-domain-model-repositories-persist`
- `cpt-cf-oagw-algo-domain-model-repositories-tenant-scope`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`, `cpt-cf-oagw-constraint-in-memory-storage`

**Touches**:
- API: none
- DB: `oagw_upstream`, `oagw_route`, `oagw_route_http_match`, `oagw_route_method`, `oagw_route_grpc_match`, `oagw_upstream_tag`, `oagw_route_tag`
- Entities: `Upstream`, `Route`, `RouteMatch`, `RouteMethod`, `Plugin`, `PluginConfig`

### Hierarchical Configuration Merge

- [ ] `p2` - **ID**: `cpt-cf-oagw-dod-domain-model-repositories-merge`

The system **MUST** merge effective configuration across the tenant hierarchy per sharing mode: auth override-under-`inherit`/forced-under-`enforce`, rate limits via `min(ancestor, descendant)`, plugin concatenation (ancestor first), CORS union-under-`inherit`/forced-under-`enforce`, and add-only tag union, with `private` hiding a configuration from descendants.

**Implements**:
- `cpt-cf-oagw-algo-domain-model-repositories-merge`
- `cpt-cf-oagw-flow-domain-model-repositories-effective`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`, `cpt-cf-oagw-constraint-in-memory-storage`

**Touches**:
- API: none
- DB: `oagw_upstream`, `oagw_route`, `oagw_upstream_tag`, `oagw_route_tag`
- Entities: `Upstream`, `Route`, `RateLimitConfig`, `CorsConfig`, `PluginsConfig`

### Tenant-Scoped Repository Reads and Writes

- [ ] `p2` - **ID**: `cpt-cf-oagw-dod-domain-model-repositories-tenant-scope`

The system **MUST** scope all repository reads and writes to the calling tenant through the `SecurityContext`, keep ancestor-owned records invisible to descendants at the management boundary, and provide no cross-tenant access path.

**Implements**:
- `cpt-cf-oagw-algo-domain-model-repositories-tenant-scope`
- `cpt-cf-oagw-flow-domain-model-repositories-persist`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`, `cpt-cf-oagw-constraint-in-memory-storage`

**Touches**:
- API: none
- DB: `oagw_upstream`, `oagw_route`, `oagw_plugin`
- Entities: `Upstream`, `Route`, `Plugin`

### Configuration-Boundary SSRF Guard

- [ ] `p2` - **ID**: `cpt-cf-oagw-dod-domain-model-repositories-ssrf`

The system **MUST** apply the SSRF guard to upstream endpoint URLs at the configuration boundary: scheme allowlist (`https` by default, plaintext only when `allow_http_upstream` is enabled) and RFC 1123 hostname validation, rejecting non-conforming endpoints before persistence.

**Implements**:
- `cpt-cf-oagw-algo-domain-model-repositories-ssrf-validate`
- `cpt-cf-oagw-flow-domain-model-repositories-persist`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`, `cpt-cf-oagw-constraint-in-memory-storage`

**Touches**:
- API: none
- DB: `oagw_upstream` (server definition)
- Entities: `ServerConfig`, `Endpoint`

### Reserved gRPC Match Shape

- [ ] `p2` - **ID**: `cpt-cf-oagw-dod-domain-model-repositories-grpc-reserved`

The system **MUST** reserve the in-memory shape of `oagw_route_grpc_match` (`route_id`, `service`, `method`) with no gRPC proxy code path implemented or reachable in this or any dependent feature (gRPC is Phase 3 and out of scope).

**Implements**:
- `cpt-cf-oagw-dod-domain-model-repositories-repo-traits`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`, `cpt-cf-oagw-constraint-in-memory-storage`

**Touches**:
- API: none
- DB: `oagw_route_grpc_match`
- Entities: `RouteMatch` (gRPC variant, reserved)

## 13. Acceptance Criteria

- [ ] A configuration write for an upstream with a non-allowlisted scheme (e.g., plaintext `http` while `allow_http_upstream=false`) is rejected at the configuration boundary and nothing is persisted
- [ ] An upstream endpoint whose hostname is not RFC 1123 valid is rejected at the configuration boundary
- [ ] A tenant-scoped read from tenant B cannot return records owned by tenant A (no cross-tenant access)
- [ ] Merging configurations with sharing modes yields: descendant overrides under `inherit`, forced ancestor values under `enforce`, `min(ancestor, descendant)` for rate limits, ancestor-then-descendant plugin concatenation, origin union under `inherit` CORS, and add-only tag union
- [ ] Records persisted through the repository traits are read back with server-generated UUID identifiers and tenant scoping intact
- [ ] The `oagw_route_grpc_match` shape is present in the domain/storage model but no gRPC code path is reachable

## 14. Additional Context (optional)

This feature defines the domain model and persistence layer only; no API surface is introduced here beyond the repository trait contracts consumed by the Control-Plane Management API. Automated tests for this gear live in the crate's own test modules (unit and integration tests under the `oagw` crate), not under `testing/e2e/gears/oagw/`, which is reserved for the acceptance suite.

# Feature: OAGW Control-Plane Management API

- [ ] `p3` - **ID**: `cpt-cf-oagw-featstatus-control-plane-api`

## 15. Feature Context

- [ ] `p3` - `cpt-cf-oagw-feature-control-plane-api`

### 15.1 Overview

Expose the tenant-scoped REST management surface for upstreams, routes, and plugins with DTO validation, alias derivation, GTS permission checks, ancestor invisibility, OData-style list semantics, and RFC 9457 error serialization, per the `cpt-cf-oagw-seq-management-crud-flow` orchestration.

### 15.2 Purpose

This feature implements `cpt-cf-oagw-component-api-layer` and `cpt-cf-oagw-component-control-plane`: management CRUD for upstreams (`cpt-cf-oagw-fr-upstream-mgmt`) and routes (`cpt-cf-oagw-fr-route-mgmt`), plus plugin CRUD and source retrieval, honoring the `(tenant_id, alias)` and match-rule uniqueness invariants from the domain layer, the alias derivation rules from `cpt-cf-oagw-fr-alias-resolution`, and GTS permission enforcement.

**Requirements**: `cpt-cf-oagw-fr-upstream-mgmt`, `cpt-cf-oagw-fr-route-mgmt`

**Principles**: None

### 15.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Creates, reads, updates, and deletes upstreams, routes, and plugins across the tenant hierarchy under their permissions and may set sharing mode `enforce` |
| `cpt-cf-oagw-actor-tenant-admin` | Manages upstreams, routes, and plugins within their own tenant, overriding `inherit` configurations where permitted |

### 15.4 References

- **PRD**: [PRD.md](./PRD.md)
- **Design**: [DESIGN.md](./DESIGN.md)
- **ADRs**: `cpt-cf-oagw-adr-request-routing` (management routing, alias/target-host input, plugin deletion conflict), `cpt-cf-oagw-adr-error-source-distinction` (management errors serialized via the error framework)
- **Parent feature**: `cpt-cf-oagw-feature-control-plane-api`
- **Dependencies**: `cpt-cf-oagw-feature-domain-model-repositories`
- **Related design elements**: `cpt-cf-oagw-component-api-layer`, `cpt-cf-oagw-component-control-plane`, `cpt-cf-oagw-seq-management-crud-flow`, `cpt-cf-oagw-interface-management-api`

## 16. Actor Flows (CDSL)

### Manage Upstreams

- [ ] `p3` - **ID**: `cpt-cf-oagw-flow-control-plane-api-upstream-crud`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- An upstream is created, listed, read, replaced, and deleted with alias derivation, uniqueness enforcement, and tenant scoping.

**Error Scenarios**:
- Validation fails (400), the alias already exists in the tenant (409), the caller lacks permission (403), or the resource belongs to an ancestor tenant (404).

**Steps**:
1. [ ] - `p3` - Caller authenticates via `toolkit-auth` bearer token; the handler extracts the `SecurityContext` and checks the `gts.cf.core.oagw.upstream.v1~:{create;override;read;delete}` permission through `authz_resolver` - `inst-cp-up-create-authz`
2. [ ] - `p3` - API: POST /api/oagw/v1/upstreams (body: server endpoints, protocol, auth, headers, rate_limit, cors, plugins, tags, sharing modes) - `inst-cp-up-create-api`
3. [ ] - `p3` - Validate the DTO (endpoint format, pool uniformity, CORS `allow_credentials`/wildcard conflict, `cred://` reference validity) - `inst-cp-up-create-validate`
4. [ ] - `p3` - Derive the alias from the endpoint set or accept the explicit alias per endpoint type - `inst-cp-up-create-alias`
5. [ ] - `p3` - **IF** `(tenant_id, alias)` already exists **THEN** return 409 conflict via the error framework - `inst-cp-up-create-conflict`
6. [ ] - `p3` - DB: INSERT `oagw_upstream` (and tag rows) through `UpstreamRepository` - `inst-cp-up-create-persist`
7. [ ] - `p3` - API: RETURN 201 Created with the `UpstreamConfig` representation - `inst-cp-up-create-return`
8. [ ] - `p3` - API: GET /api/oagw/v1/upstreams (OData list params), GET /api/oagw/v1/upstreams/{id} (404 when absent or ancestor-owned) - `inst-cp-up-read`
9. [ ] - `p3` - API: PUT /api/oagw/v1/upstreams/{id} (replace; alias immutable once set, attempting to change it returns a validation error) - `inst-cp-up-update`
10. [ ] - `p3` - API: DELETE /api/oagw/v1/upstreams/{id} (cascades to routes; 404 when ancestor-owned) - `inst-cp-up-delete`

### Manage Routes

- [ ] `p3` - **ID**: `cpt-cf-oagw-flow-control-plane-api-route-crud`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A route referencing an existing upstream is created with HTTP match rules, method allowlists, and plugin bindings, satisfying match-rule uniqueness.

**Error Scenarios**:
- The referenced upstream does not exist or is ancestor-owned (validation error / 404), or two enabled routes collide on `(path_prefix, priority)` for the same method (409).

**Steps**:
1. [ ] - `p3` - Caller authenticates and is checked for `gts.cf.core.oagw.route.v1~:{create;override;read;delete}` - `inst-cp-route-create-authz`
2. [ ] - `p3` - API: POST /api/oagw/v1/routes (body: upstream_id, match rules (methods, path_prefix, query allowlist, suffix mode), priority, rate_limit, cors, plugins, tags) - `inst-cp-route-create-api`
3. [ ] - `p3` - **IF** the referenced upstream does not exist **THEN** return a validation error - `inst-cp-route-create-upstream-ref`
4. [ ] - `p3` - Enforce match-rule uniqueness: no two enabled routes under the same upstream share `(path_prefix, priority)` for the same method - `inst-cp-route-create-unique`
5. [ ] - `p3` - DB: INSERT `oagw_route`, `oagw_route_http_match`, `oagw_route_method`, and plugin binding rows (contiguous positions from 0) - `inst-cp-route-create-persist`
6. [ ] - `p3` - API: RETURN 201 Created with the `RouteConfig` representation - `inst-cp-route-create-return`
7. [ ] - `p3` - API: GET /api/oagw/v1/routes (OData list params), GET /api/oagw/v1/routes/{id} (404 when absent or ancestor-owned) - `inst-cp-route-read`
8. [ ] - `p3` - API: PUT /api/oagw/v1/routes/{id} (replace; `upstream_id` immutable, changing it returns a validation error) - `inst-cp-route-update`
9. [ ] - `p3` - API: DELETE /api/oagw/v1/routes/{id} (404 when absent or ancestor-owned) - `inst-cp-route-delete`

### Manage Plugins

- [ ] `p3` - **ID**: `cpt-cf-oagw-flow-control-plane-api-plugin-crud`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A custom plugin is created, listed, read, and its Starlark source retrieved; an unreferenced plugin is deleted with 204.

**Error Scenarios**:
- Deleting a referenced plugin returns 409 `PluginInUse` with a `referenced_by` list of upstream/route identifiers.

**Steps**:
1. [ ] - `p3` - Caller authenticates and is checked for `gts.cf.core.oagw.{auth_plugin|guard_plugin|transform_plugin}.v1~:{create;override;read;delete}` per plugin type - `inst-cp-plugin-create-authz`
2. [ ] - `p3` - API: POST /api/oagw/v1/plugins (body: plugin_type, name, config_schema, source_code) - `inst-cp-plugin-create-api`
3. [ ] - `p3` - **IF** `(tenant_id, name)` already exists **THEN** return 409 conflict - `inst-cp-plugin-create-conflict`
4. [ ] - `p3` - DB: INSERT `oagw_plugin` with a server-generated UUID (plugin is immutable after creation; there is no PUT) - `inst-cp-plugin-create-persist`
5. [ ] - `p3` - API: GET /api/oagw/v1/plugins (OData list params), GET /api/oagw/v1/plugins/{id} (404 when absent or ancestor-owned) - `inst-cp-plugin-read`
6. [ ] - `p3` - API: GET /api/oagw/v1/plugins/{id}/source (returns the Starlark source) - `inst-cp-plugin-source`
7. [ ] - `p3` - API: DELETE /api/oagw/v1/plugins/{id} - `inst-cp-plugin-delete`
8. [ ] - `p3` - **IF** the plugin is referenced by any upstream or route binding **THEN** return 409 `PluginInUse` with a `referenced_by` shape listing the referencing upstream and route identifiers - `inst-cp-plugin-delete-conflict`
9. [ ] - `p3` - **ELSE** remove the `oagw_plugin` row and return 204 No Content - `inst-cp-plugin-delete-ok`

## 17. Processes / Business Logic (CDSL)

### Derive Upstream Alias

- [ ] `p3` - **ID**: `cpt-cf-oagw-algo-control-plane-api-derive-alias`

**Input**: The upstream endpoint pool (scheme/host/port) and an optional user-provided alias

**Output**: Derived alias, or an explicit-alias requirement

**Steps**:
1. [ ] - `p3` - **IF** the pool has a single hostname endpoint **THEN** alias is the hostname (port dropped for standard ports); a user-provided alias on a hostname endpoint that differs from the auto-derived value is rejected with 400, while providing the exact derived value is tolerated silently as an idempotent no-op - `inst-cp-alias-single`
2. [ ] - `p3` - **IF** the pool has multiple hostname endpoints **THEN** compute the longest common domain suffix (at least two labels) - `inst-cp-alias-common-suffix`
3. [ ] - `p3` - **IF** the common suffix is a bare public suffix according to the public suffix list (e.g., `co.uk`) **THEN** an explicit alias is required - `inst-cp-alias-psl`
4. [ ] - `p3` - **IF** the pool contains IP addresses **OR** no common suffix exists **THEN** an explicit alias is required - `inst-cp-alias-explicit`
5. [ ] - `p3` - Normalize the alias to ASCII lowercase and strip trailing dots; resolution is case-insensitive - `inst-cp-alias-normalize`
6. [ ] - `p3` - **RETURN** the derived or explicit alias - `inst-cp-alias-return`

### Validate Alias Uniqueness and Immutability

- [ ] `p3` - **ID**: `cpt-cf-oagw-algo-control-plane-api-validate-alias`

**Input**: Candidate alias, tenant id, and (for updates) the existing upstream

**Output**: Unique-and-mutable, immutable-violation, or conflict outcome

**Steps**:
1. [ ] - `p3` - DB: SELECT `oagw_upstream` WHERE `(tenant_id, alias)` = the candidate - `inst-cp-alias-check-exists`
2. [ ] - `p3` - **IF** a different upstream already holds the alias in the tenant **THEN** return conflict - `inst-cp-alias-conflict`
3. [ ] - `p3` - **IF** updating an existing upstream **AND** the alias would change **THEN** return an immutability validation error - `inst-cp-alias-immutable`
4. [ ] - `p3` - **RETURN** unique - `inst-cp-alias-ok`

### Validate Route Match-Rule Uniqueness

- [ ] `p3` - **ID**: `cpt-cf-oagw-algo-control-plane-api-validate-match-rule`

**Input**: Route candidate (methods, path_prefix, priority, enabled) and parent upstream

**Output**: Unique or conflict outcome

**Steps**:
1. [ ] - `p3` - DB: SELECT enabled routes of the upstream sharing the candidate method - `inst-cp-match-select`
2. [ ] - `p3` - **FOR EACH** existing enabled route **IF** `(path_prefix, priority)` equals the candidate for a shared method **THEN** return conflict - `inst-cp-match-collision`
3. [ ] - `p3` - **RETURN** unique - `inst-cp-match-ok`

### Authorize Management Operation via GTS Permissions

- [ ] `p3` - **ID**: `cpt-cf-oagw-algo-control-plane-api-authorize`

**Input**: SecurityContext (principal + tenant chain), resource type, action

**Output**: Authorized or denied (403)

**Steps**:
1. [ ] - `p3` - Resolve the action and resource GTS set (`gts.cf.core.oagw.{upstream|route|auth_plugin|guard_plugin|transform_plugin}.v1~:{create;override;read;delete}`) for the operation - `inst-cp-authz-resolve`
2. [ ] - `p3` - Invoke `authz_resolver` with the `SecurityContext` - `inst-cp-authz-check`
3. [ ] - `p3` - **IF** denied **THEN** return 403 - `inst-cp-authz-deny`
4. [ ] - `p3` - **RETURN** authorized - `inst-cp-authz-ok`

### Determine Target-Host Routing Mode for Multi-Endpoint Upstreams

- [ ] `p3` - **ID**: `cpt-cf-oagw-algo-control-plane-api-target-host-mode`

**Input**: The upstream endpoint pool and its alias type

**Output**: Target-host routing mode consumed at proxy time

**Steps**:
1. [ ] - `p3` - **IF** the pool has a single endpoint **THEN** mode is single (the `X-OAGW-Target-Host` header is optional but validated if present) - `inst-cp-th-single`
2. [ ] - `p3` - **IF** the pool has multiple endpoints **AND** the alias is explicit (no common suffix) **THEN** mode is multi-explicit (header optional, round-robin default) - `inst-cp-th-multi-explicit`
3. [ ] - `p3` - **IF** the pool has multiple endpoints **AND** the alias has a common suffix **THEN** mode is multi-common-suffix (header required at proxy time) - `inst-cp-th-multi-suffix`
4. [ ] - `p3` - **RETURN** the assigned routing mode stored with the upstream - `inst-cp-th-return`

### Validate Management DTOs

- [ ] `p3` - **ID**: `cpt-cf-oagw-algo-control-plane-api-validate-dto`

**Input**: Inbound management DTO

**Output**: Valid or RFC 9457 validation error (400)

**Steps**:
1. [ ] - `p3` - **IF** an endpoint is malformed (bad scheme/host/port) **THEN** return validation error - `inst-cp-dto-endpoint`
2. [ ] - `p3` - **IF** endpoints in a pool disagree on protocol, scheme, or port **THEN** return validation error - `inst-cp-dto-pool`
3. [ ] - `p3` - **IF** a `cred://` reference is malformed **THEN** return validation error - `inst-cp-dto-cred`
4. [ ] - `p3` - **IF** CORS config combines `allow_credentials: true` with the wildcard origin `*` **THEN** return validation error (rejected at configuration validation time) - `inst-cp-dto-cors`
5. [ ] - `p3` - **RETURN** valid - `inst-cp-dto-ok`

### Apply OData List Query Parameters

- [ ] `p3` - **ID**: `cpt-cf-oagw-algo-control-plane-api-odata`

**Input**: List request with `$filter`, `$select`, `$orderby`, `$top`, `$skip`

**Output**: Filtered, selected, ordered, paged result

**Steps**:
1. [ ] - `p3` - Parse the OData query parameters - `inst-cp-odata-parse`
2. [ ] - `p3` - Apply `$top` with default 50 and maximum 100, and `$skip` - `inst-cp-odata-paging`
3. [ ] - `p3` - Apply `$filter`, `$select`, and `$orderby` to the tenant-scoped collection - `inst-cp-odata-apply`
4. [ ] - `p3` - **RETURN** the paged result - `inst-cp-odata-return`

## 18. States (CDSL)

Not applicable — the resources managed by this feature carry no explicit lifecycle state machine: upstreams and routes expose an `enabled` boolean honored during resolution (enforced by the Data-Plane Proxy feature), and custom plugins are immutable after creation with their lifecycle owned by the Plugin System feature. Ancestor invisibility and deletion cascades are behavioral rules, not entity states.

## 19. Definitions of Done

### Upstream CRUD

- [ ] `p3` - **ID**: `cpt-cf-oagw-dod-control-plane-api-upstream-crud`

The system **MUST** provide tenant-scoped create, list, get, replace, and delete operations for upstreams at `/api/oagw/v1/upstreams` with server-generated UUIDs, endpoint pool validation, alias derivation/enforcement, and `(tenant_id, alias)` uniqueness.

**Implements**:
- `cpt-cf-oagw-flow-control-plane-api-upstream-crud`
- `cpt-cf-oagw-algo-control-plane-api-derive-alias`
- `cpt-cf-oagw-algo-control-plane-api-validate-alias`
- `cpt-cf-oagw-algo-control-plane-api-validate-dto`

**Constraints**: None

**Touches**:
- API: `POST /api/oagw/v1/upstreams`, `GET /api/oagw/v1/upstreams`, `GET /api/oagw/v1/upstreams/{id}`, `PUT /api/oagw/v1/upstreams/{id}`, `DELETE /api/oagw/v1/upstreams/{id}`
- DB: `oagw_upstream`, `oagw_upstream_tag`
- Entities: `Upstream`, `ServerConfig`, `Endpoint`, `HeadersConfig`, `RateLimitConfig`, `CorsConfig`, `PluginsConfig`

### Route CRUD with Sub-Resources

- [ ] `p3` - **ID**: `cpt-cf-oagw-dod-control-plane-api-route-crud`

The system **MUST** provide tenant-scoped create, list, get, replace, and delete operations for routes at `/api/oagw/v1/routes` including HTTP match/method sub-resources and plugin bindings, with `upstream_id` immutable, upstream-reference validation, and match-rule uniqueness enforcement.

**Implements**:
- `cpt-cf-oagw-flow-control-plane-api-route-crud`
- `cpt-cf-oagw-algo-control-plane-api-validate-match-rule`

**Constraints**: None

**Touches**:
- API: `POST /api/oagw/v1/routes`, `GET /api/oagw/v1/routes`, `GET /api/oagw/v1/routes/{id}`, `PUT /api/oagw/v1/routes/{id}`, `DELETE /api/oagw/v1/routes/{id}`
- DB: `oagw_route`, `oagw_route_http_match`, `oagw_route_method`, `oagw_route_tag`, `oagw_route_plugin`
- Entities: `Route`, `RouteMatch`, `RouteMethod`

### Plugin CRUD and Source Retrieval

- [ ] `p3` - **ID**: `cpt-cf-oagw-dod-control-plane-api-plugin-crud`

The system **MUST** provide tenant-scoped custom-plugin create, list, get, and delete operations at `/api/oagw/v1/plugins`, plus `GET /api/oagw/v1/plugins/{id}/source`, with `(tenant_id, name)` uniqueness and the 409 `PluginInUse` conflict carrying a `referenced_by` shape when a referenced plugin is deleted.

**Implements**:
- `cpt-cf-oagw-flow-control-plane-api-plugin-crud`
- `cpt-cf-oagw-algo-control-plane-api-authorize`

**Constraints**: None

**Touches**:
- API: `POST /api/oagw/v1/plugins`, `GET /api/oagw/v1/plugins`, `GET /api/oagw/v1/plugins/{id}`, `DELETE /api/oagw/v1/plugins/{id}`, `GET /api/oagw/v1/plugins/{id}/source`
- DB: `oagw_plugin`, `oagw_upstream_plugin`, `oagw_route_plugin`
- Entities: `Plugin`, `PluginConfig`

### Alias Derivation and Enforcement

- [ ] `p3` - **ID**: `cpt-cf-oagw-dod-control-plane-api-alias`

The system **MUST** derive aliases per endpoint type (single hostname; longest common domain suffix of at least two labels validated against the public suffix list for multiple hostnames; explicit alias required for IP/non-derivable pools), normalize to ASCII lowercase with trailing dots stripped, and enforce alias immutability and `(tenant_id, alias)` uniqueness.

**Implements**:
- `cpt-cf-oagw-algo-control-plane-api-derive-alias`
- `cpt-cf-oagw-algo-control-plane-api-validate-alias`

**Constraints**: None

**Touches**:
- API: `POST /api/oagw/v1/upstreams`, `PUT /api/oagw/v1/upstreams/{id}`
- DB: `oagw_upstream` (`alias` column)
- Entities: `Upstream`, `Endpoint`, `HostEntry`

### GTS Permission Enforcement

- [ ] `p3` - **ID**: `cpt-cf-oagw-dod-control-plane-api-authz`

The system **MUST** authenticate all management requests via `toolkit-auth` and authorize each operation against `gts.cf.core.oagw.{upstream|route|auth_plugin|guard_plugin|transform_plugin}.v1~:{create;override;read;delete}` through `authz_resolver`, returning 403 when denied.

**Implements**:
- `cpt-cf-oagw-algo-control-plane-api-authorize`
- `cpt-cf-oagw-flow-control-plane-api-upstream-crud`
- `cpt-cf-oagw-flow-control-plane-api-route-crud`
- `cpt-cf-oagw-flow-control-plane-api-plugin-crud`

**Constraints**: None

**Touches**:
- API: all management endpoints
- DB: none
- Entities: none

### Tenant-Scoped Visibility and Ancestor 404

- [ ] `p3` - **ID**: `cpt-cf-oagw-dod-control-plane-api-tenant-scope`

The system **MUST** return 404 for management reads/updates/deletes of resources that are absent or owned by an ancestor tenant, keeping ancestor resources invisible through the management surface.

**Implements**:
- `cpt-cf-oagw-flow-control-plane-api-upstream-crud`
- `cpt-cf-oagw-flow-control-plane-api-route-crud`
- `cpt-cf-oagw-flow-control-plane-api-plugin-crud`

**Constraints**: None

**Touches**:
- API: `GET/PUT/DELETE /api/oagw/v1/upstreams/{id}`, `GET/PUT/DELETE /api/oagw/v1/routes/{id}`, `GET/DELETE /api/oagw/v1/plugins/{id}`
- DB: `oagw_upstream`, `oagw_route`, `oagw_plugin`
- Entities: `Upstream`, `Route`, `Plugin`

### OData List Semantics

- [ ] `p3` - **ID**: `cpt-cf-oagw-dod-control-plane-api-odata`

The system **MUST** support `$filter`, `$select`, `$orderby`, `$top` (default 50, max 100), and `$skip` on all list endpoints, with utoipa-documented OpenAPI contracts.

**Implements**:
- `cpt-cf-oagw-algo-control-plane-api-odata`

**Constraints**: None

**Touches**:
- API: `GET /api/oagw/v1/upstreams`, `GET /api/oagw/v1/routes`, `GET /api/oagw/v1/plugins`
- DB: none
- Entities: `Upstream`, `Route`, `Plugin`

## 20. Acceptance Criteria

- [ ] POST /api/oagw/v1/upstreams with a duplicate alias in the tenant returns 409 with a problem+json body carrying instance `gts.cf.core.errors.err.v1~cf.oagw.upstream.alias_conflict.v1` and `X-OAGW-Error-Source: gateway`
- [ ] POST /api/oagw/v1/upstreams for a single hostname endpoint with a user-provided alias that differs from the auto-derived value is rejected with 400 (hostname aliases are auto-derived; providing the exact derived value is accepted as an idempotent no-op)
- [ ] POST /api/oagw/v1/upstreams with a pool of IP endpoints and no explicit alias is rejected with 400 (an explicit alias is required)
- [ ] PUT /api/oagw/v1/upstreams/{id} attempting to change the alias is rejected with 400 (alias immutable)
- [ ] POST /api/oagw/v1/routes referencing a nonexistent upstream returns 400
- [ ] POST /api/oagw/v1/routes creating two enabled routes under one upstream with the same `(path_prefix, priority)` for the same method returns 409
- [ ] PUT /api/oagw/v1/routes/{id} attempting to change `upstream_id` returns 400
- [ ] GET/PUT/DELETE on the id of an ancestor-owned upstream, route, or plugin returns 404
- [ ] POST /api/oagw/v1/plugins with a duplicate `(tenant_id, name)` returns 409
- [ ] DELETE /api/oagw/v1/plugins/{id} for a plugin referenced by an upstream or route returns 409 `PluginInUse` whose problem+json carries instance `gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1` and a `referenced_by` shape; an unreferenced plugin returns 204
- [ ] GET /api/oagw/v1/plugins/{id}/source returns the Starlark source of the custom plugin
- [ ] A caller without the required `gts.cf.core.oagw.upstream.v1~:{create;...}` permission receives 403 on the management operation
- [ ] Upstream creation carrying CORS config with `allow_credentials: true` and the wildcard origin is rejected at validation time with 400
- [ ] List endpoints honor `$top` (capped at 100, default 50) and `$skip`
- [ ] All management validation failures return RFC 9457 problem+json with type `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`

## 21. Additional Context (optional)

The RFC 9457 error envelope referenced by this feature is fully defined by the Error Semantics feature (its GTS instance catalog, header contract, and retriability classification); the Error Semantics feature depends on this feature so the serialization contract is defined alongside the API layer that emits it. Automated tests for this gear live in the crate's own test modules (unit and integration tests under the `oagw` crate), not under `testing/e2e/gears/oagw/`, which is reserved for the acceptance suite.


# Feature: OAGW Plugin System

- [ ] `p3` - **ID**: `cpt-cf-oagw-featstatus-plugin-system`

## 22. Feature Context

- [ ] `p3` - `cpt-cf-oagw-feature-plugin-system`

### 22.1 Overview

Deliver the three plugin trait families (`AuthPlugin`, `GuardPlugin`, `TransformPlugin`), GTS identifier resolution, built-ins and catalog-only identifiers, credential isolation through `cred://` references, the OAuth2 token cache, and the immutable sandboxed Starlark custom-plugin lifecycle.

### 22.2 Purpose

This feature implements `cpt-cf-oagw-component-plugin-system`: registry-based plugin resolution for `cpt-cf-oagw-fr-plugin-system`, built-in plugin coverage for `cpt-cf-oagw-fr-builtin-plugins` and `cpt-cf-oagw-fr-auth-injection`, credential isolation (`cpt-cf-oagw-principle-cred-isolation`, `cpt-cf-oagw-nfr-credential-isolation`), plugin immutability (`cpt-cf-oagw-principle-plugin-immutable`), and the sandboxed Starlark runtime (`cpt-cf-oagw-nfr-starlark-sandbox`).

**Requirements**: `cpt-cf-oagw-fr-auth-injection`, `cpt-cf-oagw-fr-plugin-system`, `cpt-cf-oagw-fr-builtin-plugins`, `cpt-cf-oagw-nfr-credential-isolation`, `cpt-cf-oagw-nfr-starlark-sandbox`

**Principles**: `cpt-cf-oagw-principle-cred-isolation`, `cpt-cf-oagw-principle-plugin-immutable`

### 22.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Creates and binds custom plugins and selects built-in plugin references for upstreams and routes |
| `cpt-cf-oagw-actor-app-developer` | Has authentication injected transparently and consumes plugin rejections (auth failure, guard rejections) through the proxy |
| `cpt-cf-oagw-actor-cred-store` | Returns secret material by `cred://` reference with tenant access checks for auth plugins |

### 22.4 References

- **PRD**: [PRD.md](./PRD.md)
- **Design**: [DESIGN.md](./DESIGN.md)
- **ADRs**: `cpt-cf-oagw-adr-plugin-system` (three traits, execution order, catalog-only identifiers), `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin` (token cache), `cpt-cf-oagw-adr-required-headers-guard-plugin` (required-headers guard)
- **Parent feature**: `cpt-cf-oagw-feature-plugin-system`
- **Dependencies**: `cpt-cf-oagw-feature-domain-model-repositories`
- **Related design elements**: `cpt-cf-oagw-component-plugin-system`, `cpt-cf-oagw-dbtable-plugin`, `cpt-cf-oagw-dbtable-upstream-plugin`, `cpt-cf-oagw-dbtable-route-plugin`

## 23. Actor Flows (CDSL)

### Create, Bind, and Inspect a Custom Plugin

- [ ] `p3` - **ID**: `cpt-cf-oagw-flow-plugin-system-custom-plugin`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A custom Starlark plugin is created, bound to an upstream or route, and its source is retrievable; an unreferenced plugin is deleted.

**Error Scenarios**:
- Binding validation fails (position contiguity, `plugin_uuid`/`plugin_ref` mismatch) or deleting a referenced plugin returns 409.

**Steps**:
1. [ ] - `p3` - Operator submits a custom plugin definition (plugin_type, name, config_schema, Starlark source) through the management surface - `inst-ps-custom-create`
2. [ ] - `p3` - **IF** the Starlark source fails to parse or validate against the declared config schema **THEN** the creation is rejected - `inst-ps-custom-validate`
3. [ ] - `p3` - DB: INSERT `oagw_plugin` (server-generated UUID; the plugin becomes immutable) - `inst-ps-custom-persist`
4. [ ] - `p3` - Operator binds the plugin on an upstream or route with `(position, plugin_ref, plugin_uuid, config)` - `inst-ps-custom-bind`
5. [ ] - `p3` - **IF** binding positions are not contiguous from 0 **OR** `plugin_uuid` does not match `plugin_ref` **THEN** reject the binding - `inst-ps-custom-bind-validate`
6. [ ] - `p3` - Operator retrieves the Starlark source via the plugin source endpoint - `inst-ps-custom-source`
7. [ ] - `p3` - Operator deletes the plugin; **IF** it is referenced by a binding **THEN** 409 `PluginInUse`; **ELSE** the row is removed - `inst-ps-custom-delete`

### Authenticate a Proxy Request via an Auth Plugin

- [ ] `p3` - **ID**: `cpt-cf-oagw-flow-plugin-system-proxy-auth`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- Credentials are injected transparently before the request reaches the upstream.

**Error Scenarios**:
- Credentials cannot be resolved or the upstream rejects the credential material, producing the authentication-failed outcome.

**Steps**:
1. [ ] - `p3` - The plugin chain reaches the auth phase for the proxied request - `inst-ps-auth-phase`
2. [ ] - `p3` - Resolve the plugin by GTS identifier from the `AuthPluginRegistry` (built-in or custom) - `inst-ps-auth-resolve`
3. [ ] - `p3` - **IF** the plugin references `cred://` material **THEN** resolve it through the CredStore SDK with tenant access checks - `inst-ps-auth-cred`
4. [ ] - `p3` - Execute the auth phase: `noop` performs no injection; `apikey` injects a header or query parameter; the OAuth2 client-credentials variants inject `Authorization: Bearer` using the token cache - `inst-ps-auth-execute`
5. [ ] - `p3` - **IF** the auth phase fails **THEN** return the authentication-failed outcome (401) - `inst-ps-auth-fail`
6. [ ] - `p3` - **RETURN** the authenticated request to continue to the guard phase - `inst-ps-auth-return`

## 24. Processes / Business Logic (CDSL)

### Resolve Plugin by GTS Identifier

- [ ] `p3` - **ID**: `cpt-cf-oagw-algo-plugin-system-resolve-gts`

**Input**: `plugin_ref` (canonical GTS plugin identifier) and optional `plugin_uuid`

**Output**: Resolved plugin instance or `PluginNotFound`

**Steps**:
1. [ ] - `p3` - Parse the `plugin_ref` into type (`auth_plugin`, `guard_plugin`, `transform_plugin`) and instance segments - `inst-ps-gts-parse`
2. [ ] - `p3` - **IF** type is auth **THEN** look up `AuthPluginRegistry`; guard **THEN** `GuardPluginRegistry`; transform **THEN** `TransformPluginRegistry` - `inst-ps-gts-registry`
3. [ ] - `p3` - **IF** the instance is a catalog-only identifier (`basic`, `bearer`, `timeout`, `cors`, `logging`, `metrics`) **THEN** it is not resolvable via a registry and binding returns `PluginNotFound` (503) - `inst-ps-gts-catalog-only`
4. [ ] - `p3` - **IF** `plugin_uuid` is present **THEN** load the custom plugin from `oagw_plugin` and verify its schema type matches - `inst-ps-gts-custom`
5. [ ] - `p3` - **IF** no entry resolves **THEN** return `PluginNotFound` - `inst-ps-gts-notfound`
6. [ ] - `p3` - **RETURN** the resolved plugin instance - `inst-ps-gts-return`

### Execute Plugin Chain in Deterministic Order

- [ ] `p3` - **ID**: `cpt-cf-oagw-algo-plugin-system-execute-chain`

**Input**: Request context, effective plugin bindings (upstream then route)

**Output**: Mutated/rejected request and transformed response/error

**Steps**:
1. [ ] - `p3` - Concatenate upstream plugins before route plugins (`[U1, U2] + [R1, R2]` yields `[U1, U2, R1, R2]`) - `inst-ps-chain-order`
2. [ ] - `p3` - **FOR EACH** auth plugin run `authenticate(ctx)` (once per request, before guards) - `inst-ps-chain-auth`
3. [ ] - `p3` - **FOR EACH** guard plugin run `guard_request(ctx)`; **IF** a guard rejects **THEN** return the rejection - `inst-ps-chain-guard-req`
4. [ ] - `p3` - **FOR EACH** transform plugin run `transform_request(ctx)` - `inst-ps-chain-transform-req`
5. [ ] - `p3` - Forward to the upstream (executed by the Data-Plane Proxy) - `inst-ps-chain-upstream`
6. [ ] - `p3` - **FOR EACH** guard plugin run `guard_response(ctx)`; **IF** a guard rejects **THEN** return the rejection - `inst-ps-chain-guard-resp`
7. [ ] - `p3` - **FOR EACH** transform plugin run `transform_response(ctx)` on success or `transform_error(ctx)` on error - `inst-ps-chain-transform-resp`
8. [ ] - `p3` - **RETURN** the final response - `inst-ps-chain-return`

### Resolve OAuth2 Token with Internal Cache

- [ ] `p3` - **ID**: `cpt-cf-oagw-algo-plugin-system-oauth2-cache`

**Input**: OAuth2 client-credentials plugin config (`token_endpoint`/`issuer_url`, `client_id_ref`, `client_secret_ref`, `scopes`) and cache state

**Output**: Cached or freshly fetched bearer token (zeroizing secret type)

**Steps**:
1. [ ] - `p3` - Build the cache key `subject_tenant_id:subject_id:auth_method:config_hash` - `inst-ps-oauth-key`
2. [ ] - `p3` - Look up the token in `pingora-memory-cache` - `inst-ps-oauth-lookup`
3. [ ] - `p3` - **IF** hit **AND** the stored key re-verifies against the requested key **THEN** inject the cached token; a key mismatch is treated as a cache miss (defense-in-depth against hash collisions) - `inst-ps-oauth-hit`
4. [ ] - `p3` - **IF** miss **THEN** resolve `client_id_ref`/`client_secret_ref` via CredStore (zeroizing types) and call `toolkit_auth::oauth2::fetch_token` (OIDC discovery or direct `token_endpoint`) - `inst-ps-oauth-fetch`
5. [ ] - `p3` - Compute `ttl = min(token_cache_ttl_secs, expires_in - 30s)`; **IF** `expires_in <= 30s` **THEN** do not cache - `inst-ps-oauth-ttl`
6. [ ] - `p3` - Store the token with the computed TTL (evicted values are zeroized); failed token fetches are **not** cached so the next request retries the IdP - `inst-ps-oauth-store`
7. [ ] - `p3` - **RETURN** the bearer token for `Authorization: Bearer` injection - `inst-ps-oauth-return`

### Enforce Required Headers Guard

- [ ] `p3` - **ID**: `cpt-cf-oagw-algo-plugin-system-required-headers`

**Input**: Request or response headers and guard config (`required_request_headers`, `required_response_headers`)

**Output**: Allow, or phase-specific rejection

**Steps**:
1. [ ] - `p3` - Read the phase-appropriate header list from config - `inst-ps-rh-config`
2. [ ] - `p3` - **IF** the list is absent or blank after trimming entries **THEN** allow (fail-open) - `inst-ps-rh-failopen`
3. [ ] - `p3` - Split on commas, trim, lowercase, and drop empty entries - `inst-ps-rh-normalize`
4. [ ] - `p3` - Scan the headers for each required name (case-insensitive presence only; values are not validated) - `inst-ps-rh-scan`
5. [ ] - `p3` - **IF** a header is missing **THEN** reject on the first missing name: request phase returns 400 with `REQUIRED_HEADER_MISSING`, response phase returns 502 with `REQUIRED_HEADER_MISSING` - `inst-ps-rh-reject`
6. [ ] - `p3` - **RETURN** allow when all required names are present - `inst-ps-rh-return`

### Resolve cred:// Secret Reference

- [ ] `p3` - **ID**: `cpt-cf-oagw-algo-plugin-system-resolve-secret`

**Input**: A `cred://` URI reference and the calling tenant context

**Output**: Secret material wrapped in a zeroizing secret type, or `SecretNotFound`

**Steps**:
1. [ ] - `p3` - Parse the `cred://` URI - `inst-ps-secret-parse`
2. [ ] - `p3` - Call `CredStoreClientV1` with the tenant access check - `inst-ps-secret-fetch`
3. [ ] - `p3` - **IF** the secret is not found or access is denied **THEN** return `SecretNotFound` (500) - `inst-ps-secret-notfound`
4. [ ] - `p3` - Wrap the material in a zeroizing secret type; it is never serialized into logs, metrics, or responses - `inst-ps-secret-zeroize`
5. [ ] - `p3` - **RETURN** the secret - `inst-ps-secret-return`

### Execute Custom Starlark Plugin in a Sandbox

- [ ] `p3` - **ID**: `cpt-cf-oagw-algo-plugin-system-starlark-sandbox`

**Input**: Compiled Starlark plugin and invocation context

**Output**: Transform result, or a sandbox violation outcome

**Steps**:
1. [ ] - `p3` - Parse and compile the plugin's Starlark source and validate its `config_schema` at creation - `inst-ps-star-parse`
2. [ ] - `p3` - Execute with no network I/O, no file I/O, and no imports available - `inst-ps-star-restrict`
3. [ ] - `p3` - Enforce the execution timeout (at most 100ms per invocation) and memory limit (at most 10MB per invocation) - `inst-ps-star-limits`
4. [ ] - `p3` - **IF** a disallowed operation or limit violation occurs **THEN** terminate and report the sandbox violation - `inst-ps-star-violation`
5. [ ] - `p3` - **RETURN** the plugin result - `inst-ps-star-return`

### Garbage-Collect Expired Custom Plugins

- [ ] `p3` - **ID**: `cpt-cf-oagw-algo-plugin-system-gc`

**Input**: Custom plugin rows and their binding references

**Output**: Removed unreferenced expired plugins

**Steps**:
1. [ ] - `p3` - Periodically scan `oagw_plugin` rows whose `gc_eligible_at` is in the past (default 30 days after last use) - `inst-ps-gc-scan`
2. [ ] - `p3` - **IF** a row is not referenced by any `oagw_upstream_plugin`/`oagw_route_plugin` binding **THEN** delete the row - `inst-ps-gc-delete`
3. [ ] - `p3` - **RETURN** the count of collected plugins - `inst-ps-gc-return`

## 25. States (CDSL)

### Custom Plugin Lifecycle

- [ ] `p3` - **ID**: `cpt-cf-oagw-state-plugin-system-plugin-lifecycle`

**States**: ACTIVE, GC_ELIGIBLE, GARBAGE_COLLECTED

**Initial State**: ACTIVE

**Transitions**:
1. [ ] - `p3` - **FROM** ACTIVE **TO** GC_ELIGIBLE **WHEN** the plugin's `gc_eligible_at` timestamp passes (immutable after creation — there is no update transition) - `inst-ps-state-gc-eligible`
2. [ ] - `p3` - **FROM** GC_ELIGIBLE **TO** GARBAGE_COLLECTED **WHEN** a GC sweep finds no upstream or route binding referencing the plugin - `inst-ps-state-collected`
3. [ ] - `p3` - **FROM** GC_ELIGIBLE **TO** ACTIVE **WHEN** the plugin is referenced again by a binding before the sweep (updating `last_used_at`) - `inst-ps-state-referenced`

## 26. Definitions of Done

### Plugin Trait Contracts and Registries

- [ ] `p3` - **ID**: `cpt-cf-oagw-dod-plugin-system-traits`

The system **MUST** define the `AuthPlugin` (with `authenticate(ctx)`), `GuardPlugin` (with `guard_request(ctx)`/`guard_response(ctx)`, can reject), and `TransformPlugin` (with `transform_request(ctx)`/`transform_response(ctx)`/`transform_error(ctx)`) traits exposing `id()` and `plugin_type()` accessors, and provide `AuthPluginRegistry`, `GuardPluginRegistry`, and `TransformPluginRegistry` keyed by GTS identifier.

**Implements**:
- `cpt-cf-oagw-algo-plugin-system-resolve-gts`
- `cpt-cf-oagw-flow-plugin-system-proxy-auth`

**Constraints**: None

**Touches**:
- API: none (in-process contracts)
- DB: none
- Entities: `AuthPlugin`, `GuardPlugin`, `TransformPlugin`, `Plugin`

### Built-in Plugins

- [ ] `p3` - **ID**: `cpt-cf-oagw-dod-plugin-system-builtins`

The system **MUST** ship built-ins in the `oagw` crate: auth `noop`, `apikey` (header/query injection), `oauth2_client_cred`, `oauth2_client_cred_basic`; guard `required_headers`; transform `request_id` (X-Request-ID propagation); all registered under their GTS identifiers and bindable.

**Implements**:
- `cpt-cf-oagw-algo-plugin-system-resolve-gts`
- `cpt-cf-oagw-algo-plugin-system-required-headers`
- `cpt-cf-oagw-flow-plugin-system-proxy-auth`

**Constraints**: None

**Touches**:
- API: none
- DB: `oagw_upstream_plugin`, `oagw_route_plugin`
- Entities: `Plugin`, `PluginConfig`

### Catalog-Only GTS Identifiers

- [ ] `p3` - **ID**: `cpt-cf-oagw-dod-plugin-system-catalog-only`

The system **MUST** register the catalog-only identifiers `basic`, `bearer` (auth), `timeout`, `cors` (guard), and `logging`, `metrics` (transform) in the types registry with no backing registry implementation, and resolve none of them via a plugin registry (binding one returns `PluginNotFound`).

**Implements**:
- `cpt-cf-oagw-algo-plugin-system-resolve-gts`

**Constraints**: None

**Touches**:
- API: none (types-registry cataloging)
- DB: none
- Entities: `Plugin` (identifier only)

### Deterministic Execution Order

- [ ] `p3` - **ID**: `cpt-cf-oagw-dod-plugin-system-execution-order`

The system **MUST** execute plugin chains in the fixed order Auth, then Guards, then Transform(request), then the upstream call, then Transform(response/error), with upstream plugins before route plugins, and never re-order bindings at runtime.

**Implements**:
- `cpt-cf-oagw-algo-plugin-system-execute-chain`

**Constraints**: None

**Touches**:
- API: none
- DB: `oagw_upstream_plugin`, `oagw_route_plugin` (`position`)
- Entities: `PluginConfig`, `PluginsConfig`

### Plugin Identification and Binding Conflict Detection

- [ ] `p3` - **ID**: `cpt-cf-oagw-dod-plugin-system-identification`

The system **MUST** carry `plugin_ref` (canonical GTS identifier) and `plugin_uuid` (set only for UUID-backed custom plugins; null for named plugins) on every binding, and validate binding positions contiguous from 0, `plugin_uuid` matching `plugin_ref`, and `(tenant_id, name)` uniqueness for custom plugins.

**Implements**:
- `cpt-cf-oagw-flow-plugin-system-custom-plugin`
- `cpt-cf-oagw-algo-plugin-system-resolve-gts`

**Constraints**: None

**Touches**:
- API: none (enforced through management CRUD)
- DB: `oagw_upstream_plugin`, `oagw_route_plugin`, `oagw_plugin`
- Entities: `Plugin`, `PluginConfig`

### OAuth2 Client-Credentials Token Cache

- [ ] `p3` - **ID**: `cpt-cf-oagw-dod-plugin-system-oauth2-cache`

The system **MUST** implement the OAuth2 client-credentials auth plugins (Form and Basic variants) with an internal `pingora-memory-cache` keyed by `subject_tenant_id:subject_id:auth_method:config_hash`, key re-verification on every hit, TTL `min(token_cache_ttl_secs, expires_in - 30s)`, no caching when `expires_in <= 30s`, and failed fetches never cached, with token values held in zeroizing secret types.

**Implements**:
- `cpt-cf-oagw-algo-plugin-system-oauth2-cache`
- `cpt-cf-oagw-flow-plugin-system-proxy-auth`

**Constraints**: None

**Touches**:
- API: none (in-process plugin state)
- DB: none
- Entities: `PluginConfig` (`token_endpoint`/`issuer_url`, `client_id_ref`, `client_secret_ref`, `scopes`)

### Credential Isolation

- [ ] `p3` - **ID**: `cpt-cf-oagw-dod-plugin-system-cred-isolation`

The system **MUST** resolve credentials exclusively through `cred://` references via the CredStore SDK, hold any in-memory copy in a zeroizing secret type, and never serialize credential material into logs, metrics, error messages, or responses.

**Implements**:
- `cpt-cf-oagw-algo-plugin-system-resolve-secret`
- `cpt-cf-oagw-flow-plugin-system-proxy-auth`

**Constraints**: None

**Touches**:
- API: none
- DB: none
- Entities: `PluginConfig` (secret references), `CredStoreClientV1` (interaction)

### Immutable Sandboxed Custom Plugins

- [ ] `p3` - **ID**: `cpt-cf-oagw-dod-plugin-system-starlark`

The system **MUST** treat custom plugins as immutable after creation, execute them in a Starlark sandbox with no network I/O, no file I/O, and no imports, enforce the 100ms timeout and 10MB memory limit per invocation, and garbage-collect unreferenced plugins whose `gc_eligible_at` has passed.

**Implements**:
- `cpt-cf-oagw-algo-plugin-system-starlark-sandbox`
- `cpt-cf-oagw-algo-plugin-system-gc`
- `cpt-cf-oagw-state-plugin-system-plugin-lifecycle`
- `cpt-cf-oagw-flow-plugin-system-custom-plugin`

**Constraints**: None

**Touches**:
- API: none
- DB: `oagw_plugin` (`source_code`, `gc_eligible_at`, `last_used_at`)
- Entities: `Plugin` (custom)

## 27. Acceptance Criteria

- [ ] Each built-in plugin resolves under its GTS identifier through its registry type (`noop`, `apikey`, `oauth2_client_cred`, `oauth2_client_cred_basic`, `required_headers`, `request_id`)
- [ ] Binding a catalog-only identifier (`basic`, `bearer`, `timeout`, `cors`, `logging`, `metrics`) to an upstream or route yields a 503 gateway error carrying instance `gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1`
- [ ] With upstream bindings `[U1, U2]` and route bindings `[R1, R2]`, execution order is `[U1, U2, R1, R2]` across Auth, Guards, Transform(request), and Transform(response/error) phases
- [ ] A guard rejection in the request phase stops the chain before the upstream call
- [ ] A `required_headers` guard with a missing configured request header rejects with 400 `REQUIRED_HEADER_MISSING`; a missing configured response header rejects with 502 `REQUIRED_HEADER_MISSING`; header names match case-insensitively; absent/blank config is fail-open
- [ ] The OAuth2 client-credentials plugin serves a cached token without an IdP call on a cache hit, computes TTL as `min(ttl, expires_in - 30s)`, does not cache tokens with `expires_in <= 30s`, does not cache failed fetches, and its cache key isolates separate (tenant, subject, auth method, config) tuples with re-verification on hit (a key mismatch returns a miss, never another tenant's token)
- [ ] A `cred://` reference to a nonexistent or denied secret yields the 500 gateway error with instance `gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1`
- [ ] No log line, metric label, or response body ever contains secret material (API keys, tokens, client secrets)
- [ ] A custom Starlark plugin attempting network I/O, file I/O, or imports is blocked; execution exceeding 100ms or 10MB is terminated
- [ ] A custom plugin is immutable after creation (no update path) and is garbage-collected only after `gc_eligible_at` passes and no binding references it
- [ ] Plugin binding positions must be contiguous from 0 and `plugin_uuid` must match `plugin_ref`, otherwise the binding is rejected

## 28. Additional Context (optional)

The `request_id` transform plugin propagates `X-Request-ID`; full correlation behavior (metrics, logs, error envelopes) is owned by the Observability feature. Rate limiting, CORS, timeout enforcement, logging, and metrics remain core Data Plane functionality represented here only by catalog identifiers. Automated tests for this gear live in the crate's own test modules (unit and integration tests under the `oagw` crate), not under `testing/e2e/gears/oagw/`, which is reserved for the acceptance suite.


# Feature: OAGW Error Semantics & RFC 9457 Framework

- [ ] `p4` - **ID**: `cpt-cf-oagw-featstatus-error-semantics`

## 29. Feature Context

- [ ] `p4` - `cpt-cf-oagw-feature-error-semantics`

### 29.1 Overview

Provide the single RFC 9457 problem+json error framework used by every plane: the central error envelope, the full GTS instance catalog from the DESIGN error table, retriability classification, the `X-OAGW-Error-Source` header contract, and `request_id`/`trace_id` propagation into error responses.

### 29.2 Purpose

This feature implements `cpt-cf-oagw-principle-rfc9457` (standard fields plus OAGW extensions), `cpt-cf-oagw-principle-error-source` (uniform `X-OAGW-Error-Source: gateway|upstream` attribution), and `cpt-cf-oagw-fr-error-codes` (consistent, well-defined error outcomes with retriability signaling) per `cpt-cf-oagw-adr-error-source-distinction`.

**Requirements**: `cpt-cf-oagw-fr-error-codes`

**Principles**: `cpt-cf-oagw-principle-rfc9457`, `cpt-cf-oagw-principle-error-source`

### 29.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Consumes gateway errors (problem+json with `X-OAGW-Error-Source: gateway`) and upstream errors (passthrough with `X-OAGW-Error-Source: upstream`) and uses the type-driven retriability signal to implement correct retry behavior |
| `cpt-cf-oagw-actor-upstream-service` | Its original error bodies are passed through unchanged and labeled `upstream` |

### 29.4 References

- **PRD**: [PRD.md](./PRD.md)
- **Design**: [DESIGN.md](./DESIGN.md)
- **ADRs**: `cpt-cf-oagw-adr-error-source-distinction` (header contract, RFC 9457 format, target-host error examples), `cpt-cf-oagw-adr-rate-limiting` (429 rejection with `Retry-After`), `cpt-cf-oagw-adr-request-routing` (target-host routing errors)
- **Parent feature**: `cpt-cf-oagw-feature-error-semantics`
- **Dependencies**: `cpt-cf-oagw-feature-control-plane-api`
- **Related design elements**: `cpt-cf-oagw-principle-rfc9457`, `cpt-cf-oagw-principle-error-source`, `cpt-cf-oagw-fr-error-codes`

## 30. Actor Flows (CDSL)

### Consume a Gateway Error Response

- [ ] `p4` - **ID**: `cpt-cf-oagw-flow-error-semantics-gateway-error`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The client receives a machine-readable problem+json body with a GTS type that determines retriability and an error-source header.

**Error Scenarios**:
- The response is an upstream passthrough rather than a gateway error (distinguished by the header).

**Steps**:
1. [ ] - `p4` - App receives an error response from the gateway - `inst-es-gw-receive`
2. [ ] - `p4` - **IF** `X-OAGW-Error-Source: gateway` **THEN** the body is `application/problem+json` with `type`, `title`, `status`, `detail`, `instance` and the OAGW extension fields (`upstream_id`, `host`, `path`, `retry_after_seconds`, `trace_id`) - `inst-es-gw-envelope`
3. [ ] - `p4` - App reads the `type` GTS identifier and the retriability classification to decide whether to retry - `inst-es-gw-retry`
4. [ ] - `p4` - **IF** `retry_after_seconds` is present **THEN** app paces the retry accordingly - `inst-es-gw-ratelimit`
5. [ ] - `p4` - **RETURN** the parsed error to the caller - `inst-es-gw-return`

### Consume an Upstream Error Passthrough

- [ ] `p4` - **ID**: `cpt-cf-oagw-flow-error-semantics-upstream-error`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The upstream's original error response is returned unchanged with the source clearly labeled.

**Steps**:
1. [ ] - `p4` - Upstream service returns an error response - `inst-es-up-error`
2. [ ] - `p4` - Gateway forwards the upstream body unchanged with `X-OAGW-Error-Source: upstream` - `inst-es-up-passthrough`
3. [ ] - `p4` - App attributes the response to the upstream via the header - `inst-es-up-attribute`
4. [ ] - `p4` - **RETURN** the passthrough response - `inst-es-up-return`

## 31. Processes / Business Logic (CDSL)

### Build an RFC 9457 Problem+JSON Instance

- [ ] `p4` - **ID**: `cpt-cf-oagw-algo-error-semantics-build-instance`

**Input**: A domain error and request context

**Output**: A serialized `application/problem+json` document

**Steps**:
1. [ ] - `p4` - Map the domain error to its GTS instance, status code, and title from the error catalog - `inst-es-build-map`
2. [ ] - `p4` - Populate the standard fields `type`, `title`, `status`, `detail`, `instance` - `inst-es-build-standard`
3. [ ] - `p4` - Attach extension fields `upstream_id`, `host`, `path` from the request context - `inst-es-build-context`
4. [ ] - `p4` - **IF** the error is a rate-limit or retriable rejection **THEN** attach `retry_after_seconds` - `inst-es-build-retry`
5. [ ] - `p4` - Attach `trace_id` and the request `request_id` - `inst-es-build-trace`
6. [ ] - `p4` - **RETURN** the serialized problem+json document - `inst-es-build-return`

### Classify Error Retriability

- [ ] `p4` - **ID**: `cpt-cf-oagw-algo-error-semantics-classify-retry`

**Input**: A GTS error instance

**Output**: Retriable, non-retriable, or depends classification

**Steps**:
1. [ ] - `p4` - **IF** the instance is `cf.oagw.rate_limit.exceeded.v1`, `cf.oagw.link.unavailable.v1`, `cf.oagw.circuit_breaker.open.v1`, `cf.oagw.timeout.connection.v1`, `cf.oagw.timeout.request.v1`, or `cf.oagw.timeout.idle.v1` **THEN** it is retriable - `inst-es-cls-retriable`
2. [ ] - `p4` - **IF** the instance is `cf.oagw.downstream.error.v1` **THEN** classification depends on the caller - `inst-es-cls-depends`
3. [ ] - `p4` - **ELSE** (validation, routing target-host, auth, route-not-found, payload, secret, protocol, stream-aborted, plugin, and idle classes other than those above) it is not retriable - `inst-es-cls-no`
4. [ ] - `p4` - **RETURN** the classification - `inst-es-cls-return`

### Attach X-OAGW-Error-Source Headers

- [ ] `p4` - **ID**: `cpt-cf-oagw-algo-error-semantics-attach-source`

**Input**: A response and its origin (gateway or upstream)

**Output**: A response tagged with `X-OAGW-Error-Source`

**Steps**:
1. [ ] - `p4` - **IF** the response originates from the gateway **THEN** set `X-OAGW-Error-Source: gateway` - `inst-es-src-gateway`
2. [ ] - `p4` - **IF** the response originates from the upstream **THEN** set `X-OAGW-Error-Source: upstream` and leave the body untouched - `inst-es-src-upstream`
3. [ ] - `p4` - Ensure the header is present on every response, success or error - `inst-es-src-every`
4. [ ] - `p4` - **RETURN** the tagged response - `inst-es-src-return`

### Propagate request_id into Error Envelopes

- [ ] `p4` - **ID**: `cpt-cf-oagw-algo-error-semantics-propagate-request-id`

**Input**: Ingress request and its correlation identifiers

**Output**: Error envelope and logs carrying the identifiers

**Steps**:
1. [ ] - `p4` - Capture the `request_id` assigned at ingress (or the existing `X-Request-ID`) and the `trace_id` - `inst-es-rid-capture`
2. [ ] - `p4` - Embed `request_id`/`trace_id` into every gateway error envelope extension - `inst-es-rid-embed`
3. [ ] - `p4` - Emit the identifiers into the structured log record for the operation - `inst-es-rid-log`
4. [ ] - `p4` - **RETURN** the correlated error - `inst-es-rid-return`

## 32. States (CDSL)

Not applicable — error envelopes are immutable value objects produced per response; they carry no lifecycle state machine.

## 33. Definitions of Done

### Central RFC 9457 Error Envelope

- [ ] `p4` - **ID**: `cpt-cf-oagw-dod-error-semantics-envelope`

The system **MUST** provide a single `application/problem+json` error type with the standard RFC 9457 fields (`type`, `title`, `status`, `detail`, `instance`) and OAGW extension fields (`upstream_id`, `host`, `path`, `retry_after_seconds`, `trace_id`) used by all management and proxy handlers.

**Implements**:
- `cpt-cf-oagw-algo-error-semantics-build-instance`
- `cpt-cf-oagw-flow-error-semantics-gateway-error`

**Constraints**: None

**Touches**:
- API: all management and proxy endpoints (serialization contract)
- DB: none
- Entities: `GatewayError`

### GTS Instance Catalog

- [ ] `p4` - **ID**: `cpt-cf-oagw-dod-error-semantics-catalog`

The system **MUST** expose the full GTS instance catalog from the DESIGN error table: `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` (400), `gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1` (400), `gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1` (400), `gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1` (400), `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1` (401), `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` (403), `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1` (403), `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1` (404), `gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1` (409), `gts.cf.core.errors.err.v1~cf.oagw.upstream.alias_conflict.v1` (409), `gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1` (413), `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1` (429), `gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1` (500), `gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1` (502), `gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1` (502), `gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1` (502), `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1` (503), `gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1` (503), `gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1` (503), `gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1` (504), `gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1` (504), and `gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1` (504).

**Implements**:
- `cpt-cf-oagw-algo-error-semantics-build-instance`
- `cpt-cf-oagw-algo-error-semantics-classify-retry`

**Constraints**: None

**Touches**:
- API: all endpoints (error responses)
- DB: none
- Entities: `GatewayError`, error instance catalog

### Retriability Classification

- [ ] `p4` - **ID**: `cpt-cf-oagw-dod-error-semantics-retriability`

The system **MUST** classify each error instance as retriable (rate-limit exceeded, link unavailable, circuit-breaker open, connection/request/idle timeouts), depends (downstream error), or non-retriable (validation, routing target-host, authentication failed, route not found, payload too large, secret not found, protocol, stream aborted, plugin in use/not found), and surface the classification to callers via the instance `type`.

**Implements**:
- `cpt-cf-oagw-algo-error-semantics-classify-retry`

**Constraints**: None

**Touches**:
- API: all endpoints (error responses)
- DB: none
- Entities: `GatewayError`

### Error-Source Header Contract

- [ ] `p4` - **ID**: `cpt-cf-oagw-dod-error-semantics-source`

The system **MUST** set `X-OAGW-Error-Source: gateway` on gateway-generated responses (problem+json) and `X-OAGW-Error-Source: upstream` on upstream responses passed through unchanged, with the header present on every response.

**Implements**:
- `cpt-cf-oagw-algo-error-semantics-attach-source`
- `cpt-cf-oagw-flow-error-semantics-upstream-error`

**Constraints**: None

**Touches**:
- API: all endpoints (response header)
- DB: none
- Entities: `GatewayError`

### Correlation Propagation

- [ ] `p4` - **ID**: `cpt-cf-oagw-dod-error-semantics-request-id`

The system **MUST** propagate the `request_id` and `trace_id` into every gateway error envelope and structured log record, so errors are correlated end-to-end.

**Implements**:
- `cpt-cf-oagw-algo-error-semantics-propagate-request-id`

**Constraints**: None

**Touches**:
- API: all endpoints (error envelopes)
- DB: none
- Entities: `GatewayError` (extensions)

## 34. Acceptance Criteria

- [ ] Every gateway error response is `application/problem+json` with `type`, `title`, `status`, `detail`, `instance`, and, where applicable, `upstream_id`, `host`, `path`, `retry_after_seconds`, and `trace_id`
- [ ] Every response (success and error) carries `X-OAGW-Error-Source` set to either `gateway` or `upstream`
- [ ] An upstream 4xx/5xx response is returned to the caller with its body byte-for-byte unchanged and `X-OAGW-Error-Source: upstream`
- [ ] Each of the nineteen GTS instances in the catalog is emittable with its documented HTTP status: 400 (`validation.error`, `routing.missing_target_host`, `routing.invalid_target_host`, `routing.unknown_target_host`), 401 (`auth.failed`), 404 (`route.not_found`), 409 (`plugin.in_use`), 413 (`payload.too_large`), 429 (`rate_limit.exceeded`), 500 (`secret.not_found`), 502 (`protocol.error`, `downstream.error`, `stream.aborted`), 503 (`link.unavailable`, `circuit_breaker.open`, `plugin.not_found`), 504 (`timeout.connection`, `timeout.request`, `timeout.idle`)
- [ ] Retriability classification: rate-limit exceeded, link unavailable, circuit-breaker open, and the three timeout instances are retriable; downstream error is depends; all other instances are non-retriable
- [ ] A rate-limit exceeded gateway error includes `retry_after_seconds` in the envelope
- [ ] Gateway error envelopes and the corresponding log records carry the same `request_id`/`trace_id`

## 35. Additional Context (optional)

The instances are emitted by the owning features: target-host and proxy transport instances by the Data-Plane Proxy, `rate_limit.exceeded` by Rate Limiting, auth/secret/plugin instances by the Plugin System, and validation/plugin-in-use by the Control-Plane Management API; this feature defines the framework and catalog only. Automated tests for this gear live in the crate's own test modules (unit and integration tests under the `oagw` crate), not under `testing/e2e/gears/oagw/`, which is reserved for the acceptance suite.


# Feature: OAGW Rate Limiting & Caching Policies

- [ ] `p4` - **ID**: `cpt-cf-oagw-featstatus-rate-limiting`

## 36. Feature Context

- [ ] `p4` - `cpt-cf-oagw-feature-rate-limiting`

### 36.1 Overview

Enforce dual-rate token-bucket rate limiting at upstream and route levels with hierarchical `min()` inheritance, the `reject`/`queue`/`degrade` strategies, the 429 rejection contract with `Retry-After` and `X-RateLimit-*` headers, and Control-Plane configuration caching feeding Data-Plane lookup, per `cpt-cf-oagw-seq-rate-limit-flow`.

### 36.2 Purpose

This feature implements `cpt-cf-oagw-fr-rate-limiting` (dual-rate configuration, strategies, retry-timing communication) per `cpt-cf-oagw-adr-rate-limiting`, the `min()` hierarchical budget allocation, and `cpt-cf-oagw-principle-no-cache` (no upstream response caching; configuration caching only) per `cpt-cf-oagw-adr-data-plane-caching`.

**Requirements**: `cpt-cf-oagw-fr-rate-limiting`

**Principles**: `cpt-cf-oagw-principle-no-cache`

### 36.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Receives 429 rejections with retry timing or queued/degraded handling when limits are exceeded |
| `cpt-cf-oagw-actor-platform-operator` | Configures upstream/route rate limits and sharing/budget modes that drive the per-instance limiters |

### 36.4 References

- **PRD**: [PRD.md](./PRD.md)
- **Design**: [DESIGN.md](./DESIGN.md)
- **ADRs**: `cpt-cf-oagw-adr-rate-limiting` (token bucket, dual-rate, hierarchical min, rejection headers), `cpt-cf-oagw-adr-data-plane-caching` (L1/L2 configuration caching), `cpt-cf-oagw-adr-state-management` (DP-owned rate limiters)
- **Parent feature**: `cpt-cf-oagw-feature-rate-limiting`
- **Dependencies**: `cpt-cf-oagw-feature-plugin-system`, `cpt-cf-oagw-feature-error-semantics`
- **Related design elements**: `cpt-cf-oagw-seq-rate-limit-flow`, `cpt-cf-oagw-principle-no-cache`

## 37. Actor Flows (CDSL)

### Rate-Limit Exceeded with the Reject Strategy

- [ ] `p4` - **ID**: `cpt-cf-oagw-flow-rate-limiting-reject`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The caller receives a 429 with precise retry timing and rate-limit headers.

**Error Scenarios**:
- No tokens remain and the configured strategy is `reject`.

**Steps**:
1. [ ] - `p4` - App sends a proxy request - `inst-rl-reject-request`
2. [ ] - `p4` - Data Plane consumes tokens from the effective bucket (`min(ancestor, descendant)`) for the (upstream, route) pair - `inst-rl-reject-consume`
3. [ ] - `p4` - **IF** the bucket has insufficient tokens **AND** strategy is `reject` **THEN** emit the `RateLimitExceeded` gateway error (problem+json, `X-OAGW-Error-Source: gateway`) - `inst-rl-reject-emit`
4. [ ] - `p4` - **RETURN** 429 with `Retry-After` and `X-RateLimit-Limit` / `X-RateLimit-Remaining` / `X-RateLimit-Reset` headers - `inst-rl-reject-return`

### Rate-Limit Exceeded with Queue or Degrade Strategy

- [ ] `p4` - **ID**: `cpt-cf-oagw-flow-rate-limiting-queue-degrade`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- Requests are queued within bounded capacity or processed with reduced functionality instead of being rejected.

**Error Scenarios**:
- The queue is full, falling back to a rejection.

**Steps**:
1. [ ] - `p4` - Data Plane finds the effective bucket exhausted - `inst-rl-qd-exhausted`
2. [ ] - `p4` - **IF** strategy is `queue` **AND** the bounded queue has capacity **THEN** enqueue the request for later execution - `inst-rl-qd-queue`
3. [ ] - `p4` - **ELSE IF** strategy is `queue` **AND** the queue is full **THEN** reject with the `RateLimitExceeded` outcome - `inst-rl-qd-queue-full`
4. [ ] - `p4` - **IF** strategy is `degrade` **THEN** process the request with reduced functionality - `inst-rl-qd-degrade`
5. [ ] - `p4` - **RETURN** the queued, degraded, or rejected outcome - `inst-rl-qd-return`

## 38. Processes / Business Logic (CDSL)

### Consume Tokens from a Dual-Rate Bucket

- [ ] `p4` - **ID**: `cpt-cf-oagw-algo-rate-limiting-token-bucket`

**Input**: A per-(upstream, route) bucket with dual-rate config and a request cost

**Output**: Allowed, or rate-limit exceeded

**Steps**:
1. [ ] - `p4` - Maintain the bucket for the (upstream, route) key with `algorithm` (`token_bucket` default or `sliding_window`), `sustained` (rate + window of `second`/`minute`/`hour`/`day`), `burst` capacity (defaults to the sustained rate), `scope`, `cost` (default 1), and `strategy` - `inst-rl-bucket-state`
2. [ ] - `p4` - Refill tokens at the sustained replenishment rate up to the burst capacity - `inst-rl-bucket-refill`
3. [ ] - `p4` - Consume `cost` tokens for the request - `inst-rl-bucket-consume`
4. [ ] - `p4` - **IF** tokens are insufficient **THEN** return rate-limit exceeded - `inst-rl-bucket-empty`
5. [ ] - `p4` - Track `remaining` and `reset` time for the response headers - `inst-rl-bucket-track`
6. [ ] - `p4` - **RETURN** allowed - `inst-rl-bucket-return`

### Compute Effective Rate via Hierarchical min()

- [ ] `p4` - **ID**: `cpt-cf-oagw-algo-rate-limiting-effective-min`

**Input**: Rate configs across the tenant chain with sharing modes and budget modes

**Output**: Effective rate and burst capacity

**Steps**:
1. [ ] - `p4` - Walk the tenant chain collecting rate configs per sharing mode (`private`/`inherit`/`enforce`) - `inst-rl-min-walk`
2. [ ] - `p4` - **IF** parent is `private` **THEN** the child's own limit applies - `inst-rl-min-private`
3. [ ] - `p4` - **IF** parent is `inherit` **AND** child specifies none **THEN** the parent's limit applies; **IF** child specifies one **THEN** effective is `min(parent, child)` - `inst-rl-min-inherit`
4. [ ] - `p4` - **IF** parent is `enforce` **THEN** effective is `min(parent, child)` and cannot exceed the parent - `inst-rl-min-enforce`
5. [ ] - `p4` - Apply the budget mode (`unlimited` default, `allocated`, or `shared`) - `inst-rl-min-budget`
6. [ ] - `p4` - **IF** validating a child creation **THEN** reject when the sum of child allocations exceeds the parent total times `overcommit_ratio` (default 1.0, range 1.0-2.0) - `inst-rl-min-overcommit`
7. [ ] - `p4` - **RETURN** the effective rate and capacity - `inst-rl-min-return`

### Emit the 429 RateLimitExceeded Rejection

- [ ] `p4` - **ID**: `cpt-cf-oagw-algo-rate-limiting-emit-429`

**Input**: Exhausted bucket state and request context

**Output**: A 429 rejection response

**Steps**:
1. [ ] - `p4` - Build the `RateLimitExceeded` problem+json envelope (instance `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1`, status 429, `X-OAGW-Error-Source: gateway`, `request_id`/`trace_id`) - `inst-rl-429-envelope`
2. [ ] - `p4` - Set `Retry-After` to the seconds until reset - `inst-rl-429-retry`
3. [ ] - `p4` - **IF** `response_headers` is enabled (default true) **THEN** set `X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset` - `inst-rl-429-headers`
4. [ ] - `p4` - **RETURN** the 429 response - `inst-rl-429-return`

### Resolve Configuration via the Control-Plane Cache

- [ ] `p4` - **ID**: `cpt-cf-oagw-algo-rate-limiting-cache-lookup`

**Input**: A proxy-time resolution key

**Output**: Fresh or cached (upstream, route) configuration

**Steps**:
1. [ ] - `p4` - Look up the Data Plane L1 cache (1000 entries, no TTL, explicit invalidation) by `upstream:{tenant_id}:{alias}`, `route:{upstream_id}:{method}:{path_prefix}`, or `plugin:{plugin_id}` - `inst-rl-cache-l1`
2. [ ] - `p4` - **IF** miss **THEN** consult the Control Plane L1 cache (10,000 entries, authoritative) and the optional L2 (Redis, 5-minute TTL) and repository - `inst-rl-cache-cp`
3. [ ] - `p4` - Populate the caches lazily on read (no proactive warming) - `inst-rl-cache-populate`
4. [ ] - `p4` - **IF** a configuration write occurs **THEN** flush the affected keys in the Control Plane L1/L2 (if enabled) and notify the Data Plane to flush its L1 - `inst-rl-cache-invalidate`
5. [ ] - `p4` - **IF** the cached item is the data-plane resolution target **THEN** return it; upstream responses are **never** cached (client/upstream responsibility) - `inst-rl-cache-no-response`
6. [ ] - `p4` - **RETURN** the resolved configuration - `inst-rl-cache-return`

## 39. States (CDSL)

Not applicable — token buckets are per-instance counters updated on the hot path rather than entity lifecycle state machines; the L1/L2 caches are LRU eviction structures whose consistency is governed by explicit invalidation, not by entity states.

## 40. Definitions of Done

### Dual-Rate Token-Bucket Limiter

- [ ] `p4` - **ID**: `cpt-cf-oagw-dod-rate-limiting-bucket`

The system **MUST** implement per-instance token-bucket limiters keyed by (upstream, route) with dual-rate configuration (`sustained` rate + window, `burst` capacity defaulting to the sustained rate, `algorithm` `token_bucket`/`sliding_window`, `scope`, `cost` default 1, `strategy`, `response_headers` default true) executed on the proxy hot path.

**Implements**:
- `cpt-cf-oagw-algo-rate-limiting-token-bucket`
- `cpt-cf-oagw-flow-rate-limiting-reject`

**Constraints**: None

**Touches**:
- API: `* /api/oagw/v1/proxy/{alias}/...` (hot-path check)
- DB: none
- Entities: `RateLimitConfig`, token-bucket state keyed by (upstream, route)

### Hierarchical min() Inheritance

- [ ] `p4` - **ID**: `cpt-cf-oagw-dod-rate-limiting-hierarchy`

The system **MUST** compute the effective rate as `min(ancestor, descendant)` across the tenant hierarchy honoring sharing modes and budget modes (`unlimited` default, `allocated`, `shared`), and reject child creations whose summed allocations exceed the parent's budget times `overcommit_ratio` (default 1.0, range 1.0-2.0).

**Implements**:
- `cpt-cf-oagw-algo-rate-limiting-effective-min`

**Constraints**: None

**Touches**:
- API: `POST/PUT /api/oagw/v1/upstreams`, `POST/PUT /api/oagw/v1/routes` (config validation)
- DB: `oagw_upstream`, `oagw_route` (rate_limit JSONB)
- Entities: `RateLimitConfig`

### Reject, Queue, and Degrade Strategies

- [ ] `p4` - **ID**: `cpt-cf-oagw-dod-rate-limiting-strategies`

The system **MUST** apply the configured strategy when the effective bucket is exhausted: `reject` returns the `RateLimitExceeded` gateway error, `queue` enqueues within bounded capacity (rejecting when full), and `degrade` processes the request with reduced functionality.

**Implements**:
- `cpt-cf-oagw-flow-rate-limiting-queue-degrade`
- `cpt-cf-oagw-algo-rate-limiting-token-bucket`

**Constraints**: None

**Touches**:
- API: `* /api/oagw/v1/proxy/{alias}/...` (hot-path behavior)
- DB: none
- Entities: `RateLimitConfig`

### 429 Rejection Headers

- [ ] `p4` - **ID**: `cpt-cf-oagw-dod-rate-limiting-headers`

The system **MUST** return 429 with `X-OAGW-Error-Source: gateway` and, per the reject strategy, `Retry-After`, `X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset` headers (header emission governed by `response_headers`), with the problem+json instance `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1` and `retry_after_seconds` extension.

**Implements**:
- `cpt-cf-oagw-algo-rate-limiting-emit-429`
- `cpt-cf-oagw-flow-rate-limiting-reject`

**Constraints**: None

**Touches**:
- API: `* /api/oagw/v1/proxy/{alias}/...` (429 responses)
- DB: none
- Entities: `RateLimitConfig`, `GatewayError`

### Control-Plane Configuration Caching

- [ ] `p4` - **ID**: `cpt-cf-oagw-dod-rate-limiting-cache`

The system **MUST** provide the Control Plane L1 configuration cache (10,000 entries), an optional shared L2 (Redis, 5-minute TTL), and the Data Plane L1 cache (1,000 entries, explicit invalidation) keyed by `upstream:{tenant_id}:{alias}`, `route:{upstream_id}:{method}:{path_prefix}`, and `plugin:{plugin_id}`, with affected-key flush on configuration writes, and **MUST NOT** cache upstream responses.

**Implements**:
- `cpt-cf-oagw-algo-rate-limiting-cache-lookup`

**Constraints**: None

**Touches**:
- API: all management endpoints (write invalidation)
- DB: none
- Entities: `Upstream`, `Route`, `Plugin` (cached views)

## 41. Acceptance Criteria

- [ ] A token bucket with sustained rate R and burst capacity B allows bursts up to B and then enforces the sustained rate
- [ ] With an ancestor limit of 100 and a descendant limit of 30, the effective downstream limit is 30; with an ancestor limit of 30 (enforce) and a descendant limit of 100, the effective limit is 30
- [ ] A child creation whose allocated sum exceeds the parent budget with `overcommit_ratio` of 1.0 is rejected with a validation error
- [ ] Under the reject strategy, an exhausted bucket returns 429 with `Retry-After`, `X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset`, `X-OAGW-Error-Source: gateway`, and problem+json instance `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1` carrying `retry_after_seconds`
- [ ] Under the queue strategy, requests are enqueued within bounded capacity and queued-request handling does not exceed capacity; a full queue falls back to the `RateLimitExceeded` outcome
- [ ] Under the degrade strategy, requests are processed with reduced functionality rather than rejected
- [ ] A configuration write invalidates the affected keys in the Control Plane L1 (and L2 when enabled) and triggers a Data Plane L1 flush
- [ ] An upstream response is never stored by OAGW (no response caching)
- [ ] Rate-limit counters and the rate check complete in-process without an external round-trip

## 42. Additional Context (optional)

Rate limiting in this delivery is per-instance (no distributed coordination); Redis-backed distributed mode and counter persistence across restarts are future work behind the local mode. Automated tests for this gear live in the crate's own test modules (unit and integration tests under the `oagw` crate), not under `testing/e2e/gears/oagw/`, which is reserved for the acceptance suite.


# Feature: OAGW CORS Handling

- [ ] `p4` - **ID**: `cpt-cf-oagw-featstatus-cors-handling`

## 43. Feature Context

- [ ] `p4` - `cpt-cf-oagw-feature-cors-handling`

### 43.1 Overview

Provide per-upstream/route CORS handling: a permissive preflight fast path (204) handled without tenant resolution, and strict exact-match origin and method validation on actual cross-origin requests, per `cpt-cf-oagw-adr-cors`.

### 43.2 Purpose

This feature implements the built-in CORS handler from `cpt-cf-oagw-adr-cors`: preflight detection (OPTIONS + Origin + `Access-Control-Request-Method`) returning a permissive 204, exact-match origin validation (protocol- and port-sensitive, no regex), method validation, 403 rejections with `Vary: Origin`, and secure-by-default behavior (CORS disabled unless configured). It deliberately carries no PRD requirement checkbox: CORS behavior is governed by the `cors` catalog identifier registered under `cpt-cf-oagw-fr-builtin-plugins` (Plugin System) and by upstream/route CORS configuration CRUD (Control-Plane Management API) — documented explicitly so the omission is not silent.

**Requirements**: None (explicitly — see Note above)

**Principles**: None

### 43.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Browser-based clients send preflight and actual cross-origin requests through the proxy |
| `cpt-cf-oagw-actor-platform-operator` | Configures upstream/route CORS settings (allowed origins/methods, credentials) that drive enforcement |

### 43.4 References

- **PRD**: [PRD.md](./PRD.md)
- **Design**: [DESIGN.md](./DESIGN.md)
- **ADRs**: `cpt-cf-oagw-adr-cors` (preflight handling, exact-match origin validation, hierarchical CORS config)
- **Parent feature**: `cpt-cf-oagw-feature-cors-handling`
- **Dependencies**: `cpt-cf-oagw-feature-domain-model-repositories`
- **Related design elements**: `CorsConfig` (upstream/route effective configuration), `cpt-cf-oagw-component-data-plane` (actual-request enforcement hook)

## 44. Actor Flows (CDSL)

### Handle a CORS Preflight Request

- [ ] `p4` - **ID**: `cpt-cf-oagw-flow-cors-handling-preflight`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The browser preflight receives a permissive 204 with the requested origin, method, and headers echoed, without an upstream round-trip.

**Error Scenarios**:
- None at the gateway level — preflights are permissive by design; enforcement is deferred to the actual request.

**Steps**:
1. [ ] - `p4` - Browser sends OPTIONS with `Origin` and `Access-Control-Request-Method` headers - `inst-co-preflight-req`
2. [ ] - `p4` - **IF** the request is a CORS preflight **THEN** handle it in the handler fast path without tenant resolution - `inst-co-preflight-detect`
3. [ ] - `p4` - **RETURN** 204 No Content echoing `Access-Control-Allow-Origin` (the requested origin), `Access-Control-Allow-Methods`, `Access-Control-Allow-Headers`, `Access-Control-Max-Age: 86400`, and `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers` - `inst-co-preflight-204`
4. [ ] - `p4` - Preflight bypasses per-request auth and plugin checks (remaining subject to global/edge infrastructure controls) - `inst-co-preflight-bypass`

### Handle an Actual Cross-Origin Request

- [ ] `p4` - **ID**: `cpt-cf-oagw-flow-cors-handling-actual`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A request from an allowed origin with an allowed method proceeds and receives CORS response headers.

**Error Scenarios**:
- The origin or method is not allowed, producing a 403 with `Vary: Origin`.

**Steps**:
1. [ ] - `p4` - App sends an actual cross-origin request; the system resolves the upstream and route with the authenticated tenant context - `inst-co-actual-resolve`
2. [ ] - `p4` - **IF** CORS is not configured on the effective upstream/route configuration **THEN** no CORS validation is applied (disabled unless explicitly enabled) - `inst-co-actual-disabled`
3. [ ] - `p4` - **IF** the request `Origin` is not in `allowed_origins` (exact match) **THEN** return 403 with `Vary: Origin` and the `cors.origin_not_allowed` type - `inst-co-actual-origin`
4. [ ] - `p4` - **IF** the request method is not in `allowed_methods` **THEN** return 403 with `Vary: Origin` and the `cors.method_not_allowed` type - `inst-co-actual-method`
5. [ ] - `p4` - **RETURN** the proxied request with CORS response headers (`Access-Control-Allow-Origin`, `Access-Control-Expose-Headers`, `Access-Control-Allow-Credentials`) - `inst-co-actual-ok`

## 45. Processes / Business Logic (CDSL)

### Detect a CORS Preflight

- [ ] `p4` - **ID**: `cpt-cf-oagw-algo-cors-handling-detect-preflight`

**Input**: A request

**Output**: Preflight or non-preflight classification

**Steps**:
1. [ ] - `p4` - **IF** method is OPTIONS **AND** `Origin` is present **AND** `Access-Control-Request-Method` is present **THEN** classify as a CORS preflight - `inst-co-detect-check`
2. [ ] - `p4` - **ELSE** classify as a regular (actual) request - `inst-co-detect-else`
3. [ ] - `p4` - **RETURN** the classification - `inst-co-detect-return`

### Validate Origin by Exact Match

- [ ] `p4` - **ID**: `cpt-cf-oagw-algo-cors-handling-validate-origin`

**Input**: Request `Origin`, configured `allowed_origins`, and credentials setting

**Output**: Allowed or 403 `cors.origin_not_allowed`

**Steps**:
1. [ ] - `p4` - **IF** `*` is configured **THEN** match any origin - `inst-co-origin-wildcard`
2. [ ] - `p4` - **ELSE** compare the `Origin` by exact string match (protocol- and port-sensitive); no regex patterns are supported - `inst-co-origin-exact`
3. [ ] - `p4` - **IF** no match **THEN** return 403 with `cors.origin_not_allowed` and `Vary: Origin` - `inst-co-origin-reject`
4. [ ] - `p4` - **RETURN** allowed - `inst-co-origin-ok`

### Validate Method Against Allowed Methods

- [ ] `p4` - **ID**: `cpt-cf-oagw-algo-cors-handling-validate-method`

**Input**: Request method and `allowed_methods`

**Output**: Allowed or 403 `cors.method_not_allowed`

**Steps**:
1. [ ] - `p4` - **IF** the method is not in `allowed_methods` **THEN** return 403 with `cors.method_not_allowed` and `Vary: Origin` - `inst-co-method-reject`
2. [ ] - `p4` - **RETURN** allowed - `inst-co-method-ok`

### Merge CORS Configuration Across the Hierarchy

- [ ] `p4` - **ID**: `cpt-cf-oagw-algo-cors-handling-merge-hierarchy`

**Input**: Effective upstream/route CORS config with sharing modes

**Output**: The effective `CorsConfig` for enforcement

**Steps**:
1. [ ] - `p4` - Read `CorsConfig` from the effective upstream/route configuration - `inst-co-merge-read`
2. [ ] - `p4` - **IF** CORS is disabled (not configured) **THEN** no CORS handling applies - `inst-co-merge-disabled`
3. [ ] - `p4` - **IF** sharing is `inherit` **THEN** union descendant origins with ancestor origins - `inst-co-merge-inherit`
4. [ ] - `p4` - **IF** sharing is `enforce` **THEN** the ancestor origin set is forced; descendants cannot add origins - `inst-co-merge-enforce`
5. [ ] - `p4` - **IF** `allow_credentials: true` is combined with the wildcard origin `*` **THEN** the configuration is rejected at validation time - `inst-co-merge-creds`
6. [ ] - `p4` - **RETURN** the effective `CorsConfig` - `inst-co-merge-return`

## 46. States (CDSL)

Not applicable — `CorsConfig` is a static configuration value on the effective upstream/route configuration; CORS enforcement is per-request logic with no entity lifecycle state machine.

## 47. Definitions of Done

### Permissive Preflight Fast Path

- [ ] `p4` - **ID**: `cpt-cf-oagw-dod-cors-handling-preflight`

The system **MUST** detect CORS preflights (OPTIONS + `Origin` + `Access-Control-Request-Method`) and return a permissive 204 No Content echoing the requested origin, method, and headers with `Access-Control-Max-Age: 86400` and `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers`, handled in the API layer without tenant resolution and without proxying to the upstream.

**Implements**:
- `cpt-cf-oagw-flow-cors-handling-preflight`
- `cpt-cf-oagw-algo-cors-handling-detect-preflight`

**Constraints**: None

**Touches**:
- API: `OPTIONS /api/oagw/v1/proxy/{alias}/...` (preflight fast path)
- DB: none
- Entities: none

### Actual-Request Enforcement

- [ ] `p4` - **ID**: `cpt-cf-oagw-dod-cors-handling-actual`

The system **MUST** validate the `Origin` by exact match (protocol- and port-sensitive; `*` matches any; no regex) against `allowed_origins` and the method against `allowed_methods` on actual cross-origin requests after upstream resolution, rejecting with 403 (`cors.origin_not_allowed` / `cors.method_not_allowed`) and `Vary: Origin`, and adding CORS response headers (`Access-Control-Allow-Origin`, `Access-Control-Expose-Headers`, `Access-Control-Allow-Credentials`) to allowed responses.

**Implements**:
- `cpt-cf-oagw-flow-cors-handling-actual`
- `cpt-cf-oagw-algo-cors-handling-validate-origin`
- `cpt-cf-oagw-algo-cors-handling-validate-method`

**Constraints**: None

**Touches**:
- API: `* /api/oagw/v1/proxy/{alias}/...` (actual-request enforcement)
- DB: none
- Entities: `CorsConfig`

### CORS Configuration and Secure Defaults

- [ ] `p4` - **ID**: `cpt-cf-oagw-dod-cors-handling-config`

The system **MUST** read `CorsConfig` from the effective upstream/route configuration, default to CORS disabled unless explicitly enabled, apply hierarchical union-under-`inherit`/forced-under-`enforce` semantics, and reject `allow_credentials: true` combined with the wildcard origin at configuration validation time.

**Implements**:
- `cpt-cf-oagw-algo-cors-handling-merge-hierarchy`
- `cpt-cf-oagw-flow-cors-handling-actual`

**Constraints**: None

**Touches**:
- API: `POST/PUT /api/oagw/v1/upstreams`, `POST/PUT /api/oagw/v1/routes` (cors JSONB)
- DB: `oagw_upstream`, `oagw_route` (`cors` JSONB)
- Entities: `CorsConfig`

## 48. Acceptance Criteria

- [ ] An OPTIONS preflight with `Origin` and `Access-Control-Request-Method` returns 204 with the origin, methods, and headers echoed, `Access-Control-Max-Age: 86400`, and `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers`, without contacting the upstream
- [ ] An actual request from an origin in `allowed_origins` with an allowed method proceeds and receives `Access-Control-Allow-Origin` (and Expose-Headers/Allow-Credentials as configured)
- [ ] An actual request from an origin not in `allowed_origins` returns 403 with the `cors.origin_not_allowed` type and `Vary: Origin`
- [ ] An actual request with a method not in `allowed_methods` returns 403 with the `cors.method_not_allowed` type and `Vary: Origin`
- [ ] Origin matching is exact: `https://evil.com.example.com` does not match `allowed_origins` containing `https://example.com`
- [ ] Upstream/route configuration combining `allow_credentials: true` with the wildcard origin is rejected at validation time
- [ ] An upstream/route with no CORS configuration applies no CORS validation or response headers (disabled by default)
- [ ] With `inherit` CORS sharing, a descendant's origins are unioned with the parent's; with `enforce`, a descendant cannot add origins

## 49. Additional Context (optional)

CORS is core Data Plane logic (not a `GuardPlugin` implementation); there is no CORS plugin implementation — only the `cors` catalog identifier registered under Plugin System. Preflight requests remain subject to infrastructure-level controls (global/edge rate limiting, WAF/DDoS). Automated tests for this gear live in the crate's own test modules (unit and integration tests under the `oagw` crate), not under `testing/e2e/gears/oagw/`, which is reserved for the acceptance suite.


# Feature: OAGW Data-Plane Proxy Service

- [ ] `p5` - **ID**: `cpt-cf-oagw-featstatus-data-plane-proxy`

## 50. Feature Context

- [ ] `p5` - `cpt-cf-oagw-feature-data-plane-proxy`

### 50.1 Overview

Orchestrate the proxy hot path: alias resolution across the tenant hierarchy, route matching, effective-configuration application, plugin chain execution, upstream forwarding with streaming passthrough, header handling, target-host selection, circuit breaking, SSRF enforcement on the outbound surface, and gateway/upstream error-source attribution, per `cpt-cf-oagw-seq-proxy-flow`.

### 50.2 Purpose

This feature implements `cpt-cf-oagw-component-data-plane`: request proxying (`cpt-cf-oagw-fr-request-proxy`), enable/disable semantics (`cpt-cf-oagw-fr-enable-disable`), header transformation (`cpt-cf-oagw-fr-header-transform`), streaming (`cpt-cf-oagw-fr-streaming`), alias resolution (`cpt-cf-oagw-fr-alias-resolution`), and the proxy-path NFRs (low latency, high availability, SSRF protection, input validation), under `cpt-cf-oagw-principle-no-retry` and the no-direct-internet / body-limit / https-only constraints.

**Requirements**: `cpt-cf-oagw-fr-request-proxy`, `cpt-cf-oagw-fr-enable-disable`, `cpt-cf-oagw-fr-header-transform`, `cpt-cf-oagw-fr-streaming`, `cpt-cf-oagw-fr-alias-resolution`, `cpt-cf-oagw-nfr-low-latency`, `cpt-cf-oagw-nfr-high-availability`, `cpt-cf-oagw-nfr-ssrf-protection`, `cpt-cf-oagw-nfr-input-validation`

**Principles**: `cpt-cf-oagw-principle-no-retry`

### 50.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Sends proxy requests by alias with optional path and query and consumes the streamed response with error-source attribution |
| `cpt-cf-oagw-actor-upstream-service` | Receives the forwarded request and returns responses that are passed through unchanged |
| `cpt-cf-oagw-actor-cred-store` | Supplies secret material to auth plugins executed on the hot path |

### 50.4 References

- **PRD**: [PRD.md](./PRD.md)
- **Design**: [DESIGN.md](./DESIGN.md)
- **ADRs**: `cpt-cf-oagw-adr-request-routing` (proxy routing rules and X-OAGW-Target-Host matrix), `cpt-cf-oagw-adr-state-management` (DP L1 and DP-owned rate limiters), `cpt-cf-oagw-adr-error-source-distinction` (error attribution), `cpt-cf-oagw-adr-cors` (actual-request enforcement hook), `cpt-cf-oagw-adr-plugin-system` (chain execution)
- **Parent feature**: `cpt-cf-oagw-feature-data-plane-proxy`
- **Dependencies**: `cpt-cf-oagw-feature-control-plane-api`, `cpt-cf-oagw-feature-plugin-system`, `cpt-cf-oagw-feature-rate-limiting`, `cpt-cf-oagw-feature-cors-handling`, `cpt-cf-oagw-feature-error-semantics`
- **Related design elements**: `cpt-cf-oagw-component-data-plane`, `cpt-cf-oagw-seq-proxy-flow`, `cpt-cf-oagw-interface-proxy-api`, `cpt-cf-oagw-constraint-no-direct-internet`, `cpt-cf-oagw-constraint-body-limit`, `cpt-cf-oagw-constraint-https-only`

## 51. Actor Flows (CDSL)

### Execute a Proxy Request

- [ ] `p5` - **ID**: `cpt-cf-oagw-flow-data-plane-proxy-execute`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The request is resolved, matched, transformed, forwarded to the upstream with streaming passthrough, and the response is returned with error-source attribution.

**Error Scenarios**:
- Alias unresolvable, upstream disabled, auth failure, rate-limit rejection, payload too large, SSRF rejection, upstream failure, or timeout — each mapped to the framework error instances.

**Steps**:
1. [ ] - `p5` - App sends `{METHOD} /api/oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]` - `inst-dp-exec-request`
2. [ ] - `p5` - API handler extracts the `SecurityContext`, classifies the request as a proxy operation, and checks the `gts.cf.core.oagw.proxy.v1~:invoke` permission - `inst-dp-exec-authz`
3. [ ] - `p5` - Data Plane runs the rate-limit check against the effective bucket (hook to Rate Limiting) - `inst-dp-exec-rl`
4. [ ] - `p5` - Resolve the upstream by alias with the descendant-to-root chain walk (shadowing) - `inst-dp-exec-resolve`
5. [ ] - `p5` - **IF** the upstream is disabled **THEN** return 503 service-unavailable gateway error - `inst-dp-exec-disabled`
6. [ ] - `p5` - Match a route by method allowlist and longest path prefix among enabled routes - `inst-dp-exec-match`
7. [ ] - `p5` - **IF** no route matches **THEN** return 404 `RouteNotFound` - `inst-dp-exec-noroute`
8. [ ] - `p5` - Merge the effective configuration (upstream base, then route, then tenant chain) - `inst-dp-exec-merge`
9. [ ] - `p5` - Enforce CORS actual-request validation when configured (hook to CORS Handling) - `inst-dp-exec-cors`
10. [ ] - `p5` - Execute the plugin chain: auth, guards (request), transform (request) - `inst-dp-exec-plugins`
11. [ ] - `p5` - Apply target-host selection, header transforms and hop-by-hop stripping, and SSRF checks, then forward the streaming body via the toolkit HTTP client - `inst-dp-exec-forward`
12. [ ] - `p5` - Apply the `proxy_timeout_secs` timeout policy; never retry automatically - `inst-dp-exec-timeout`
13. [ ] - `p5` - Stream the response passthrough and run transform (response/error) - `inst-dp-exec-stream`
14. [ ] - `p5` - **RETURN** the response with `X-OAGW-Error-Source` (`gateway` for gateway errors, `upstream` for passthrough) and emit metrics/audit hooks - `inst-dp-exec-return`

### Proxy a Request Targeting a Disabled Resource

- [ ] `p5` - **ID**: `cpt-cf-oagw-flow-data-plane-proxy-disable`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- Disabled upstreams and routes are handled with the documented semantics.

**Error Scenarios**:
- A disabled upstream causes all proxy requests to it to be rejected.

**Steps**:
1. [ ] - `p5` - **IF** the resolved upstream has `enabled = false` **THEN** reject the request with a 503 service-unavailable gateway error - `inst-dp-dis-upstream`
2. [ ] - `p5` - **IF** an ancestor tenant disabled the upstream **THEN** it remains disabled for all descendants and descendants cannot re-enable it - `inst-dp-dis-ancestor`
3. [ ] - `p5` - **IF** only disabled routes match the request **THEN** those routes are excluded from matching, producing the route-not-found outcome - `inst-dp-dis-route`
4. [ ] - `p5` - **RETURN** the disabled-resource outcome - `inst-dp-dis-return`

### Route to a Specific Endpoint via X-OAGW-Target-Host

- [ ] `p5` - **ID**: `cpt-cf-oagw-flow-data-plane-proxy-target-host`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A specific endpoint in a multi-endpoint upstream is selected, or load balancing applies by default.

**Error Scenarios**:
- The header is missing when required, malformed, or names an unknown endpoint — each with its dedicated 400 instance.

**Steps**:
1. [ ] - `p5` - **IF** the upstream has a single endpoint **THEN** route to it (a present `X-OAGW-Target-Host` is validated) - `inst-dp-th-single`
2. [ ] - `p5` - **IF** the upstream has multiple endpoints **AND** an explicit alias (no common suffix) **THEN** route to the header-specified endpoint when present, or round-robin across the pool - `inst-dp-th-multi-explicit`
3. [ ] - `p5` - **IF** the upstream has multiple endpoints **AND** a common-suffix alias **THEN** the header is required: absence returns 400 `MissingTargetHost` with the valid hosts - `inst-dp-th-multi-missing`
4. [ ] - `p5` - **IF** the header value is not a valid hostname or IP (no port, path, or special characters) **THEN** return 400 `InvalidTargetHost` echoing the invalid value - `inst-dp-th-invalid`
5. [ ] - `p5` - **IF** the header value matches no configured endpoint **THEN** return 400 `UnknownTargetHost` with the invalid value and valid hosts - `inst-dp-th-unknown`
6. [ ] - `p5` - **RETURN** the selected endpoint - `inst-dp-th-return`

## 52. Processes / Business Logic (CDSL)

### Resolve Upstream by Alias Across the Tenant Hierarchy

- [ ] `p5` - **ID**: `cpt-cf-oagw-algo-data-plane-proxy-resolve-alias`

**Input**: Alias and the calling tenant chain

**Output**: Nearest `UpstreamConfig` or `RouteNotFound`

**Steps**:
1. [ ] - `p5` - Start at the calling tenant and walk the chain descendant to root - `inst-dp-alias-walk`
2. [ ] - `p5` - At each level, look up the upstream by alias (case-insensitive, trailing dots stripped) - `inst-dp-alias-lookup`
3. [ ] - `p5` - The closest match wins; a descendant upstream shadows an ancestor's upstream with the same alias - `inst-dp-alias-shadow`
4. [ ] - `p5` - Ancestor enforced limits (rate, disabled state, `enforce` sharing) still apply across shadowing - `inst-dp-alias-enforced`
5. [ ] - `p5` - **IF** no level yields a match **THEN** return `RouteNotFound` (404) - `inst-dp-alias-notfound`
6. [ ] - `p5` - **RETURN** the resolved upstream - `inst-dp-alias-return`

### Match Route by Method Allowlist and Longest Path Prefix

- [ ] `p5` - **ID**: `cpt-cf-oagw-algo-data-plane-proxy-match-route`

**Input**: Effective upstream, request method, and normalized path

**Output**: Matched `RouteConfig` or `RouteNotFound`

**Steps**:
1. [ ] - `p5` - Consider only enabled routes of the upstream - `inst-dp-match-enabled`
2. [ ] - `p5` - **IF** the request method is not in the route's allowlist (an empty allowlist rejects all methods) **THEN** the route does not match - `inst-dp-match-method`
3. [ ] - `p5` - Match the route whose `path_prefix` is the longest prefix of the normalized request path - `inst-dp-match-prefix`
4. [ ] - `p5` - **IF** multiple routes tie **THEN** the higher `priority` wins - `inst-dp-match-priority`
5. [ ] - `p5` - **IF** no route matches **THEN** return `RouteNotFound` (404) - `inst-dp-match-notfound`
6. [ ] - `p5` - **RETURN** the matched route - `inst-dp-match-return`

### Apply the Effective Configuration Merge

- [ ] `p5` - **ID**: `cpt-cf-oagw-algo-data-plane-proxy-apply-config`

**Input**: Upstream base config, route config, and tenant-chain effective config

**Output**: Effective configuration for the request

**Steps**:
1. [ ] - `p5` - Start from the upstream base configuration (lowest priority) - `inst-dp-cfg-base`
2. [ ] - `p5` - Overlay the route-level overrides - `inst-dp-cfg-route`
3. [ ] - `p5` - Overlay the tenant-chain effective values per sharing mode (highest priority), using the domain-layer merge semantics - `inst-dp-cfg-tenant`
4. [ ] - `p5` - Honor `enforce` fields so ancestor values override descendant values - `inst-dp-cfg-enforce`
5. [ ] - `p5` - **RETURN** the effective configuration - `inst-dp-cfg-return`

### Apply Target-Host Selection and the Behavior Matrix

- [ ] `p5` - **ID**: `cpt-cf-oagw-algo-data-plane-proxy-target-host`

**Input**: Endpoint pool, alias type, and the `X-OAGW-Target-Host` header

**Output**: Selected endpoint or the dedicated 400 gateway error

**Steps**:
1. [ ] - `p5` - **IF** the pool has a single endpoint **THEN** select it; validate the header when present - `inst-dp-thx-single`
2. [ ] - `p5` - **IF** the pool has multiple endpoints **AND** an explicit alias **THEN** select the header-specified endpoint, or round-robin when absent (bypassing load balancing when the header selects) - `inst-dp-thx-explicit`
3. [ ] - `p5` - **IF** the pool has multiple endpoints **AND** a common-suffix alias **THEN** require the header; **IF** absent **THEN** return 400 `MissingTargetHost` listing valid hosts - `inst-dp-thx-required`
4. [ ] - `p5` - **IF** the header format is invalid **THEN** return 400 `InvalidTargetHost` - `inst-dp-thx-format`
5. [ ] - `p5` - **IF** the header names an unknown endpoint **THEN** return 400 `UnknownTargetHost` - `inst-dp-thx-unknown`
6. [ ] - `p5` - **RETURN** the selected endpoint - `inst-dp-thx-return`

### Apply Header Transforms and Hop-by-Hop Stripping

- [ ] `p5` - **ID**: `cpt-cf-oagw-algo-data-plane-proxy-headers`

**Input**: Request/response headers, upstream `headers` transform config, and routing headers

**Output**: Transformed header set

**Steps**:
1. [ ] - `p5` - Categorize headers as routing, hop-by-hop, or passthrough - `inst-dp-hdr-categorize`
2. [ ] - `p5` - Apply the upstream `headers` transform operations (set, add, remove) - `inst-dp-hdr-transform`
3. [ ] - `p5` - Strip hop-by-hop headers: `Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade` - `inst-dp-hdr-strip`
4. [ ] - `p5` - Reject header names/values containing CR or LF (HTTP smuggling defense) - `inst-dp-hdr-crlf`
5. [ ] - `p5` - Validate the request query against the route's query allowlist and apply the path suffix mode (`disabled` or `append`) - `inst-dp-hdr-query`
6. [ ] - `p5` - Apply `X-OAGW-Target-Host` on request selection and `X-OAGW-Error-Source` on the response - `inst-dp-hdr-osrc`
7. [ ] - `p5` - **RETURN** the transformed header set - `inst-dp-hdr-return`

### Enforce SSRF on the Outbound Surface

- [ ] `p5` - **ID**: `cpt-cf-oagw-algo-data-plane-proxy-ssrf`

**Input**: Outbound request (scheme, host, headers, body size)

**Output**: Forwardable request, or the mapped gateway error

**Steps**:
1. [ ] - `p5` - **IF** the scheme is not HTTPS **AND** `allow_http_upstream` is disabled **THEN** reject (plaintext upstream blocked) - `inst-dp-ssrf-scheme`
2. [ ] - `p5` - **IF** the hostname fails RFC 1123 validation **THEN** reject - `inst-dp-ssrf-host`
3. [ ] - `p5` - **IF** `Content-Length` and `Transfer-Encoding` are both present or otherwise inconsistent **THEN** reject (smuggling defense) - `inst-dp-ssrf-te`
4. [ ] - `p5` - **IF** the body size exceeds the 100MB cap **THEN** reject with 413 `PayloadTooLarge` before buffering - `inst-dp-ssrf-body`
5. [ ] - `p5` - **RETURN** the forwardable request - `inst-dp-ssrf-return`

### Proxy Bodies with Streaming Passthrough

- [ ] `p5` - **ID**: `cpt-cf-oagw-algo-data-plane-proxy-passthrough`

**Input**: Request body stream and upstream response stream

**Output**: Streamed passthrough response

**Steps**:
1. [ ] - `p5` - Stream the request body to the upstream via the toolkit HTTP client (SSE-compatible) - `inst-dp-pass-req`
2. [ ] - `p5` - Stream the response body back to the caller without buffering - `inst-dp-pass-resp`
3. [ ] - `p5` - Enforce the total timeout from `proxy_timeout_secs` (default 2) with connection, request, and idle timeout classes - `inst-dp-pass-timeout`
4. [ ] - `p5` - **IF** the upstream closes mid-stream **THEN** return `StreamAborted` (502) - `inst-dp-pass-abort`
5. [ ] - `p5` - **IF** the client disconnects **THEN** close the upstream connection and log the event - `inst-dp-pass-client`
6. [ ] - `p5` - Never re-issue the entire client request automatically (endpoint-level connector retries remain permitted) - `inst-dp-pass-noretry`
7. [ ] - `p5` - **RETURN** the streamed response - `inst-dp-pass-return`

### Enforce the Circuit Breaker

- [ ] `p5` - **ID**: `cpt-cf-oagw-algo-data-plane-proxy-circuit-breaker`

**Input**: Per-upstream failure history and circuit state

**Output**: Forward decision or `CircuitBreakerOpen` rejection

**Steps**:
1. [ ] - `p5` - Track failed requests per upstream window - `inst-dp-cb-track`
2. [ ] - `p5` - **IF** 5 failed requests occur within a 30s window **THEN** open the circuit - `inst-dp-cb-open`
3. [ ] - `p5` - **IF** the circuit is open **THEN** reject requests with `CircuitBreakerOpen` (503) - `inst-dp-cb-reject`
4. [ ] - `p5` - **IF** the cooldown elapses **THEN** allow a probe request (half-open) - `inst-dp-cb-probe`
5. [ ] - `p5` - **IF** the probe succeeds **THEN** close the circuit; **IF** it fails **THEN** reopen - `inst-dp-cb-closed`
6. [ ] - `p5` - **RETURN** the forward decision - `inst-dp-cb-return`

### Attribute Response Source

- [ ] `p5` - **ID**: `cpt-cf-oagw-algo-data-plane-proxy-error-attribution`

**Input**: A response and whether it originated at the gateway or the upstream

**Output**: A response attributed with the error-source header

**Steps**:
1. [ ] - `p5` - **IF** the failure is gateway-originated **THEN** emit the RFC 9457 problem+json envelope with the mapped GTS instance and `X-OAGW-Error-Source: gateway` - `inst-dp-att-gw`
2. [ ] - `p5` - **IF** the upstream returned an error **THEN** pass the body through unchanged with `X-OAGW-Error-Source: upstream` - `inst-dp-att-up`
3. [ ] - `p5` - **RETURN** the attributed response - `inst-dp-att-return`

## 53. States (CDSL)

### Circuit Breaker State Machine

- [ ] `p5` - **ID**: `cpt-cf-oagw-state-data-plane-proxy-circuit-breaker`

**States**: CIRCUIT_CLOSED, CIRCUIT_OPEN, CIRCUIT_HALF_OPEN

**Initial State**: CIRCUIT_CLOSED

**Transitions**:
1. [ ] - `p5` - **FROM** CIRCUIT_CLOSED **TO** CIRCUIT_OPEN **WHEN** 5 failed requests occur within a 30s window - `inst-dp-cbs-open`
2. [ ] - `p5` - **FROM** CIRCUIT_OPEN **TO** CIRCUIT_HALF_OPEN **WHEN** the cooldown elapses and a probe request is admitted - `inst-dp-cbs-half`
3. [ ] - `p5` - **FROM** CIRCUIT_HALF_OPEN **TO** CIRCUIT_CLOSED **WHEN** the probe request succeeds - `inst-dp-cbs-close`
4. [ ] - `p5` - **FROM** CIRCUIT_HALF_OPEN **TO** CIRCUIT_OPEN **WHEN** the probe request fails - `inst-dp-cbs-reopen`

## 54. Definitions of Done

### Alias Resolution and Shadowing

- [ ] `p5` - **ID**: `cpt-cf-oagw-dod-data-plane-proxy-alias-resolution`

The system **MUST** resolve the upstream by alias with the descendant-to-root tenant-chain walk, closest-match shadowing, case-insensitive matching, and ancestor enforced limits (rate, disabled state, `enforce` sharing) applied across shadowing, returning `RouteNotFound` when no level matches.

**Implements**:
- `cpt-cf-oagw-algo-data-plane-proxy-resolve-alias`
- `cpt-cf-oagw-flow-data-plane-proxy-execute`

**Constraints**: `cpt-cf-oagw-constraint-no-direct-internet`, `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `* /api/oagw/v1/proxy/{alias}/...`
- DB: `oagw_upstream` (alias, enabled)
- Entities: `Upstream`, `HostEntry`

### Enable/Disable Semantics

- [ ] `p5` - **ID**: `cpt-cf-oagw-dod-data-plane-proxy-enable-disable`

The system **MUST** reject all proxy requests to a disabled upstream with a 503 service-unavailable gateway error, exclude disabled routes from route matching, and keep ancestor-disabled resources disabled for descendants with no re-enable path.

**Implements**:
- `cpt-cf-oagw-flow-data-plane-proxy-disable`
- `cpt-cf-oagw-algo-data-plane-proxy-match-route`

**Constraints**: `cpt-cf-oagw-constraint-no-direct-internet`, `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `* /api/oagw/v1/proxy/{alias}/...`
- DB: `oagw_upstream` (`enabled`), `oagw_route` (`enabled`)
- Entities: `Upstream`, `Route`

### Route Matching

- [ ] `p5` - **ID**: `cpt-cf-oagw-dod-data-plane-proxy-route-matching`

The system **MUST** match routes by HTTP method allowlist (empty allowlist rejects all methods) and longest path prefix among enabled routes, breaking ties by priority, and return `RouteNotFound` (404) with instance `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1` when no route matches.

**Implements**:
- `cpt-cf-oagw-algo-data-plane-proxy-match-route`
- `cpt-cf-oagw-flow-data-plane-proxy-execute`

**Constraints**: `cpt-cf-oagw-constraint-no-direct-internet`, `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `* /api/oagw/v1/proxy/{alias}/...`
- DB: `oagw_route`, `oagw_route_http_match`, `oagw_route_method`
- Entities: `Route`, `RouteMatch`, `RouteMethod`

### Effective Configuration Application

- [ ] `p5` - **ID**: `cpt-cf-oagw-dod-data-plane-proxy-effective-config`

The system **MUST** apply the effective configuration to each proxy request with the merge order upstream (base), then route, then tenant (highest), honoring field sharing modes (`private`/`inherit`/`enforce`) using the domain-layer merge semantics.

**Implements**:
- `cpt-cf-oagw-algo-data-plane-proxy-apply-config`
- `cpt-cf-oagw-flow-data-plane-proxy-execute`

**Constraints**: `cpt-cf-oagw-constraint-no-direct-internet`, `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `* /api/oagw/v1/proxy/{alias}/...`
- DB: `oagw_upstream`, `oagw_route`
- Entities: `Upstream`, `Route`, `ProxyContext`

### Plugin Chain Execution

- [ ] `p5` - **ID**: `cpt-cf-oagw-dod-data-plane-proxy-plugin-chain`

The system **MUST** execute the plugin chain on the hot path in the deterministic order Auth, Guards, Transform(request), upstream, Transform(response/error) with upstream plugins before route plugins, rejecting when a guard rejects and mapping auth failures to the authentication-failed instance.

**Implements**:
- `cpt-cf-oagw-flow-data-plane-proxy-execute`

**Constraints**: `cpt-cf-oagw-constraint-no-direct-internet`, `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `* /api/oagw/v1/proxy/{alias}/...`
- DB: `oagw_upstream_plugin`, `oagw_route_plugin`
- Entities: `Plugin`, `ProxyContext`

### Target-Host Selection Matrix

- [ ] `p5` - **ID**: `cpt-cf-oagw-dod-data-plane-proxy-target-host`

The system **MUST** implement the `X-OAGW-Target-Host` behavior matrix: single-endpoint pools validate an optional header; multi-endpoint explicit-alias pools round-robin by default and route to the header-selected endpoint; multi-endpoint common-suffix pools require the header — emitting 400 `MissingTargetHost` (instance `gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1`), 400 `InvalidTargetHost` (`...cf.oagw.routing.invalid_target_host.v1`), or 400 `UnknownTargetHost` (`...cf.oagw.routing.unknown_target_host.v1`) with valid-host details as appropriate.

**Implements**:
- `cpt-cf-oagw-algo-data-plane-proxy-target-host`
- `cpt-cf-oagw-flow-data-plane-proxy-target-host`

**Constraints**: `cpt-cf-oagw-constraint-no-direct-internet`, `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `* /api/oagw/v1/proxy/{alias}/...`
- DB: `oagw_upstream` (server pool)
- Entities: `Endpoint`, `HostEntry`, `TargetHost`

### Header Matrix and Hop-by-Hop Stripping

- [ ] `p5` - **ID**: `cpt-cf-oagw-dod-data-plane-proxy-header-matrix`

The system **MUST** apply the header matrix: routing headers (`X-OAGW-Target-Host`, `X-OAGW-Error-Source`), upstream `headers` set/add/remove transforms, passthrough control, stripping of hop-by-hop headers (`Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`), CR/LF rejection, route query-allowlist validation, and path suffix `disabled`/`append` modes.

**Implements**:
- `cpt-cf-oagw-algo-data-plane-proxy-headers`
- `cpt-cf-oagw-flow-data-plane-proxy-execute`

**Constraints**: `cpt-cf-oagw-constraint-no-direct-internet`, `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `* /api/oagw/v1/proxy/{alias}/...`
- DB: `oagw_upstream` (`headers` JSONB), `oagw_route_http_match`
- Entities: `HeadersConfig`, `ProxyContext`

### Streaming, Timeout, and No-Retry Policy

- [ ] `p5` - **ID**: `cpt-cf-oagw-dod-data-plane-proxy-streaming`

The system **MUST** stream request and response bodies (SSE-compatible) without buffering through the toolkit HTTP client, enforce the `proxy_timeout_secs` timeout policy with connection/request/idle timeout classes, never automatically re-issue a client request, and map stream aborts to `StreamAborted` (502).

**Implements**:
- `cpt-cf-oagw-algo-data-plane-proxy-passthrough`
- `cpt-cf-oagw-flow-data-plane-proxy-execute`

**Constraints**: `cpt-cf-oagw-constraint-no-direct-internet`, `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `* /api/oagw/v1/proxy/{alias}/...`
- DB: none
- Entities: `ProxyContext`, `ProxyResponse`

### Outbound SSRF, Payload, and Smuggling Enforcement

- [ ] `p5` - **ID**: `cpt-cf-oagw-dod-data-plane-proxy-ssrf`

The system **MUST** enforce on the outbound surface: HTTPS-only scheme by default (plaintext only with `allow_http_upstream`), RFC 1123 hostname validation, `Content-Length`/`Transfer-Encoding` consistency, and the 100MB body cap returning 413 with instance `gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1` before buffering.

**Implements**:
- `cpt-cf-oagw-algo-data-plane-proxy-ssrf`
- `cpt-cf-oagw-flow-data-plane-proxy-execute`

**Constraints**: `cpt-cf-oagw-constraint-no-direct-internet`, `cpt-cf-oagw-constraint-https-only`, `cpt-cf-oagw-constraint-body-limit`

**Touches**:
- API: `* /api/oagw/v1/proxy/{alias}/...`
- DB: none
- Entities: `ProxyContext`

### Circuit Breaker

- [ ] `p5` - **ID**: `cpt-cf-oagw-dod-data-plane-proxy-circuit-breaker`

The system **MUST** maintain a per-upstream circuit breaker (core policy, not a plugin) that opens after 5 failed requests in a 30s window, rejects with 503 `CircuitBreakerOpen` (instance `gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1`) while open, admits probes in half-open, and closes on a successful probe.

**Implements**:
- `cpt-cf-oagw-algo-data-plane-proxy-circuit-breaker`
- `cpt-cf-oagw-state-data-plane-proxy-circuit-breaker`

**Constraints**: `cpt-cf-oagw-constraint-no-direct-internet`, `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `* /api/oagw/v1/proxy/{alias}/...`
- DB: none
- Entities: circuit-breaker per-upstream state

### Error-Source Attribution

- [ ] `p5` - **ID**: `cpt-cf-oagw-dod-data-plane-proxy-error-attribution`

The system **MUST** emit gateway errors as RFC 9457 problem+json with `X-OAGW-Error-Source: gateway` (protocol error, downstream error, stream aborted, link unavailable, circuit breaker open, connection/request/idle timeouts mapped to `gts.cf.core.errors.err.v1~cf.oagw.{protocol.error|downstream.error|stream.aborted|link.unavailable|circuit_breaker.open|timeout.connection|timeout.request|timeout.idle}.v1`) and pass upstream error bodies through unchanged with `X-OAGW-Error-Source: upstream`.

**Implements**:
- `cpt-cf-oagw-algo-data-plane-proxy-error-attribution`
- `cpt-cf-oagw-flow-data-plane-proxy-execute`

**Constraints**: `cpt-cf-oagw-constraint-no-direct-internet`, `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `* /api/oagw/v1/proxy/{alias}/...`
- DB: none
- Entities: `GatewayError`, `ProxyResponse`

## 55. Acceptance Criteria

- [ ] A proxy request to an alias defined at a descendant tenant shadows the ancestor alias of the same name and resolves to the descendant upstream
- [ ] A proxy request to an unresolvable alias returns 404 with instance `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1` and `X-OAGW-Error-Source: gateway`
- [ ] A proxy request to a disabled upstream returns 503 with `X-OAGW-Error-Source: gateway`; a request matching only disabled routes returns the route-not-found outcome; an ancestor-disabled upstream stays disabled for descendants
- [ ] Route matching honors the method allowlist (an empty allowlist rejects all methods) and the longest path prefix, with higher priority winning ties
- [ ] For a multi-endpoint upstream with a common-suffix alias: absence of `X-OAGW-Target-Host` returns 400 with instance `gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1` listing valid hosts; a malformed value returns 400 `...cf.oagw.routing.invalid_target_host.v1`; an unmatched value returns 400 `...cf.oagw.routing.unknown_target_host.v1`
- [ ] For a multi-endpoint upstream with an explicit alias and no header, requests are load-balanced round-robin; with a matching header, the specific endpoint is used (bypassing load balancing)
- [ ] Hop-by-hop headers (`Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`) are stripped from outbound requests, and upstream `headers` set/add/remove transforms are applied
- [ ] A header value containing CR or LF is rejected
- [ ] An outbound request whose body exceeds 100MB is rejected with 413 and instance `gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1` before buffering
- [ ] A request with inconsistent `Content-Length` and `Transfer-Encoding` is rejected
- [ ] A plaintext upstream request is blocked when `allow_http_upstream` is false
- [ ] An SSE response is streamed to the caller with correct open/close lifecycle; an upstream mid-stream close returns 502 `...cf.oagw.stream.aborted.v1`
- [ ] The upstream call times out per `proxy_timeout_secs`, returning 504 with the `timeout.connection`, `timeout.request`, or `timeout.idle` instance as applicable
- [ ] No automatic re-issue of a failed client request occurs (endpoint-level connector retries remain permitted)
- [ ] After 5 upstream failures within 30s, subsequent requests return 503 with instance `gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1` until the probe (half-open) succeeds
- [ ] Upstream 4xx/5xx responses pass through unchanged with `X-OAGW-Error-Source: upstream`; gateway failures return problem+json with `X-OAGW-Error-Source: gateway`
- [ ] An upstream protocol parse failure returns 502 `...cf.oagw.protocol.error.v1`; an unreachable upstream returns 503 `...cf.oagw.link.unavailable.v1`
- [ ] The full proxy flow completes in-process without database or network round-trips on the resolution path for hot (cached) configurations

## 56. Additional Context (optional)

This feature consumes, and never redefines: the error framework and GTS instance catalog (Error Semantics), the plugin trait contracts and execution (Plugin System), rate-limit hooks (Rate Limiting), CORS actual-request enforcement (CORS Handling), and metrics/audit hooks (Observability). WebSocket/WebTransport tunneling named in `cpt-cf-oagw-fr-streaming` is not elaborated in DESIGN; this delivery covers HTTP/SSE streaming passthrough only, with WS/WT session flows explicitly omitted. gRPC proxying is Phase 3. Automated tests for this gear live in the crate's own test modules (unit and integration tests under the `oagw` crate), not under `testing/e2e/gears/oagw/`, which is reserved for the acceptance suite.


# Feature: OAGW Observability and Audit

- [ ] `p5` - **ID**: `cpt-cf-oagw-featstatus-observability-audit`

## 57. Feature Context

- [ ] `p5` - `cpt-cf-oagw-feature-observability-audit`

### 57.1 Overview

Define and correlate the observability plane: request/error/circuit-breaker/rate-limit/routing/upstream metric families, the audit log, request-ID correlation across phases, and the cardinality controls that keep the metric surface bounded, served over an admin-only `GET /metrics` endpoint.

### 57.2 Purpose

This feature implements the observability requirement (`cpt-cf-oagw-nfr-observability`): every proxy event emits a consistent record that supports tracing (via request IDs), monitoring (via the bounded metric set), and audit (via the deterministic audit-log fields).

**Requirements**: `cpt-cf-oagw-nfr-observability`

**Principles**: `cpt-cf-oagw-principle-no-retry`

### 57.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Correlates observed proxy behavior against per-request metrics and audit entries by request ID |
| `cpt-cf-oagw-actor-platform-operator` | Scrapes `GET /metrics` (admin surface), reviews the audit log, and sizes/defends metric cardinality |

### 57.4 References

- **PRD**: [PRD.md](./PRD.md)
- **Design**: [DESIGN.md](./DESIGN.md)
- **ADRs**: `cpt-cf-oagw-adr-rate-limiting` (usage-ratio metrics), `cpt-cf-oagw-adr-state-management` (per-instance counters), `cpt-cf-oagw-adr-error-source-distinction` (error-source attribution in observations), `cpt-cf-oagw-adr-data-plane-caching` (cache observation hooks)
- **Parent feature**: `cpt-cf-oagw-feature-observability-audit`
- **Dependencies**: `cpt-cf-oagw-feature-data-plane-proxy`, `cpt-cf-oagw-feature-rate-limiting`, `cpt-cf-oagw-feature-error-semantics`
- **Related design elements**: `cpt-cf-oagw-interface-api` (admin surface hosting `/metrics`), `cpt-cf-oagw-seq-proxy-flow` (instrumented hot path), `cpt-cf-oagw-nfr-observability`, `cpt-cf-oagw-principle-no-retry`

## 58. Actor Flows (CDSL)

### Collect Metrics Over the Admin Surface

- [ ] `p5` - **ID**: `cpt-cf-oagw-flow-observability-audit-scrape`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The operator scrapes metric families and observes bounded, labeled series per the vocabulary.

**Error Scenarios**:
- Unauthorized or non-admin callers are rejected before metric output.

**Steps**:
1. [ ] - `p5` - Operator calls `GET /metrics` on the admin surface - `inst-ob-scrape-call`
2. [ ] - `p5` - **IF** the caller is not an authenticated administrator **THEN** reject without exposing metric output - `inst-ob-scrape-authz`
3. [ ] - `p5` - **IF** authorized **THEN** the runtime exposes the registered Prometheus-registry metric families - `inst-ob-scrape-cols`
4. [ ] - `p5` - **RETURN** the counter, gauge, histogram, and rate-limit ratio series - `inst-ob-scrape-return`

### Correlate an Event Across Phases

- [ ] `p5` - **ID**: `cpt-cf-oagw-flow-observability-audit-correlate`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A single request is correlated across metrics, audit entries, and upstream events via its request ID.

**Error Scenarios**:
- No record exists for an unknown request ID.

**Steps**:
1. [ ] - `p5` - A request receives a generated request ID (or a client-supplied value when structured logging is enabled) - `inst-ob-cor-relid`
2. [ ] - `p5` - The request ID is recorded on every metric, audit entry, and error envelope for the request - `inst-ob-cor-record`
3. [ ] - `p5` - **IF** an upstream's structured logging is enabled **THEN** the request ID is passed to the upstream and returned in responses - `inst-ob-cor-propagate`
4. [ ] - `p5` - **IF** the ID is not found in the retention window **THEN** return no records - `inst-ob-cor-unknown`
5. [ ] - `p5` - **RETURN** the correlated observation set - `inst-ob-cor-return`

## 59. Processes / Business Logic (CDSL)

### Record Per-Request Metrics

- [ ] `p5` - **ID**: `cpt-cf-oagw-algo-observability-audit-record-request`

**Input**: Request completion context (endpoint, method, status code, duration)

**Output**: Updated Prometheus metrics only (never on the proxy hot path's request/response path)

**Steps**:
1. [ ] - `p5` - Increment `oagw_requests_total` labeled by endpoint, method, and status - `inst-ob-rr-requests`
2. [ ] - `p5` - Observe `oagw_request_duration_seconds` as the elapsed time of the full proxy request - `inst-ob-rr-duration`
3. [ ] - `p5` - Track `oagw_requests_in_flight` (gauge, incremented/decremented) - `inst-ob-rr-inflight`
4. [ ] - `p5` - **IF** the request errored **THEN** increment `oagw_errors_total` labeled by endpoint, error type, and error source - `inst-ob-rr-errors`
5. [ ] - `p5` - **RETURN** the recorded series - `inst-ob-rr-return`

### Record Rate-Limit Observations

- [ ] `p5` - **ID**: `cpt-cf-oagw-algo-observability-audit-record-rate-limit`

**Input**: Rate-limit outcomes from the Rate Limiting hooks

**Output**: Rate-limit metric series

**Steps**:
1. [ ] - `p5` - **IF** a request is rejected by rate limiting **THEN** increment `oagw_rate_limit_exceeded_total` - `inst-ob-rl-exceeded`
2. [ ] - `p5` - Observe `oagw_rate_limit_usage_ratio` (clamped to the canonical gauge bound [0.0, 1.0]; 1.0 = bucket exhausted) as the sustaining-ratio reading - `inst-ob-rl-ratio`
3. [ ] - `p5` - **RETURN** the rate-limit series - `inst-ob-rl-return`

### Record Routing and Upstream Observations

- [ ] `p5` - **ID**: `cpt-cf-oagw-algo-observability-audit-record-routing`

**Input**: Resolution and selection outcomes from the Data-Plane Proxy feature

**Output**: Routing and upstream metric series

**Steps**:
1. [ ] - `p5` - **IF** the target host is used **THEN** increment `oagw_routing_target_host_used`; otherwise increment the endpoint/load-balancing path counter - `inst-ob-ru-target`
2. [ ] - `p5` - Record the endpoint selection in the observed routing path - `inst-ob-ru-endpoint`
3. [ ] - `p5` - Track `oagw_upstream_available` (gauge, 1/0 from the circuit-breaker state) and `oagw_upstream_connections` (gauge) - `inst-ob-ru-up`
4. [ ] - `p5` - Track circuit-breaker state (`oagw_circuit_breaker_state` gauge) and increment `oagw_circuit_breaker_transitions_total` on each transition - `inst-ob-ru-cb`
5. [ ] - `p5` - **RETURN** the routing/upstream series - `inst-ob-ru-return`

### Emit the Audit Log Entry

- [ ] `p5` - **ID**: `cpt-cf-oagw-algo-observability-audit-audit-log`

**Input**: Completed request context with all identity and outcome fields

**Output**: One audit-log entry per request (structured JSON)

**Steps**:
1. [ ] - `p5` - Record UTC timestamp, level, and event name - `inst-ob-al-time`
2. [ ] - `p5` - Record the request ID, tenant ID, and principal ID (when known) - `inst-ob-al-identity`
3. [ ] - `p5` - Record the host, path, method, status code, and duration - `inst-ob-al-req`
4. [ ] - `p5` - Record the request and response sizes and the error type (empty when successful) - `inst-ob-al-size`
5. [ ] - `p5` - **RETURN** the complete audit-log entry - `inst-ob-al-return`

### Bound Label Cardinality

- [ ] `p5` - **ID**: `cpt-cf-oagw-algo-observability-audit-cardinality`

**Input**: The set of labels for every metric family

**Output**: A bounded cardinality surface, or rejection of the observation

**Steps**:
1. [ ] - `p5` - **IF** a metric would acquire a tenant-, principal-, or request-specific label **THEN** reject the observation — no per-tenant label dimensions - `inst-ob-card-reject`
2. [ ] - `p5` - **IF** an observation falls outside the configured cardinality limits **THEN** drop it before exposition - `inst-ob-card-drop`
3. [ ] - `p5` - **RETURN** the bounded series (log a dropped-observation counter) - `inst-ob-card-return`

## 60. States (CDSL)

### Circuit Breaker Metric State

- [ ] `p5` - **ID**: `cpt-cf-oagw-state-observability-audit-cb-metric`

**States**: METRIC_CLOSED, METRIC_OPEN, METRIC_HALF_OPEN

**Initial State**: METRIC_CLOSED

**Transitions**:
1. [ ] - `p5` - **FROM** METRIC_CLOSED **TO** METRIC_OPEN **WHEN** the circuit breaker opens; the state gauge reports 0 -> 1 - `inst-ob-cbm-open`
2. [ ] - `p5` - **FROM** METRIC_OPEN **TO** METRIC_HALF_OPEN **WHEN** a probe is admitted; the state gauge reports 1 -> 0.5 - `inst-ob-cbm-half`
3. [ ] - `p5` - **FROM** METRIC_HALF_OPEN **TO** METRIC_CLOSED **WHEN** the probe succeeds; the state gauge reports 0.5 -> 0 - `inst-ob-cbm-close`

## 61. Definitions of Done

### Metrics Vocabulary and Exposition

- [ ] `p5` - **ID**: `cpt-cf-oagw-dod-observability-audit-metrics`

The system **MUST** expose the metric families `oagw_requests_total`, `oagw_request_duration_seconds`, `oagw_requests_in_flight`, `oagw_errors_total`, `oagw_circuit_breaker_state`, `oagw_circuit_breaker_transitions_total`, `oagw_rate_limit_exceeded_total`, `oagw_rate_limit_usage_ratio`, `oagw_routing_target_host_used`, `oagw_routing_endpoint_selected`, `oagw_upstream_available`, and `oagw_upstream_connections` on the admin surface (`GET /metrics`), restricted to authenticated administrators, without per-tenant label dimensions.

**Implements**:
- `cpt-cf-oagw-algo-observability-audit-record-request`
- `cpt-cf-oagw-algo-observability-audit-record-rate-limit`
- `cpt-cf-oagw-algo-observability-audit-record-routing`
- `cpt-cf-oagw-algo-observability-audit-cardinality`

**Constraints**: `cpt-cf-oagw-constraint-no-direct-internet`, `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `GET /api/oagw/v1/metrics` (admin surface; served as `/metrics`)
- Entities: `MetricsRegistry`, metric families

### Audit Log

- [ ] `p5` - **ID**: `cpt-cf-oagw-dod-observability-audit-audit-log`

The system **MUST** emit exactly one structured audit-log entry per request with the fields timestamp (UTC), level, event, request ID, tenant ID, principal ID, host, path, method, status code, duration (ms), request size, response size, and error type (empty on success).

**Implements**:
- `cpt-cf-oagw-algo-observability-audit-audit-log`
- `cpt-cf-oagw-flow-observability-audit-correlate`

**Constraints**: `cpt-cf-oagw-constraint-no-direct-internet`, `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: none (out-of-band log emission)
- Entities: `AuditLogEntry`

### Request-ID Correlation

- [ ] `p5` - **ID**: `cpt-cf-oagw-dod-observability-audit-correlation`

The system **MUST** generate a request ID per request, record it on every metric, audit entry, and error envelope for that request, propagate it to upstreams (and return it in responses) when the upstream enables structured logging, and support lookup of the correlated observation set within the retention window.

**Implements**:
- `cpt-cf-oagw-flow-observability-audit-correlate`
- `cpt-cf-oagw-algo-observability-audit-audit-log`

**Constraints**: `cpt-cf-oagw-constraint-no-direct-internet`, `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: none
- Entities: `RequestContext` (request ID)

### Error-Source Attribution in Observations

- [ ] `p5` - **ID**: `cpt-cf-oagw-dod-observability-audit-error-source`

The system **MUST** label `oagw_errors_total` by error type and error source (`gateway` vs `upstream` per the Error Source Distinction ADR), so thresholding and alerting can distinguish gateway-originated failures from upstream ones.

**Implements**:
- `cpt-cf-oagw-algo-observability-audit-record-request`
- `cpt-cf-oagw-flow-observability-audit-correlate`

**Constraints**: `cpt-cf-oagw-constraint-no-direct-internet`, `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `GET /api/oagw/v1/metrics` (admin surface)
- Entities: `MetricsRegistry` (`oagw_errors_total`)

## 62. Acceptance Criteria

- [ ] `GET /metrics` returns all metric families above with bounded, cardinality-controlled labeled series and is rejected for non-admin callers
- [ ] Every completed proxy request increments `oagw_requests_total` labeled by endpoint, method, and status, and records duration in `oagw_request_duration_seconds`
- [ ] `oagw_errors_total` is labeled by error type and error source (`gateway` vs `upstream`) per the Error Source Distinction ADR
- [ ] Circuit-breaker state and transitions are reflected in `oagw_circuit_breaker_state` (0/0.5/1) and `oagw_circuit_breaker_transitions_total`
- [ ] A rate-limited request increments `oagw_rate_limit_exceeded_total` and the usage ratio is observable in `oagw_rate_limit_usage_ratio` clamped to the canonical gauge bound [0.0, 1.0] (1.0 = bucket exhausted)
- [ ] Target-host selection increments `oagw_routing_target_host_used` and endpoint selection is recorded in `oagw_routing_endpoint_selected`
- [ ] Upstream availability and connections are tracked in `oagw_upstream_available` and `oagw_upstream_connections`
- [ ] No metric series carries a tenant-, principal-, or request-specific label dimension
- [ ] One audit-log entry per request contains the timestamp, level, event, request ID, tenant ID, principal ID, host, path, method, status, duration ms, request size, response size, and error type (empty on success)
- [ ] A single request's metrics, audit entry, and error envelope share the same request ID; with structured logging enabled the ID is returned to the caller and propagated to the upstream
- [ ] Observability recording never blocks or slows the proxy hot path (no in-band instrumentation on the request/response path)

## 63. Additional Context (optional)

The error field of the audit entry reuses the GTS instance identifiers chosen in Error Semantics; the request-ID header name and the audit-sink destination (filesystem sink in MVP) follow the `cpt-cf-oagw-nfr-observability` logging requirement without freezing transport details beyond the deterministic field set. The circuit-breaker state gauge mirrors the Data-Plane Proxy circuit-breaker state machine via a dedicated observation state. Automated tests for this gear live in the crate's own test modules (unit and integration tests under the `oagw` crate), not under `testing/e2e/gears/oagw/`, which is reserved for the acceptance suite.

