---
description: "Specifies the Gear Foundation feature of the mini-chat gear: SDK and gear crate wiring, lifecycle, configuration, the P1 schema and outbox queues, the canonical error contract, the authorization PEP, the model-policy and audit plugins, the provider registry with OAGW provisioning, the license gate, observability and the public Models API."
---

# Feature: Gear Foundation


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Chat User Lists Visible Models](#chat-user-lists-visible-models)
  - [Chat User Gets One Model](#chat-user-gets-one-model)
  - [Chat User Request Admission](#chat-user-request-admission)
  - [Admin Observes Operational Signals](#admin-observes-operational-signals)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Gear Init](#gear-init)
  - [Gear Configuration Validation](#gear-configuration-validation)
  - [Gear Start](#gear-start)
  - [Gear Stop](#gear-stop)
  - [REST Route Registration and OpenAPI](#rest-route-registration-and-openapi)
  - [Schema Migration](#schema-migration)
  - [Scoped Persistence](#scoped-persistence)
  - [Outbox Queues](#outbox-queues)
  - [Canonical Error Mapping](#canonical-error-mapping)
  - [Authorization Operation Matrix](#authorization-operation-matrix)
  - [PEP Authorization](#pep-authorization)
  - [Policy Snapshot Access](#policy-snapshot-access)
  - [Model Catalog Rules](#model-catalog-rules)
  - [Model Visibility](#model-visibility)
  - [Static Model Policy Plugin](#static-model-policy-plugin)
  - [Audit Delivery](#audit-delivery)
  - [Static Audit Plugin](#static-audit-plugin)
  - [Provider Registry](#provider-registry)
  - [OAGW Provisioning With Deferred Retry](#oagw-provisioning-with-deferred-retry)
  - [OAGW Provisioning Reconcile Task](#oagw-provisioning-reconcile-task)
  - [License Gate](#license-gate)
  - [Observability: Metrics, Correlation and Logging](#observability-metrics-correlation-and-logging)
- [4. States (CDSL)](#4-states-cdsl)
  - [Gear Lifecycle State Machine](#gear-lifecycle-state-machine)
  - [Provider Provisioning State Machine](#provider-provisioning-state-machine)
  - [Audit Event Delivery State Machine](#audit-event-delivery-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [SDK Crate and Crate Wiring](#sdk-crate-and-crate-wiring)
  - [Gear Registration, Lifecycle and REST Surface](#gear-registration-lifecycle-and-rest-surface)
  - [Configuration Keys, Defaults and Validation](#configuration-keys-defaults-and-validation)
  - [P1 Database Schema and Migrations](#p1-database-schema-and-migrations)
  - [Secure-ORM Scoped Persistence Layer](#secure-orm-scoped-persistence-layer)
  - [Outbox Pipeline Queues](#outbox-pipeline-queues)
  - [Canonical Error Contract](#canonical-error-contract)
  - [Authorization PEP](#authorization-pep)
  - [Model Policy Gateway, Catalog Rules and Static Policy Plugin](#model-policy-gateway-catalog-rules-and-static-policy-plugin)
  - [Audit Gateway and Static Audit Plugin](#audit-gateway-and-static-audit-plugin)
  - [Provider Registry and OAGW Provisioning](#provider-registry-and-oagw-provisioning)
  - [Base-License Gate](#base-license-gate)
  - [Metrics, Correlation and Logging Conventions](#metrics-correlation-and-logging-conventions)
  - [Public Models API](#public-models-api)
  - [Deferred MCP Scope Stays Unimplemented](#deferred-mcp-scope-stays-unimplemented)
  - [Test Layering, Traceability and Feature-Local Deviations](#test-layering-traceability-and-feature-local-deviations)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-featstatus-gear-foundation-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-mini-chat-feature-gear-foundation`

<!--
=============================================================================
FEATURE SPECIFICATION: Gear Foundation (mini-chat)
Source entry: DECOMPOSITION section 2.1 (gears/mini-chat/docs/DECOMPOSITION.md)
Precedence: gears/mini-chat/docs/ (PRD, DESIGN, ADR, acceptance-criteria) wins over
config/mini-chat.yaml and over platform documents under docs/ wherever they disagree.
=============================================================================
-->
## 1. Feature Context

### 1.1 Overview

The Gear Foundation feature builds the parts of the `mini-chat` gear that every other feature relies on: the SDK crate and the gear crate wiring, gear registration and lifecycle, configuration, the full P1 database schema with its outbox queues, the canonical error contract, owner and tenant scoped authorization, the model-policy and audit plugins, the provider registry with OAGW provisioning, the base-license gate, observability conventions and the read-only Models API.

### 1.2 Purpose

Without this feature no other Mini Chat feature can be implemented or tested over HTTP. It gives the later features one registered gear, one validated configuration, one schema, one error shape, one authorization path and one place to read the model catalog and write audit events. It also realizes the Models API functional driver of the DESIGN (the DESIGN drivers table names that requirement, and the PRD defines no separate ID for it, so it is traced through `cpt-cf-mini-chat-interface-models-api`).

Scope boundary: this feature owns the foundations only. Chat CRUD, quota arithmetic, the streaming pipeline, attachments, turn mutations, reactions, background work handlers and the per-provider adapters are owned by the other six features of the DECOMPOSITION. This feature provides the registration points, tables, queues, contracts and shared behavior they build on.

**Requirements**: `cpt-cf-mini-chat-fr-license-gate`, `cpt-cf-mini-chat-fr-audit`, `cpt-cf-mini-chat-nfr-tenant-isolation`, `cpt-cf-mini-chat-nfr-authz-alignment`, `cpt-cf-mini-chat-nfr-observability-supportability`

**Requirements tracked as deferred, not implemented in P1 (ADR-0006)**: `cpt-cf-mini-chat-fr-mcp-server-registry`, `cpt-cf-mini-chat-fr-mcp-hub-discovery`, `cpt-cf-mini-chat-fr-mcp-role-access`, `cpt-cf-mini-chat-fr-mcp-tool-discovery`, `cpt-cf-mini-chat-fr-mcp-tool-execution`

**Principles**: `cpt-cf-mini-chat-principle-tenant-scoped`, `cpt-cf-mini-chat-principle-owner-only-content`

**Constraints**: `cpt-cf-mini-chat-constraint-openai-compatible` (relaxed by ADR-0005 to adapter-based providers), `cpt-cf-mini-chat-constraint-no-credentials`, `cpt-cf-mini-chat-constraint-license-gate`

**Design elements**: `cpt-cf-mini-chat-component-chat-service`, `cpt-cf-mini-chat-component-chat-store`, `cpt-cf-mini-chat-component-authz-integration`, `cpt-cf-mini-chat-design-authz-pep`, `cpt-cf-mini-chat-design-model-catalog`, `cpt-cf-mini-chat-interface-public-api`, `cpt-cf-mini-chat-interface-models-api`

**Design elements tracked as deferred, not implemented in P1 (ADR-0006)**: `cpt-cf-mini-chat-component-mcp-pool`, `cpt-cf-mini-chat-component-mcp-service`, `cpt-cf-mini-chat-design-mcp-servers`, `cpt-cf-mini-chat-interface-mcp-api`, `cpt-cf-mini-chat-dbtable-mcp-servers`, `cpt-cf-mini-chat-dbtable-mcp-server-tools`, `cpt-cf-mini-chat-dbtable-role-mcp-servers`

**Database tables created by this feature**: `cpt-cf-mini-chat-dbtable-chats`, `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-attachments`, `cpt-cf-mini-chat-dbtable-message-attachments`, `cpt-cf-mini-chat-dbtable-thread-summaries`, `cpt-cf-mini-chat-dbtable-chat-vector-stores`, `cpt-cf-mini-chat-dbtable-quota-usage`, `cpt-cf-mini-chat-dbtable-message-reactions`. The projection `cpt-cf-mini-chat-dbtable-tenant-closure-ref` is not required for chat content access and no local table is created for it.

**ADRs traced**: `cpt-cf-mini-chat-adr-canonical-error-contract`, `cpt-cf-mini-chat-adr-multi-provider-adapters`, `cpt-cf-mini-chat-adr-mcp-deferred`, `cpt-cf-mini-chat-adr-quota-policy-scope` (policy and licensing parts), `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope` (audit transport part), `cpt-cf-mini-chat-adr-runtime-consistency-limitations` (configuration validation and CHECK constraint limits)

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-mini-chat-actor-chat-user` | Calls the public REST API under the gear URL prefix. Every call passes the license gate and the authorization PEP, and every resource is scoped to the calling user and tenant. Lists and reads models through the Models API. |
| `cpt-cf-mini-chat-actor-admin` | Operates the deployment and reads aggregated usage and operational metrics, audit events and configuration only. Never has access to chat content, because content is owner-only. |
| `cpt-cf-mini-chat-actor-cleanup-scheduler` | System actor served by the outbox queues and the attachment `cleanup_status` columns created here. Its behavior is exercised in `cpt-cf-mini-chat-feature-background-maintenance`. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) section 2.1 (`Gear Foundation`)
- **Acceptance criteria**: [acceptance-criteria.md](../acceptance-criteria.md)
- **ADRs (all accepted)**: [ADR-0004 canonical error contract](../ADR/0004-cpt-cf-mini-chat-adr-canonical-error-contract.md), [ADR-0005 multi-provider adapters](../ADR/0005-cpt-cf-mini-chat-adr-multi-provider-adapters.md), [ADR-0006 MCP deferred](../ADR/0006-cpt-cf-mini-chat-adr-mcp-deferred.md), [ADR-0008 quota and policy scope](../ADR/0008-cpt-cf-mini-chat-adr-quota-policy-scope.md), [ADR-0009 data lifecycle and audit scope](../ADR/0009-cpt-cf-mini-chat-adr-data-lifecycle-audit-scope.md), [ADR-0010 runtime and consistency limitations](../ADR/0010-cpt-cf-mini-chat-adr-runtime-consistency-limitations.md)
- **Workspace conventions** (constraints only): [ToolKit README](../../../../docs/toolkit_unified_system/README.md), [gear layout and SDK pattern](../../../../docs/toolkit_unified_system/02_gear_layout_and_sdk_pattern.md)
- **Dependencies**: None. This feature is the first in the dependency order. `cpt-cf-mini-chat-feature-chat-management` and `cpt-cf-mini-chat-feature-quota-and-billing` depend on it directly, and the others depend on it transitively.

**Normative value sources** (the DESIGN holds the exact values; this document states the values it must preserve and names the DESIGN heading that carries the rest):

| Topic | DESIGN location (section and heading) |
|-------|---------------------------------------|
| Gear lifecycle, OAGW provisioning, audit plugin, model policy gateway | 3.2 Component Model, `Components` and `Gear lifecycle` |
| Endpoint overview, URL prefix, REST error mapping, streaming error codes | 3.3 API Contracts, `Endpoints Overview`, `Error Codes`, `Streaming error codes` |
| Models API response fields, visibility, non-exposure | 3.3 API Contracts, `Models API`, `Visibility Algorithm (Normative)`, `Non-Exposure Rules (Models API)` |
| Provider provisioning table | 3.5 External Dependencies, `LLM Provider (OpenAI / Azure OpenAI)` |
| Table columns, constraints, indexes | 3.7 Database Schemas & Tables, one heading per table |
| Authorization matrix, fail-closed rules, token scopes | 3.8 Authorization (PEP), `Per-Operation Authorization Matrix`, `Fail-Closed Behavior`, `Token Scopes` |
| Model catalog entry fields and rules | 4 Additional Context, `Model Catalog Configuration` |
| Metrics, correlation, logging | 4 Additional Context, `Metrics (OTLP) and Alerts (P1)` and `Operational Traceability and Debugging (P1)` |
| Policy plugin contract | 5.2 Policy Plugin, `5.2.1 Policy Snapshot Concept`, `5.2.2 Plugin Contract` |
| Outbox integration | 5.6 Reliable Usage Event Publication, and Appendix B `B.9.3 Shared outbox integration` |
| Configuration keys, defaults, ranges | Appendix B, `B.1 Gear config`, `B.4` to `B.9` |

## 2. Actor Flows (CDSL)

User-facing and operator-facing interactions that start with an actor and describe the end-to-end behavior of the foundation.

**Use cases**: none are implemented by this feature. The MCP use cases `cpt-cf-mini-chat-usecase-mcp-tool-execution`, `cpt-cf-mini-chat-usecase-assign-mcp-server-role` and `cpt-cf-mini-chat-usecase-revoke-mcp-server-role` are deferred and not implemented in P1 (ADR-0006).

### Chat User Lists Visible Models

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-gear-foundation-list-models`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- The user receives HTTP 200 with an `items` array that holds exactly the globally enabled catalog entries, each projected to the seven public fields. An empty catalog yields an empty `items` array.

**Error Scenarios**:
- The bearer token is missing, invalid or expired: 401 `unauthenticated`.
- The tenant lacks the base license feature: 403 `permission_denied` with `LICENSE_FEATURE_REQUIRED`.
- The PDP denies the request: 403 `permission_denied` with `AUTHZ_DENIED`.
- The PDP cannot evaluate the request: 503 `service_unavailable` with `Retry-After: 5`.
- The model-policy plugin cannot be resolved or fails: 500 `internal`.

**Steps**:
1. [x] - `p1` - Chat User sends `GET /v1/models` with a bearer token to the gear URL prefix (`url_prefix`, default `/mini-chat`) - `inst-list-models-request`
2. [x] - `p1` - Run the request admission flow for the operation `GET /v1/models`, which authenticates the caller, applies the license gate and evaluates authorization for the `Model` resource - `inst-list-models-admission`
3. [x] - `p1` - **IF** admission returned a refusal - `inst-list-models-refused`
   1. [x] - `p1` - **RETURN** the refusal Problem unchanged (401, 403 or 503) and do not read the catalog - `inst-list-models-return-refusal`
4. [x] - `p1` - Read the policy snapshot through the model-policy gateway (policy snapshot access algorithm) - `inst-list-models-read-snapshot`
5. [x] - `p1` - **IF** the snapshot could not be read - `inst-list-models-snapshot-failed`
   1. [x] - `p1` - **RETURN** 500 `internal` with a generic detail and log the cause - `inst-list-models-return-500`
6. [x] - `p1` - Compute the visible models and their public projections (model visibility algorithm) - `inst-list-models-project`
7. [x] - `p1` - **RETURN** 200 with the `items` array, in catalog order (catalog order is not part of the public contract) - `inst-list-models-return`

### Chat User Gets One Model

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-gear-foundation-get-model`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- The user receives HTTP 200 with a single model object of the same shape as one list item.

**Error Scenarios**:
- The model does not exist or is globally disabled: 404 `not_found` with `context.resource_type` equal to `gts.cf.core.mini_chat.model.v1~`. The two cases are indistinguishable, so catalog details do not leak.
- 401, 403 and 503 admission refusals and the 500 plugin failure behave as in the list flow.

**Steps**:
1. [x] - `p1` - Chat User sends `GET /v1/models/{id}` where `{id}` is the opaque `model_id` string (not required to be a UUID, so no path-UUID validation applies to it) - `inst-get-model-request`
2. [x] - `p1` - Run the request admission flow for the operation `GET /v1/models/{id}` (authorization with action `read`, no `resource.id`, `require_constraints` false) - `inst-get-model-admission`
3. [x] - `p1` - **IF** admission returned a refusal - `inst-get-model-refused`
   1. [x] - `p1` - **RETURN** the refusal Problem unchanged - `inst-get-model-return-refusal`
4. [x] - `p1` - Read the policy snapshot through the model-policy gateway - `inst-get-model-read-snapshot`
5. [x] - `p1` - **IF** the snapshot could not be read - `inst-get-model-snapshot-failed`
   1. [x] - `p1` - **RETURN** 500 `internal` - `inst-get-model-return-500`
6. [x] - `p1` - Resolve the requested `id` against the visible models (model visibility algorithm) - `inst-get-model-resolve`
7. [x] - `p1` - **IF** no visible model has that `model_id` - `inst-get-model-not-visible`
   1. [x] - `p1` - **RETURN** 404 `not_found` with `context.resource_type` equal to `gts.cf.core.mini_chat.model.v1~` - `inst-get-model-return-404`
8. [x] - `p1` - **RETURN** 200 with the single public projection - `inst-get-model-return`

### Chat User Request Admission

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-gear-foundation-request-admission`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- An authenticated, licensed caller is authorized and the handler receives an `AccessScope` limited to the caller's tenant and user (or a permission-only decision for the Models API).
- A chat-addressed operation (its path carries a chat id) resolves its chat through an owner-scoped query and then reads child rows only through that chat.

**Error Scenarios**:
- 401 `unauthenticated` with `context.reason` `MISSING_BEARER` or `AUTHN_FAILED` (api-gateway).
- 403 `permission_denied` with `LICENSE_FEATURE_REQUIRED` (api-gateway license middleware).
- 403 `permission_denied` with `AUTHZ_DENIED` for a PDP deny, for a decision without usable constraints, and for a constraint compile failure.
- 503 `service_unavailable` with `Retry-After: 5` when the PDP cannot evaluate (still fail-closed).
- 404 `not_found` when the scoped chat query returns no row (missing, soft-deleted, another user's or another tenant's chat). A foreign resource is never reported as 403.

**Steps**:
1. [x] - `p1` - Chat User calls any route registered under `url_prefix` - `inst-admission-request`
2. [x] - `p1` - api-gateway authentication middleware extracts `user_id` and `tenant_id` from the token - `inst-admission-authn`
3. [x] - `p1` - **IF** the token is missing, invalid or expired - `inst-admission-unauthenticated`
   1. [x] - `p1` - **RETURN** 401 `unauthenticated` - `inst-admission-return-401`
4. [x] - `p1` - Apply the license gate (license gate algorithm), which runs before any handler and before any PDP call - `inst-admission-license`
5. [x] - `p1` - **IF** the tenant lacks the required license feature - `inst-admission-unlicensed`
   1. [x] - `p1` - **RETURN** 403 `permission_denied` with `LICENSE_FEATURE_REQUIRED` - `inst-admission-return-license-403`
6. [x] - `p1` - Resolve the operation to its action, resource type and `require_constraints` flag (authorization operation matrix algorithm) - `inst-admission-resolve-operation`
7. [x] - `p1` - Evaluate authorization (PEP authorization algorithm) - `inst-admission-authorize`
8. [x] - `p1` - **IF** evaluation returned a refusal (403 or 503) - `inst-admission-authz-refused`
   1. [x] - `p1` - **RETURN** the refusal Problem - `inst-admission-return-refusal`
9. [x] - `p1` - **IF** the operation is chat-addressed, meaning its path carries a chat id (this is separate from the owner predicate of the PEP authorization algorithm, which applies to every Chat and UserQuota operation) - `inst-admission-chat-scoped`
   1. [x] - `p1` - Load the chat through the scoped persistence algorithm using the returned `AccessScope` and the chat id from the path - `inst-admission-load-chat`
   2. [x] - `p1` - **IF** the scoped query returned no row - `inst-admission-chat-missing`
      1. [x] - `p1` - **RETURN** 404 `not_found` with `context.resource_type` equal to `gts.cf.core.mini_chat.chat.v1~` - `inst-admission-return-404`
10. [x] - `p1` - **RETURN** the `AccessScope` (and the loaded chat) to the owning handler - `inst-admission-return-scope`

### Admin Observes Operational Signals

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-flow-gear-foundation-operator-observability`

**Actor**: `cpt-cf-mini-chat-actor-admin`

**Success Scenarios**:
- The admin reads `mini_chat_*` metrics exported over OTLP and audit events delivered to the audit plugin, and can follow one request through its identifiers, without opening any chat content.

**Error Scenarios**:
- The admin tries to read another user's chat through the public API: the scoped query returns no row and the call ends in 404.
- No audit plugin is registered: events are acknowledged and dropped and the drop is visible in `mini_chat_audit_emit` with `result` equal to `dropped`.

**Steps**:
1. [x] - `p1` - Admin collects the gear metrics from the OTLP pipeline (the process exposes no Prometheus endpoint) - `inst-observe-collect-metrics`
2. [ ] - `p1` - Admin filters series by the allowed low-cardinality labels only (`provider`, `model`, `provider_kind`, `resource_type`, `kind`, `tier`, `state`, `stage`, `op`, `reason`, `error_code`, `decision`, `period`, `trigger`, `result`, `from_status`) - `inst-observe-filter-labels`
3. [x] - `p1` - Admin starts from `request_id`, `provider_response_id` or `trace_id` and correlates logs, traces and audit events (observability algorithm) - `inst-observe-correlate`
4. [x] - `p1` - Admin checks audit delivery health through `mini_chat_audit_emit` for the results `ok`, `retry`, `reject` and `dropped` - `inst-observe-audit-health`
5. [ ] - `p1` - **IF** the admin needs conversation content - `inst-observe-content-request`
   1. [ ] - `p1` - **RETURN** no content: the PEP scope is owner-only and no operator endpoint, metric label or audit field carries prompts, responses or attachments - `inst-observe-no-content`
6. [ ] - `p1` - **RETURN** the operational view assembled from metrics, correlation identifiers and audit events - `inst-observe-return`

## 3. Processes / Business Logic (CDSL)

Internal functions of the foundation. The flows above and the other six features call them.

### Gear Init

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-gear-init`

**Input**: the `gears.mini-chat.config` section, the ClientHub, the gear database handle

**Output**: an initialized gear (validated configuration, gateways, clients, services, migrations) or a startup error

**Steps**:
1. [x] - `p1` - Load the gear configuration with explicit config loading that requires the gear config section, applying `${VAR}` expansion only to the fields that allow it (provider `host` and `auth_config`, the same two fields in `tenant_overrides.<tenant_id>`, and `client_credentials`) - `inst-init-load-config`
2. [x] - `p1` - Validate every section (configuration validation algorithm) - `inst-init-validate-config`
3. [x] - `p1` - **IF** validation failed - `inst-init-validation-failed`
   1. [x] - `p1` - **RETURN** a startup error that names the offending key or section - `inst-init-return-validation-error`
4. [x] - `p1` - Build the provider registry from the validated provider entries (provider registry algorithm), which fills `upstream_alias` with the host when it is not configured - `inst-init-build-registry`
5. [x] - `p1` - Create the model-policy gateway and the audit gateway; the plugins are resolved lazily through types-registry, not at init - `inst-init-create-gateways`
6. [x] - `p1` - Resolve the `authz_resolver`, `oagw` and `authn_resolver` clients from the ClientHub - `inst-init-resolve-clients`
7. [x] - `p1` - **IF** a required client cannot be resolved - `inst-init-client-missing`
   1. [x] - `p1` - **RETURN** a startup error naming the missing dependency - `inst-init-return-client-error`
8. [x] - `p1` - Build the metrics instruments (observability algorithm) and the outbox enqueuer; the outbox pipeline is not started yet - `inst-init-build-metrics-outbox`
9. [ ] - `p1` - Build the wiring points that later features fill: the per-provider file and vector-store dispatch by `storage_kind`, the knowledge retriever (only when `knowledge_search.enabled` is true) and the Anthropic Files client (only when at least one entry has kind `anthropic_messages`) - `inst-init-build-wiring-points`
10. [x] - `p1` - Build the domain services, including the policy enforcement point (`cpt-cf-mini-chat-component-chat-service`) - `inst-init-build-services`
11. [x] - `p1` - Declare the gear migrations, which include the platform outbox migrations (schema migration algorithm) - `inst-init-declare-migrations`
12. [x] - `p1` - Keep the services for the separate REST registration step, which registers routes from the services built here (route registration algorithm) - `inst-init-keep-services`
13. [x] - `p1` - **RETURN** the initialized gear - `inst-init-return`

### Gear Configuration Validation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-config-validation`

**Input**: the raw gear configuration after `${VAR}` expansion

**Output**: the typed configuration with defaults applied, or a startup error. The exact keys, types, defaults and ranges are in DESIGN Appendix B: `B.1 Gear config (ToolKit config)`, `B.4 Streaming (SSE)`, `B.5 Token budgets / quota knobs`, `B.6 Web search configuration`, `B.6.1 Code interpreter configuration`, `B.7 File search / RAG configuration`, `B.8 Uploads / Attachments / Images`, `B.9 Background workers`.

**Steps**:
1. [x] - `p1` - Reject unknown keys at the top level and inside each of these sections: `streaming`, `estimation_budgets`, `quota`, `outbox`, `context`, `rag`, `client_credentials`, `metrics`, `providers.<id>` with its `tenant_overrides`, `thumbnail`, `knowledge_search`; an unknown or misspelled key there fails startup (ADR-0010) - `inst-config-reject-unknown`
2. [x] - `p1` - Accept and ignore unknown keys inside the worker sections `orphan_watchdog`, `upload_reaper`, `thread_summary_worker` and `cleanup_worker` - `inst-config-ignore-worker-unknown`
3. [x] - `p1` - Fail startup for a config that still sets a removed key: `providers.<id>.supports_file_search_filters`, `streaming.web_search_context_size` or any `mcp` key (the unknown-key rule applies to all three) - `inst-config-removed-keys`
4. [x] - `p1` - Apply the defaults and validate the top-level keys: `url_prefix` default `/mini-chat`; `vendor` default `constructorfabric` and non-empty; `client_credentials.client_id` and `client_credentials.client_secret` required and non-empty, with the secret redacted in logs; `metrics.prefix` default empty, which means the prefix `mini_chat` - `inst-config-top-level`
5. [x] - `p1` - Validate `streaming`: `sse_ping_interval_seconds` default `15` within `5..=60`; `sse_channel_capacity` default `32` within `16..=64`; `max_output_tokens` default `32768` - `inst-config-streaming`
6. [x] - `p1` - Validate `estimation_budgets.minimal_generation_floor`: default `50`, greater than 0 and not greater than `streaming.max_output_tokens` - `inst-config-estimation-floor`
7. [x] - `p1` - Parse the other `estimation_budgets` fields (`bytes_per_token_conservative`, `fixed_overhead_tokens`, `safety_margin_pct`, `image_token_budget`, `tool_surcharge_tokens`, `web_search_surcharge_tokens`, `code_interpreter_surcharge_tokens`) without validating them, and log one warning per field set to a non-default value - `inst-config-estimation-deprecated`
8. [x] - `p1` - Validate `quota`: `overshoot_tolerance_factor` default `1.10` within `1.0..=1.5`; `warning_threshold_pct` default `80` within `1..=99`; `web_search_max_calls_per_message` default `2`, `web_search_daily_quota` default `75`, `code_interpreter_max_calls_per_message` default `10`, `code_interpreter_daily_quota` default `50`, each greater than 0 - `inst-config-quota`
9. [x] - `p1` - Validate `outbox`: queue names `queue_name` default `mini-chat.usage_snapshot`, `cleanup_queue_name` default `mini-chat.attachment_cleanup`, `chat_cleanup_queue_name` default `mini-chat.chat_cleanup`, `thread_summary_queue_name` default `mini-chat.thread_summary`, `audit_queue_name` default `mini-chat.audit`, each non-empty; `num_partitions` default `4`, a power of 2 within `1..=64` - `inst-config-outbox`
10. [x] - `p1` - Validate `context.recent_messages_limit` default `10` within `0..=100`, and accept `context.web_search_guard` and `context.file_search_guard` with built-in default texts - `inst-config-context`
11. [x] - `p1` - Validate `rag`: `max_documents_per_chat` default `50`, `max_total_upload_mb_per_chat` default `100`, `uploaded_file_max_size_kb` default `25600`, `uploaded_image_max_size_kb` default `5120`, `max_images_per_message` default `4`, each greater than 0; `allow_csv_upload` default `true`; `max_concurrent_uploads` default `10` within `1..=256` - `inst-config-rag`
12. [x] - `p1` - Validate `thumbnail`: `width` default `128`, `height` default `128`, `max_bytes` default `131072`, `max_pixels` default `100000000`, `max_decode_bytes` default `33554432`, each greater than 0 - `inst-config-thumbnail`
13. [x] - `p1` - Validate `knowledge_search`: `enabled` default `false`; when enabled, `vector_store_id` and `provider_id` are required; `max_calls_per_message` default `3`, `top_k` default `5`, `max_chunk_chars` default `2000`, each greater than 0; `guard` has a built-in default text - `inst-config-knowledge-search`
14. [x] - `p1` - Validate the worker sections: `orphan_watchdog.enabled` default `true`, `timeout_secs` default `300` within `90..=3600`, `scan_interval_secs` default `60` within `1..=3600`; `upload_reaper.enabled` default `true`, `scan_interval_secs` default `60` within `1..=3600`, `stale_after_secs` default `300` within `60..=86400`; `thread_summary_worker.enabled` default `true`, `claim_timeout_secs` default `300` within `30..=3600`, `max_attempts` default `3` greater than 0, `compression_threshold_pct` default `80` within `1..=99`, `message_content_limit` default `4000`; `cleanup_worker.max_attempts` default `5` greater than 0 - `inst-config-workers`
15. [x] - `p1` - Log one warning per field set to a non-default value for the deprecated no-effect fields `cleanup_worker.enabled`, `cleanup_worker.poll_interval_secs`, `cleanup_worker.reconcile_interval_secs`, `cleanup_worker.stale_in_progress_timeout_secs`, `cleanup_worker.batch_size` and `thread_summary_worker.reconcile_interval_secs` (parsed, not validated, no effect; ADR-0010) - `inst-config-deprecated-warnings`
16. [x] - `p1` - **FOR EACH** provider entry in `providers` - `inst-config-each-provider`
   1. [x] - `p1` - Require `kind` (one of `openai_responses`, `openai_chat_completions`, `vllm_responses`, `anthropic_messages`), a non-empty `host` and `storage_kind` (`openai` or `azure`) - `inst-config-provider-required`
   2. [x] - `p1` - After `${VAR}` expansion, allow only letters, digits and the characters `.`, `-`, `_`, `:`, `[`, `]` in `host`, because the host becomes the OAGW alias in the proxied path - `inst-config-provider-host-chars`
   3. [x] - `p1` - Reject `port` equal to 0; the default is `443`, or `80` when `use_http` is true - `inst-config-provider-port`
   4. [x] - `p1` - **IF** `storage_kind` is `azure` - `inst-config-provider-azure`
      1. [x] - `p1` - Require a non-blank `api_version` containing only letters, digits, `.` and `-`, because it is sent unencoded as the `api-version` query value - `inst-config-provider-api-version`
   5. [x] - `p1` - **IF** `rag_provider` is set and names no existing provider entry - `inst-config-provider-rag-ref`
      1. [x] - `p1` - **RETURN** a startup error naming the dangling reference - `inst-config-provider-rag-error`
   6. [x] - `p1` - **FOR EACH** tenant override of the entry, require `host` or `upstream_alias` and apply the same host character check to an override `host` - `inst-config-provider-override`
17. [x] - `p1` - **RETURN** the typed configuration with all defaults applied - `inst-config-return`

### Gear Start

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-gear-start`

**Input**: the initialized gear and its cancellation token

**Output**: a started gear, or a startup error

**Steps**:
1. [x] - `p1` - **IF** a leader-only worker (orphan watchdog or upload reaper) is enabled - `inst-start-leader-needed`
   1. [x] - `p1` - Prepare the leader elector: a gear-local Kubernetes Lease elector when the gear is built with the `k8s` feature, otherwise a no-op elector that always reports leadership - `inst-start-leader-elector`
2. [x] - `p1` - Exchange `client_credentials` for an S2S security context through `authn_resolver` and register the OAGW upstreams and routes (OAGW provisioning algorithm, a synchronous first pass that returns the deferred set) - `inst-start-provisioning`
3. [x] - `p1` - **IF** provisioning reported a deterministic misconfiguration - `inst-start-provisioning-failed`
   1. [x] - `p1` - **RETURN** a startup error and do not start the outbox pipeline - `inst-start-return-provisioning-error`
4. [x] - `p1` - **IF** the first pass returned deferred entries - `inst-start-deferred-pending`
   1. [x] - `p1` - Spawn the OAGW provisioning reconcile task with the deferred set and the gear cancellation token, and continue to the outbox step right away without waiting for it - `inst-start-spawn-reconcile`
5. [x] - `p1` - Start the outbox pipeline with the five queues (outbox queues algorithm) - `inst-start-outbox`
6. [x] - `p1` - Check that `thread_summary_worker.summary_model_id` names an enabled catalog entry; a missing or disabled model is logged as an error and startup continues - `inst-start-summary-model-check`
7. [x] - `p1` - Spawn the orphan watchdog and the upload reaper when enabled, after the outbox pipeline is running because the reaper enqueues attachment cleanup events; the behavior of both workers belongs to `cpt-cf-mini-chat-feature-background-maintenance` - `inst-start-spawn-workers`
8. [x] - `p1` - **RETURN** a started gear - `inst-start-return`

### Gear Stop

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-gear-stop`

**Input**: the stop signal of the runtime

**Output**: a stopped gear

**Steps**:
1. [x] - `p1` - Cancel the background workers and the OAGW provisioning reconcile task through the cancellation token - `inst-stop-cancel-workers`
2. [x] - `p1` - Join the workers and the OAGW provisioning reconcile task with a bounded timeout - `inst-stop-join-workers`
3. [x] - `p1` - Stop the outbox pipeline, or give up on it when the framework deadline fires - `inst-stop-outbox`
4. [x] - `p1` - **RETURN** a stopped gear - `inst-stop-return`

### REST Route Registration and OpenAPI

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-route-registration`

**Input**: the services built at init and the platform route registry

**Output**: routes and an OpenAPI declaration under the gear URL prefix

**Steps**:
1. [x] - `p1` - Register every route under `url_prefix` (default `/mini-chat`) with the platform operation builder, as an authenticated operation with the standard errors declared - `inst-routes-prefix`
2. [x] - `p1` - Declare the base license feature on every route (license gate algorithm) - `inst-routes-license`
3. [x] - `p1` - Register `GET /v1/models` and `GET /v1/models/{id}` as the endpoints owned by this feature, with the response schema of the Models API and the Problem error responses - `inst-routes-models`
4. [x] - `p1` - Provide the shared registration surface for the endpoints of the other features, so each owning feature adds its own endpoints from the DESIGN endpoint overview (`Endpoints Overview` in 3.3) to this same registration step - `inst-routes-shared-surface`
5. [ ] - `p1` - Do not register the MCP endpoints (`/v1/mcp-servers*`, `/v1/admin/roles/*/mcp-servers*`, `/v1/chats/{id}/mcp-tools/effective`, `/v1/mcp-connections:complete`) or `POST /v1/chats/{id}:temporary`; they are deferred and not implemented in P1 - `inst-routes-deferred`
6. [x] - `p1` - Declare the routes in the OpenAPI document served by api-gateway, so the generated `docs/api/api.json` at the repository root stays the reference for request and response schemas; add no endpoint beyond the DESIGN overview - `inst-routes-openapi`
7. [x] - `p1` - **RETURN** the registered router - `inst-routes-return`

### Schema Migration

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-schema-migration`

**Input**: the gear database (PostgreSQL or SQLite)

**Output**: the full P1 schema and the shared outbox tables. Columns, types, defaults and descriptions are normative in DESIGN 3.7 (`Database Schemas & Tables`), one heading per table, and are not restated here beyond the constraints and indexes below.

**Steps**:
1. [x] - `p1` - Provide migrations that the runtime executes (the gear never opens a raw connection), each runnable on PostgreSQL and SQLite, with a PostgreSQL variant and a SQLite variant only where the dialects differ - `inst-schema-dialects`
2. [x] - `p1` - Include the platform outbox migrations in the gear migration list, so the outbox tables exist before the pipeline starts - `inst-schema-outbox-tables`
3. [x] - `p1` - Declare UUID columns as `TEXT` in SQLite and write their values as 16-byte `BLOB` values (the UUID bytes, not the text form) - `inst-schema-uuid-blob`
4. [x] - `p1` - Create `chats` (`cpt-cf-mini-chat-dbtable-chats`): primary key `id`; NOT NULL `tenant_id`, `user_id`, `created_at`, `updated_at`; soft delete through `deleted_at`; partial index on `(tenant_id, user_id, updated_at DESC)` where `deleted_at IS NULL` - `inst-schema-chats`
5. [x] - `p1` - Create `messages` (`cpt-cf-mini-chat-dbtable-messages`): primary key `id`; NOT NULL `tenant_id`, `chat_id`, `role`, `content`, `content_type`, `created_at`; foreign key `chat_id` to `chats.id` with ON DELETE CASCADE; token and cache columns NOT NULL or defaulting to 0 and `features_used` defaulting to an empty array as DESIGN lists; UNIQUE `(chat_id, request_id, role)` where `request_id IS NOT NULL AND deleted_at IS NULL`; index `(chat_id, created_at)` where `deleted_at IS NULL`; UNIQUE `(id, chat_id)` to back the composite foreign keys of `message_attachments` - `inst-schema-messages`
6. [x] - `p1` - Create `chat_turns` (`cpt-cf-mini-chat-dbtable-chat-turns`): primary key `id`; UNIQUE `(chat_id, request_id)`; foreign key `chat_id` to `chats.id` with ON DELETE CASCADE; enforced CHECKs `requester_type IN ('user', 'system')` and `state IN ('running', 'completed', 'failed', 'cancelled')`; the nullable preflight columns (`reserve_tokens`, `max_output_tokens_applied`, `reserved_credits_micro`, `policy_version_applied`, `effective_model`, `minimal_generation_floor_applied`); `started_at` set by the application clock with no database default; nullable `last_progress_at`; counters `web_search_completed_count`, `code_interpreter_completed_count`, `file_search_completed_count` NOT NULL default 0 and `web_search_enabled` NOT NULL default false; indexes `(chat_id, started_at DESC)` where `deleted_at IS NULL`, UNIQUE `(chat_id)` where `state = 'running' AND deleted_at IS NULL`, and `(last_progress_at)` where `state = 'running' AND deleted_at IS NULL` - `inst-schema-chat-turns`
7. [x] - `p1` - Create `attachments` (`cpt-cf-mini-chat-dbtable-attachments`): primary key `id`; NOT NULL `tenant_id`, `chat_id`, `uploaded_by_user_id`, `filename`, `status`, `attachment_kind`, `storage_backend`, `created_at`; foreign key `chat_id` to `chats.id` with ON DELETE CASCADE; CHECKs `attachment_kind IN ('document', 'image')`, `status IN ('pending', 'uploaded', 'ready', 'failed')` and CHECKs on `secondary_status` and `secondary_provider_kind`; `storage_backend` default `azure`; `secondary_status` default `not_attempted`; indexes `(tenant_id, chat_id)` where `deleted_at IS NULL`, `(cleanup_status)` where `cleanup_status IS NOT NULL AND deleted_at IS NULL`, the plain index `idx_attachments_stale_upload` on `(status, cleanup_status, deleted_at, updated_at)`, and UNIQUE `(id, chat_id)` - `inst-schema-attachments`
8. [x] - `p1` - Create `message_attachments` (`cpt-cf-mini-chat-dbtable-message-attachments`): composite primary key `(chat_id, message_id, attachment_id)`; composite foreign keys `(message_id, chat_id)` to `messages(id, chat_id)` and `(attachment_id, chat_id)` to `attachments(id, chat_id)`, both ON DELETE CASCADE; indexes `(tenant_id, chat_id)` and `(attachment_id, chat_id)` - `inst-schema-message-attachments`
9. [x] - `p1` - Create `thread_summaries` (`cpt-cf-mini-chat-dbtable-thread-summaries`): primary key `id`; UNIQUE `chat_id` with a foreign key to `chats.id` ON DELETE CASCADE; NOT NULL `summarized_up_to_created_at`, `summarized_up_to_message_id`, `created_at`, `updated_at` - `inst-schema-thread-summaries`
10. [x] - `p1` - Create `chat_vector_stores` (`cpt-cf-mini-chat-dbtable-chat-vector-stores`): primary key `id`; UNIQUE `(tenant_id, chat_id)`; NOT NULL `provider` and `created_at`; nullable `vector_store_id` (NULL means provider creation in progress); `file_count` NOT NULL with CHECK `file_count >= 0` - `inst-schema-chat-vector-stores`
11. [x] - `p1` - Create `quota_usage` (`cpt-cf-mini-chat-dbtable-quota-usage`, `cpt-cf-mini-chat-design-quota-usage-accounting`): primary key `id`; UNIQUE `(tenant_id, user_id, period_type, period_start, bucket)` and a lookup index on the same columns; `period_start` as a DATE; counters defaulting to 0 as DESIGN lists; the columns `file_search_calls`, `image_inputs` and `image_upload_bytes` exist and stay unpopulated (ADR-0007, ADR-0008) - `inst-schema-quota-usage`
12. [x] - `p1` - Create `message_reactions` (`cpt-cf-mini-chat-dbtable-message-reactions`): primary key `id`; UNIQUE `(message_id, user_id)`; NOT NULL `message_id`, `user_id`, `tenant_id`, `reaction`, `created_at`; foreign key `message_id` to `messages(id)` with ON DELETE CASCADE and no `chat_id` column; CHECK `reaction IN ('like', 'dislike')` - `inst-schema-message-reactions`
13. [x] - `p1` - Keep the cross-column CHECKs on `chat_turns` (`completed_at` and `last_progress_at` against `state`) and the value CHECK on `attachments.cleanup_status` out of the database; the repositories keep those invariants (ADR-0010) - `inst-schema-unenforced-checks`
14. [x] - `p1` - Create no table for `mcp_servers`, `mcp_server_tools`, `role_mcp_servers` or the `tenant_closure` projection - `inst-schema-no-deferred-tables`
15. [x] - `p1` - **RETURN** the migrated schema - `inst-schema-return`

### Scoped Persistence

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-scoped-persistence`

**Input**: an `AccessScope` from the PEP, a resource kind and an optional chat id

**Output**: the scoped row, or a not-found result. Each table's scoping declaration is the `Secure ORM` line of its DESIGN 3.7 heading.

**Steps**:
1. [x] - `p1` - Declare the Secure ORM scoping per table: tenant column `tenant_id` on every table; owner column `user_id` on `chats`, `message_reactions` and `quota_usage` only; resource column `id` where the table has one; no type column anywhere - `inst-scope-declare`
2. [x] - `p1` - Load a chat only through a query constrained by the `AccessScope` (tenant and owner) and by `deleted_at IS NULL` - `inst-scope-load-chat`
3. [x] - `p1` - **IF** the query returns no row - `inst-scope-no-row`
   1. [x] - `p1` - **RETURN** not found, with the same result for a missing, soft-deleted, foreign-tenant or foreign-owner chat - `inst-scope-return-not-found`
4. [x] - `p1` - Read child rows (`messages`, `chat_turns`, `attachments`, `message_attachments`, `thread_summaries`, `chat_vector_stores`) only through the `chat_id` obtained from the owner-scoped chat query, and always with `tenant_id` in the filter - `inst-scope-children`
5. [x] - `p1` - Forbid request-handling code from reaching `chat_vector_stores` by primary key or by `vector_store_id` alone; background processes that run outside a user scope still include `tenant_id` in every query - `inst-scope-vector-store-rule`
6. [x] - `p1` - Populate `tenant_id` and `user_id` of new rows from the security context and the resolved chat, never from request input - `inst-scope-writers`
7. [x] - `p1` - **RETURN** the scoped row - `inst-scope-return`

### Outbox Queues

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-outbox-queues`

**Input**: the validated `outbox` configuration and the shared ToolKit DB outbox

**Output**: five registered leased queues and a transactional enqueuer. The queue handlers of usage, attachment cleanup, chat cleanup and thread summary belong to the features that own that behavior; the audit handler belongs to this feature (audit delivery algorithm).

**Steps**:
1. [x] - `p1` - **FOR EACH** queue in the list usage (`outbox.queue_name`, default `mini-chat.usage_snapshot`), attachment cleanup (`outbox.cleanup_queue_name`, default `mini-chat.attachment_cleanup`), chat cleanup (`outbox.chat_cleanup_queue_name`, default `mini-chat.chat_cleanup`), thread summary (`outbox.thread_summary_queue_name`, default `mini-chat.thread_summary`), audit (`outbox.audit_queue_name`, default `mini-chat.audit`) - `inst-outbox-each-queue`
   1. [x] - `p1` - Register the queue with `outbox.num_partitions` partitions (default `4`) and a leased handler slot - `inst-outbox-register`
2. [x] - `p1` - Partition usage and audit messages by `tenant_id`, and chat cleanup and thread summary messages by `chat_id` - `inst-outbox-partition-keys`
3. [x] - `p1` - Set the lease of the thread summary queue to `thread_summary_worker.claim_timeout_secs` (default 300 s) and the lease of the audit queue to 60 s (hardcoded); every other queue uses the shared outbox default of 30 s; leave backoff, batch sizes, sequencer cadence and vacuum cadence to the shared outbox defaults, with no Mini Chat config key - `inst-outbox-leases`
4. [x] - `p1` - Accept the handler outcomes `Ok`, `Retry` and `Reject(reason)` only, and let a `Reject` move the message to the shared dead-letter store, without a separate `dead` row state - `inst-outbox-outcomes`
5. [x] - `p1` - Provide enqueueing inside the caller's database transaction, so that a domain write and its outbox message commit or roll back together - `inst-outbox-transactional-enqueue`
6. [x] - `p1` - Validate before serialization that a `dedupe_key` starts with the 32-character lowercase hex form of `tenant_id` followed by `/`; user-turn keys are `{tenant_id}/{turn_id}/{request_id}` and system-task keys are `{tenant_id}/{system_task_type}/{system_request_id}`, every UUID component in 32-character lowercase hex without hyphens - `inst-outbox-dedupe-validation`
7. [x] - `p1` - **IF** a serialized payload exceeds the outbox payload size limit - `inst-outbox-payload-too-large`
   1. [x] - `p1` - Report an enqueue error that the caller maps by the error contract (chat deletion returns 400 `invalid_argument`; attachment delete and turn retry, edit and delete return 500 `internal`) - `inst-outbox-payload-error`
8. [x] - `p1` - **RETURN** the registered queues and the enqueuer - `inst-outbox-return`

### Canonical Error Mapping

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-error-mapping`

**Input**: a domain or infrastructure error and the phase of the request (before the response stream opens, or after)

**Output**: a canonical Problem for REST, or the `{code, message}` payload of the SSE `error` event. The per-condition table (condition, category, HTTP status, reason or violation) is normative in DESIGN 3.3 `Error Codes` and in ADR-0004, and the code list is in DESIGN 3.3 `Streaming error codes`.

**Steps**:
1. [x] - `p1` - **IF** the response stream is already open - `inst-error-stream-open`
   1. [x] - `p1` - Emit one terminal `event: error` whose `data` carries only `code` and `message`; take `code` from the streaming error code list and use a sanitized `message`; send nothing after a client disconnect - `inst-error-sse-event`
   2. [x] - `p1` - **RETURN** the SSE payload; quota exhaustion never appears here because it is rejected before the stream opens - `inst-error-return-sse`
2. [x] - `p1` - Build a Problem with the fields `type`, `title`, `status`, `detail`, `instance`, `trace_id` and `context`, and no top-level `code` field - `inst-error-build-problem`
3. [x] - `p1` - **MATCH** the error category to its HTTP status - `inst-error-match-category`
   - [x] - `p1` - **CASE** `not_found`: 404 - `inst-error-case-not-found`
   - [x] - `p1` - **CASE** `invalid_argument`: 400, except 422 for a body that does not match the schema (`field_violations[body].reason` equal to `invalid_json_body`) and 415 for a JSON body without a JSON content type (`missing_json_content_type`) - `inst-error-case-invalid-argument`
   - [ ] - `p1` - **CASE** `out_of_range` and `failed_precondition`: 400 - `inst-error-case-out-of-range`
   - [ ] - `p1` - **CASE** `unauthenticated`: 401 - `inst-error-case-unauthenticated`
   - [x] - `p1` - **CASE** `permission_denied`: 403 - `inst-error-case-permission-denied`
   - [x] - `p1` - **CASE** `aborted` and `already_exists`: 409 - `inst-error-case-conflict`
   - [ ] - `p1` - **CASE** `resource_exhausted`: 429 - `inst-error-case-exhausted`
   - [x] - `p1` - **CASE** `service_unavailable`: 503 with a `Retry-After` header and `context.retry_after_seconds` carrying the same value (5 for a PDP failure and for the upload concurrency limit, 10 for a storage backend failure) - `inst-error-case-unavailable`
   - [x] - `p1` - **CASE** `internal`: 500 - `inst-error-case-internal`
4. [x] - `p1` - Put the machine-readable reason in exactly one of `context.reason` (`aborted`, `permission_denied`), `context.field_violations[].reason` (`invalid_argument`, `out_of_range`) or `context.violations[]` (`failed_precondition` with `subject`, `description`, `type`; `resource_exhausted` with `subject`, `description`) - `inst-error-reason-location`
5. [x] - `p1` - For `not_found`, set `context.resource_type` to the missing resource type `gts.cf.core.mini_chat.chat.v1~`, `gts.cf.core.mini_chat.message.v1~`, `gts.cf.core.mini_chat.turn.v1~`, `gts.cf.core.mini_chat.attachment.v1~` or `gts.cf.core.mini_chat.model.v1~` - `inst-error-resource-type`
6. [ ] - `p1` - For list query errors raised by the OData extractor or the repository, set `context.resource_type` to `gts.cf.core.odata.query.v1~` and carry the platform reason codes (`INVALID_FILTER`, `INVALID_ORDERBY_FIELD`, `INVALID_LIMIT`, `INVALID_CURSOR`, `ORDER_MISMATCH`, `FILTER_MISMATCH`, `ORDER_WITH_CURSOR`, `FILTER_TOO_LONG`, `FILTER_TOO_COMPLEX`, `INVALID_SELECT`, `UNSUPPORTED_QUERY_PARAM`, `INVALID_QUERY_PARAMS`) in `field_violations[].reason` - `inst-error-odata`
7. [x] - `p1` - Map an unhandled unique-constraint violation to 409 `already_exists` with `resource_name` equal to the conflict code (`unique_violation`) and a generic `detail` - `inst-error-unique-violation`
8. [x] - `p1` - Map an unclassified internal or database error to 500 `internal` with a generic `detail` - `inst-error-internal`
9. [x] - `p1` - Keep `detail` and `context.format` as human-readable text that is not part of the contract; log driver text, turn ids and causes only, and scrub provider file, assistant and vector store identifiers from anything returned to a client - `inst-error-detail-policy`
10. [x] - `p1` - Declare the error responses of every registered operation in the OpenAPI document, except the 415 missing-content-type response, which is not declared - `inst-error-openapi`
11. [x] - `p1` - **RETURN** the Problem - `inst-error-return-problem`

### Authorization Operation Matrix

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-authz-matrix`

**Input**: the HTTP method and path template of the request

**Output**: the action name, resource type, whether `resource.id` is sent, and the `require_constraints` flag. The matrix is normative in DESIGN 3.8 `Per-Operation Authorization Matrix`. Chat actions send the chat id as `resource.id`; sub-resource identifiers (`request_id`, `attachment_id`, `msg_id`) are not independently protected.

**Steps**:
1. [x] - `p1` - **MATCH** the operation - `inst-matrix-match`
   - [x] - `p1` - **CASE** `POST /v1/chats`: action `create`, Chat type, no `resource.id`, `require_constraints` true; the owner properties come from the security context and the insert is scoped by the returned `AccessScope` - `inst-matrix-create`
   - [x] - `p1` - **CASE** `GET /v1/chats`: action `list`, Chat type, no `resource.id`, constraints required - `inst-matrix-list`
   - [x] - `p1` - **CASE** `GET /v1/chats/{id}`: action `read`, with `resource.id` - `inst-matrix-read`
   - [x] - `p1` - **CASE** `DELETE /v1/chats/{id}`: action `delete`, with `resource.id` - `inst-matrix-delete`
   - [x] - `p1` - **CASE** `PATCH /v1/chats/{id}`: action `update`, with `resource.id` - `inst-matrix-update`
   - [x] - `p1` - **CASE** `GET /v1/chats/{id}/messages`: action `list_messages`, `resource.id` is the chat id - `inst-matrix-list-messages`
   - [x] - `p1` - **CASE** `POST /v1/chats/{id}/messages:stream`: action `send_message` only, with no `read` evaluation; the decision is taken before the SSE stream opens and covers the whole session - `inst-matrix-send-message`
   - [x] - `p1` - **CASE** `POST /v1/chats/{id}/attachments`: action `upload_attachment` - `inst-matrix-upload`
   - [x] - `p1` - **CASE** `GET /v1/chats/{id}/attachments/{attachment_id}`: action `read_attachment` - `inst-matrix-read-attachment`
   - [x] - `p1` - **CASE** `DELETE /v1/chats/{id}/attachments/{attachment_id}`: action `delete_attachment` - `inst-matrix-delete-attachment`
   - [x] - `p1` - **CASE** `GET /v1/chats/{id}/turns/{request_id}`: action `read_turn` - `inst-matrix-read-turn`
   - [x] - `p1` - **CASE** `POST /v1/chats/{id}/turns/{request_id}/retry`: action `retry_turn`, evaluated once and the decision reused by the mutation - `inst-matrix-retry`
   - [x] - `p1` - **CASE** `PATCH /v1/chats/{id}/turns/{request_id}`: action `edit_turn`, evaluated once - `inst-matrix-edit`
   - [x] - `p1` - **CASE** `DELETE /v1/chats/{id}/turns/{request_id}`: action `delete_turn` - `inst-matrix-delete-turn`
   - [x] - `p1` - **CASE** `PUT /v1/chats/{id}/messages/{msg_id}/reaction`: action `set_reaction` - `inst-matrix-set-reaction`
   - [x] - `p1` - **CASE** `DELETE /v1/chats/{id}/messages/{msg_id}/reaction`: action `delete_reaction` - `inst-matrix-delete-reaction`
   - [x] - `p1` - **CASE** `GET /v1/models`: action `list`, Model type, no `resource.id`, `require_constraints` false (decision only) - `inst-matrix-models-list`
   - [x] - `p1` - **CASE** `GET /v1/models/{id}`: action `read`, Model type, no `resource.id`, `require_constraints` false - `inst-matrix-models-read`
   - [x] - `p1` - **CASE** `GET /v1/quota/status`: action `read`, UserQuota type, no `resource.id`, constraints required - `inst-matrix-quota-status`
   - [x] - `p1` - **DEFAULT**: no authorization entry exists, so the route is not registered (this covers `POST /v1/chats/{id}:temporary`, deferred to P2) - `inst-matrix-default`
2. [x] - `p1` - Select the resource type: Chat `gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~` for every chat operation, Model `gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~` for the Models API, UserQuota `gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~` for the quota status endpoint - `inst-matrix-resource-types`
3. [x] - `p1` - **RETURN** the operation descriptor - `inst-matrix-return`

### PEP Authorization

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-pep-authorize`

**Input**: the security context (subject id, tenant id, token scopes), the operation descriptor and the optional chat id

**Output**: an `AccessScope`, a permission-only decision, or a refusal Problem

**Steps**:
1. [x] - `p1` - Build the PDP evaluation request: subject type `gts.cf.core.security.subject_user.v1~` with the subject id and the `tenant_id` property; the action name; the resource type and, when the matrix says so, the chat id as `resource.id`; for `create`, the `owner_tenant_id` and `owner_id` resource properties taken from the security context - `inst-pep-build-request`
2. [x] - `p1` - Fill the request context: `tenant_context` with mode `root_only` and the subject tenant as `root_id`; `token_scopes` from the token (first-party UI tokens carry `*`); `require_constraints` from the matrix; `capabilities` as an empty list (`tenant_hierarchy`, `group_membership` and `group_hierarchy` are not declared in P1); `supported_properties` `owner_tenant_id`, `owner_id` and `id` (mapped to `chats.tenant_id`, `chats.user_id` and `chats.id`), or `owner_tenant_id` and `owner_id` for the UserQuota resource over `quota_usage` - `inst-pep-build-context`
3. [x] - `p1` - Forward the token scopes and leave their enforcement to the PDP: `ai:mini_chat` permits all operations, `ai:mini_chat:read` permits `list`, `read`, `list_messages`, `read_attachment` and `read_turn`, and `ai:mini_chat:write` permits `create`, `update`, `delete`, `send_message`, `upload_attachment`, `delete_attachment`, `retry_turn`, `edit_turn`, `delete_turn`, `set_reaction` and `delete_reaction` - `inst-pep-token-scopes`
4. [x] - `p1` - **TRY** - `inst-pep-try`
   1. [x] - `p1` - Send the request to `authz_resolver` through the policy enforcer - `inst-pep-call-pdp`
5. [x] - `p1` - **CATCH** the PDP being unreachable, timing out or failing evaluation - `inst-pep-catch`
   1. [x] - `p1` - Log the cause at `error` level and **RETURN** 503 `service_unavailable` with `Retry-After: 5`, `context.retry_after_seconds` equal to 5 and a generic detail; access stays refused (fail-closed), and the outage stays distinguishable from a denial - `inst-pep-return-503`
6. [x] - `p1` - **IF** the decision is false, with or without `resource.id` - `inst-pep-denied`
   1. [x] - `p1` - **RETURN** 403 `permission_denied` with `AUTHZ_DENIED` and do not expose `deny_reason.details` - `inst-pep-return-403-denied`
7. [x] - `p1` - **IF** `require_constraints` is false - `inst-pep-permission-only`
   1. [x] - `p1` - **RETURN** the permission-only decision (the Models API case), with no `AccessScope` - `inst-pep-return-permission-only`
8. [x] - `p1` - **IF** the decision is true and the constraints are absent or an empty list - `inst-pep-no-constraints`
   1. [x] - `p1` - **RETURN** 403 `permission_denied` - `inst-pep-return-403-no-constraints`
9. [x] - `p1` - Compile the constraints into an `AccessScope`; treat a predicate of an unknown type as false, treat a predicate on an unknown `resource_property` as false and log it as a PDP contract violation, and treat the constraint set as failed when every constraint is false or compilation fails - `inst-pep-compile`
10. [x] - `p1` - **IF** compilation failed or every constraint is false - `inst-pep-compile-failed`
    1. [x] - `p1` - **RETURN** 403 `permission_denied` - `inst-pep-return-403-compile`
11. [x] - `p1` - Add an owner predicate to the compiled scope, in addition to the tenant predicate, for every operation whose resource type is Chat (including `create` and `list` of chats and every operation on a chat sub-resource) and for the UserQuota operation `GET /v1/quota/status` (defence in depth): `owner_id` equals the subject id (column `chats.user_id`) for Chat, and the `user_id` owner column of `quota_usage` for UserQuota, so a PDP that returns only a tenant predicate never exposes another user's rows - `inst-pep-owner-predicate`
12. [x] - `p1` - **RETURN** the `AccessScope`; for `send_message`, `retry_turn` and `edit_turn` the decision is made once before the stream opens, with no per-message re-authorization - `inst-pep-return-scope`

### Policy Snapshot Access

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access`

**Input**: the vendor from the gear configuration, and an optional user id and policy version

**Output**: the current policy version, the policy snapshot (model catalog, kill switches) and the user limits, or a plugin error. The contract is in DESIGN 5.2 (`5.2.1 Policy Snapshot Concept`, `5.2.2 Plugin Contract`).

**Steps**:
1. [x] - `p1` - Resolve the `mini-chat-model-policy-plugin` instance through types-registry for the plugin spec `MiniChatModelPolicyPluginSpecV1`, selecting among instances with the configured `vendor` by the lowest `priority` value - `inst-policy-resolve-plugin`
2. [x] - `p1` - Call the plugin client `MiniChatModelPolicyPluginClientV1` for the current policy version and the snapshot of that version - `inst-policy-call-snapshot`
3. [x] - `p1` - Call `get_user_limits(user_id, policy_version)` when user limits are needed; keep `UserLimits` apart from the shared `PolicySnapshot` and never merge them - `inst-policy-user-limits`
4. [x] - `p1` - Keep no local snapshot cache, no snapshot table and no `POST /internal/policy:notify` endpoint: every request that needs the catalog asks the plugin, and settlement asks only for the snapshot of the turn's `policy_version_applied` (ADR-0008) - `inst-policy-no-cache`
5. [ ] - `p1` - Never call `check_user_license` of the plugin client; the license gate is enforced on the routes - `inst-policy-no-license-call`
6. [x] - `p1` - **IF** the plugin cannot be resolved or the call fails - `inst-policy-failed`
   1. [x] - `p1` - **RETURN** a plugin error that the caller maps to 500 `internal` - `inst-policy-return-error`
7. [x] - `p1` - **RETURN** the version, snapshot and limits - `inst-policy-return`

### Model Catalog Rules

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-catalog-rules`

**Input**: the `model_catalog` of the policy snapshot

**Output**: the catalog rules that later features consume. The entry fields are normative in DESIGN 4 `Model Catalog Configuration` (`Fields per entry`, `Rules`).

**Steps**:
1. [x] - `p1` - Treat `ModelCatalogEntry.enabled` as false when the entry omits it, and treat a disabled entry as excluded from the runtime catalog and from the Models API - `inst-catalog-enabled-default`
2. [x] - `p1` - Resolve the default model for new chats as the first enabled entry with `preference.is_default` true, else the first enabled entry, without considering the tier - `inst-catalog-default-model`
3. [x] - `p1` - Validate a user-selected model at chat creation as present in the catalog and enabled, otherwise 400 `invalid_argument` with `field_violations[model].reason` equal to `INVALID_MODEL` (the call is made by `cpt-cf-mini-chat-feature-chat-management`) - `inst-catalog-selection`
4. [x] - `p1` - Fix the downgrade tier order as `premium` then `standard` and skip a tier that has no enabled entry; the cascade itself belongs to `cpt-cf-mini-chat-feature-quota-and-billing` - `inst-catalog-tier-order`
5. [x] - `p1` - Expose the six kill switches `disable_premium_tier`, `force_standard_tier`, `disable_web_search`, `disable_file_search`, `disable_images` and `disable_code_interpreter` from the snapshot; their enforcement belongs to the features that gate the matching behavior - `inst-catalog-kill-switches`
6. [x] - `p1` - Resolve image capability from `multimodal_capabilities` containing `VISION_INPUT`, and do not validate at startup that every enabled model has it - `inst-catalog-image-capability`
7. [ ] - `p1` - Leave catalog content validation to the plugin that serves it and add no catalog ordering guarantee beyond the default-model fallback - `inst-catalog-plugin-validates`
8. [ ] - `p1` - **RETURN** the rules - `inst-catalog-return`

### Model Visibility

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-model-visibility`

**Input**: the policy snapshot, and optionally one requested `model_id`

**Output**: the list of public model projections, or one projection, or not found. Normative in DESIGN 3.3 `Models API`, `Visibility Algorithm (Normative)` and `Non-Exposure Rules (Models API)`.

**Steps**:
1. [x] - `p1` - Read the catalog from the snapshot and keep only entries whose `enabled` is true - `inst-visibility-filter`
2. [x] - `p1` - **FOR EACH** visible entry, project exactly these seven fields: `model_id` (the entry `id`, the same value used in `POST /v1/chats` and `chats.model`), `display_name`, `tier` (the lowercase string `standard` or `premium`), `multiplier_display` (for example `1x`), `description` (omitted when none is configured), `multimodal_capabilities` (an array; P1 values `VISION_INPUT` and `RAG`; clients ignore unknown values) and `context_window` (an integer of tokens) - `inst-visibility-project`
3. [x] - `p1` - Exclude from every projection `provider`, `provider_model_id`, provider deployment ids, routing metadata, the credit multipliers (`input_tokens_credit_multiplier_micro`, `output_tokens_credit_multiplier_micro`, `credits_micro`), `policy_version`, `max_output_tokens`, `max_input_tokens` and `is_default`; `model_id` is never a provider model name - `inst-visibility-non-exposure`
4. [x] - `p1` - **IF** a `model_id` was requested - `inst-visibility-single`
   1. [x] - `p1` - Find the visible entry whose `id` equals the requested value - `inst-visibility-find`
   2. [x] - `p1` - **IF** none is found (the model is disabled or not in the catalog) - `inst-visibility-not-found`
      1. [x] - `p1` - **RETURN** not found, mapped to 404 `not_found` with the model resource type - `inst-visibility-return-not-found`
   3. [x] - `p1` - **RETURN** the single projection - `inst-visibility-return-single`
5. [x] - `p1` - **RETURN** the list of projections - `inst-visibility-return-list`

### Static Model Policy Plugin

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin`

**Input**: the plugin config section `static-mini-chat-model-policy-plugin` (a gear of its own, keyed by gear name)

**Output**: a registered model-policy plugin that serves a fixed snapshot. The module stays reachable as `mini_chat::infra::plugins::static_model_policy`, which the example server imports. The parameter table is DESIGN Appendix B, `B.1 Gear config`, `static-mini-chat-model-policy-plugin`.

**Steps**:
1. [x] - `p1` - Load the plugin config; reject unknown top-level keys; use the defaults when the whole section is absent - `inst-static-policy-load`
2. [x] - `p1` - Apply the defaults: `vendor` `constructorfabric`; `priority` `100` (lower is higher priority); `model_catalog` an empty list when the section is absent, but a required key when the section is present (an empty list is valid); `default_standard_limits` daily `100_000_000` and monthly `1_000_000_000` micro-credits; `default_premium_limits` daily `50_000_000` and monthly `500_000_000` micro-credits - `inst-static-policy-defaults`
3. [x] - `p1` - Validate at plugin init that both credit multipliers of every catalog entry are within `1..=10,000,000,000`, and fail plugin init otherwise - `inst-static-policy-multipliers`
4. [x] - `p1` - Parse `kill_switches` as an operator-facing object whose fields `disable_premium_tier`, `force_standard_tier`, `disable_web_search`, `disable_file_search`, `disable_images` and `disable_code_interpreter` may each be omitted and then read as false, and reject an unknown key as a config error - `inst-static-policy-kill-switches`
5. [x] - `p1` - Ignore unknown keys inside the nested SDK types (`ModelCatalogEntry`, `EstimationBudgets`, `ModelGeneralConfig`, `TierLimits`), while the SDK `KillSwitches` type itself requires every field, so a snapshot from another plugin with a missing or renamed switch fails to deserialize instead of reading as false - `inst-static-policy-nested-types`
6. [x] - `p1` - Register the plugin instance in types-registry for `MiniChatModelPolicyPluginSpecV1` with the configured `vendor` and `priority`, and register its client in the ClientHub - `inst-static-policy-register`
7. [x] - `p1` - Serve the fixed snapshot with `policy_version` equal to 1 from the configured catalog and kill switches, and serve the same user limits to every user: `default_standard_limits` for the `total` bucket and `default_premium_limits` for the `tier:premium` bucket - `inst-static-policy-serve`
8. [x] - `p1` - Implement `publish_usage` as a log write only - `inst-static-policy-publish-usage`
9. [x] - `p1` - **RETURN** the registered plugin - `inst-static-policy-return`

### Audit Delivery

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-audit-delivery`

**Input**: one message of the audit queue (`outbox.audit_queue_name`, default `mini-chat.audit`) and its attempt number

**Output**: the outbox outcome `Ok`, `Retry` or `Reject`. Every path that would return `Retry` (a resolution error, a missing ClientHub client, a transient plugin error or a timeout) is bounded: on the 120th attempt it records `reject` and returns `Reject` instead. The producers (turn finalization and turn mutation transactions) belong to other features. Behavior is normative in DESIGN 3.2 (`Audit plugin and audit outbox`) and ADR-0009.

**Steps**:
1. [x] - `p1` - Deserialize the payload before anything else - `inst-audit-deserialize`
2. [x] - `p1` - **IF** the payload is corrupt - `inst-audit-corrupt`
   1. [x] - `p1` - Record `mini_chat_audit_emit` with `result` equal to `reject` and **RETURN** `Reject`, whether or not a plugin is available - `inst-audit-return-reject-corrupt`
3. [x] - `p1` - Look up the audit plugin instance through types-registry for `MiniChatAuditPluginSpecV1` by `vendor` and `priority`; cache a found instance id, and do not cache a "no plugin registered" result, so each delivery looks up again - `inst-audit-resolve`
4. [x] - `p1` - **IF** the lookup fails with a resolution error (any types-registry failure other than "no plugin registered") - `inst-audit-resolve-error`
   1. [x] - `p1` - **IF** this is the 120th attempt - `inst-audit-resolve-attempt-limit`
      1. [x] - `p1` - Record `result` equal to `reject` and **RETURN** `Reject` - `inst-audit-return-reject-resolve-limit`
   2. [x] - `p1` - Record `result` equal to `retry` and **RETURN** `Retry` - `inst-audit-return-retry-resolve`
5. [x] - `p1` - **IF** no plugin is registered - `inst-audit-no-plugin`
   1. [x] - `p1` - Log one warning per period without a plugin, record `result` equal to `dropped` and **RETURN** `Ok` (the event is acknowledged and dropped, so a missing plugin cannot block chat traffic) - `inst-audit-return-dropped`
6. [x] - `p1` - **IF** the instance resolves in types-registry but its client is missing from the ClientHub - `inst-audit-client-missing`
   1. [x] - `p1` - Reset the cached instance id - `inst-audit-reset-cached-id`
   2. [x] - `p1` - **IF** this is the 120th attempt - `inst-audit-client-attempt-limit`
      1. [x] - `p1` - Record `result` equal to `reject` and **RETURN** `Reject` - `inst-audit-return-reject-client-limit`
   3. [x] - `p1` - Record `result` equal to `retry` and **RETURN** `Retry` - `inst-audit-return-retry-client`
7. [x] - `p1` - **TRY** - `inst-audit-try`
   1. [x] - `p1` - Deliver the event through `MiniChatAuditPluginClientV1` with a 30 s timeout (hardcoded) - `inst-audit-deliver`
8. [x] - `p1` - **CATCH** a transient plugin error, a `PluginTimeout` or a resolution error - `inst-audit-catch-transient`
   1. [x] - `p1` - **IF** this is the 120th attempt (about an hour with the outbox backoff capped at 30 s) - `inst-audit-attempt-limit`
      1. [x] - `p1` - Record `result` equal to `reject` and **RETURN** `Reject`, so a misconfigured plugin does not block the partition - `inst-audit-return-reject-limit`
   2. [x] - `p1` - Record `result` equal to `retry` and **RETURN** `Retry` - `inst-audit-return-retry`
9. [x] - `p1` - **CATCH** a permanent plugin error - `inst-audit-catch-permanent`
   1. [x] - `p1` - Record `result` equal to `reject` and **RETURN** `Reject` - `inst-audit-return-reject-permanent`
10. [x] - `p1` - Record `result` equal to `ok` and **RETURN** `Ok`; no counter is recorded at enqueue time - `inst-audit-return-ok`

### Static Audit Plugin

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin`

**Input**: the plugin config section `static-mini-chat-audit-plugin` and audit events from the audit handler

**Output**: a registered audit plugin that logs events. The module stays reachable as `mini_chat::infra::plugins::static_audit`, which the example server imports.

**Steps**:
1. [x] - `p1` - Load the plugin config; reject unknown top-level keys; use the defaults when the section is absent - `inst-static-audit-load`
2. [x] - `p1` - Apply the defaults: `enabled` `true`, `vendor` `constructorfabric`, `priority` `100` - `inst-static-audit-defaults`
3. [x] - `p1` - Register the plugin instance in types-registry for `MiniChatAuditPluginSpecV1` with the configured `vendor` and `priority`, and register its client in the ClientHub - `inst-static-audit-register`
4. [x] - `p1` - **IF** `enabled` is true - `inst-static-audit-enabled`
   1. [x] - `p1` - Log the received audit event - `inst-static-audit-log`
5. [x] - `p1` - **ELSE** - `inst-static-audit-disabled`
   1. [x] - `p1` - Accept the event without logging, so the plugin stays registered but silent - `inst-static-audit-silent`
6. [x] - `p1` - **RETURN** success to the audit handler - `inst-static-audit-return`

### Provider Registry

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-provider-registry`

**Input**: the validated `providers` map

**Output**: a provider resolver keyed by OAGW alias. Field table: DESIGN Appendix B, `B.1 Gear config`, `Provider entry`. The adapters themselves are completed in `cpt-cf-mini-chat-feature-message-streaming`.

**Steps**:
1. [x] - `p1` - **IF** the `providers` section is absent - `inst-registry-default-entry`
   1. [x] - `p1` - Use the single default entry `openai` with kind `openai_responses`, host `api.openai.com`, API-key auth from `cred://openai-key` and `storage_kind` `openai` - `inst-registry-default-values`
2. [x] - `p1` - **FOR EACH** provider entry - `inst-registry-each-entry`
   1. [x] - `p1` - Apply the defaults: `port` `443` (or `80` when `use_http` is true), `use_http` false, `api_path` `/v1/responses`, `storage_backend` equal to the provider id - `inst-registry-entry-defaults`
   2. [x] - `p1` - Fill `upstream_alias` with `host` when it is not set, so an alias is always passed to OAGW - `inst-registry-fill-alias`
   3. [x] - `p1` - **FOR EACH** tenant override, fill an unset `upstream_alias` with the override `host`, and let each override field fall back to the provider entry on its own - `inst-registry-override-fallback`
3. [x] - `p1` - Build the resolver, which maps a catalog model's `provider_id`, and the tenant through `tenant_overrides`, to the entry and its OAGW alias; the adapter is selected by the entry `kind` - `inst-registry-build-resolver`
4. [x] - `p1` - Keep credentials by reference only (`auth_config.secret_ref`, `cred://` names): no secret value is stored, logged or returned, and the `client_credentials.client_secret` is redacted in logs - `inst-registry-credentials-by-reference`
5. [x] - `p1` - **IF** the resolver cannot map a model's provider (for example the entry was removed) - `inst-registry-resolution-failed`
   1. [x] - `p1` - **RETURN** a resolution error that callers map to 500 `internal` before any stream opens - `inst-registry-return-error`
6. [x] - `p1` - **RETURN** the resolver - `inst-registry-return`

### OAGW Provisioning With Deferred Retry

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning`

**Input**: a copy of the provider entries and tenant overrides, and the `client_credentials`

**Output**: registered OAGW upstreams and routes plus the deferred set of providers still pending, or a startup error. This is the synchronous first pass: it never waits for a deferred entry, whose retries run in the OAGW provisioning reconcile task. Normative in DESIGN 3.2 (`OAGW provisioning`) and 3.5 (`LLM Provider (OpenAI / Azure OpenAI)`), and in ADR-0005.

**Steps**:
1. [x] - `p1` - Exchange `client_credentials` through `authn_resolver` for an S2S security context - `inst-prov-s2s`
2. [x] - `p1` - **IF** the exchange fails - `inst-prov-s2s-failed`
   1. [x] - `p1` - **RETURN** a startup error - `inst-prov-return-s2s-error`
3. [x] - `p1` - Work on a copy of the entries, so the alias that OAGW returns never reaches the resolver built at init - `inst-prov-copy`
4. [x] - `p1` - **FOR EACH** provider entry, and each tenant override that has its own host or alias - `inst-prov-each-entry`
   1. [x] - `p1` - Create the upstream under the alias with `host`, `port`, `use_http` and the auth plugin (`auth_plugin_type`, `auth_config`), or reuse the upstream when OAGW reports that it already exists - `inst-prov-upstream`
   2. [x] - `p1` - Create the chat route: `POST` on the prefix derived from `api_path`, with the `{model}` placeholder handled as a path suffix and the query allowlist taken from `api_path` - `inst-prov-chat-route`
   3. [x] - `p1` - **IF** the entry has a `storage_kind` - `inst-prov-has-storage`
      1. [x] - `p1` - Create the RAG routes `POST {prefix}/files`, `DELETE {prefix}/files/*`, `POST {prefix}/vector_stores*`, `DELETE {prefix}/vector_stores/*` and `GET {prefix}/vector_stores/*`, where the prefix is `/v1` for `openai` and `/openai` with the `api-version` query parameter for `azure`; a missing RAG route is best-effort and only degrades RAG - `inst-prov-rag-routes`
   4. [x] - `p1` - **IF** the entry is deterministically misconfigured - `inst-prov-misconfigured`
      1. [x] - `p1` - **RETURN** a startup error naming the provider - `inst-prov-return-misconfigured`
   5. [x] - `p1` - **IF** the credstore secret of the entry is not yet readable - `inst-prov-secret-unreadable`
      1. [x] - `p1` - Mark the entry deferred and add it to the deferred set - `inst-prov-defer`
5. [x] - `p1` - **RETURN** the registered upstreams and routes and the deferred set right after the first pass, without retrying the deferred entries - `inst-prov-return`

### OAGW Provisioning Reconcile Task

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-oagw-reconcile`

**Input**: the deferred set returned by the OAGW provisioning first pass, the S2S security context and the gear cancellation token

**Output**: none; a background task that Gear Start spawns and Gear Stop cancels and joins. Normative in DESIGN 3.2 (`OAGW provisioning`: "retried by a background reconcile task").

**Steps**:
1. [x] - `p1` - **WHILE** deferred entries remain and the cancellation token is not cancelled - `inst-prov-retry-loop`
   1. [x] - `p1` - Wait 2 s before the first retry, then double the interval after each failed round up to 60 s, and keep it at 60 s after that; the wait ends early when the token is cancelled - `inst-prov-backoff`
   2. [x] - `p1` - Retry the provisioning of each deferred entry and remove the entries that succeed from the pending set - `inst-prov-retry-entries`
   3. [x] - `p1` - **IF** 2 minutes have passed without success for some entries - `inst-prov-warn-threshold`
      1. [x] - `p1` - Log one warning that names the providers still pending - `inst-prov-warn`
2. [x] - `p1` - **RETURN** when the deferred set is empty or the token is cancelled; retries continue until the gear stops - `inst-prov-reconcile-return`

### License Gate

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-license-gate`

**Input**: an inbound request to a gear route

**Output**: admission to the handler, or 403

**Steps**:
1. [x] - `p1` - Declare on every gear route, through the platform route builder, the required license feature `gts.cf.core.lic.feat.v1~cf.core.global.base.v1` (the interim base license feature; `ai_chat` is the target, ADR-0008) - `inst-license-declare`
2. [ ] - `p1` - Let the platform `license_manager` middleware of api-gateway check the tenant license on every route, before the handler runs - `inst-license-middleware`
3. [ ] - `p1` - **IF** the tenant lacks the feature - `inst-license-missing`
   1. [ ] - `p1` - **RETURN** 403 `permission_denied` with `context.reason` equal to `LICENSE_FEATURE_REQUIRED`, without reaching the PDP or the handler - `inst-license-return-403`
4. [ ] - `p1` - Perform no license check in gear code and never call the plugin `check_user_license`; a tenant with the base license but without `ai_chat` is not rejected in P1 - `inst-license-no-gear-check`
5. [ ] - `p1` - **RETURN** admission - `inst-license-return`

### Observability: Metrics, Correlation and Logging

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-algo-gear-foundation-observability`

**Input**: the `metrics` configuration and the lifecycle and request events of the gear

**Output**: registered OpenTelemetry instruments, correlation identifiers and structured logs. Normative in DESIGN 4 `Metrics (OTLP) and Alerts (P1)` and `Operational Traceability and Debugging (P1)`.

**Steps**:
1. [x] - `p1` - Create one OpenTelemetry meter and name every instrument with the prefix from `metrics.prefix`, defaulting to the gear name in snake case, `mini_chat`; export over OTLP through ToolKit, with no Prometheus exporter in the process - `inst-obs-meter`
2. [x] - `p1` - Register the instruments of the DESIGN `Emitted` table (areas Streaming, Cancellation, Orphan watchdog, Upload reaper, Attachments, Quota, Tools, Knowledge search, Thread summary, Turn mutations, Uploads, Images, Cleanup, Audit, Finalization) so the owning features record them; register the instruments listed under `Declared but not recorded (deferred)` without recording them; define none of the series listed under `Not defined` - `inst-obs-instruments`
3. [x] - `p1` - Restrict labels to the allowed low-cardinality keys (`provider`, `model`, `provider_kind`, `resource_type`, `kind`, `tier`, `state`, `stage`, `op`, `reason`, `error_code`, `decision`, `period`, `trigger`, `result`, `from_status`) and never label by `tenant_id`, `user_id`, `chat_id`, `request_id`, `provider_response_id`, filenames or free-form error strings - `inst-obs-labels`
4. [x] - `p1` - Record `mini_chat_audit_emit` only at audit delivery, with `result` in `ok`, `retry`, `reject`, `dropped` - `inst-obs-audit-series`
5. [x] - `p1` - Start a distributed trace for every inbound request and carry the trace context into the services that call other gears; record the OpenTelemetry trace id as `trace_id` on the turn audit event, absent when no span is active - `inst-obs-tracing`
6. [ ] - `p1` - Carry the correlation identifiers `trace_id`, `request_id`, `chat_id`, `turn_id` and `provider_response_id` in logs and audit events wherever the call site knows them; `provider_response_id` is persisted on `chat_turns` and on the assistant message and is never returned by an API - `inst-obs-correlation`
7. [ ] - `p1` - Emit request-scoped logs in structured JSON form with the fields `trace_id`, `request_id`, `chat_id`, `turn_id`, `tenant_id`, `user_id`, `requester_type`, `status` and `error_code` where known - `inst-obs-logging`
8. [ ] - `p1` - Keep provider identifiers and secrets out of logs, metrics and responses, and redact the `client_credentials.client_secret` - `inst-obs-sanitize`
9. [x] - `p1` - **RETURN** the registered instruments and logging conventions - `inst-obs-return`

## 4. States (CDSL)

### Gear Lifecycle State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle`

**States**: CREATED, INITIALIZED, STARTED, STOPPING, STOPPED, FAILED

**Initial State**: CREATED

**Transitions**:
1. [x] - `p1` - **FROM** CREATED **TO** INITIALIZED **WHEN** `init` loaded and validated the configuration, resolved the clients and built the services - `inst-gear-init-ok`
2. [x] - `p1` - **FROM** CREATED **TO** FAILED **WHEN** the configuration is invalid or a required client cannot be resolved - `inst-gear-init-failed`
3. [x] - `p1` - **FROM** INITIALIZED **TO** STARTED **WHEN** `start` finished the provisioning first pass (deferred entries may still be pending in the reconcile task) and the outbox pipeline is running - `inst-gear-start-ok`
4. [x] - `p1` - **FROM** INITIALIZED **TO** FAILED **WHEN** the S2S exchange fails or a provider entry is deterministically misconfigured - `inst-gear-start-failed`
5. [x] - `p1` - **FROM** STARTED **TO** STOPPING **WHEN** the runtime signals stop - `inst-gear-stop-signal`
6. [x] - `p1` - **FROM** STOPPING **TO** STOPPED **WHEN** the workers are joined and the outbox pipeline is stopped, or the framework deadline fires - `inst-gear-stopped`

### Provider Provisioning State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-gear-foundation-provider-provisioning`

**States**: UNPROVISIONED, PROVISIONED, DEFERRED, STARTUP_FAILED

**Initial State**: UNPROVISIONED

**Transitions**:
1. [x] - `p1` - **FROM** UNPROVISIONED **TO** PROVISIONED **WHEN** the upstream and routes of the entry are created or reused - `inst-prov-state-provisioned`
2. [x] - `p1` - **FROM** UNPROVISIONED **TO** DEFERRED **WHEN** the first pass finds the credstore secret of the entry not yet readable; the entry is handed to the background reconcile task and gear start continues - `inst-prov-state-deferred`
3. [x] - `p1` - **FROM** UNPROVISIONED **TO** STARTUP_FAILED **WHEN** the entry is deterministically misconfigured - `inst-prov-state-failed`
4. [x] - `p1` - **FROM** DEFERRED **TO** DEFERRED **WHEN** a retry of the reconcile task fails (the interval doubles up to 60 s, and one warning is logged after 2 minutes) - `inst-prov-state-retry`
5. [x] - `p1` - **FROM** DEFERRED **TO** PROVISIONED **WHEN** a retry of the reconcile task succeeds - `inst-prov-state-recovered`
6. [x] - `p1` - **FROM** DEFERRED **TO** UNPROVISIONED **WHEN** the gear stops and the reconcile task is cancelled before a retry succeeded - `inst-prov-state-cancelled`

### Audit Event Delivery State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-gear-foundation-audit-delivery`

**States**: ENQUEUED, RETRYING, DELIVERED, DROPPED, DEAD_LETTERED

**Initial State**: ENQUEUED

**Transitions**:
1. [x] - `p1` - **FROM** ENQUEUED **TO** DELIVERED **WHEN** the audit plugin accepts the event - `inst-audit-state-delivered`
2. [x] - `p1` - **FROM** ENQUEUED **TO** DROPPED **WHEN** no audit plugin is registered (acknowledged and counted as `dropped`) - `inst-audit-state-dropped`
3. [x] - `p1` - **FROM** ENQUEUED **TO** RETRYING **WHEN** the plugin fails transiently, times out, cannot be resolved, or has no client in the ClientHub - `inst-audit-state-retrying`
4. [x] - `p1` - **FROM** ENQUEUED **TO** DEAD_LETTERED **WHEN** the payload is corrupt or the plugin reports a permanent error - `inst-audit-state-dead-direct`
5. [x] - `p1` - **FROM** RETRYING **TO** DELIVERED **WHEN** a later attempt is accepted by the plugin - `inst-audit-state-retry-delivered`
6. [x] - `p1` - **FROM** RETRYING **TO** DROPPED **WHEN** a later attempt finds no registered plugin - `inst-audit-state-retry-dropped`
7. [x] - `p1` - **FROM** RETRYING **TO** DEAD_LETTERED **WHEN** the 120th attempt still needs a retry on any retry path (resolution error, missing ClientHub client, transient plugin error or timeout), or a later attempt reports a permanent error - `inst-audit-state-dead-limit`

## 5. Definitions of Done

Traceability mode is FULL (set for the FEATURE artifact in `artifacts.toml`): every implemented CDSL step above gets paired `@cpt-begin` and `@cpt-end` code markers that carry its flow, algorithm or state ID, phase token and `inst-*` ID, and each pair wraps only the lines that implement that one step. The `featstatus` checkbox, the flow, algorithm, state and definition-of-done checkboxes are ticked together, only when the code, the markers and the tests exist.

### SDK Crate and Crate Wiring

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-sdk-crate`

The system **MUST** provide the SDK crate at `gears/mini-chat/mini-chat-sdk/` with package name `cf-gears-mini-chat-sdk` and library name `mini_chat_sdk`, add it to the workspace members, and make the gear crate `cf-gears-mini-chat` (library `mini_chat`, at `gears/mini-chat/mini-chat/`) depend on it.

- The SDK holds the transport-agnostic contracts and no REST types, handlers or DTOs: the model-policy plugin client `MiniChatModelPolicyPluginClientV1` with its spec `MiniChatModelPolicyPluginSpecV1`, the audit plugin client `MiniChatAuditPluginClientV1` with its spec `MiniChatAuditPluginSpecV1`, and the shared types the DESIGN names (`PolicySnapshot`, `ModelCatalogEntry`, `EstimationBudgets`, `ModelGeneralConfig`, `TierLimits`, `UserLimits`, `KillSwitches`, `UsageEvent`, the audit event and the plugin error types). Field-level definitions stay in DESIGN 5.2 and Appendix B (`B.2`); this feature defines no new system-level type.
- The gear crate keeps the module paths `mini_chat::infra::plugins::static_audit` and `mini_chat::infra::plugins::static_model_policy`, because the example server (`apps/cf-gears-example-server`, `registered_gears.rs`) imports both to link the plugin registrations.
- The gear follows the DDD-light layout of the workspace convention (API, domain and infrastructure layers; domain structs and enums carry the domain-model attribute enforced by lint DE0309; the gear name `mini-chat` is kebab-case).
- No crate is added outside the existing `Cargo.lock`: the SDK crate and the gear use only dependencies that the workspace already resolves, and the SDK crate is the only new package.
- The delivery vehicle is `apps/cf-gears-example-server` built with the features `mini-chat,static-authn,static-authz,single-tenant,static-credstore`. The observable surface is HTTP and SSE, the gear database and the requests sent to an OpenAI-compatible provider.
- Workspace lints apply to both crates (`unsafe_code` forbidden and clippy pedantic denied).

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-gear-init`
- `cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin`
- `cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin`

**Constraints**: `cpt-cf-mini-chat-constraint-openai-compatible`

**Touches**:
- API: none
- DB: none
- Entities: `ModelCatalogEntry`, `PolicySnapshot`, `AuditEvent`

### Gear Registration, Lifecycle and REST Surface

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-lifecycle-routes`

The system **MUST** register the gear named `mini-chat` with the dependencies `types_registry`, `authn_resolver`, `authz_resolver` and `oagw` and the capabilities `db`, `rest` and `stateful`; run the `init`, `start` and `stop` phases as specified; register the REST routes in the separate REST registration step under `url_prefix` (default `/mini-chat`); and declare the public API in OpenAPI.

- `init` rejects unknown configuration keys, validates every section and builds the services; `start` prepares the leader elector, provisions OAGW, starts the outbox pipeline and only then spawns the workers; `stop` cancels and joins the workers with a bounded timeout and then stops the outbox pipeline.
- The OpenAPI document declares the operations of this feature and the shared Problem responses; the generated `docs/api/api.json` is the schema reference.
- Deferred surfaces (MCP endpoints, `POST /v1/chats/{id}:temporary`) are not registered.

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-gear-init`
- `cpt-cf-mini-chat-algo-gear-foundation-gear-start`
- `cpt-cf-mini-chat-algo-gear-foundation-gear-stop`
- `cpt-cf-mini-chat-algo-gear-foundation-route-registration`
- `cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle`

**Constraints**: `cpt-cf-mini-chat-constraint-license-gate`

**Touches**:
- API: `GET /v1/models`, `GET /v1/models/{id}` (and the shared registration surface for every other route)
- DB: none
- Entities: none

### Configuration Keys, Defaults and Validation

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-configuration`

The system **MUST** load and validate the gear configuration with the keys, defaults and ranges of DESIGN Appendix B (`B.1`, `B.4` to `B.9`), reject unknown keys at the top level and in the sections `streaming`, `estimation_budgets`, `quota`, `outbox`, `context`, `rag`, `client_credentials`, `metrics`, `providers.<id>` (with `tenant_overrides`), `thumbnail` and `knowledge_search`, accept and ignore unknown keys in `orphan_watchdog`, `upload_reaper`, `thread_summary_worker` and `cleanup_worker`, and fail startup on the removed keys `providers.<id>.supports_file_search_filters` and `streaming.web_search_context_size` and on any `mcp` key (ADR-0010).

- The two bundled plugin gears reject unknown top-level keys in their own sections; the static policy plugin also rejects an unknown key in `kill_switches`, while nested SDK types ignore unknown keys.
- Deprecated no-effect fields log one startup warning each when set to a non-default value and are otherwise ignored.
- Where `config/mini-chat.yaml` disagrees with the DESIGN, the DESIGN wins; the keys and defaults are those of Appendix B.

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-config-validation`
- `cpt-cf-mini-chat-algo-gear-foundation-gear-init`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- API: none
- DB: none
- Entities: none

### P1 Database Schema and Migrations

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-schema`

The system **MUST** create, through migrations that run on both PostgreSQL and SQLite, all nine P1 tables exactly as DESIGN 3.7 defines them (columns, types, defaults, NOT NULL rules, foreign keys, enforced CHECKs, UNIQUE constraints and indexes), plus the shared outbox tables, and **MUST NOT** create the MCP tables or a local `tenant_closure` table.

- The nine tables are `chats`, `messages`, `chat_turns`, `attachments`, `message_attachments`, `thread_summaries`, `chat_vector_stores`, `quota_usage` and `message_reactions`, identified by the `cpt-cf-mini-chat-dbtable-*` IDs listed in section 1.2.
- The cross-column CHECKs on `chat_turns` and the `attachments.cleanup_status` value CHECK are not enforced by the database (ADR-0010); the repositories keep those invariants.
- Migration tests check, on both engines, the presence of every table, the UNIQUE constraints (`messages`, `chat_turns`, `thread_summaries`, `chat_vector_stores`, `quota_usage`, `message_reactions`, and the `(id, chat_id)` keys), the partial indexes and the enforced CHECKs.

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-schema-migration`

**Constraints**: `cpt-cf-mini-chat-principle-tenant-scoped`

**Touches**:
- API: none
- DB: `cpt-cf-mini-chat-dbtable-chats`, `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-attachments`, `cpt-cf-mini-chat-dbtable-message-attachments`, `cpt-cf-mini-chat-dbtable-thread-summaries`, `cpt-cf-mini-chat-dbtable-chat-vector-stores`, `cpt-cf-mini-chat-dbtable-quota-usage`, `cpt-cf-mini-chat-dbtable-message-reactions`
- Entities: `Chat`, `Message`, `Attachment`, `ThreadSummary`, `ChatVectorStore`, `QuotaUsage`, `MessageReaction`

### Secure-ORM Scoped Persistence Layer

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-scoped-persistence`

The system **MUST** access every table through Secure ORM entities with the tenant column `tenant_id` on all tables and the owner column `user_id` on `chats`, `message_reactions` and `quota_usage`, resolve chats only through owner-scoped queries, and read child rows only through the `chat_id` of such a query.

- A foreign, soft-deleted or missing chat is indistinguishable and ends in 404.
- Request-handling code never reaches `chat_vector_stores` by primary key or by `vector_store_id` alone.
- Code outside a user scope still filters by `tenant_id`.
- Tests with two tenants and two users in one tenant prove that no read or write crosses the owner or tenant boundary.

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-scoped-persistence`
- `cpt-cf-mini-chat-flow-gear-foundation-request-admission`

**Constraints**: `cpt-cf-mini-chat-principle-tenant-scoped`, `cpt-cf-mini-chat-principle-owner-only-content`

**Touches**:
- API: every chat-scoped route
- DB: all tables of `cpt-cf-mini-chat-dod-gear-foundation-schema`
- Entities: `Chat`, `Message`, `Attachment`, `QuotaUsage`, `MessageReaction`

### Outbox Pipeline Queues

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-outbox`

The system **MUST** register the five leased queues (usage, attachment cleanup, chat cleanup, thread summary, audit) on the shared ToolKit DB outbox at gear start, with `outbox.num_partitions` partitions each, the lease durations and partition keys of the algorithm, transactional enqueueing and the dedupe key prefix validation.

- Handlers of the usage, attachment cleanup, chat cleanup and thread summary queues belong to the features that own that behavior; the audit handler is delivered by this feature.
- Outbox payload size violations surface through the error mapping, not as a separate error shape.

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-outbox-queues`
- `cpt-cf-mini-chat-algo-gear-foundation-gear-start`

**Constraints**: `cpt-cf-mini-chat-principle-tenant-scoped`

**Touches**:
- API: none
- DB: shared outbox tables (platform migrations)
- Entities: `AuditEvent`, `UsageEvent`

### Canonical Error Contract

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-error-contract`

The system **MUST** return every REST error as a canonical Problem with the fields `type`, `title`, `status`, `detail`, `instance`, `trace_id` and `context` and no top-level `code`, with the HTTP status following the category as in the error mapping algorithm, and **MUST** reserve the `{code, message}` shape for the SSE `error` event (ADR-0004).

- The full condition table is DESIGN 3.3 `Error Codes`; this feature implements the mapping layer and the rows that belong to it (authentication, license, authorization, model not found, OData query errors, unique violation, internal), and the other features raise their own rows through it.
- Tests pin the category, status and reason of the rows this feature owns, for both REST and the SSE `error` envelope, and assert that no top-level `code` exists and that provider identifiers never appear.

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-error-mapping`

**Constraints**: `cpt-cf-mini-chat-principle-tenant-scoped`

**Touches**:
- API: every route (`Problem` responses) and the SSE `error` event
- DB: none
- Entities: none

### Authorization PEP

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-authz-pep`

The system **MUST** act as the Policy Enforcement Point for every route: evaluate the per-operation matrix through the policy enforcer, compile PDP constraints into an `AccessScope`, add the owner predicate to the scope of every Chat operation and of the UserQuota status operation, fail closed, and answer 404 for a hidden resource, 403 for a deny or an unusable decision, and 503 with `Retry-After: 5` for a PDP failure.

- The matrix covers all 19 operations of DESIGN 3.8 with the exact action names, `resource.id` presence and `require_constraints` values; the Models API operations are decision-only.
- The evaluation request declares `tenant_context` mode `root_only`, an empty `capabilities` list and the supported properties `owner_tenant_id`, `owner_id` and `id`, and forwards `token_scopes`.
- A mock PDP drives tests for: deny, true with no constraints, empty constraints, unknown predicate type, unknown `resource_property`, compile failure, unreachable PDP and timeout.
- A mock PDP that returns only a tenant predicate for list chats (`GET /v1/chats`) and for `GET /v1/quota/status` yields only the caller's rows, never another user's rows of the same tenant.

**Implements**:
- `cpt-cf-mini-chat-flow-gear-foundation-request-admission`
- `cpt-cf-mini-chat-algo-gear-foundation-authz-matrix`
- `cpt-cf-mini-chat-algo-gear-foundation-pep-authorize`

**Constraints**: `cpt-cf-mini-chat-principle-tenant-scoped`, `cpt-cf-mini-chat-principle-owner-only-content`

**Touches**:
- API: every route
- DB: `cpt-cf-mini-chat-dbtable-chats`, `cpt-cf-mini-chat-dbtable-quota-usage`
- Entities: `Chat`, `QuotaUsage`

### Model Policy Gateway, Catalog Rules and Static Policy Plugin

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-model-policy`

The system **MUST** read the model catalog, kill switches and user limits through the model-policy gateway from the plugin resolved through types-registry (spec `MiniChatModelPolicyPluginSpecV1`, selected by `vendor` and `priority`), with no local cache, and **MUST** bundle the `static_model_policy` plugin that serves a catalog from its own configuration with fixed `policy_version` 1, the six kill switches and the default per-user limits.

- The bundled plugin applies the defaults and validations of the static policy plugin algorithm (multipliers within `1..=10,000,000,000`, `kill_switches` unknown-key rejection, absent section uses defaults).
- The catalog rules (default model, enabled filter, tier order, image capability) are available to the later features as specified.

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access`
- `cpt-cf-mini-chat-algo-gear-foundation-catalog-rules`
- `cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin`

**Constraints**: `cpt-cf-mini-chat-constraint-openai-compatible`

**Touches**:
- API: none
- DB: none
- Entities: `ModelCatalogEntry`, `PolicySnapshot`

### Audit Gateway and Static Audit Plugin

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-audit`

The system **MUST** deliver audit events from the audit queue to the audit plugin resolved through types-registry (`MiniChatAuditPluginClientV1`) with the outcomes, caching rules, 30 s plugin timeout and 120-attempt dead-letter bound of ADR-0009, and **MUST** bundle the `static_audit` plugin that logs events when `enabled` is true.

- The audit event type in the SDK carries identities, model, token usage, latency, tool-call counts and the quota decision; `prompt`, `response`, `attachments`, `license` and `quota_scope` are empty and no redaction runs (ADR-0009). The event types are `turn_completed`, `turn_failed`, `turn_retry`, `turn_edit` and `turn_delete`; chat deletion is not audited.
- The enqueue of events is performed by the features that own turn finalization and turn mutation.
- Tests cover the corrupt payload (rejected with and without a plugin), no plugin (acknowledged and dropped), a missing ClientHub client (retry, with the cached instance id reset), a missing ClientHub client at the 120th attempt (`Reject`, counted as `reject`), a resolution error (retry, and `Reject` at the 120th attempt), transient and permanent plugin errors, the 120th attempt of a transient error, and the `mini_chat_audit_emit` results.

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-audit-delivery`
- `cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin`
- `cpt-cf-mini-chat-state-gear-foundation-audit-delivery`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- API: none
- DB: shared outbox tables
- Entities: `AuditEvent`

### Provider Registry and OAGW Provisioning

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-provider-provisioning`

The system **MUST** build the provider registry from the `providers.<id>` entries and tenant overrides at init, and at start exchange `client_credentials` for an S2S context and register an OAGW upstream and route for every entry and override, fail startup for a deterministically misconfigured entry, and retry an entry whose credstore secret is not yet readable on the schedule 2 s, doubling up to 60 s, with one warning after 2 minutes, in a background reconcile task that gear start spawns without waiting for it and gear stop cancels and joins.

- Credentials are held only by reference; no secret is stored or logged (`cpt-cf-mini-chat-constraint-no-credentials`).
- The four adapter kinds (`openai_responses`, `openai_chat_completions`, `vllm_responses`, `anthropic_messages`) are selectable by the registry; the adapters are completed in `cpt-cf-mini-chat-feature-message-streaming`.

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-provider-registry`
- `cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning`
- `cpt-cf-mini-chat-algo-gear-foundation-oagw-reconcile`
- `cpt-cf-mini-chat-state-gear-foundation-provider-provisioning`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`, `cpt-cf-mini-chat-constraint-openai-compatible`

**Touches**:
- API: none (OAGW upstream and route provisioning)
- DB: none
- Entities: provider entry and tenant override

### Base-License Gate

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-license-gate`

The system **MUST** declare the base license feature `gts.cf.core.lic.feat.v1~cf.core.global.base.v1` on every gear route so that api-gateway rejects an unlicensed tenant with 403 `permission_denied` and `LICENSE_FEATURE_REQUIRED` before any handler or PDP call.

- The interim gate (ADR-0008) is the P1 behavior: the `ai_chat` feature is not checked, and the gear never calls `check_user_license`.

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-license-gate`
- `cpt-cf-mini-chat-flow-gear-foundation-request-admission`

**Constraints**: `cpt-cf-mini-chat-constraint-license-gate`

**Touches**:
- API: every route
- DB: none
- Entities: none

### Metrics, Correlation and Logging Conventions

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-observability`

The system **MUST** create the OpenTelemetry instruments with the configured prefix and export them over OTLP, keep labels to the allowed low-cardinality set, start a trace per inbound request, carry the correlation identifiers where the call site knows them, and log in structured form without secrets or provider identifiers.

- The registered instruments follow the DESIGN `Emitted`, `Declared but not recorded (deferred)` and `Not defined` lists.
- Tests assert that the instruments exist under the default prefix `mini_chat` and under a custom `metrics.prefix`, that forbidden labels are never attached, and that `mini_chat_audit_emit` is recorded only at delivery.

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-observability`
- `cpt-cf-mini-chat-flow-gear-foundation-operator-observability`

**Constraints**: `cpt-cf-mini-chat-principle-owner-only-content`

**Touches**:
- API: none
- DB: none
- Entities: none

### Public Models API

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-models-api`

The system **MUST** serve `GET /v1/models` and `GET /v1/models/{id}` as read-only endpoints that return only globally enabled catalog entries, projected to the seven fields `model_id`, `display_name`, `tier`, `multiplier_display`, `description`, `multimodal_capabilities` and `context_window`, and answer 404 `not_found` (resource type `gts.cf.core.mini_chat.model.v1~`) for a disabled or unknown model.

- The response never contains `provider`, `provider_model_id`, provider deployment ids, routing metadata, credit multipliers, `credits_micro`, `policy_version`, `max_output_tokens`, `max_input_tokens` or `is_default`.
- `tier` is the lowercase value `standard` or `premium`, `description` is omitted when absent, and the list response is an object with a single `items` array and no pagination fields.
- Standard errors: 401, 403 (license or permissions), 404 for the get endpoint, plus 503 for a PDP failure and 500 for a plugin failure.
- Authorization is decision-only on the Model resource type, so a model is visible to every authorized caller of the tenant.

**Implements**:
- `cpt-cf-mini-chat-flow-gear-foundation-list-models`
- `cpt-cf-mini-chat-flow-gear-foundation-get-model`
- `cpt-cf-mini-chat-algo-gear-foundation-model-visibility`

**Constraints**: `cpt-cf-mini-chat-principle-tenant-scoped`, `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- API: `GET /v1/models`, `GET /v1/models/{id}`
- DB: none
- Entities: `ModelCatalogEntry`

### Deferred MCP Scope Stays Unimplemented

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-deferred-scope`

The system **MUST NOT** implement MCP server support: no MCP table is migrated, no MCP route is registered, no `mcp.*` configuration key is accepted (a config containing `mcp` fails startup), and the deferred requirements, components and use cases listed in sections 1.2 and 2 stay recorded as deferred (ADR-0006). The policy snapshot cache and notification endpoint, the content redaction of audit events and the chat-deletion audit event stay unimplemented as well (ADR-0008, ADR-0009).

**Implements**:
- `cpt-cf-mini-chat-algo-gear-foundation-route-registration`
- `cpt-cf-mini-chat-algo-gear-foundation-schema-migration`
- `cpt-cf-mini-chat-algo-gear-foundation-config-validation`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- API: `cpt-cf-mini-chat-interface-mcp-api` (not registered)
- DB: `cpt-cf-mini-chat-dbtable-mcp-servers`, `cpt-cf-mini-chat-dbtable-mcp-server-tools`, `cpt-cf-mini-chat-dbtable-role-mcp-servers` (not created)
- Entities: none

### Test Layering, Traceability and Feature-Local Deviations

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-gear-foundation-tests-and-deviations`

The system **MUST** cover every acceptance-criteria item that this feature owns with tests in the layers below, and **MUST** record each feature-local deviation with its rationale and review owner.

**Test layering**:
- Unit tests live in the gear crate (`gears/mini-chat/mini-chat/`) and cover pure logic: configuration validation per section, defaults and ranges, the error mapping rows, matrix lookup, PEP evaluation against a mock PDP, model visibility and projection, plugin config validation, audit delivery outcomes, provisioning schedule and the dedupe key check. Database-backed unit tests use an in-memory SQLite database.
- Integration and API tests exercise the HTTP surface in-process through the router with stubbed platform clients (PDP, types-registry, OAGW, authn resolver); no network is used. They cover the Models API, the license gate, admission outcomes (401, 403, 404, 503), the Problem shape and the cross-tenant and cross-owner isolation scenarios.
- No tests are added under `testing/e2e/suites/mini_chat/`.
- Every implemented CDSL step carries `@cpt-begin` and `@cpt-end` markers with its ID, phase token and `inst-*` ID (traceability mode FULL).

**Compile-time gates**: workspace lints (`unsafe_code` forbidden, clippy pedantic denied), the domain-model lint DE0309 on domain types, the gear-name validation (kebab-case `mini-chat`), and a build of the example server with the features `mini-chat,static-authn,static-authz,single-tenant,static-credstore`.

**Feature-local deviations** (review owner for every item: the mini-chat gear owners who maintain `gears/mini-chat/docs/`):

| Deviation | Rationale | Source |
|-----------|-----------|--------|
| Routes require the platform base license feature, not `ai_chat` | The license plugin does not expose `ai_chat` yet; revisit when it does | ADR-0008 |
| No policy snapshot cache, table or notify endpoint; every request asks the plugin | Harmless with the in-process static plugin, to be revisited before any remote policy plugin | ADR-0008 |
| Audit events carry partial content and are dropped when no plugin is registered | Blocking chat traffic on a missing optional plugin would turn it into an outage | ADR-0009 |
| Cross-column CHECKs on `chat_turns` and the `cleanup_status` CHECK are not enforced by the database | SQLite cannot add CHECKs to existing tables without a rebuild; repositories keep the invariants | ADR-0010 |
| Removed and deprecated configuration keys are handled as in the configuration algorithm | Staged removal keeps operators informed through the startup warning | ADR-0010 |
| Full correlation identifier set, the `processing_stage` log field and guaranteed child spans are not all emitted; this feature carries the identifiers that exist at each call site | DESIGN `Operational Traceability and Debugging (P1)` records the full set as target, not implemented | DESIGN 4 |
| Where `config/mini-chat.yaml` and the DESIGN differ (for example the capitalised `tier` spelling in the sample catalog), the DESIGN governs and the Models API returns lowercase `standard` or `premium` | `gears/mini-chat/docs/` is the source of truth | DECOMPOSITION 1 |

**Implements**:
- `cpt-cf-mini-chat-flow-gear-foundation-request-admission`
- `cpt-cf-mini-chat-flow-gear-foundation-list-models`
- `cpt-cf-mini-chat-flow-gear-foundation-get-model`
- `cpt-cf-mini-chat-algo-gear-foundation-error-mapping`

**Constraints**: `cpt-cf-mini-chat-constraint-license-gate`, `cpt-cf-mini-chat-principle-tenant-scoped`

**Touches**:
- API: every route owned by this feature
- DB: all tables of `cpt-cf-mini-chat-dod-gear-foundation-schema`
- Entities: `Chat`, `ModelCatalogEntry`

## 6. Acceptance Criteria

The first four items are the DECOMPOSITION 2.1 "Acceptance Criteria Covered" and the matching items of `acceptance-criteria.md`; each is both implemented and tested.

- [ ] Tenant and owner isolation is enforced on every resource: with two tenants and two users in one tenant, a user reads, lists, updates and deletes only their own chats, rows of child tables are reachable only through the owner-scoped chat, and no response or log carries a provider identifier.
- [ ] The read-only model list and get reflect only enabled catalog entries and expose no internal fields: a disabled entry is absent from `GET /v1/models` and returns 404 `not_found` (resource type `gts.cf.core.mini_chat.model.v1~`) from `GET /v1/models/{id}`, each item carries exactly `model_id`, `display_name`, `tier`, `multiplier_display`, `description` (when configured), `multimodal_capabilities` and `context_window`, and none of the non-exposed fields appears.
- [ ] All errors map to the canonical error contract, consistently across REST and streaming: every REST error is a Problem with `type`, `title`, `status`, `detail`, `instance`, `trace_id` and `context` and no top-level `code`; statuses follow the category (400, 401, 403, 404, 409, 429, 500, 503 with `Retry-After`, and 422 or 415 for the two JSON body cases); the SSE `error` event carries only `code` and `message`.
- [ ] Every operation is scoped to its owner, and cross-tenant or foreign access is rejected consistently: a hidden chat gives 404, a PDP deny gives 403 `AUTHZ_DENIED`, a PDP failure gives 503 with `Retry-After: 5`, and a decision without usable constraints gives 403.
- [ ] The authorization matrix uses the exact action name, `resource.id` presence and `require_constraints` flag of DESIGN 3.8 for each of the 19 operations, and `messages:stream` evaluates `send_message` only.
- [ ] An unlicensed tenant is rejected with 403 and `LICENSE_FEATURE_REQUIRED` on every route before any PDP call, and a licensed tenant is admitted with the base license feature only.
- [ ] The configuration rejects unknown keys at the top level and in the listed sections, ignores them in the four worker sections, fails on the removed keys and on `mcp`, applies the Appendix B defaults, and rejects every out-of-range value named in the configuration algorithm.
- [ ] Migrations create the nine P1 tables and the outbox tables on PostgreSQL and SQLite with the constraints and indexes of DESIGN 3.7, create no MCP table and no `tenant_closure` table, and leave the unenforced CHECKs out.
- [ ] Gear start registers five outbox queues with the configured names, `outbox.num_partitions` partitions and the specified leases, and gear stop joins the workers before it stops the outbox pipeline.
- [ ] Gear start exchanges the S2S credentials, provisions one upstream and route per provider entry and tenant override, fails for a deterministically misconfigured entry, and completes with deferred entries still pending; a background reconcile task retries a deferred entry after 2 s with the interval doubling up to 60 s and one warning after 2 minutes, and gear stop cancels and joins that task.
- [ ] The static model policy plugin serves its configured catalog with `policy_version` 1, the six kill switches and the default limits, rejects multipliers outside `1..=10,000,000,000`, and the static audit plugin logs events only while `enabled` is true; both modules stay importable as `mini_chat::infra::plugins::static_model_policy` and `mini_chat::infra::plugins::static_audit`.
- [ ] Audit delivery acknowledges and drops events when no plugin is registered, retries when the plugin client is missing or fails transiently, dead-letters corrupt payloads, permanent errors and the 120th attempt of every retry path (including a missing plugin client and a resolution error), and records `mini_chat_audit_emit` with the matching `result`.
- [ ] Metrics are exported with the configured prefix and low-cardinality labels only, and the example server builds and starts with the features `mini-chat,static-authn,static-authz,single-tenant,static-credstore`.
- [ ] Unit tests live in the gear crate, integration tests run in-process over the HTTP surface without a network, no test is added under `testing/e2e/suites/mini_chat/`, and every implemented CDSL step has `@cpt-begin` and `@cpt-end` markers.
