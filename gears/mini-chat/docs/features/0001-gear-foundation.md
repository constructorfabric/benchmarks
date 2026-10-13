---
description: "Gear foundation of the mini-chat gear: SDK crate, gear registration and lifecycle, validated configuration, P1 schema and Secure ORM scoping, authorization PEP, canonical error contract, bundled policy and audit plugins, provider registry with OAGW provisioning, outbox pipeline, metrics and the read-only Models API."
---

# Feature: Gear Foundation

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-featstatus-gear-foundation-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-mini-chat-feature-gear-foundation`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Gear Initialization](#gear-initialization)
  - [Gear Start](#gear-start)
  - [Gear Stop](#gear-stop)
  - [Authenticated and Licensed Request Gate](#authenticated-and-licensed-request-gate)
  - [List Models](#list-models)
  - [Get Model](#get-model)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Policy Snapshot Decoding](#policy-snapshot-decoding)
  - [Usage and Audit Event Encoding](#usage-and-audit-event-encoding)
  - [Configuration Loading and Validation](#configuration-loading-and-validation)
  - [Provider Entry Validation](#provider-entry-validation)
  - [Database Migrations](#database-migrations)
  - [Authorization Evaluation](#authorization-evaluation)
  - [Chat-Scoped Data Access](#chat-scoped-data-access)
  - [Domain Error to Problem Mapping](#domain-error-to-problem-mapping)
  - [Static Model Policy Plugin](#static-model-policy-plugin)
  - [Static Audit Plugin](#static-audit-plugin)
  - [Plugin Resolution](#plugin-resolution)
  - [OAGW Provisioning](#oagw-provisioning)
  - [Deferred Provider Reconciliation](#deferred-provider-reconciliation)
  - [Provider Resolution](#provider-resolution)
  - [Outbox Pipeline Start](#outbox-pipeline-start)
  - [Usage Outbox Handler](#usage-outbox-handler)
  - [Audit Outbox Handler](#audit-outbox-handler)
  - [Model Visibility and Projection](#model-visibility-and-projection)
  - [Metrics Registration](#metrics-registration)
- [4. States (CDSL)](#4-states-cdsl)
  - [Gear Lifecycle State Machine](#gear-lifecycle-state-machine)
  - [Provider Provisioning State Machine](#provider-provisioning-state-machine)
  - [Plugin Resolution State Machine](#plugin-resolution-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [SDK Crate](#sdk-crate)
  - [Gear Crate and Lifecycle](#gear-crate-and-lifecycle)
  - [Configuration](#configuration)
  - [Database Schema and Migrations](#database-schema-and-migrations)
  - [Domain Entities and Secure ORM Scoping](#domain-entities-and-secure-orm-scoping)
  - [Authorization PEP](#authorization-pep)
  - [Canonical Error Contract](#canonical-error-contract)
  - [License Gate](#license-gate)
  - [Bundled Plugins and Gateways](#bundled-plugins-and-gateways)
  - [Provider Registry and OAGW Provisioning](#provider-registry-and-oagw-provisioning)
  - [Outbox Pipeline and Handlers](#outbox-pipeline-and-handlers)
  - [Models API](#models-api)
  - [Metrics Infrastructure](#metrics-infrastructure)
  - [MCP Not Implemented](#mcp-not-implemented)
  - [Cross-Cutting Quality and Verification](#cross-cutting-quality-and-verification)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

## 1. Feature Context

### 1.1 Overview

This feature builds the skeleton of the `mini-chat` gear that every later feature extends. It delivers the public SDK crate, the gear crate with its `init` / `start` / `stop` lifecycle and REST registration, the complete validated configuration, the full P1 database schema on PostgreSQL and SQLite with Secure ORM scoping, the authorization PEP, the canonical error contract, the bundled static model-policy and audit plugins, the provider registry with OAGW upstream and route provisioning, the outbox pipeline with the usage and audit handlers, metrics registration, the license gate, and the read-only Models API.

**Implementation slices** (implement in this order; each slice is independently testable):

| # | Slice | CDSL elements |
|---|-------|---------------|
| 1 | SDK crate and configuration | `cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot`, `cpt-cf-mini-chat-algo-gear-foundation-encode-events`, `cpt-cf-mini-chat-algo-gear-foundation-load-config`, `cpt-cf-mini-chat-algo-gear-foundation-validate-providers` |
| 2 | Schema, migrations and entities | `cpt-cf-mini-chat-algo-gear-foundation-run-migrations`, `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access` |
| 3 | Authorization PEP and error contract | `cpt-cf-mini-chat-algo-gear-foundation-authorize`, `cpt-cf-mini-chat-algo-gear-foundation-map-error`, `cpt-cf-mini-chat-flow-gear-foundation-request-gate` |
| 4 | Bundled plugins and plugin resolution | `cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin`, `cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin`, `cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin`, `cpt-cf-mini-chat-state-gear-foundation-plugin-resolution` |
| 5 | Provider registry and OAGW provisioning | `cpt-cf-mini-chat-algo-gear-foundation-provision-oagw`, `cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers`, `cpt-cf-mini-chat-algo-gear-foundation-resolve-provider`, `cpt-cf-mini-chat-state-gear-foundation-provider-provisioning` |
| 6 | Outbox pipeline and handlers, metrics | `cpt-cf-mini-chat-algo-gear-foundation-start-outbox`, `cpt-cf-mini-chat-algo-gear-foundation-handle-usage`, `cpt-cf-mini-chat-algo-gear-foundation-handle-audit`, `cpt-cf-mini-chat-algo-gear-foundation-register-metrics` |
| 7 | Lifecycle wiring, Models API and REST registration | `cpt-cf-mini-chat-flow-gear-foundation-gear-init`, `cpt-cf-mini-chat-flow-gear-foundation-gear-start`, `cpt-cf-mini-chat-flow-gear-foundation-gear-stop`, `cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle`, `cpt-cf-mini-chat-algo-gear-foundation-visible-models`, `cpt-cf-mini-chat-flow-gear-foundation-list-models`, `cpt-cf-mini-chat-flow-gear-foundation-get-model` |

**Out of scope** (owned by later features): chat, message, reaction, attachment, turn and quota endpoints; quota arithmetic, reserve and settlement (`cpt-cf-mini-chat-feature-quota-billing`); provider adapter request and stream translation (`cpt-cf-mini-chat-feature-streaming-turns`); the bodies of the attachment-cleanup, chat-cleanup and thread-summary handlers and the orphan watchdog and upload reaper (`cpt-cf-mini-chat-feature-attachments`, `cpt-cf-mini-chat-feature-background-workers`); policy-snapshot caching and `POST /internal/policy:notify` (Future, `cpt-cf-mini-chat-adr-quota-policy-scope`); any MCP implementation (Future, `cpt-cf-mini-chat-adr-mcp-deferred`).

### 1.2 Purpose

Every later feature needs the same base: a public SDK contract for the plugins, a gear that starts with a validated configuration, a schema that already holds every P1 table, data access that cannot leak across tenants or owners, one error shape for all REST responses, a way to reach LLM providers only through OAGW, and a durable outbox for usage and audit delivery. This feature provides that base and the first public endpoints (the Models API) that exercise it end to end.

**Requirements**: `cpt-cf-mini-chat-fr-license-gate`, `cpt-cf-mini-chat-fr-model-selection`, `cpt-cf-mini-chat-fr-audit`, `cpt-cf-mini-chat-fr-mcp-server-registry`, `cpt-cf-mini-chat-fr-mcp-hub-discovery`, `cpt-cf-mini-chat-fr-mcp-role-access`, `cpt-cf-mini-chat-nfr-tenant-isolation`, `cpt-cf-mini-chat-nfr-authz-alignment`, `cpt-cf-mini-chat-nfr-observability-supportability`

The three MCP requirements are traced only to confirm their Future status: this feature creates no MCP module, endpoint, table, migration or configuration key.

**Principles**: `cpt-cf-mini-chat-principle-tenant-scoped`, `cpt-cf-mini-chat-principle-owner-only-content`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-mini-chat-actor-chat-user` | Calls the Models API; every request passes the authentication, license and authorization gates defined here |
| `cpt-cf-mini-chat-actor-admin` | No role in P1: the MCP admin API is Future and not exposed |
| ToolKit runtime (gear host) | Loads the configuration section, runs migrations, calls `init`, REST registration, `start` and `stop`; hosts the shared outbox workers that invoke the handlers |
| Operator (deployment configuration) | Supplies the gear configuration and the bundled plugin sections; reads startup errors and warnings |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md) — §1.3, §2, §3.1, §3.2 (components, "OAGW provisioning", "Gear lifecycle"), §3.3 (REST error mapping, Models API), §3.4, §3.5, §3.7, §3.8, §3.9 "Audit Events for Turn Mutations", §4 "Model Catalog Configuration", "Metrics (OTLP) and Alerts (P1)", §5.2, §5.6, Appendix A, Appendix B
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) entry 2.1
- **Acceptance checklist**: [acceptance-criteria.md](../acceptance-criteria.md) — owned items: Principles & Constraints "Tenant and owner isolation enforced on every resource" (infrastructure part); Models API; Authorization (PEP part); Error Mapping & Sanitization "All errors map to the canonical error contract" (REST mapping part)
- **ADRs**: `cpt-cf-mini-chat-adr-llm-provider-as-library`, `cpt-cf-mini-chat-adr-internal-transport`, `cpt-cf-mini-chat-adr-canonical-error-contract`, `cpt-cf-mini-chat-adr-multi-provider-adapters`, `cpt-cf-mini-chat-adr-mcp-deferred`, `cpt-cf-mini-chat-adr-quota-policy-scope`, `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope`, `cpt-cf-mini-chat-adr-runtime-consistency-limitations`
- **Dependencies**: None

**Source precedence**: DESIGN is authoritative for details; the gear docs win over `config/mini-chat.yaml` and platform docs (DECOMPOSITION task-level overrides).

**Feature-local deviations and interpretations** (review owner: mini-chat gear maintainers):

| Topic | Deviation or interpretation | Rationale |
|-------|-----------------------------|-----------|
| PDP evaluation failure status | 503 `service_unavailable` with `Retry-After: 5`, not 403 as one sentence of `cpt-cf-mini-chat-nfr-tenant-isolation` states | DESIGN §3.8 "Fail-Closed Behavior", `cpt-cf-mini-chat-nfr-authz-alignment` and `cpt-cf-mini-chat-adr-canonical-error-contract` all specify 503; DESIGN is authoritative |
| OAGW provisioning error classification | "Credstore secret not readable" and an OAGW call that exceeds its timeout defer the target to the background reconcile task; every other upstream or chat-route error, and a failed or timed-out S2S credential exchange, fails `start`. An "already exists" / overlap Conflict on route creation counts as success. RAG route errors (including timeouts) only log a warning | DESIGN names only the two classes; treating unknown errors as misconfiguration keeps startup fail-fast and deterministic, while a bounded timeout is a transient condition that must not hang or fail `start` |
| Static policy plugin, unknown version | `get_policy_snapshot` and `get_user_limits` for a version other than `1` return a plugin error | The static plugin has exactly one version; returning version-1 data under another version would break the snapshot immutability rule of §5.2.6 |
| Catalog sampling parameters | `api_params.temperature`, `top_p`, `frequency_penalty`, `presence_penalty` may be absent (unset means not sent), in addition to `extra_body` and `reasoning_effort` | DESIGN "Model Catalog Configuration" requires that they can be left unset for reasoning models; Appendix A lists only two optional `api_params` fields |
| Upstream alias derivation (`inst-fill-aliases`) | The effective alias is the alias OAGW itself stores and resolves, not the bare host: for a hostname endpoint it is the normalized host (lower-case, no trailing dot), plus `:port` when the port is not the scheme default (80 for `use_http`, 443 otherwise); a configured `providers.<id>.upstream_alias` (or override alias) that differs is ignored with a warning, and the alias is not sent to OAGW. For an IP-literal host the normalized configured alias is used, defaulting to the same `ip[:port]` form. Aliases are compared and grouped in normalized form (lower-case, trailing dots stripped); tenant override keys must be UUIDs and are stored in canonical form | OAGW auto-derives hostname aliases and rejects any other value (`enforce_alias_create_with`), and requires an explicit alias for IP literals; `/{alias}/...` proxy paths and `find_upstream` must use exactly what OAGW stores, so a bare-host alias would crash `start` on a non-standard port |
| Cleanup and thread-summary handlers | Registered as placeholders that return `Retry` until the owning features replace them | Keeps the five-queue topology of DESIGN "Gear lifecycle" final now without acknowledging (and losing) any message before its real handler exists; no producer enqueues to these queues in this feature |

**External-call timeouts** (gear constants; every call to a dependency that can stall is bounded, and a never-answering dependency yields the bounded outcome below instead of a hang):

| Constant | Value | Call | Outcome on timeout |
|----------|-------|------|--------------------|
| `PDP_EVALUATION_TIMEOUT` | 5 s | PDP evaluation through PolicyEnforcer in `cpt-cf-mini-chat-algo-gear-foundation-authorize` | PDP-unavailable domain error: 503 `service_unavailable`, `Retry-After: 5` |
| `S2S_EXCHANGE_TIMEOUT` | 10 s | `client_credentials` exchange through `authn_resolver` at `start` | `start` fails naming `client_id` and the cause `timeout` |
| `OAGW_CALL_TIMEOUT` | 10 s | Each OAGW upstream create, route list and route create call of provisioning and of the reconcile retry | Transient error: the target is deferred to the background reconcile task (RAG routes: warning only) |
| `PUBLISH_USAGE_TIMEOUT` | 20 s (shorter than the 30 s usage-queue lease) | `publish_usage` in the usage outbox handler | `Retry` |
| `AUDIT_EMIT_TIMEOUT` | 30 s | `emit` in the audit outbox handler | `Retry` (existing behaviour, unchanged) |
| `POLICY_PLUGIN_CALL_TIMEOUT` | 10 s | `get_current_policy_version`, `get_policy_snapshot` and `get_user_limits` on the model-policy plugin (every call through the model-policy gateway) | Models API (`cpt-cf-mini-chat-algo-gear-foundation-visible-models`): 500 `internal`; summary-model check at `start`: error logged and startup continues; handlers' policy resolution and later-feature callers: `Retry` |
| `REGISTRY_QUERY_TIMEOUT` | 10 s | The types-registry instance query in `cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin` (model-policy and audit plugins) | Resolution error (nothing cached): Models API callers return 500 `internal`, outbox handlers return `Retry` |

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (human or external system) and describe the end-to-end flow of a use case. Each flow has a triggering actor and shows how the system responds to actor actions.

**Use cases**: none of the PRD use cases is completed by this feature alone; every chat use case (for example `cpt-cf-mini-chat-usecase-send-message`) runs through the request gate, PEP and error mapping defined here.

### Gear Initialization

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-gear-foundation-gear-init`

**Actor**: ToolKit runtime (gear host)

**Success Scenarios**:
- A valid configuration section produces an initialized gear whose services, gateways, provider resolver, metrics and outbox enqueuer exist and whose routes are registered under `url_prefix`.

**Error Scenarios**:
- An unknown key, a removed key, an out-of-range value or a dangling reference fails `init` with an error that names the offending key.
- Two targets (provider entries or tenant overrides) resolve to the same alias but differ in `host`, `port`, `use_http`, `auth_plugin_type`, `auth_config` or `api_path`: `init` fails naming the alias and both targets.
- A required ClientHub client (`authz_resolver`, `oagw`, `authn_resolver`) is missing and `init` fails.

**Steps**:
1. [x] - `p1` - Runtime applies the gear migrations through the `db` capability, including the shared outbox migrations (see `cpt-cf-mini-chat-algo-gear-foundation-run-migrations`) - `inst-apply-migrations`
2. [x] - `p1` - Runtime calls `init` with the `mini-chat` configuration section - `inst-call-init`
3. [x] - `p1` - Load and validate the configuration (see `cpt-cf-mini-chat-algo-gear-foundation-load-config`) - `inst-load-config`
4. [x] - `p1` - **IF** validation returns an error - `inst-if-config-invalid`
   1. [x] - `p1` - **RETURN** `init` failure carrying the validation message - `inst-return-config-error`
5. [x] - `p1` - Fill every empty `upstream_alias` of a provider entry with its expanded `host`, and of a tenant override with the override `host` - `inst-fill-aliases`
6. [x] - `p1` - Create the model-policy gateway and the audit gateway without resolving any plugin yet (plugins are resolved lazily, see `cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin`) - `inst-create-gateways`
7. [x] - `p1` - Resolve the `authz_resolver`, `oagw` and `authn_resolver` clients from ClientHub - `inst-resolve-clients`
8. [x] - `p1` - **IF** any of the three clients is missing - `inst-if-client-missing`
   1. [x] - `p1` - **RETURN** `init` failure naming the missing client - `inst-return-client-missing`
9. [x] - `p1` - Build the provider registry and provider resolver from the alias-filled entries (see `cpt-cf-mini-chat-algo-gear-foundation-resolve-provider`) - `inst-build-resolver`
10. [x] - `p1` - Register the metric instruments (see `cpt-cf-mini-chat-algo-gear-foundation-register-metrics`) - `inst-register-metrics`
11. [x] - `p1` - Build the outbox enqueuer for the five configured queue names without starting the pipeline - `inst-build-enqueuer`
12. [x] - `p1` - Build the PEP (PolicyEnforcer over the `authz_resolver` client) and the models service - `inst-build-services`
13. [x] - `p1` - Runtime runs the REST registration step: register `GET {url_prefix}/v1/models` and `GET {url_prefix}/v1/models/{id}` with OpenAPI metadata, each requiring authentication and the license feature `gts.cf.core.lic.feat.v1~cf.core.global.base.v1` - `inst-register-routes`
14. [x] - `p1` - **RETURN** initialized gear - `inst-return-initialized`

### Gear Start

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-gear-foundation-gear-start`

**Actor**: ToolKit runtime (gear host)

**Success Scenarios**:
- Every provider entry and tenant override has its OAGW upstream and chat route; storage-capable entries have their RAG routes; the outbox pipeline runs five queues.
- Entries whose credstore secret is not readable yet, or whose OAGW calls time out, are retried in the background and the gear is still started.
- Provisioning against an OAGW that already holds the upstreams and routes (restart, shared alias) succeeds and does not fail `start`.

**Error Scenarios**:
- A deterministically misconfigured provider entry or a failed or timed-out S2S credential exchange fails `start`.
- The configured thread-summary model is missing or disabled: an error is logged and startup continues.

**Steps**:
1. [x] - `p1` - Runtime calls `start` with a cancellation token - `inst-call-start`
2. [x] - `p1` - **IF** `orphan_watchdog.enabled` or `upload_reaper.enabled` is `true` - `inst-if-leader-workers`
   1. [x] - `p1` - Prepare the leader elector: the gear-local Kubernetes Lease elector when built with the `k8s` feature, otherwise the no-op elector - `inst-prepare-elector`
3. [x] - `p1` - Provision OAGW upstreams and routes (see `cpt-cf-mini-chat-algo-gear-foundation-provision-oagw`) - `inst-provision-oagw`
4. [x] - `p1` - **IF** provisioning returns a misconfiguration error - `inst-if-provision-failed`
   1. [x] - `p1` - **RETURN** `start` failure naming the cause (`timeout` for a timed-out S2S exchange) and, for a target error, the provider id (never a secret value) - `inst-return-provision-error`
5. [x] - `p1` - Start the outbox pipeline with the five queues (see `cpt-cf-mini-chat-algo-gear-foundation-start-outbox`) - `inst-start-outbox`
6. [x] - `p1` - Read the current policy snapshot through the model-policy gateway and look up the effective `thread_summary_worker.summary_model_id` (empty means `gpt-4.1-mini`); each plugin call is bounded by `POLICY_PLUGIN_CALL_TIMEOUT` (10 s) - `inst-check-summary-model`
7. [x] - `p1` - **IF** the summary model is missing or not `enabled`, or the snapshot cannot be read (plugin resolution failure, plugin error or `POLICY_PLUGIN_CALL_TIMEOUT` elapsed) - `inst-if-summary-model-bad`
   1. [x] - `p1` - Log an error naming the model id and continue startup - `inst-log-summary-model`
8. [x] - `p1` - Spawn the leader-only background workers registered by later features, after the outbox pipeline (none are registered by this feature) - `inst-spawn-workers`
9. [x] - `p1` - **RETURN** started gear - `inst-return-started`

### Gear Stop

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-gear-foundation-gear-stop`

**Actor**: ToolKit runtime (gear host)

**Success Scenarios**:
- Background tasks end within the bounded join timeout and the outbox pipeline stops.

**Error Scenarios**:
- A task does not end within the timeout: a warning is logged and the stop proceeds.
- The framework stop deadline fires first: the stop gives up without blocking.

**Steps**:
1. [x] - `p1` - Runtime calls `stop` - `inst-call-stop`
2. [x] - `p1` - Cancel every spawned background task (provider reconcile task and later-feature workers) - `inst-cancel-tasks`
3. [x] - `p1` - Join the tasks with a bounded timeout (a gear constant shorter than the framework stop deadline) - `inst-join-tasks`
4. [x] - `p1` - **IF** the join timeout elapses - `inst-if-join-timeout`
   1. [x] - `p1` - Log a warning naming the tasks still running and stop waiting for them - `inst-log-join-timeout`
5. [x] - `p1` - Stop the outbox pipeline, giving up when the framework deadline fires - `inst-stop-outbox`
6. [x] - `p1` - **RETURN** stopped gear - `inst-return-stopped`

### Authenticated and Licensed Request Gate

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-gear-foundation-request-gate`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- An authenticated user of a licensed tenant reaches the handler, is authorized by the PEP and receives the response.

**Error Scenarios**:
- Missing or invalid bearer token: 401 `unauthenticated`.
- Tenant without the base license feature: 403 `permission_denied`, `LICENSE_FEATURE_REQUIRED`.
- Extractor, authorization or domain failures: canonical `Problem` per `cpt-cf-mini-chat-algo-gear-foundation-map-error`.

**Steps**:
1. [x] - `p1` - User sends a request to a route under `{url_prefix}` - `inst-send-request`
2. [x] - `p1` - The api-gateway authentication middleware validates the bearer token - `inst-authn`
3. [x] - `p1` - **IF** the token is missing, invalid or expired - `inst-if-unauthenticated`
   1. [x] - `p1` - **RETURN** 401 `unauthenticated` with `context.reason` `MISSING_BEARER` or `AUTHN_FAILED` - `inst-return-401`
4. [x] - `p1` - The license middleware checks the route's required feature `gts.cf.core.lic.feat.v1~cf.core.global.base.v1` on the tenant license - `inst-license-check`
5. [x] - `p1` - **IF** the tenant lacks the feature - `inst-if-unlicensed`
   1. [x] - `p1` - **RETURN** 403 `permission_denied` with `LICENSE_FEATURE_REQUIRED` - `inst-return-license-403`
6. [x] - `p1` - Platform extractors parse path, query and body; their failures return the platform `invalid_argument` problems (400 `invalid_path_params`, 400 `json_syntax_error`, 422 `invalid_json_body`, 415 `missing_json_content_type`) - `inst-extract`
7. [x] - `p1` - Handler calls the domain service with the request `SecurityContext` (tenant id, user id, token scopes) - `inst-call-domain`
8. [x] - `p1` - Domain service authorizes the operation (see `cpt-cf-mini-chat-algo-gear-foundation-authorize`) before any database query or catalog read - `inst-authorize`
9. [x] - `p1` - **IF** the domain service returns a domain error - `inst-if-domain-error`
   1. [x] - `p1` - **RETURN** the `Problem` built by `cpt-cf-mini-chat-algo-gear-foundation-map-error` - `inst-return-problem`
10. [x] - `p1` - **RETURN** the success response - `inst-return-success`

### List Models

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-gear-foundation-list-models`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- 200 with `items` holding the projection of every enabled catalog entry (possibly empty).

**Error Scenarios**:
- 401 / 403 from the request gate; 403 `AUTHZ_DENIED` on PDP deny; 503 with `Retry-After: 5` on PDP failure; 500 `internal` when the policy plugin cannot be resolved or fails, or when a registry query or plugin call exceeds its timeout.

**Steps**:
1. [x] - `p1` - User calls `GET {url_prefix}/v1/models` through the request gate (see `cpt-cf-mini-chat-flow-gear-foundation-request-gate`) - `inst-call-list`
2. [x] - `p1` - Authorize resource type `gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~`, action `list`, no `resource.id`, `require_constraints` `false` - `inst-authorize-list`
3. [x] - `p1` - Compute the visible models (see `cpt-cf-mini-chat-algo-gear-foundation-visible-models`) - `inst-compute-visible`
4. [x] - `p1` - **RETURN** 200 with `{ items: [projection, ...] }` in catalog order - `inst-return-list`

### Get Model

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-gear-foundation-get-model`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- 200 with the projection of the enabled catalog entry whose `id` equals the path `id`.

**Error Scenarios**:
- 404 `not_found` with `context.resource_type` `gts.cf.core.mini_chat.model.v1~` when the model is absent or disabled; the same gate, PEP and plugin errors as List Models.

**Steps**:
1. [x] - `p1` - User calls `GET {url_prefix}/v1/models/{id}` through the request gate; `id` is an opaque string (no UUID check) - `inst-call-get`
2. [x] - `p1` - Authorize the model resource type, action `read`, no `resource.id`, `require_constraints` `false` - `inst-authorize-get`
3. [x] - `p1` - Compute the visible models (see `cpt-cf-mini-chat-algo-gear-foundation-visible-models`) - `inst-compute-visible-get`
4. [x] - `p1` - **IF** no visible model has `model_id` equal to the path `id` - `inst-if-not-visible`
   1. [x] - `p1` - **RETURN** 404 `not_found` with the model resource type, without revealing whether the model exists disabled - `inst-return-404`
5. [x] - `p1` - **RETURN** 200 with the single projection - `inst-return-model`

## 3. Processes / Business Logic (CDSL)

Internal system functions and procedures that do not interact with actors directly. Examples: database layer operations, authorization logic, middleware, validation routines, library functions, background jobs. These are reusable building blocks called by Actor Flows or other processes.

### Policy Snapshot Decoding

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot`

**Input**: Serialized `PolicySnapshot` (as returned by a model-policy plugin), or a single `ModelCatalogEntry`

**Output**: Decoded SDK `PolicySnapshot`, or a decode error naming the field

**Steps**:
1. [x] - `p1` - Decode `policy_version`, `model_catalog` and `kill_switches` - `inst-decode-top`
2. [x] - `p1` - **FOR EACH** entry in `model_catalog` - `inst-for-each-entry`
   1. [x] - `p1` - **IF** any required field is absent (`id`, `provider_model_id`, `display_name`, `provider_id`, `provider_display_name`, `tier`, `context_window`, `max_output_tokens`, `max_input_tokens`, `input_tokens_credit_multiplier_micro`, `output_tokens_credit_multiplier_micro`, `max_num_results`, `general_config`) - `inst-if-required-missing`
      1. [x] - `p1` - **RETURN** decode error naming the field - `inst-return-required-error`
   2. [x] - `p1` - **IF** `tier` is not `premium` or `standard` - `inst-if-bad-tier`
      1. [x] - `p1` - **RETURN** decode error - `inst-return-tier-error`
   3. [x] - `p1` - Apply defaults for absent optional fields: `description`, `icon`, `multiplier_display`, `system_prompt`, `thread_summary_prompt` empty; `enabled` `false`; `multimodal_capabilities` empty; `web_search_context_size` `low`; `max_tool_calls` `2`; `preference` null - `inst-apply-entry-defaults`
   4. [x] - `p1` - **IF** `estimation_budgets` is absent - `inst-if-budgets-absent`
      1. [x] - `p1` - Use the default budgets (`bytes_per_token_conservative` 4, `fixed_overhead_tokens` 100, `safety_margin_pct` 10, `image_token_budget` 1000, `tool_surcharge_tokens` 500, `web_search_surcharge_tokens` 500, `code_interpreter_surcharge_tokens` 1000, `minimal_generation_floor` 50) - `inst-default-budgets`
   5. [x] - `p1` - **ELSE** - `inst-else-budgets-present`
      1. [x] - `p1` - Fill each absent budget field with its default - `inst-fill-budget-fields`
   6. [x] - `p1` - Decode `general_config` with all fields required except `api_params.extra_body`, `api_params.reasoning_effort` and the four sampling parameters (`temperature`, `top_p`, `frequency_penalty`, `presence_penalty`) - `inst-decode-general-config`
   7. [x] - `p1` - Ignore unknown keys inside the entry and its nested objects - `inst-ignore-unknown-nested`
3. [x] - `p1` - **IF** any of the six kill switches (`disable_premium_tier`, `force_standard_tier`, `disable_web_search`, `disable_file_search`, `disable_images`, `disable_code_interpreter`) is absent - `inst-if-switch-missing`
   1. [x] - `p1` - **RETURN** decode error (a missing or renamed switch is never read as `false`) - `inst-return-switch-error`
4. [x] - `p1` - **RETURN** decoded snapshot - `inst-return-snapshot`

### Usage and Audit Event Encoding

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-encode-events`

**Input**: SDK `UsageEvent`, `TurnAuditEvent` or turn-mutation audit event

**Output**: JSON payload for the outbox, or the decoded value on the consumer side

**Steps**:
1. [x] - `p1` - Encode `UsageEvent` with `tenant_id`, `chat_id`, `request_id`, `effective_model`, `selected_model`, `terminal_state`, `billing_outcome`, `usage`, `actual_credits_micro`, `settlement_method`, `policy_version_applied`, `web_search_calls`, `code_interpreter_calls`, `file_search_calls`, `timestamp`, `requester_type`, `dedupe_key`; there is no `event_type` field - `inst-encode-usage-core`
2. [x] - `p1` - **IF** `requester_type` is `system` - `inst-if-system-task`
   1. [x] - `p1` - Omit `user_id` and `turn_id` (absent, not `null`) and include `system_task_type` - `inst-omit-user-turn`
3. [x] - `p1` - **ELSE** - `inst-else-user-turn`
   1. [x] - `p1` - Include `user_id` and `turn_id` and omit `system_task_type` - `inst-include-user-turn`
4. [x] - `p1` - **IF** the provider reported no usage - `inst-if-no-usage`
   1. [x] - `p1` - Encode `usage` as `null`; otherwise encode `input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_write_input_tokens`, `reasoning_tokens` - `inst-usage-null`
5. [x] - `p1` - Never encode `completion_signal`, `reserved_credits_micro`, `committed_credits_micro`, `overshoot_capped` or `error_code` - `inst-no-unimplemented-fields`
6. [x] - `p1` - Encode `TurnAuditEvent` with `event_type` `turn_completed` or `turn_failed`, tenant, requester user, `chat_id`, turn and `request_id` identities, `selected_model`, `effective_model`, token usage, latency, tool-call counts (web search, file search; no code-interpreter count), `policy_decisions.quota.decision` (including `unknown`), `timestamp`, and empty `prompt`, `response`, `attachments`, `license`, `quota_scope` - `inst-encode-turn-audit`
7. [x] - `p1` - Encode mutation audit events: `turn_retry` and `turn_edit` with `actor_user_id`, `chat_id`, `original_request_id`, `new_request_id`, `timestamp`; `turn_delete` with `actor_user_id`, `chat_id`, `request_id`, `timestamp`; each also carries the tenant identity - `inst-encode-mutation-audit`
8. [x] - `p1` - **RETURN** payload; decoding is the exact inverse and fails on a malformed payload - `inst-return-payload`

### Configuration Loading and Validation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-load-config`

**Input**: Raw `mini-chat` configuration section (absent keys allowed)

**Output**: Validated gear configuration with defaults applied, plus startup warnings; or a validation error naming the key

**Steps**:
1. [x] - `p1` - Parse the section; reject unknown keys at the top level and in `streaming`, `estimation_budgets`, `quota`, `outbox`, `context`, `rag`, `client_credentials`, `metrics`, `providers.<id>`, `providers.<id>.tenant_overrides.<tenant_id>`, `thumbnail`, `knowledge_search` - `inst-reject-unknown`
2. [x] - `p1` - Accept and ignore unknown keys in `orphan_watchdog`, `upload_reaper`, `thread_summary_worker`, `cleanup_worker` - `inst-accept-worker-unknown`
3. [x] - `p1` - Treat the removed keys `providers.<id>.supports_file_search_filters` and `streaming.web_search_context_size`, and any top-level `mcp` key, as unknown keys that fail startup - `inst-removed-keys`
4. [x] - `p1` - Apply top-level defaults: `url_prefix` `/mini-chat`; `vendor` `constructorfabric` (must be non-empty); `metrics.prefix` empty, meaning `mini_chat` - `inst-top-defaults`
5. [x] - `p1` - Expand `${VAR}` in `client_credentials.client_id` and `client_credentials.client_secret`; **IF** either is empty after expansion, **RETURN** error; `client_secret` is a secret and is never written to logs, error `detail`s or validation messages (messages name only the key path) - `inst-client-credentials`
6. [x] - `p1` - Validate `streaming`: `sse_ping_interval_seconds` default 15 in `5..=60`; `sse_channel_capacity` default 32 in `16..=64`; `max_output_tokens` default 32768 - `inst-validate-streaming`
7. [x] - `p1` - Validate `estimation_budgets.minimal_generation_floor` default 50, greater than 0 and not above `streaming.max_output_tokens` - `inst-validate-floor`
8. [x] - `p1` - **FOR EACH** deprecated budget field (`bytes_per_token_conservative` 4, `fixed_overhead_tokens` 100, `safety_margin_pct` 10, `image_token_budget` 1000, `tool_surcharge_tokens` 500, `web_search_surcharge_tokens` 500, `code_interpreter_surcharge_tokens` 1000) - `inst-for-each-deprecated-budget`
   1. [x] - `p1` - Parse without validation; **IF** set to a non-default value, log one warning naming the field - `inst-warn-deprecated-budget`
9. [x] - `p1` - Validate `quota`: `overshoot_tolerance_factor` default 1.10 in `1.0..=1.5`; `warning_threshold_pct` default 80 in `1..=99`; `web_search_max_calls_per_message` default 2, `web_search_daily_quota` default 75, `code_interpreter_max_calls_per_message` default 10, `code_interpreter_daily_quota` default 50, each greater than 0 - `inst-validate-quota`
10. [x] - `p1` - Validate `context`: `recent_messages_limit` default 10 in `0..=100`; `web_search_guard` and `file_search_guard` default to the built-in guard texts - `inst-validate-context`
11. [x] - `p1` - Validate `rag`, each greater than 0: `max_documents_per_chat` 50, `max_total_upload_mb_per_chat` 100, `uploaded_file_max_size_kb` 25600, `uploaded_image_max_size_kb` 5120, `max_images_per_message` 4; `max_concurrent_uploads` default 10 in `1..=256`; `allow_csv_upload` default `true` - `inst-validate-rag`
12. [x] - `p1` - Validate `thumbnail`, each greater than 0: `width` 128, `height` 128, `max_bytes` 131072, `max_pixels` 100000000, `max_decode_bytes` 33554432 - `inst-validate-thumbnail`
13. [x] - `p1` - Validate `knowledge_search`: `enabled` default `false`; when enabled, `vector_store_id` and `provider_id` are required and non-empty; `max_calls_per_message` 3, `top_k` 5, `max_chunk_chars` 2000, each greater than 0; `guard` defaults to the built-in text - `inst-validate-knowledge`
14. [x] - `p1` - Validate `orphan_watchdog`: `enabled` default `true`; `timeout_secs` default 300 in `90..=3600`; `scan_interval_secs` default 60 in `1..=3600` - `inst-validate-watchdog`
15. [x] - `p1` - Validate `upload_reaper`: `enabled` default `true`; `scan_interval_secs` default 60 in `1..=3600`; `stale_after_secs` default 300 in `60..=86400` - `inst-validate-reaper`
16. [x] - `p1` - Validate `thread_summary_worker`: `enabled` default `true`; `claim_timeout_secs` default 300 in `30..=3600`; `max_attempts` default 3 greater than 0; `compression_threshold_pct` default 80 in `1..=99`; `summary_model_id` default empty (meaning `gpt-4.1-mini`); `summary_system_prompt` default built-in text; `message_content_limit` default 4000 (0 means no truncation) - `inst-validate-summary-worker`
17. [x] - `p1` - Validate `cleanup_worker.max_attempts` default 5 greater than 0 - `inst-validate-cleanup`
18. [x] - `p1` - Log one startup warning for each deprecated worker field that has no effect: `cleanup_worker.enabled` set to `false`; `cleanup_worker.poll_interval_secs` (60), `reconcile_interval_secs` (300), `stale_in_progress_timeout_secs` (900), `batch_size` (32) and `thread_summary_worker.reconcile_interval_secs` (60) set to a non-default value - `inst-warn-deprecated-workers`
19. [x] - `p1` - Validate `outbox`: `queue_name` `mini-chat.usage_snapshot`, `cleanup_queue_name` `mini-chat.attachment_cleanup`, `chat_cleanup_queue_name` `mini-chat.chat_cleanup`, `thread_summary_queue_name` `mini-chat.thread_summary`, `audit_queue_name` `mini-chat.audit`, each non-empty; `num_partitions` default 4, a power of two in `1..=64` - `inst-validate-outbox`
20. [x] - `p1` - Validate the provider map (see `cpt-cf-mini-chat-algo-gear-foundation-validate-providers`) - `inst-validate-providers`
21. [x] - `p1` - **IF** any step failed - `inst-if-any-failed`
   1. [x] - `p1` - **RETURN** validation error naming the key path and the rule - `inst-return-validation-error`
22. [x] - `p1` - **RETURN** validated configuration and the collected warnings - `inst-return-config`

### Provider Entry Validation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-validate-providers`

**Input**: `providers` map (absent means the single default entry `openai`)

**Output**: Validated provider entries with `${VAR}` expanded, or a validation error

**Steps**:
1. [x] - `p1` - **IF** `providers` is absent - `inst-if-providers-absent`
   1. [x] - `p1` - Use one entry `openai`: `kind` `openai_responses`, `host` `api.openai.com`, API-key auth from `cred://openai-key`, `storage_kind` `openai` - `inst-default-provider`
2. [x] - `p1` - **FOR EACH** entry `providers.<id>` - `inst-for-each-provider`
   1. [x] - `p1` - Require `kind` in `openai_responses`, `openai_chat_completions`, `vllm_responses`, `anthropic_messages` - `inst-check-kind`
   2. [x] - `p1` - Expand `${VAR}` in `host` and in every `auth_config` value (every expanded `auth_config` value is a secret: it is never written to a log, error `detail` or validation message, and messages name only the key path); require `host` non-empty and containing only letters, digits, `.`, `-`, `_`, `:`, `[`, `]` after expansion - `inst-check-host`
   3. [x] - `p1` - Default `use_http` to `false` and `port` to 443, or 80 when `use_http` is `true`; reject `port` 0 - `inst-check-port`
   4. [x] - `p1` - Default `api_path` to `/v1/responses` (`{model}` is the `provider_model_id` placeholder) - `inst-default-api-path`
   5. [x] - `p1` - Require `storage_kind` in `openai`, `azure`; default `storage_backend` to the provider id - `inst-check-storage-kind`
   6. [x] - `p1` - **IF** `storage_kind` is `azure` - `inst-if-azure`
      1. [x] - `p1` - Require `api_version` non-blank and containing only letters, digits, `.` and `-` - `inst-check-api-version`
   7. [x] - `p1` - **IF** `rag_provider` is set and names no existing entry - `inst-if-rag-dangling`
      1. [x] - `p1` - **RETURN** validation error naming both ids - `inst-return-rag-error`
   8. [x] - `p1` - **FOR EACH** `tenant_overrides.<tenant_id>` - `inst-for-each-override`
      1. [x] - `p1` - Require `host` or `upstream_alias`; expand `${VAR}` in `host` and `auth_config` and apply the same host character check - `inst-check-override`
      2. [x] - `p1` - Resolve each unset override field (`auth_plugin_type`, `auth_config`) from the provider entry independently - `inst-override-fallback`
3. [x] - `p1` - Build one effective target per provider entry and one per tenant override in configuration order (override fields falling back to the entry; `port`, `use_http` and `api_path` always taken from the entry); the effective alias is `upstream_alias`, or the expanded `host` when it is empty; group the targets by effective alias - `inst-group-by-alias`
4. [x] - `p1` - **FOR EACH** alias group with more than one target, compare every later target with the first target of the group - `inst-for-each-alias-group`
   1. [x] - `p1` - **IF** the two targets differ in any of `host`, `port`, `use_http`, `auth_plugin_type`, `auth_config` (compared as expanded key-value maps) or `api_path` - `inst-if-alias-conflict`
      1. [x] - `p1` - **RETURN** validation error naming the alias, both targets (provider id, plus `tenant_overrides.<tenant_id>` for an override) and the names of the differing fields, never any `auth_config` value; `init` fails (targets sharing an alias are provisioned once, so they must be identical) - `inst-return-alias-conflict`
5. [x] - `p1` - **RETURN** validated entries - `inst-return-providers`

### Database Migrations

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-run-migrations`

**Input**: Database connection (PostgreSQL or SQLite) supplied by the `db` capability

**Output**: Schema containing every P1 table, constraint and index, plus the shared outbox tables

**Steps**:
1. [x] - `p1` - Detect the engine; use one statement for both engines where the dialects agree and a PostgreSQL and a SQLite variant where they differ - `inst-detect-engine`
2. [x] - `p1` - On SQLite declare UUID columns `TEXT` and write UUID values as 16-byte `BLOB`s; map `TIMESTAMPTZ`, `JSONB`, `BYTEA` and `BOOLEAN` to the SQLite equivalents - `inst-sqlite-types`
3. [x] - `p1` - DB: CREATE `chats`, `messages`, `chat_turns`, `attachments`, `message_attachments`, `thread_summaries`, `chat_vector_stores`, `quota_usage`, `message_reactions` with the columns, defaults, keys, CHECKs and indexes listed in `cpt-cf-mini-chat-dod-gear-foundation-schema` - `inst-create-tables`
4. [x] - `p1` - Include the shared platform outbox migrations in the gear migration set - `inst-outbox-migrations`
5. [x] - `p1` - Create no `mcp_servers`, `mcp_server_tools`, `role_mcp_servers` or `tenant_closure` table - `inst-no-mcp-tables`
6. [x] - `p1` - **RETURN** migrated schema; re-running the migrations is a no-op - `inst-return-schema`

### Authorization Evaluation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-authorize`

**Input**: `SecurityContext`, resource type, action, optional `resource.id`, `require_constraints` flag, create-time properties

**Output**: `AccessScope` (chat and user-quota resources), a decision-only allow (model resource), or a domain authorization error

**Steps**:
1. [x] - `p1` - Build the PDP evaluation request: subject (type `gts.cf.core.security.subject_user.v1~`, user id, tenant id), the action, the resource type and `resource.id` when present; for `create` add `resource.properties.owner_tenant_id` and `owner_id` from the `SecurityContext` - `inst-build-request`
2. [x] - `p1` - Declare `context.capabilities` without `tenant_hierarchy`, `group_membership` or `group_hierarchy`, `context.supported_properties` `owner_tenant_id` (`chats.tenant_id`), `owner_id` (`chats.user_id`), `id` (`chats.id`), the request `token_scopes`, and the `require_constraints` flag - `inst-build-context`
3. [x] - `p1` - **TRY** - `inst-try-pdp`
   1. [x] - `p1` - Evaluate through PolicyEnforcer against `authz_resolver`, bounded by `PDP_EVALUATION_TIMEOUT` (5 s) - `inst-evaluate`
4. [x] - `p1` - **CATCH** PDP unreachable, `PDP_EVALUATION_TIMEOUT` elapsed or evaluation failure - `inst-catch-pdp`
   1. [x] - `p1` - Log the cause (naming `timeout` when the 5 s bound elapsed) at `error` and **RETURN** the PDP-unavailable domain error (503, `Retry-After: 5`), identical to any other PDP failure - `inst-return-pdp-unavailable`
5. [x] - `p1` - **IF** `decision` is `false` - `inst-if-denied`
   1. [x] - `p1` - **RETURN** the denied domain error (403 `AUTHZ_DENIED`) without exposing `deny_reason` details - `inst-return-denied`
6. [x] - `p1` - **IF** `require_constraints` is `false` - `inst-if-decision-only`
   1. [x] - `p1` - **RETURN** decision-only allow - `inst-return-decision-only`
7. [x] - `p1` - **IF** the constraint list is absent or empty - `inst-if-no-constraints`
   1. [x] - `p1` - **RETURN** the denied domain error - `inst-return-no-constraints`
8. [x] - `p1` - **FOR EACH** constraint - `inst-for-each-constraint`
   1. [x] - `p1` - Compile its predicates to Secure ORM conditions; a predicate of unknown type makes the constraint false; an unknown `resource_property` makes it false and logs an error (PDP contract violation) - `inst-compile-constraint`
9. [x] - `p1` - **IF** every constraint is false or compilation fails - `inst-if-compile-failed`
   1. [x] - `p1` - **RETURN** the denied domain error - `inst-return-compile-denied`
10. [x] - `p1` - **IF** the resource type is the chat type `gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~` - `inst-if-chat-resource`
    1. [x] - `p1` - Add the owner predicates `tenant_id` equal to the subject tenant and `user_id` equal to the subject id to the compiled scope (defence in depth) - `inst-add-owner-predicate`
11. [x] - `p1` - **RETURN** the compiled `AccessScope` - `inst-return-scope`

### Chat-Scoped Data Access

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access`

**Input**: `AccessScope` from `cpt-cf-mini-chat-algo-gear-foundation-authorize`, `chat_id`, optional child lookup

**Output**: The chat row (and the child row) or a not-found domain error

**Steps**:
1. [x] - `p1` - DB: SELECT `chats` through Secure ORM with the scope (tenant column `tenant_id`, owner column `user_id`, resource column `id`) and `deleted_at IS NULL` - `inst-select-chat`
2. [x] - `p1` - **IF** the scoped query returns 0 rows (missing, soft-deleted, other tenant or other user) - `inst-if-chat-missing`
   1. [x] - `p1` - **RETURN** not-found with resource type `gts.cf.core.mini_chat.chat.v1~` - `inst-return-chat-404`
3. [x] - `p1` - **IF** a child row is requested - `inst-if-child`
   1. [x] - `p1` - DB: SELECT the child table through Secure ORM (tenant column `tenant_id`) filtered by the `chat_id` of the row from step 1, by the child id and, for the child tables that have a `deleted_at` column (`messages`, `chat_turns`, `attachments`), by `deleted_at IS NULL` - `inst-select-child`
   2. [x] - `p1` - **IF** 0 rows (missing, soft-deleted, or belonging to another chat) - `inst-if-child-missing`
      1. [x] - `p1` - **RETURN** not-found with the child's resource type (`message`, `turn` or `attachment`) - `inst-return-child-404`
4. [x] - `p1` - Never look up `chat_vector_stores` by `id` or `vector_store_id` alone in request code; background code without a user scope still filters by `tenant_id` - `inst-forbid-unscoped`
5. [x] - `p1` - **RETURN** the chat and the child row - `inst-return-rows`

### Domain Error to Problem Mapping

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-map-error`

**Input**: Domain error

**Output**: RFC 9457 `Problem` (`type`, `title`, `status`, `detail`, `instance`, `trace_id`, `context`) and headers

**Steps**:
1. [x] - `p1` - Look up the category and HTTP status of the error kind in the mapping table below - `inst-lookup-category`
2. [x] - `p1` - **MATCH** the category - `inst-match-category`
   - [x] - `p1` - **CASE** `not_found`: set `context.resource_type` to the missing resource type - `inst-case-not-found`
   - [x] - `p1` - **CASE** `invalid_argument` or `out_of_range`: set `context.field_violations[]` with field and reason - `inst-case-field-violations`
   - [x] - `p1` - **CASE** `failed_precondition`: set `context.violations[]` with `subject`, `description`, `type` - `inst-case-precondition`
   - [x] - `p1` - **CASE** `resource_exhausted`: set `context.violations[]` with `subject` and `description` `quota_exceeded`, or the limit reason - `inst-case-exhausted`
   - [x] - `p1` - **CASE** `aborted` or `permission_denied`: set `context.reason` - `inst-case-reason`
   - [x] - `p1` - **CASE** `already_exists`: set `resource_name` to the conflict code - `inst-case-already-exists`
   - [x] - `p1` - **CASE** `service_unavailable`: set the `Retry-After` header and `context.retry_after_seconds` to the same value - `inst-case-unavailable`
   - [x] - `p1` - **DEFAULT**: `internal` with a generic `detail` - `inst-case-internal`
3. [x] - `p1` - Log driver, backend, PDP and internal messages; never place them, provider identifiers (`file-…`, `file_…`, `assistant-…`, `vs_…`) or secrets in `detail` - `inst-sanitize-detail`
4. [x] - `p1` - Emit no top-level `code` field - `inst-no-code-field`
5. [x] - `p1` - **RETURN** the `Problem` with its status - `inst-return-problem-object`

**Mapping table** (domain error kinds shared by all features; rows marked *platform* are produced by api-gateway or platform extractors and are only verified here):

| Domain error kind | Category | HTTP | Machine-readable reason |
|---|---|---|---|
| Resource not found (chat, message, turn, attachment, model) | `not_found` | 404 | `context.resource_type` = `gts.cf.core.mini_chat.{chat,message,turn,attachment,model}.v1~` |
| Invalid or disabled model on create / chat model removed from catalog | `invalid_argument` | 400 | `field_violations[model].reason = INVALID_MODEL` |
| Empty content | `invalid_argument` | 400 | `field_violations[content].reason = EMPTY_CONTENT` |
| Invalid title | `invalid_argument` | 400 | `field_violations[title].reason = INVALID_TITLE` |
| Invalid reaction value | `invalid_argument` | 400 | `field_violations[reaction].reason = INVALID_REACTION` |
| Bad OData query | `invalid_argument` | 400 | `context.resource_type = gts.cf.core.odata.query.v1~`; OData library and extractor reasons |
| Unsupported upload MIME type | `invalid_argument` | 400 | `UNSUPPORTED_CONTENT_TYPE` |
| Code-interpreter-only upload unavailable | `invalid_argument` | 400 | `field_violations[file].reason = CODE_INTERPRETER_UNAVAILABLE`, attachment resource type |
| Invalid multipart | `invalid_argument` | 400 | `BOUNDARY_REQUIRED`, `MULTIPART_ERROR`, `MISSING_FILE`, `MISSING_CONTENT_TYPE` |
| Chat-cleanup payload too large on chat delete | `invalid_argument` | 400 | `detail` mirrored in `context.format` |
| Vision not supported | `invalid_argument` | 400 | `VISION_NOT_SUPPORTED` |
| Invalid attachment ids | `invalid_argument` | 400 | `field_violations[attachment].reason = invalid_attachment` |
| File too large | `out_of_range` | 400 | `field_violations[content_length].reason = FILE_TOO_LARGE` |
| Too many images | `out_of_range` | 400 | `field_violations[image_count].reason = TOO_MANY_IMAGES` |
| Input too long / context budget exceeded | `out_of_range` | 400 | `INPUT_TOO_LONG` / `CONTEXT_BUDGET_EXCEEDED` |
| Feature disabled by kill switch | `failed_precondition` | 400 | `violations[{subject: web_search or images, type: FEATURE_DISABLED}]` |
| Turn not terminal / reaction on non-assistant message | `failed_precondition` | 400 | `violations[{subject: turn_state or reaction_target, type: STATE}]` |
| Authorization denied (PDP deny, compile failure, foreign turn requester) | `permission_denied` | 403 | `AUTHZ_DENIED` |
| PDP unavailable | `service_unavailable` | 503 | `Retry-After: 5`, `context.retry_after_seconds = 5`, generic detail |
| Turn already running / request id conflict / not latest turn / generation in progress / replay | `aborted` | 409 | `turn_already_running` / `request_id_conflict` / `NOT_LATEST_TURN` / `GENERATION_IN_PROGRESS` / `REPLAY` |
| Attachment locked / provider mismatch / other unique violation | `already_exists` | 409 | `resource_name` = `attachment_locked` / `provider_mismatch` / `unique_violation` |
| Quota exhausted | `resource_exhausted` | 429 | `violations[{subject: tokens or web_search or code_interpreter, description: quota_exceeded}]` |
| Per-chat document or storage limit | `resource_exhausted` | 429 | `document_limit` / `storage_limit` |
| Storage backend failure / upload concurrency limit | `service_unavailable` | 503 | `Retry-After: 10` / `Retry-After: 5` |
| Provider or policy resolution failure, internal or database error | `internal` | 500 | generic detail |
| *platform* unauthenticated | `unauthenticated` | 401 | `MISSING_BEARER` / `AUTHN_FAILED` |
| *platform* license feature missing | `permission_denied` | 403 | `LICENSE_FEATURE_REQUIRED` |
| *platform* extractor failures | `invalid_argument` | 400 / 422 / 415 | `invalid_path_params`, `json_syntax_error`, `invalid_json_body`, `missing_json_content_type` |

### Static Model Policy Plugin

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin`

**Input**: `static-mini-chat-model-policy-plugin` configuration section; plugin client calls

**Output**: Registered plugin instance serving a fixed snapshot (version 1)

**Steps**:
1. [x] - `p1` - Parse the section, rejecting unknown top-level keys and unknown keys inside `kill_switches`; an absent section uses all defaults - `inst-parse-plugin-config`
2. [x] - `p1` - Apply defaults: `vendor` `constructorfabric`, `priority` 100, `model_catalog` empty when the section is absent (the key is required when the section is present; an empty list is valid), every kill switch `false` when omitted, `default_standard_limits` daily 100_000_000 / monthly 1_000_000_000, `default_premium_limits` daily 50_000_000 / monthly 500_000_000 - `inst-plugin-defaults`
3. [x] - `p1` - **FOR EACH** catalog entry - `inst-for-each-catalog-entry`
   1. [x] - `p1` - **IF** either credit multiplier is outside `1..=10_000_000_000` or `estimation_budgets.bytes_per_token_conservative` is 0 - `inst-if-bad-entry`
      1. [x] - `p1` - **RETURN** plugin init failure naming the model id and field - `inst-return-plugin-error`
4. [x] - `p1` - Register a GTS instance of `MiniChatModelPolicyPluginSpecV1` with `vendor` and `priority` in types-registry and its `MiniChatModelPolicyPluginClientV1` in ClientHub scoped to that instance - `inst-register-policy-instance`
5. [x] - `p1` - Serve `get_current_policy_version` with version 1 - `inst-serve-version`
6. [x] - `p1` - Serve `get_policy_snapshot` for version 1 with the configured catalog and kill switches; any other version returns a plugin error - `inst-serve-snapshot`
7. [x] - `p1` - Serve `get_user_limits` for version 1 with the requesting `user_id`, version 1, `standard` set to `default_standard_limits` and `premium` set to `default_premium_limits`; any other version returns a plugin error - `inst-serve-limits`
8. [x] - `p1` - Serve `publish_usage` by logging the event and returning success - `inst-serve-publish`
9. [x] - `p1` - Keep the trait default of `check_user_license` (`active` `false`); the gear never calls it - `inst-keep-license-default`
10. [x] - `p1` - **RETURN** registered plugin - `inst-return-policy-plugin`

### Static Audit Plugin

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin`

**Input**: `static-mini-chat-audit-plugin` configuration section; emitted audit events

**Output**: Registered plugin instance that logs audit events

**Steps**:
1. [x] - `p1` - Parse the section, rejecting unknown top-level keys; defaults `enabled` `true`, `vendor` `constructorfabric`, `priority` 100 - `inst-parse-audit-config`
2. [x] - `p1` - Register a GTS instance of `MiniChatAuditPluginSpecV1` with `vendor` and `priority` and its `MiniChatAuditPluginClientV1` in ClientHub scoped to that instance - `inst-register-audit-instance`
3. [x] - `p1` - **IF** an event is emitted and `enabled` is `true` - `inst-if-audit-enabled`
   1. [x] - `p1` - Log the event as one structured log record - `inst-log-audit-event`
4. [x] - `p1` - **RETURN** success for every emitted event (also when `enabled` is `false`) - `inst-return-audit-ok`

### Plugin Resolution

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin`

**Input**: Plugin spec (`MiniChatModelPolicyPluginSpecV1` or `MiniChatAuditPluginSpecV1`), gear `vendor`, gateway cache

**Output**: Plugin client, "no plugin registered", or a resolution error (registry query failure or timeout, or missing client)

**Steps**:
1. [x] - `p1` - **IF** the gateway holds a cached instance id - `inst-if-cached`
   1. [x] - `p1` - **SKIP TO** step 5 - `inst-skip-to-client`
2. [x] - `p1` - Query types-registry for instances of the spec whose vendor equals the gear `vendor`, bounded by `REGISTRY_QUERY_TIMEOUT` (10 s); **IF** the query fails or the timeout elapses, **RETURN** resolution error (nothing is cached; callers return 500 `internal` or `Retry`) - `inst-query-registry`
3. [x] - `p1` - **IF** no instance matches - `inst-if-none`
   1. [x] - `p1` - **RETURN** "no plugin registered" without caching the result - `inst-return-none`
4. [x] - `p1` - **ELSE** - `inst-else-instance-found`
   1. [x] - `p1` - Select the instance with the lowest `priority` value (ties broken deterministically by instance id ascending) and cache its id - `inst-select-instance`
5. [x] - `p1` - Get the client for the instance id from ClientHub - `inst-get-client`
6. [x] - `p1` - **IF** the client is missing - `inst-if-client-absent`
   1. [x] - `p1` - Reset the cached instance id and **RETURN** resolution error (missing client) - `inst-reset-cache`
7. [x] - `p1` - **RETURN** client - `inst-return-client`

### OAGW Provisioning

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-provision-oagw`

**Input**: Copy of the alias-filled provider entries, `client_credentials`

**Output**: Provisioned targets and the set of deferred targets, or a misconfiguration error

**Idempotency rule** (shared with `cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers`): OAGW rejects an overlapping enabled route on the same upstream with a Conflict, and an upstream can be shared (a tenant override with the same host or alias as its entry, two entries with the same host, a reconcile retry, a restart against an OAGW that already holds the routes). Provisioning therefore never blindly creates a route: for a reused upstream it lists the existing routes first and skips each route whose method and path prefix already exist, and an already-exists / overlap Conflict returned by route creation (a concurrent or unlisted duplicate) is treated as success, never as an error.

**Steps**:
1. [x] - `p1` - API: exchange `client_credentials` through `authn_resolver` for an S2S security context, bounded by `S2S_EXCHANGE_TIMEOUT` (10 s); **IF** the exchange fails or the timeout elapses, **RETURN** misconfiguration error naming `client_id` and the cause (`timeout` when the bound elapsed) - `inst-s2s-exchange`
2. [x] - `p1` - Build one target per provider entry and one per tenant override (override fields falling back to the entry, `storage_kind` inherited) - `inst-build-targets`
3. [x] - `p1` - De-duplicate the targets by alias before any OAGW call: targets with the same alias form one group in configuration order (validation at init, see `cpt-cf-mini-chat-algo-gear-foundation-validate-providers`, guarantees that all members of a group have identical `host`, `port`, `use_http`, `auth_plugin_type`, `auth_config` and `api_path`, so the group upstream and routes are built from any member), the upstream and each distinct route (method and path prefix) are provisioned once per group, and every target of the group takes the group outcome (`provisioned` or `deferred`) - `inst-dedupe-targets`
4. [x] - `p1` - **FOR EACH** target group - `inst-for-each-target`
   1. [x] - `p1` - **TRY** - `inst-try-target`
      1. [x] - `p1` - API: create the OAGW upstream under the target alias with `host`, `port`, `use_http`, `auth_plugin_type`, `auth_config`, bounded by `OAGW_CALL_TIMEOUT` (10 s); reuse it when OAGW reports that it already exists and remember that it is reused - `inst-create-upstream`
      2. [x] - `p1` - **IF** the upstream is reused, API: list its existing routes, bounded by `OAGW_CALL_TIMEOUT` - `inst-list-routes`
      3. [x] - `p1` - API: create the chat route unless a route with method `POST` and the same path prefix already exists: method `POST`, path prefix = `api_path` up to the `{model}` placeholder (the model is a path suffix), query allowlist = the parameter names in the `api_path` query; bounded by `OAGW_CALL_TIMEOUT`; an already-exists / overlap Conflict is treated as success - `inst-create-chat-route`
      4. [x] - `p1` - Write the alias returned by OAGW into the copy only; the provider resolver keeps the configured alias - `inst-write-copy`
   2. [x] - `p1` - **CATCH** credstore secret not readable - `inst-catch-secret`
      1. [x] - `p1` - Add the target to the deferred set - `inst-defer-target`
   3. [x] - `p1` - **CATCH** an OAGW call exceeding `OAGW_CALL_TIMEOUT` (transient error) - `inst-catch-timeout`
      1. [x] - `p1` - Log a warning naming the provider id and add the target to the deferred set instead of waiting or failing `start` - `inst-defer-timeout`
   4. [x] - `p1` - **CATCH** any other error - `inst-catch-misconfig`
      1. [x] - `p1` - **RETURN** misconfiguration error naming the provider id (and tenant for an override) - `inst-return-misconfig`
   5. [x] - `p1` - **IF** the target has a `storage_kind` and its upstream exists - `inst-if-storage`
      1. [x] - `p1` - API: create the RAG routes `POST {prefix}/files`, `DELETE {prefix}/files/*`, `POST {prefix}/vector_stores*`, `DELETE {prefix}/vector_stores/*`, `GET {prefix}/vector_stores/*` with prefix `/v1` for `openai`, or `/openai` with the `api-version` query parameter allowed for `azure`; skip every route whose method and path prefix already exist on the upstream and treat an already-exists / overlap Conflict as success; each call is bounded by `OAGW_CALL_TIMEOUT`; on any other failure or a timeout log a warning (RAG degraded) and continue - `inst-create-rag-routes`
5. [x] - `p1` - **IF** the deferred set is not empty - `inst-if-deferred`
   1. [x] - `p1` - Spawn the reconcile task (see `cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers`) - `inst-spawn-reconcile`
6. [x] - `p1` - **RETURN** provisioned and deferred targets - `inst-return-provisioned`

### Deferred Provider Reconciliation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers`

**Input**: Deferred targets, S2S security context, cancellation token

**Output**: Targets provisioned in the background; one warning when they stay pending for 2 minutes

**Steps**:
1. [x] - `p1` - Set the retry interval to 2 s and wait one interval after `start` - `inst-initial-delay`
2. [x] - `p1` - **WHILE** the deferred set is not empty and the gear is not stopping - `inst-while-pending`
   1. [x] - `p1` - **FOR EACH** deferred target - `inst-for-each-deferred`
      1. [x] - `p1` - Retry the upstream, chat route and RAG route creation of `cpt-cf-mini-chat-algo-gear-foundation-provision-oagw` under the same idempotency rule (alias de-duplication, listing the routes of a reused upstream, skipping existing routes by method and path prefix, treating an already-exists / overlap Conflict as success) and the same `OAGW_CALL_TIMEOUT` (10 s) per call; on success remove the target from the set; on failure or timeout keep it and log the cause - `inst-retry-target`
   2. [x] - `p1` - **IF** 2 minutes have passed since `start`, targets are still pending and no warning was logged yet - `inst-if-warn-due`
      1. [x] - `p1` - Log one warning naming the pending provider ids - `inst-warn-pending`
   3. [x] - `p1` - Wait the interval, then double it up to 60 s (it stays at 60 s) - `inst-backoff`
3. [x] - `p1` - **RETURN** when the set is empty or the gear stops - `inst-return-reconcile`

### Provider Resolution

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-resolve-provider`

**Input**: Catalog `provider_id`, request tenant id

**Output**: Adapter kind, OAGW alias, `api_path`, storage provider id; or a provider-not-configured error

**Steps**:
1. [x] - `p1` - Look up `providers.<provider_id>`; **IF** absent, **RETURN** provider-not-configured error (mapped to 500 `internal`) - `inst-lookup-provider`
2. [x] - `p1` - **IF** the entry has a tenant override for the tenant id - `inst-if-tenant-override`
   1. [x] - `p1` - Use the override alias (filled with the override `host` at init) - `inst-use-override-alias`
3. [x] - `p1` - **ELSE** - `inst-else-entry-alias`
   1. [x] - `p1` - Use the entry alias (filled with the entry `host` at init) - `inst-use-entry-alias`
4. [x] - `p1` - Set the storage provider id to `rag_provider` when configured, otherwise to the entry itself - `inst-storage-provider`
5. [x] - `p1` - **RETURN** `kind`, alias, `api_path` and storage provider id - `inst-return-resolution`

### Outbox Pipeline Start

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-start-outbox`

**Input**: Validated `outbox` and `thread_summary_worker` configuration, handlers

**Output**: Running shared outbox pipeline with five leased queues

**Steps**:
1. [x] - `p1` - Register queue `outbox.queue_name` (usage) with the usage handler and the outbox default lease (30 s) - `inst-register-usage-queue`
2. [x] - `p1` - Register queue `outbox.cleanup_queue_name` (attachment cleanup) with the outbox default lease - `inst-register-attachment-cleanup`
3. [x] - `p1` - Register queue `outbox.chat_cleanup_queue_name` (chat cleanup) with the outbox default lease - `inst-register-chat-cleanup`
4. [x] - `p1` - Register queue `outbox.thread_summary_queue_name` with lease `thread_summary_worker.claim_timeout_secs` - `inst-register-summary-queue`
5. [x] - `p1` - Register queue `outbox.audit_queue_name` with the audit handler and a 60 s lease - `inst-register-audit-queue`
6. [x] - `p1` - Use `outbox.num_partitions` partitions for all five queues; producers partition usage and audit messages by `tenant_id`, chat cleanup and thread summary messages by `chat_id` - `inst-partitions`
7. [x] - `p1` - Attach placeholder handlers to the two cleanup queues and the thread-summary queue: each logs a warning and returns `Retry` until its owning feature replaces it - `inst-placeholder-handlers`
8. [x] - `p1` - Keep backoff, batch sizes, sequencer and vacuum cadence at the shared outbox defaults - `inst-outbox-defaults`
9. [x] - `p1` - **RETURN** running pipeline - `inst-return-pipeline`

### Usage Outbox Handler

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-handle-usage`

**Input**: Outbox message (payload, attempt count) from the usage queue

**Output**: `Ok`, `Retry` or `Reject(reason)`

**Steps**:
1. [x] - `p1` - Decode the payload as `UsageEvent` (see `cpt-cf-mini-chat-algo-gear-foundation-encode-events`) - `inst-decode-usage`
2. [x] - `p1` - **IF** decoding fails - `inst-if-usage-malformed`
   1. [x] - `p1` - **RETURN** `Reject` with the decode reason (dead letter) - `inst-reject-malformed-usage`
3. [x] - `p1` - Resolve the model-policy plugin (see `cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin`); **IF** it is not registered or resolution fails (including `REGISTRY_QUERY_TIMEOUT`), **RETURN** `Retry` - `inst-resolve-policy`
4. [x] - `p1` - **TRY** - `inst-try-publish`
   1. [x] - `p1` - Call `publish_usage` with the event, bounded by `PUBLISH_USAGE_TIMEOUT` (20 s, shorter than the 30 s usage-queue lease so the handler returns before the lease expires) - `inst-publish`
5. [x] - `p1` - **CATCH** `PUBLISH_USAGE_TIMEOUT` elapsed - `inst-catch-publish-timeout`
   1. [x] - `p1` - Log a warning and **RETURN** `Retry` - `inst-retry-publish-timeout`
6. [x] - `p1` - **CATCH** `PublishError` - `inst-catch-publish`
   1. [x] - `p1` - **IF** the error is transient, **RETURN** `Retry`; otherwise **RETURN** `Reject` with the reason - `inst-classify-publish`
7. [x] - `p1` - **RETURN** `Ok` - `inst-return-usage-ok`

### Audit Outbox Handler

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-handle-audit`

**Input**: Outbox message (payload, attempt count) from the audit queue

**Output**: `Ok`, `Retry` or `Reject(reason)`; one `mini_chat_audit_emit` count per delivery

**Steps**:
1. [x] - `p1` - Decode the payload as an audit event before resolving any plugin - `inst-decode-audit`
2. [x] - `p1` - **IF** decoding fails - `inst-if-audit-malformed`
   1. [x] - `p1` - Count `result` `reject` and **RETURN** `Reject` (also when no plugin is registered) - `inst-reject-malformed-audit`
3. [x] - `p1` - Resolve the audit plugin (see `cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin`) - `inst-resolve-audit`
4. [x] - `p1` - **IF** no plugin is registered - `inst-if-no-audit-plugin`
   1. [x] - `p1` - Log a warning once per period without a plugin, count `result` `dropped` and **RETURN** `Ok` (acknowledged, not delivered) - `inst-drop-event`
5. [x] - `p1` - **IF** resolution failed (including `REGISTRY_QUERY_TIMEOUT`) or the resolved instance has no ClientHub client - `inst-if-resolution-failed`
   1. [x] - `p1` - Set the outcome to `Retry` and **GO TO** step 8 - `inst-resolution-retry`
6. [x] - `p1` - Call `emit` on the plugin with the 30 s `AUDIT_EMIT_TIMEOUT` - `inst-emit`
7. [x] - `p1` - **MATCH** the emit result - `inst-match-emit`
   - [x] - `p1` - **CASE** success: count `ok` and **RETURN** `Ok` - `inst-case-emit-ok`
   - [x] - `p1` - **CASE** `Transient` or `PluginTimeout` (including the 30 s timeout): set the outcome to `Retry` - `inst-case-emit-transient`
   - [x] - `p1` - **CASE** `Permanent`: count `reject` and **RETURN** `Reject` - `inst-case-emit-permanent`
8. [x] - `p1` - **IF** the outcome is `Retry` and this is the 120th delivery attempt - `inst-if-last-attempt`
   1. [x] - `p1` - Count `reject` and **RETURN** `Reject` (dead letter, the partition is unblocked) - `inst-reject-last-attempt`
9. [x] - `p1` - Count `retry` and **RETURN** `Retry` - `inst-return-audit-retry`

### Model Visibility and Projection

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-visible-models`

**Input**: `SecurityContext` (user id for plugin calls)

**Output**: Ordered list of model projections, or an internal error

**Steps**:
1. [x] - `p1` - Resolve the model-policy plugin; **IF** it is not registered or resolution fails (including `REGISTRY_QUERY_TIMEOUT`), **RETURN** internal error (500) - `inst-resolve-catalog-plugin`
2. [x] - `p1` - Read the current policy version and then the snapshot of that version (no local cache), each call bounded by `POLICY_PLUGIN_CALL_TIMEOUT` (10 s); **IF** either call fails or the timeout elapses, **RETURN** internal error - `inst-read-snapshot`
3. [x] - `p1` - Keep only catalog entries with `enabled` `true`, in catalog order - `inst-filter-enabled`
4. [x] - `p1` - **FOR EACH** kept entry - `inst-for-each-visible`
   1. [x] - `p1` - Project exactly `model_id` (from `id`), `display_name`, `tier`, `multiplier_display`, `description` (omitted when empty), `multimodal_capabilities`, `context_window` - `inst-project-fields`
5. [x] - `p1` - Never project `provider_id`, `provider_model_id`, `provider_display_name`, credit multipliers, `policy_version`, `max_output_tokens`, `max_input_tokens`, `preference` or `is_default`, `estimation_budgets`, `general_config`, prompts or routing data - `inst-non-exposure`
6. [x] - `p1` - **RETURN** projections - `inst-return-projections`

### Metrics Registration

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-register-metrics`

**Input**: `metrics.prefix`

**Output**: Registered OpenTelemetry instruments

**Steps**:
1. [x] - `p1` - Create the gear meter; the prefix is `metrics.prefix`, or `mini_chat` when empty - `inst-create-meter`
2. [x] - `p1` - Register every instrument of DESIGN "Metric series" (the emitted set and the declared-but-not-recorded set) with its type; register no `mini_chat_mcp_*` instrument - `inst-register-instruments`
3. [x] - `p1` - Restrict label keys to the low-cardinality set of DESIGN "Naming and cardinality rules"; never use `tenant_id`, `user_id`, `chat_id`, `request_id`, `provider_response_id`, filenames or free-form error text as labels - `inst-label-rules`
4. [x] - `p1` - **RETURN** the metrics handle shared by all services and handlers - `inst-return-metrics`

## 4. States (CDSL)

### Gear Lifecycle State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle`

**States**: `created`, `initialized`, `init_failed`, `started`, `start_failed`, `stopped`

**Initial State**: `created`

**Transitions**:
1. [x] - `p1` - **FROM** `created` **TO** `initialized` **WHEN** configuration validation, client resolution and service construction succeed - `inst-to-initialized`
2. [x] - `p1` - **FROM** `created` **TO** `init_failed` **WHEN** configuration validation fails or a required ClientHub client is missing - `inst-to-init-failed`
3. [x] - `p1` - **FROM** `initialized` **TO** `started` **WHEN** OAGW provisioning has no misconfiguration and the outbox pipeline runs (deferred providers allowed) - `inst-to-started`
4. [x] - `p1` - **FROM** `initialized` **TO** `start_failed` **WHEN** the S2S exchange fails or times out or a provider entry is misconfigured - `inst-to-start-failed`
5. [x] - `p1` - **FROM** `started` **TO** `stopped` **WHEN** `stop` cancels and joins the background tasks and stops the outbox pipeline - `inst-to-stopped`

### Provider Provisioning State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-gear-foundation-provider-provisioning`

**States**: `pending`, `provisioned`, `deferred`, `failed`

**Initial State**: `pending`

**Transitions**:
1. [x] - `p1` - **FROM** `pending` **TO** `provisioned` **WHEN** the upstream is created or reused and the chat route is created or already exists (skipped or reported as an already-exists Conflict) - `inst-pending-to-provisioned`
2. [x] - `p1` - **FROM** `pending` **TO** `deferred` **WHEN** OAGW reports that the credstore secret of the target is not readable, or an OAGW call exceeds `OAGW_CALL_TIMEOUT` - `inst-pending-to-deferred`
3. [x] - `p1` - **FROM** `pending` **TO** `failed` **WHEN** any other provisioning error occurs (gear `start` fails) - `inst-pending-to-failed`
4. [x] - `p1` - **FROM** `deferred` **TO** `provisioned` **WHEN** a background reconcile attempt succeeds (existing routes and Conflicts count as success) - `inst-deferred-to-provisioned`
5. [x] - `p1` - **FROM** `deferred` **TO** `deferred` **WHEN** a background reconcile attempt fails or times out (retried at 2 s doubling to 60 s until the gear stops) - `inst-deferred-retry`

### Plugin Resolution State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-gear-foundation-plugin-resolution`

**States**: `unresolved`, `resolved`

**Initial State**: `unresolved`

**Transitions**:
1. [x] - `p1` - **FROM** `unresolved` **TO** `resolved` **WHEN** types-registry returns a matching instance; its id is cached - `inst-to-resolved`
2. [x] - `p1` - **FROM** `unresolved` **TO** `unresolved` **WHEN** no instance is registered (not cached; the next call looks up again) - `inst-stay-unresolved`
3. [x] - `p1` - **FROM** `resolved` **TO** `unresolved` **WHEN** the cached instance has no client in ClientHub (cache reset) - `inst-to-unresolved`

## 5. Definitions of Done

Specific implementation tasks derived from flows/algorithms above.

### SDK Crate

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-sdk-crate`

The system **MUST** provide the crate `gears/mini-chat/mini-chat-sdk/` (package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`), added to the workspace, depended on by the gear crate and depending on no gear crate. It holds:

- Models: `ModelCatalogEntry` (all fields of DESIGN "Model Catalog Configuration" and Appendix A with the required/default rules of `cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot`), `EstimationBudgets` (eight fields with defaults), `ModelGeneralConfig` (`type`, `available_from`, `max_file_size_mb`, `api_params`, `features`, `tool_support`, `supported_endpoints`), `ApiParams`, `ModelToolSupport` (`web_search`, `file_search`, `image_generation`, `code_interpreter`, `mcp` — unused), `ModelTier` (`premium` / `standard`), `KillSwitches` (six required fields), `PolicySnapshot` (`policy_version`, `model_catalog`, `kill_switches`), `UserLimits` (`user_id`, `policy_version`, `standard`, `premium`), `TierLimits` (`limit_daily_credits_micro`, `limit_monthly_credits_micro`), `UsageEvent` (fields and omission rules of §5.6 / A.3), `TurnAuditEvent` (`event_type` `turn_completed` / `turn_failed`) and the `turn_retry`, `turn_edit`, `turn_delete` mutation events (§3.9, `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope`).
- GTS plugin specs `MiniChatModelPolicyPluginSpecV1` and `MiniChatAuditPluginSpecV1`.
- `MiniChatModelPolicyPluginClientV1`: get current policy version, get policy snapshot by version, get user limits (by `user_id` and version, no tenant), publish usage (error `PublishError` with transient and permanent kinds), `check_user_license` with a default body returning `active` `false`.
- `MiniChatAuditPluginClientV1`: emit event; error enum `Transient`, `PluginTimeout`, `Permanent`.

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot`
- `cpt-cf-mini-chat-algo-gear-foundation-encode-events`

**Constraints**: `cpt-cf-mini-chat-constraint-openai-compatible`

**Touches**:
- Entities: `ModelCatalogEntry`, `PolicySnapshot`, `KillSwitches`, `UserLimits`, `TierLimits`, `UsageEvent`, `TurnAuditEvent`, AuditEvent
- Design: `cpt-cf-mini-chat-design-model-catalog`

### Gear Crate and Lifecycle

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-gear-crate`

The system **MUST** provide the gear crate `gears/mini-chat/mini-chat/` (package `cf-gears-mini-chat`, lib `mini_chat`) registered as ToolKit gear `mini-chat` with deps `types_registry`, `authn_resolver`, `authz_resolver`, `oagw` and capabilities `db`, `rest`, `stateful`; layered into API, domain and infrastructure modules (DESIGN §1.3); keeping the public modules `infra::plugins::static_audit` and `infra::plugins::static_model_policy` that the example server imports; and implementing `init`, REST registration, `start` and `stop` as specified.

**Implements**:
- `cpt-cf-mini-chat-flow-gear-foundation-gear-init`
- `cpt-cf-mini-chat-flow-gear-foundation-gear-start`
- `cpt-cf-mini-chat-flow-gear-foundation-gear-stop`
- `cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- Components: `cpt-cf-mini-chat-component-chat-service`, `cpt-cf-mini-chat-component-chat-store`
- API: REST registration under `url_prefix` (`cpt-cf-mini-chat-interface-public-api`)

### Configuration

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-configuration`

The system **MUST** load the gear configuration with every key, default and validation rule of DESIGN Appendix B.1, B.4, B.5, B.6, B.6.1, B.7, B.8 and B.9.x as listed in `cpt-cf-mini-chat-algo-gear-foundation-load-config` and `cpt-cf-mini-chat-algo-gear-foundation-validate-providers`: unknown keys rejected in the listed sections and accepted in the worker sections, removed keys and `mcp` failing startup, deprecated keys warned (`cpt-cf-mini-chat-adr-runtime-consistency-limitations`), `${VAR}` expansion only in provider and override `host` and `auth_config` and in `client_credentials`, and `client_secret` and every expanded `auth_config` value treated as secrets that never appear in logs, error `detail`s or validation messages (messages name only the key path).

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-load-config`
- `cpt-cf-mini-chat-algo-gear-foundation-validate-providers`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- Config: `mini-chat` section

### Database Schema and Migrations

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-schema`

The system **MUST** ship migrations that create, on PostgreSQL and SQLite, every P1 table of DESIGN §3.7 with the following columns, constraints and indexes, plus the shared outbox migrations. Simple value CHECKs are enforced; the cross-column `chat_turns` CHECKs and the `attachments.cleanup_status` value set are not enforced by the database and are kept by the repositories (`cpt-cf-mini-chat-adr-runtime-consistency-limitations`).

| Table | Columns | Keys, constraints and indexes |
|---|---|---|
| `chats` | `id`, `tenant_id`, `user_id`, `model` VARCHAR(1024), `title` VARCHAR(255) nullable, `is_temporary` default `false`, `created_at`, `updated_at`, `deleted_at` | PK `id`; NOT NULL `tenant_id`, `user_id`, `created_at`, `updated_at`; index `(tenant_id, user_id, updated_at DESC) WHERE deleted_at IS NULL` |
| `messages` | `id`, `tenant_id`, `chat_id`, `request_id` nullable, `role` VARCHAR(16), `content`, `content_type` VARCHAR(32), `token_estimate`, `provider_response_id` VARCHAR(128), `request_kind` VARCHAR(16), `features_used` (NOT NULL default `[]`), `input_tokens`, `output_tokens` (NOT NULL default 0), `cache_read_input_tokens`, `cache_write_input_tokens`, `reasoning_tokens` (default 0), `model` VARCHAR(1024), `is_compressed`, `created_at`, `deleted_at` | PK `id`; NOT NULL `tenant_id`, `chat_id`, `role`, `content`, `content_type`, `created_at`; FK `chat_id` to `chats` ON DELETE CASCADE; partial UNIQUE `(chat_id, request_id, role) WHERE request_id IS NOT NULL AND deleted_at IS NULL`; index `(chat_id, created_at) WHERE deleted_at IS NULL`; UNIQUE `(id, chat_id)` |
| `chat_turns` | `id`, `tenant_id`, `chat_id`, `request_id`, `requester_type`, `requester_user_id`, `state`, `provider_name`, `provider_response_id`, `assistant_message_id`, `error_code` VARCHAR(64), `reserve_tokens`, `max_output_tokens_applied`, `reserved_credits_micro`, `policy_version_applied`, `effective_model`, `minimal_generation_floor_applied`, `error_detail`, `deleted_at`, `replaced_by_request_id`, `started_at` (no DB default), `last_progress_at` nullable, `web_search_enabled` (NOT NULL default `false`), `web_search_completed_count`, `code_interpreter_completed_count`, `file_search_completed_count` (NOT NULL default 0), `completed_at`, `updated_at` | PK `id`; UNIQUE `(chat_id, request_id)`; FK `chat_id` ON DELETE CASCADE; CHECK `requester_type IN ('user','system')`; CHECK `state IN ('running','completed','failed','cancelled')`; non-negative CHECKs on the three completed counts; index `(chat_id, started_at DESC) WHERE deleted_at IS NULL`; partial UNIQUE `(chat_id) WHERE state = 'running' AND deleted_at IS NULL`; index `(last_progress_at) WHERE state = 'running' AND deleted_at IS NULL` |
| `attachments` | `id`, `tenant_id`, `chat_id`, `uploaded_by_user_id`, `filename` VARCHAR(255), `content_type` VARCHAR(128), `size_bytes`, `storage_backend` (default `azure`), `provider_file_id`, `status`, `error_code`, `attachment_kind`, `for_file_search`, `for_code_interpreter` (default `false`), `doc_summary`, `img_thumbnail` (bytes), `img_thumbnail_width`, `img_thumbnail_height`, `summary_model`, `summary_updated_at`, `cleanup_status`, `cleanup_attempts` (default 0), `last_cleanup_error`, `cleanup_updated_at`, `created_at`, `updated_at` (NOT NULL default now), `deleted_at`, `secondary_file_id`, `secondary_status` (default `not_attempted`), `secondary_provider_kind` | PK `id`; NOT NULL `tenant_id`, `chat_id`, `uploaded_by_user_id`, `filename`, `status`, `attachment_kind`, `storage_backend`, `created_at`; FK `chat_id` ON DELETE CASCADE; CHECK `attachment_kind IN ('document','image')`; CHECK `status IN ('pending','uploaded','ready','failed')`; CHECK `secondary_status IN ('not_attempted','pending','uploaded','failed')`; CHECK `secondary_provider_kind` is `anthropic` or NULL; index `(tenant_id, chat_id) WHERE deleted_at IS NULL`; index `(cleanup_status) WHERE cleanup_status IS NOT NULL AND deleted_at IS NULL`; plain index `idx_attachments_stale_upload (status, cleanup_status, deleted_at, updated_at)`; UNIQUE `(id, chat_id)` |
| `message_attachments` | `tenant_id`, `chat_id`, `message_id`, `attachment_id`, `created_at` | PK `(chat_id, message_id, attachment_id)`; NOT NULL `tenant_id`, `chat_id`, `created_at`; composite FK `(message_id, chat_id)` to `messages(id, chat_id)` and `(attachment_id, chat_id)` to `attachments(id, chat_id)`, both ON DELETE CASCADE; indexes `(tenant_id, chat_id)` and `(attachment_id, chat_id)` |
| `thread_summaries` | `id`, `tenant_id`, `chat_id`, `summary_text`, `summarized_up_to_created_at`, `summarized_up_to_message_id`, `token_estimate`, `created_at`, `updated_at` | PK `id`; UNIQUE `chat_id`; FK `chat_id` ON DELETE CASCADE; NOT NULL `tenant_id`, both frontier columns, `created_at`, `updated_at` |
| `chat_vector_stores` | `id`, `tenant_id`, `chat_id`, `vector_store_id` VARCHAR(128) nullable, `provider` VARCHAR(128), `file_count` (NOT NULL default 0), `created_at` | PK `id`; UNIQUE `(tenant_id, chat_id)`; NOT NULL `provider`, `created_at`; CHECK `file_count >= 0` |
| `quota_usage` | `id`, `tenant_id`, `user_id`, `period_type` VARCHAR(16), `period_start` DATE, `bucket` VARCHAR(32), `spent_credits_micro`, `reserved_credits_micro`, `calls`, `input_tokens`, `output_tokens`, `file_search_calls`, `web_search_calls`, `code_interpreter_calls`, `rag_retrieval_calls`, `image_inputs`, `image_upload_bytes` (all default 0), `updated_at` | PK `id`; NOT NULL `bucket`; UNIQUE `(tenant_id, user_id, period_type, period_start, bucket)` serving the quota lookup index |
| `message_reactions` | `id`, `message_id`, `user_id`, `tenant_id`, `reaction` VARCHAR(16), `created_at` | PK `id`; NOT NULL on all columns; FK `message_id` to `messages(id)` ON DELETE CASCADE; CHECK `reaction IN ('like','dislike')`; UNIQUE `(message_id, user_id)` |

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-run-migrations`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-chats`, `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-attachments`, `cpt-cf-mini-chat-dbtable-message-attachments`, `cpt-cf-mini-chat-dbtable-thread-summaries`, `cpt-cf-mini-chat-dbtable-chat-vector-stores`, `cpt-cf-mini-chat-dbtable-quota-usage`, `cpt-cf-mini-chat-dbtable-message-reactions`
- DB Table (not created): `cpt-cf-mini-chat-dbtable-tenant-closure-ref`, `cpt-cf-mini-chat-dbtable-mcp-servers`, `cpt-cf-mini-chat-dbtable-mcp-server-tools`, `cpt-cf-mini-chat-dbtable-role-mcp-servers`

### Domain Entities and Secure ORM Scoping

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-entities-scoping`

The system **MUST** provide domain entities (Chat, Message, ChatTurn, Attachment, MessageAttachment, ThreadSummary, ChatVectorStore, QuotaUsage, MessageReaction) and Secure ORM scoped persistence entities with these scoping columns: `chats` tenant `tenant_id`, owner `user_id`, resource `id`; `messages`, `chat_turns`, `attachments`, `thread_summaries` tenant `tenant_id`, resource `id`; `message_attachments` and `chat_vector_stores` tenant `tenant_id` only; `quota_usage` and `message_reactions` tenant `tenant_id`, owner `user_id`, resource `id`; no type column anywhere. Child tables are reached only through `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access`, so owner isolation of child rows comes from the owner-scoped chat query.

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-chats`, `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-attachments`, `cpt-cf-mini-chat-dbtable-message-attachments`, `cpt-cf-mini-chat-dbtable-thread-summaries`, `cpt-cf-mini-chat-dbtable-chat-vector-stores`, `cpt-cf-mini-chat-dbtable-quota-usage`, `cpt-cf-mini-chat-dbtable-message-reactions`
- Entities: Chat, Message, Attachment, ThreadSummary, ChatVectorStore, QuotaUsage, MessageReaction

### Authorization PEP

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-authz-pep`

The system **MUST** implement the PEP of DESIGN §3.8 (`cpt-cf-mini-chat-design-authz-pep`) as a domain-layer service used before every data access or catalog read: resource types chat `gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~`, model `gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~` (`require_constraints` `false`, decision only) and user quota `gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~`; the chat actions `create`, `list`, `read`, `update`, `delete`, `list_messages`, `send_message`, `upload_attachment`, `read_attachment`, `delete_attachment`, `read_turn`, `retry_turn`, `edit_turn`, `delete_turn`, `set_reaction`, `delete_reaction`, the model actions `list` and `read`, and the user-quota action `read`, with `resource.id` per the per-operation matrix; the owner predicate added to every chat scope; and the fail-closed mapping (deny or compile failure 403 `AUTHZ_DENIED`, PDP evaluation failure 503 with `Retry-After: 5`, scoped 0 rows 404).

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-authorize`

**Touches**:
- Components: `cpt-cf-mini-chat-component-authz-integration`
- DB Table: `cpt-cf-mini-chat-dbtable-chats`

### Canonical Error Contract

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-error-contract`

The system **MUST** define one domain error enum covering every row of the mapping table in `cpt-cf-mini-chat-algo-gear-foundation-map-error` and map it in the API layer to the canonical `Problem` (`cpt-cf-mini-chat-adr-canonical-error-contract`): category-derived status, machine-readable reason fields, `gts.cf.core.mini_chat.{chat,message,turn,attachment,model}.v1~` resource types, `Retry-After` for `service_unavailable`, no top-level `code`, sanitized `detail`. All later features reuse this enum; the SSE `error` event (`{code, message}`) is out of scope here.

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-map-error`
- `cpt-cf-mini-chat-flow-gear-foundation-request-gate`

**Touches**:
- API: every route under `url_prefix` (`cpt-cf-mini-chat-interface-public-api`)

### License Gate

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-license-gate`

The system **MUST** declare the license feature `gts.cf.core.lic.feat.v1~cf.core.global.base.v1` on every route the gear registers (interim gate, `cpt-cf-mini-chat-adr-quota-policy-scope`), so the platform license middleware rejects unlicensed tenants with 403 `LICENSE_FEATURE_REQUIRED` before the handler runs; the gear never calls `check_user_license`.

**Implements**:
- `cpt-cf-mini-chat-flow-gear-foundation-request-gate`

**Constraints**: `cpt-cf-mini-chat-constraint-license-gate`

**Touches**:
- API: `GET /v1/models`, `GET /v1/models/{id}` and every route of later features

### Bundled Plugins and Gateways

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-bundled-plugins`

The system **MUST** provide the bundled gears `static-mini-chat-model-policy-plugin` (module `infra::plugins::static_model_policy`) and `static-mini-chat-audit-plugin` (module `infra::plugins::static_audit`) with the configuration and behaviour of their algorithms, and the model-policy and audit gateways that resolve plugins lazily through types-registry by the gear `vendor` and lowest `priority`.

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin`
- `cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin`
- `cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin`
- `cpt-cf-mini-chat-state-gear-foundation-plugin-resolution`

**Touches**:
- Entities: `PolicySnapshot`, `UserLimits`, AuditEvent

### Provider Registry and OAGW Provisioning

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-provider-registry`

The system **MUST** build the provider registry and resolver at `init` and provision OAGW upstreams and routes at `start` per `cpt-cf-mini-chat-adr-multi-provider-adapters`, `cpt-cf-mini-chat-adr-llm-provider-as-library` and `cpt-cf-mini-chat-adr-internal-transport`: alias defaulting to the host, S2S client-credentials exchange bounded by `S2S_EXCHANGE_TIMEOUT` (10 s), targets sharing an alias required to be identical at `init` (a mismatch fails `init` naming the alias and both targets) and de-duplicated by alias, upstream plus chat route per entry and tenant override created idempotently (existing routes of a reused upstream skipped by method and path prefix, an already-exists / overlap Conflict treated as success, in provisioning and in the reconcile retry alike), RAG routes for storage-capable targets, every OAGW call bounded by `OAGW_CALL_TIMEOUT` (10 s), deterministic misconfiguration failing startup, unreadable secrets and OAGW timeouts retried in the background (2 s doubling to 60 s, one warning after 2 minutes), and the resolver mapping `provider_id` plus tenant to adapter kind and alias. Adapter request and stream translation are not part of this feature; the gear never handles API keys.

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-provision-oagw`
- `cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers`
- `cpt-cf-mini-chat-algo-gear-foundation-resolve-provider`
- `cpt-cf-mini-chat-state-gear-foundation-provider-provisioning`

**Constraints**: `cpt-cf-mini-chat-constraint-openai-compatible`, `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- Components: `cpt-cf-mini-chat-component-llm-provider`
- External: OAGW upstream and route CRUD, `authn_resolver` client credentials

### Outbox Pipeline and Handlers

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-outbox`

The system **MUST** start the shared outbox pipeline with the five queues, partitions and leases of `cpt-cf-mini-chat-algo-gear-foundation-start-outbox`, deliver usage events through the model-policy plugin and audit events through the audit plugin with the `Ok` / `Retry` / `Reject` rules of their handlers (`cpt-cf-mini-chat-fr-audit`, `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope`), and register placeholder handlers for the cleanup and thread-summary queues.

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-start-outbox`
- `cpt-cf-mini-chat-algo-gear-foundation-handle-usage`
- `cpt-cf-mini-chat-algo-gear-foundation-handle-audit`

**Touches**:
- DB: shared outbox tables (platform migrations)
- Entities: `UsageEvent`, `TurnAuditEvent`, AuditEvent

### Models API

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-models-api`

The system **MUST** serve `GET /v1/models` and `GET /v1/models/{id}` (`cpt-cf-mini-chat-interface-models-api`) under `url_prefix`: enabled catalog entries only, the seven-field projection, the non-exposure rules, 404 with resource type `gts.cf.core.mini_chat.model.v1~` for an absent or disabled model, decision-only PEP on the model resource, and OpenAPI documentation of the 200, 401, 403, 404, 500 and 503 responses.

**Implements**:
- `cpt-cf-mini-chat-flow-gear-foundation-list-models`
- `cpt-cf-mini-chat-flow-gear-foundation-get-model`
- `cpt-cf-mini-chat-algo-gear-foundation-visible-models`

**Touches**:
- API: `GET /v1/models`, `GET /v1/models/{id}`
- Design: `cpt-cf-mini-chat-design-model-catalog`

### Metrics Infrastructure

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-metrics`

The system **MUST** register the `mini_chat_*` OpenTelemetry instruments with the configurable prefix and the label rules (`cpt-cf-mini-chat-nfr-observability-supportability`), exported over OTLP by ToolKit. This feature records only `mini_chat_audit_emit{result}`; later features record the other instruments.

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-register-metrics`

**Touches**:
- Observability: `mini_chat_*` meter

### MCP Not Implemented

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-mcp-absent`

The system **MUST NOT** contain an MCP module, `McpPool`, `McpService`, MCP endpoint, MCP admin API, MCP table or migration, `mcp` configuration key or `mini_chat_mcp_*` metric (`cpt-cf-mini-chat-adr-mcp-deferred`); the only trace is the unused catalog flag `ModelToolSupport.mcp`. A configuration containing `mcp` fails startup as an unknown key.

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-run-migrations`
- `cpt-cf-mini-chat-algo-gear-foundation-load-config`

**Touches**:
- Components (Future, absent): `cpt-cf-mini-chat-component-mcp-pool`, `cpt-cf-mini-chat-component-mcp-service`
- API (Future, absent): `cpt-cf-mini-chat-interface-mcp-api`
- DB Table (Future, absent): `cpt-cf-mini-chat-dbtable-mcp-servers`, `cpt-cf-mini-chat-dbtable-mcp-server-tools`, `cpt-cf-mini-chat-dbtable-role-mcp-servers`

### Cross-Cutting Quality and Verification

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-quality-gates`

The system **MUST** meet these cross-cutting requirements and completion signals:

- **Security**: fail-closed authorization on every operation; tenant and owner predicates on every chat-scoped query; responses contain no provider identifiers, driver messages or `deny_reason` details; secrets (`client_secret`, expanded `auth_config` values) never appear in responses or logs; credentials only as OAGW and credstore references.
- **Reliability**: lazy plugin resolution never blocks `init`; outbox delivery is at-least-once with bounded audit retries; deferred providers do not block startup; `stop` is bounded; every external call has an explicit timeout (PDP evaluation 5 s giving 503 `Retry-After: 5`, S2S exchange 10 s failing `start`, each OAGW provisioning call 10 s deferring the target to the reconcile task, `publish_usage` 20 s giving `Retry` below the 30 s usage lease, audit `emit` 30 s giving `Retry`, policy plugin calls 10 s giving 500 on the Models API, a logged error at `start` and `Retry` in handlers, types-registry instance query 10 s giving a resolution error: 500 or `Retry`); OAGW provisioning is idempotent (alias de-duplication, existing routes skipped, Conflict treated as success) so restarts and shared aliases converge.
- **Data integrity**: database keys, foreign keys, CHECKs and partial unique indexes of `cpt-cf-mini-chat-dod-gear-foundation-schema` on both engines.
- **Observability**: startup errors and warnings name the key, provider or model; `mini_chat_audit_emit{result}` counts every audit delivery; PDP failures are logged at `error`.
- **Rollback**: migrations only create new tables and indexes; rolling back the feature means removing the gear from the deployment, which leaves the tables unused; no down migration is required.
- **Test layering**: unit tests for configuration validation, snapshot decoding and event encoding, error mapping, PEP compilation, plugin resolution, provisioning classification and the handlers (with fake plugins, PDP and OAGW); integration tests for migrations and Secure ORM scoping on SQLite and PostgreSQL and for the Models API through the REST layer with a mock PDP; no live provider is called.
- **Compile-time gates**: the SDK crate does not depend on the gear crate; `make dylint` passes on both crates.
- **Completion signals**: automated tests exist and pass for every owned acceptance item (tenant and owner isolation infrastructure, Models API, Authorization PEP part, REST error mapping); `make gear-ci GEAR=mini-chat`, `make dylint` and `cfs validate` pass; every implemented CDSL instruction of this feature is wrapped in `@cpt-begin:{cpt-id}:p1:inst-{id}` / `@cpt-end:{cpt-id}:p1:inst-{id}` code markers (FULL traceability) and every flow, algorithm, state and definition-of-done ID of this feature appears in code; the checkboxes of this document and the featstatus are set to `[x]` only when their markers and tests exist.

**Implements**:
- `cpt-cf-mini-chat-flow-gear-foundation-request-gate`
- `cpt-cf-mini-chat-algo-gear-foundation-authorize`
- `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access`
- `cpt-cf-mini-chat-algo-gear-foundation-map-error`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- Build: `make gear-ci GEAR=mini-chat`, `make dylint`, `cfs validate`

## 6. Acceptance Criteria

- [ ] Tenant and owner isolation (infrastructure): on SQLite and PostgreSQL, a chat of another tenant and a chat of another user in the same tenant are both invisible (404 with the chat resource type), also when the PDP returns only a tenant predicate; child rows (message, turn, attachment) of a foreign chat are never returned; a soft-deleted turn, message or attachment of the caller's own chat returns 404 with the child resource type (`message`, `turn` or `attachment`); `chat_vector_stores` is never read without `tenant_id`.
- [ ] Models API: `GET /v1/models` returns exactly the enabled catalog entries in catalog order; `GET /v1/models/{id}` returns 404 `not_found` with `gts.cf.core.mini_chat.model.v1~` for a disabled or unknown id; each item has exactly the keys `model_id`, `display_name`, `tier`, `multiplier_display`, `description` (absent when empty), `multimodal_capabilities`, `context_window`.
- [ ] Authorization (PEP): PDP deny returns 403 `AUTHZ_DENIED`; PDP failure returns 503 with `Retry-After: 5` and `context.retry_after_seconds = 5`; empty or missing constraints with `require_constraints` `true` return 403; an unknown predicate type or property makes its constraint false; model endpoints evaluate decision-only with `require_constraints` `false`; the create request carries `owner_tenant_id` and `owner_id`.
- [ ] Error mapping (REST): every domain error kind produces the category, status and machine-readable field of the mapping table, with no top-level `code` and no provider identifier or driver message in `detail`.
- [ ] License gate: a request from a tenant without `gts.cf.core.lic.feat.v1~cf.core.global.base.v1` gets 403 `LICENSE_FEATURE_REQUIRED` and an unauthenticated request gets 401 before any handler runs.
- [ ] Configuration: defaults and ranges of every section are applied; unknown keys fail in the strict sections and are ignored in the worker sections; `supports_file_search_filters`, `streaming.web_search_context_size` and `mcp` fail startup; deprecated fields log one warning each; `${VAR}` is expanded in provider and override `host` and `auth_config` and in `client_credentials`; a dangling `rag_provider` and an `azure` entry without `api_version` fail.
- [ ] Schema: migrations apply and re-apply cleanly on both engines; a second running turn per chat, a duplicate `(chat_id, request_id, role)` message, a reaction outside `like` / `dislike` and a cross-chat `message_attachments` row are rejected by the database; no MCP or `tenant_closure` table exists.
- [ ] Bundled plugins: the static policy plugin serves version 1 with the configured catalog and default tier limits, rejects multipliers outside `1..=10_000_000_000`, a zero `bytes_per_token_conservative`, unknown top-level keys and unknown kill-switch keys; the static audit plugin logs events only when `enabled`.
- [ ] OAGW provisioning: an unset `upstream_alias` becomes the host; a misconfigured entry or failed S2S exchange fails `start`; an unreadable secret is retried at 2, 4, 8 ... 60 s with one warning after 2 minutes; a failed or timed-out RAG route only logs a warning; the resolver returns the override alias for an overridden tenant.
- [ ] OAGW provisioning idempotency: provisioning on restart against an OAGW that already holds the upstream and routes, with a tenant override sharing its entry alias, and with two entries sharing a host succeeds; the upstream is created once per alias, existing routes (same method and path prefix) are not re-created, an already-exists / overlap Conflict on route creation is treated as success, no target fails `start`, and each target becomes `provisioned`; the reconcile retry of a deferred target applies the same rule.
- [ ] OAGW alias consistency: two targets (entries or tenant overrides) with the same alias but a different `auth_config` (or `host`, `port`, `use_http`, `auth_plugin_type`, `api_path`) fail `init` with an error naming the alias and both provider ids / tenant overrides and no `auth_config` value; targets sharing an alias with identical configuration pass and provision the upstream once.
- [ ] Bounded external calls: a never-answering dependency yields its bounded outcome and never hangs the caller: PDP evaluation beyond 5 s returns 503 with `Retry-After: 5`; an S2S exchange beyond 10 s fails `start` naming `client_id` and the cause `timeout`; an OAGW upstream or route call beyond 10 s defers the target to the reconcile task and `start` completes; `publish_usage` beyond 20 s returns `Retry` before the 30 s lease expires; audit `emit` beyond 30 s returns `Retry`; `get_current_policy_version`, `get_policy_snapshot` or `get_user_limits` beyond 10 s returns 500 `internal` on the Models API, logs an error and continues startup for the summary-model check, and yields `Retry` in handlers; a types-registry instance query beyond 10 s yields a resolution error (500 `internal` for the Models API, `Retry` for the outbox handlers) and caches nothing.
- [ ] Outbox: five queues run with the configured names, `num_partitions` and leases (thread summary `claim_timeout_secs`, audit 60 s, others 30 s); the usage handler returns `Reject` for a malformed payload or permanent error and `Retry` for resolution failure or a transient error; the audit handler rejects a malformed payload before plugin lookup, drops (`Ok`, `dropped`) when no plugin is registered, retries on transient errors, the 30 s timeout and a missing ClientHub client (resetting the cache), rejects on a permanent error and on the 120th retry attempt.
- [ ] Secrets in logs: with a sentinel value configured as `client_secret` and as an `auth_config` value, no log record, validation message or error `detail` produced by `init`, `start` (including a failed or deferred provisioning) or the handlers contains the sentinel; messages name only the key path.
- [ ] Quality gates: `make gear-ci GEAR=mini-chat`, `make dylint` and `cfs validate` pass, and every implemented CDSL ID of this feature has `@cpt-begin` / `@cpt-end` markers.
