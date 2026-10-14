---
description: "Specifies the Quota and Billing feature of the mini-chat gear: credit arithmetic with overflow protection, token estimation, the preflight reserve with the premium-to-standard downgrade cascade over daily and monthly buckets, the daily web search and code interpreter quota check, CAS-guarded exactly-once settlement by terminal outcome, the usage outbox publication to the model-policy plugin, the quota status API with warning thresholds, and the cost metrics."
---

# Feature: Quota and Billing


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Chat User Reads Quota Status](#chat-user-reads-quota-status)
  - [Chat User Turn Is Admitted Under Quota](#chat-user-turn-is-admitted-under-quota)
  - [Chat User Requests a Tool Under Daily Quota](#chat-user-requests-a-tool-under-daily-quota)
  - [Chat User Turn Reaches a Terminal Outcome and Usage Is Published](#chat-user-turn-reaches-a-terminal-outcome-and-usage-is-published)
  - [Admin Observes Cost Metrics and Quota Alerts](#admin-observes-cost-metrics-and-quota-alerts)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Period Boundaries](#period-boundaries)
  - [Credit Arithmetic](#credit-arithmetic)
  - [Policy Binding](#policy-binding)
  - [Token Estimation](#token-estimation)
  - [Reserve Calculation](#reserve-calculation)
  - [Tier Availability](#tier-availability)
  - [Downgrade Cascade](#downgrade-cascade)
  - [Daily Tool Quota Check](#daily-tool-quota-check)
  - [Preflight Reserve](#preflight-reserve)
  - [Reserve Write](#reserve-write)
  - [Quota Rejection Mapping](#quota-rejection-mapping)
  - [Billing Outcome Derivation](#billing-outcome-derivation)
  - [Settlement Computation](#settlement-computation)
  - [Finalization Settlement](#finalization-settlement)
  - [Usage Event Construction](#usage-event-construction)
  - [Usage Outbox Handler](#usage-outbox-handler)
  - [Quota Status Computation](#quota-status-computation)
  - [Quota Warnings Projection](#quota-warnings-projection)
  - [Cost Metrics](#cost-metrics)
  - [Quota Configuration](#quota-configuration)
- [4. States (CDSL)](#4-states-cdsl)
  - [Turn Billing State Machine](#turn-billing-state-machine)
  - [Turn Reserve Lifecycle State Machine](#turn-reserve-lifecycle-state-machine)
  - [Usage Event Delivery State Machine](#usage-event-delivery-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Credit Arithmetic and Overflow Protection](#credit-arithmetic-and-overflow-protection)
  - [Period Boundaries and Bucket Rows](#period-boundaries-and-bucket-rows)
  - [Token Estimation and Reserve Calculation](#token-estimation-and-reserve-calculation)
  - [Preflight Reserve and Tier Downgrade](#preflight-reserve-and-tier-downgrade)
  - [Reserve Write With Limit Re-Check](#reserve-write-with-limit-re-check)
  - [Policy Version Persisted Per Turn](#policy-version-persisted-per-turn)
  - [Daily Web Search and Code Interpreter Quota (Preflight Part)](#daily-web-search-and-code-interpreter-quota-preflight-part)
  - [Quota Rejection Contract](#quota-rejection-contract)
  - [Settlement Computation by Terminal Outcome](#settlement-computation-by-terminal-outcome)
  - [CAS-Guarded Settlement With Exactly-One Usage Message](#cas-guarded-settlement-with-exactly-one-usage-message)
  - [Usage Event Payload and Dedupe Key](#usage-event-payload-and-dedupe-key)
  - [Reliable Usage Publication](#reliable-usage-publication)
  - [Quota Status API](#quota-status-api)
  - [Warning Thresholds and Quota Warnings Inputs](#warning-thresholds-and-quota-warnings-inputs)
  - [Cost Metrics and Quota Alerts](#cost-metrics-and-quota-alerts)
  - [Deferred and Not-Implemented Scope Stays Unimplemented](#deferred-and-not-implemented-scope-stays-unimplemented)
  - [Test Layering, Traceability and Feature-Local Deviations](#test-layering-traceability-and-feature-local-deviations)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-featstatus-quota-billing-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-mini-chat-feature-quota-and-billing`

<!--
=============================================================================
FEATURE SPECIFICATION: Quota and Billing (mini-chat)
Source entry: DECOMPOSITION section 2.3 (gears/mini-chat/docs/DECOMPOSITION.md)
Precedence: gears/mini-chat/docs/ (PRD, DESIGN, ADR, acceptance-criteria) wins over
config/mini-chat.yaml and over platform documents under docs/ wherever they disagree.
=============================================================================
-->
## 1. Feature Context

### 1.1 Overview

The Quota and Billing feature bounds and accounts for the LLM cost of every user turn of the `mini-chat` gear. It provides credit arithmetic with overflow protection, two-phase token estimation, the preflight reserve with the premium-to-standard downgrade cascade over the daily and monthly `total` and `tier:premium` buckets, the daily web search and code interpreter quota check, CAS-guarded exactly-once settlement for every terminal outcome, reliable usage publication through the outbox to the model-policy plugin, the quota status API with warning thresholds, and the cost metrics series.

### 1.2 Purpose

The DESIGN requires reserve-before-execute and exactly-once settlement for every provider call of a user turn. This feature is built before the streaming slice so that the streaming feature consumes finished reserve and settlement contracts instead of defining them: no provider call can start without a committed reserve, no turn can settle twice, and no committed quota debit can exist without a durably enqueued usage message.

Scope boundary: this feature owns the contracts and their verification at domain and repository level, plus the quota status endpoint. The HTTP/SSE send pipeline that calls them, the composition of the turn-finalization transaction (assistant message, CAS update shape per outcome, audit event, thread-summary trigger), tool-start detection, the per-message tool call limits, the orphan watchdog and the thread-summary worker belong to other features. End-to-end verification of the contracts inside a real turn happens in `cpt-cf-mini-chat-feature-message-streaming`; here the contracts are verified against seeded rows, the SQLite repositories and the in-process quota status route.

**Requirements**: `cpt-cf-mini-chat-fr-quota-enforcement`, `cpt-cf-mini-chat-fr-quota-billing-architecture`, `cpt-cf-mini-chat-fr-cost-metrics`, `cpt-cf-mini-chat-nfr-cost-control`

**Principles**: none. No design principle is specific to quota; cost bounds are expressed as the constraint below and as requirements.

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Design elements**: `cpt-cf-mini-chat-component-quota-service`, `cpt-cf-mini-chat-design-quota-usage-accounting`, `cpt-cf-mini-chat-design-throttling-tiers`, `cpt-cf-mini-chat-design-nonterminal-reconciliation`, `cpt-cf-mini-chat-design-terminal-error-reconciliation`, `cpt-cf-mini-chat-flow-quota-enforced-chat-turn`, `cpt-cf-mini-chat-algo-quota-billing-token-estimation`, `cpt-cf-mini-chat-dod-policy-version-per-turn`, `cpt-cf-mini-chat-dod-preflight-reserve-and-downgrade`, `cpt-cf-mini-chat-dod-cas-guarded-settlement-and-outbox`. The DESIGN also defines `cpt-cf-mini-chat-featstatus-quota-billing-implemented` in section 5; this document carries the same featstatus identifier as its status marker.

**Database tables used (created by `cpt-cf-mini-chat-feature-gear-foundation`; this feature owns their reserve, settlement and read behavior)**: `cpt-cf-mini-chat-dbtable-quota-usage` (all bucket counters) and `cpt-cf-mini-chat-dbtable-chat-turns` (the preflight billing columns `reserve_tokens`, `max_output_tokens_applied`, `reserved_credits_micro`, `policy_version_applied`, `effective_model`, `minimal_generation_floor_applied`, and the CAS on `state`).

**Domain entities**: `QuotaUsage`, `UserLimits` and tier definitions, the turn billing state and `UsageEvent`.

**ADRs traced**: `cpt-cf-mini-chat-adr-quota-policy-scope` (the quota, policy and licensing scope: what is and is not implemented, TOCTOU resolution, per-candidate reserve, estimation budgets source), `cpt-cf-mini-chat-adr-canonical-error-contract` (429 `resource_exhausted` shape), `cpt-cf-mini-chat-adr-runtime-consistency-limitations` (separate orphan finalization path, replay payload limits), `cpt-cf-mini-chat-adr-document-retrieval-scope` (no per-user daily `file_search` limit), `cpt-cf-mini-chat-adr-group-chat-usage-attribution` (deferred, P1 has single-user chats only).

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-mini-chat-actor-chat-user` | Sends turns that are admitted, downgraded or rejected by the quota service, has each turn settled against the personal credit wallet, reads the quota status through `GET /v1/quota/status`, and is rejected with 429 when a token or daily tool quota is exhausted. |
| `cpt-cf-mini-chat-actor-admin` | Observes aggregated cost and quota behavior through the metric series and alerts of this feature. Never has access to chat content and never reads per-user usage through labels, because metric labels carry no tenant or user identifier. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) section 2.3 (`Quota and Billing`)
- **Acceptance criteria**: [acceptance-criteria.md](../acceptance-criteria.md)
- **ADRs (all accepted)**: [ADR-0003 group chat usage attribution](../ADR/0003-cpt-cf-mini-chat-adr-group-chat-usage-attribution.md), [ADR-0004 canonical error contract](../ADR/0004-cpt-cf-mini-chat-adr-canonical-error-contract.md), [ADR-0007 document retrieval scope](../ADR/0007-cpt-cf-mini-chat-adr-document-retrieval-scope.md), [ADR-0008 quota and policy scope](../ADR/0008-cpt-cf-mini-chat-adr-quota-policy-scope.md), [ADR-0010 runtime and consistency limitations](../ADR/0010-cpt-cf-mini-chat-adr-runtime-consistency-limitations.md)
- **Reference OpenAPI** for `GET /v1/quota/status` (operation `mini_chat.get_quota_status`, schemas `QuotaStatusResponse`, `QuotaTierStatus`, `QuotaPeriodStatus`, `QuotaTier`, `QuotaPeriod`, `QuotaWarning`): `.cf-studio/.cache/mini-chat/openapi-mini-chat.json`
- **Sibling features**: [0001-gear-foundation.md](./0001-gear-foundation.md) (outbox queues and the dedupe key prefix check, model-policy gateway and policy snapshot access, catalog rules and tier order, kill switches, error mapping, PEP and the `UserQuota` resource, configuration validation, scoped persistence, schema), [0002-chat-management.md](./0002-chat-management.md) (independent of this feature; the two are developed in parallel)
- **Dependencies**: `cpt-cf-mini-chat-feature-gear-foundation`. Features that consume the contracts of this feature: `cpt-cf-mini-chat-feature-message-streaming` (send pipeline, finalization transaction composition, tool-start detection and per-message limits), `cpt-cf-mini-chat-feature-turn-mutations-and-reactions` (retry and edit run the same preflight and reserve), and `cpt-cf-mini-chat-feature-background-maintenance` (the orphan watchdog calls the settlement contract; the thread-summary worker enqueues system-task usage events with the payload contract defined here).

**Normative value sources** (the DESIGN holds the exact values; this document states the values it must preserve and names the DESIGN heading that carries the rest):

| Topic | DESIGN location (section and heading) |
|-------|---------------------------------------|
| Quota service, two-phase counting, tier availability rule, streaming constraint, cancel and disconnect rule | 3.2 Component Model, `quota service` |
| Period reset semantics and the period boundary invariant | 3.2 Component Model, `Quota Period Reset Semantics` |
| Warning thresholds and the quota status endpoint | 3.2 Component Model, `Quota Warning Thresholds (P1)`, `Quota Status Endpoint (P1)`; PRD `Quota Status API (read-only)` |
| System task attribution, isolation and usage payload | 3.2 Component Model, `System Task Isolation Invariant (P1)`, `System Task Attribution Rules (Normative)` |
| `quota_usage` columns, bucket model, commit semantics, bucket naming | 3.7 Database Schemas & Tables, `Table: quota_usage`, `Bucket Semantics and Naming Clarification (Normative)` |
| `chat_turns` billing columns and preflight columns on retry and edit | 3.7 Database Schemas & Tables, `Table: chat_turns` |
| 429 `resource_exhausted` shape, quota scopes, 400 `FEATURE_DISABLED` | 3.3 API Contracts, `Error Codes` (REST error mapping), `Streaming error codes` |
| Downgrade matrix and vision capability after downgrade | 2.2 Constraints, `Downgrade Decision Matrix`, `Quota Before Outbound` |
| Web search and code interpreter daily quota, kill switches | 4 Additional Context, `Web Search Quota Enforcement (P1 Determinism Rules)`, `Code Interpreter Quota Enforcement (P1 Determinism Rules)` |
| Three ownership tiers, quota counting flow, cascade truth table, downgrade decision flow, effective model computed once | 4 Additional Context, `Two-Tier Rate Limiting & Throttling`, `Downgrade Decision Flow`, `Effective Model Computed Once (P1 Determinism Rule)` |
| Metrics, naming and cardinality, alerts | 4 Additional Context, `Metrics (OTLP) and Alerts (P1)` |
| Policy plugin contract, snapshot and user limits, estimation budgets source, deterministic settlement | 5.2 Policy Plugin, `5.2.1`, `5.2.2`, `5.2.6`, `5.2.9` |
| Credit arithmetic, overflow protection, reserve and settlement glossary | 5.3 Credit Arithmetic, `Overflow Protection (Normative)`, `5.3.1 Reserve vs Settlement Variables (Canonical Glossary)` |
| Reserve, period enforcement, TOCTOU, writing the reserve, finalization settlement, overshoot | 5.4 Quota Enforcement Flow, `5.4.1` to `5.4.5` |
| Token estimation | 5.5 Token Estimation Strategy (P1), `5.5.3` to `5.5.14` |
| Usage outbox, dedupe key, UUID normalization, handler | 5.6 Reliable Usage Event Publication, `UUID Normalization (Normative)`, `Shared Outbox Processing Model (P1)` |
| Terminal outcomes, CAS winner, outbox validation, usage accounting rules | 5.7 Turn Finalization Contract (P1) |
| Billing state, error code taxonomy, billing outcome derivation, charged token formula, completeness | 5.8 Non-Terminal Stream Reconciliation Invariant |
| Failed invocation reconciliation, race resolution | 5.9 Terminal Error Reconciliation Rule |
| Worked numbers | 5.10 Calculation Example |
| Wire fields of the usage event | Appendix A, `A.2 Credit Accounting Model`, `A.3 Usage Reporting API (PublishUsage)` |
| Configuration keys and defaults | Appendix B, `B.1`, `B.5.2`, `B.5.3`, `B.6`, `B.6.1`, `B.9.3` |

## 2. Actor Flows (CDSL)

User-facing and operator-facing interactions that start with an actor and describe how the quota and billing contracts behave end to end. The send pipeline that calls the quota contracts is owned by `cpt-cf-mini-chat-feature-message-streaming`; the flows below describe the quota-visible part of that pipeline and the behavior of the contracts it calls.

**Use cases**: `cpt-cf-mini-chat-usecase-send-message` is realized here only in its quota steps: step 2 (preflight reserve), the usage counters update of its postconditions, and its alternative flow `Quota exceeded` (429 `resource_exhausted`, no LLM call, no SSE stream). Its remaining steps belong to `cpt-cf-mini-chat-feature-message-streaming`.

### Chat User Reads Quota Status

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-quota-and-billing-quota-status`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- The user receives HTTP 200 with `tiers` and `warning_threshold_pct`. Each tier is `premium` or `total` and carries one entry per period (`daily`, `monthly`) whose limit is greater than 0, with `limit_credits_micro`, `used_credits_micro`, `remaining_credits_micro`, `remaining_percentage`, `next_reset`, `warning` and `exhausted`.
- A user with no usage rows in the current periods sees `used_credits_micro` equal to 0 and `remaining_percentage` equal to 100 for every listed period.
- A user whose limits are all 0 or negative receives an empty `tiers` array.

**Error Scenarios**:
- The bearer token is missing, invalid or expired: 401 `unauthenticated`.
- The tenant lacks the base license feature: 403 `permission_denied` with `LICENSE_FEATURE_REQUIRED`.
- The PDP denies the request or returns no usable constraints: 403 `permission_denied` with `AUTHZ_DENIED`.
- The PDP cannot evaluate the request: 503 `service_unavailable` with `Retry-After: 5`.
- The model-policy plugin cannot be resolved or fails, or the usage read fails: 500 `internal`.

**Steps**:
1. [x] - `p1` - Chat User sends `GET /v1/quota/status` with a bearer token to the gear URL prefix (`url_prefix`, default `/mini-chat`) and no query parameters - `inst-status-request`
2. [x] - `p1` - Run the request admission flow `cpt-cf-mini-chat-flow-gear-foundation-request-admission` for the operation `GET /v1/quota/status`, which authenticates the caller, applies the license gate and evaluates the action `read` on the `UserQuota` resource with no `resource.id` and constraints required - `inst-status-admission`
3. [x] - `p1` - **IF** admission returned a refusal - `inst-status-refused`
   1. [x] - `p1` - **RETURN** the refusal Problem unchanged (401, 403 or 503) and read nothing - `inst-status-return-refusal`
4. [x] - `p1` - Resolve the current policy version and the calling user's limits through the policy binding algorithm - `inst-status-policy`
5. [x] - `p1` - **IF** the policy could not be resolved - `inst-status-policy-failed`
   1. [x] - `p1` - **RETURN** 500 `internal` with a generic detail and log the cause - `inst-status-return-500-policy`
6. [x] - `p1` - Compute the `period_start` of the daily and monthly period for the current UTC instant through the period boundaries algorithm - `inst-status-periods`
7. [x] - `p1` - DB: SELECT `quota_usage` rows through the `AccessScope` (tenant predicate and the owner predicate on `user_id`) for the buckets `total` and `tier:premium`, the period types `daily` and `monthly` and the computed `period_start` values, in one consistent read - `inst-status-read-rows`
8. [x] - `p1` - **IF** the read failed - `inst-status-read-failed`
   1. [x] - `p1` - **RETURN** 500 `internal` with a generic detail and log the cause - `inst-status-return-500-read`
9. [x] - `p1` - Compute the tiers and periods through the quota status computation algorithm - `inst-status-compute`
10. [x] - `p1` - **RETURN** 200 with `tiers` and `warning_threshold_pct` equal to the configured `quota.warning_threshold_pct`; billing outcomes, settlement details, web search and code interpreter counters are never part of the response - `inst-status-return`

### Chat User Turn Is Admitted Under Quota

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-flow-quota-and-billing-turn-admission`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- The turn is allowed at the selected model, or downgraded to a lower-tier model, a reserve of worst-case credits is committed in the `quota_usage` bucket rows together with the preflight columns of the turn, and the provider call may begin with `max_output_tokens_applied` as its hard cap.

**Error Scenarios**:
- `web_search.enabled` is true while the `disable_web_search` kill switch is on: 400 `failed_precondition`, `violations[{subject: web_search, type: FEATURE_DISABLED}]`, before any estimation.
- No tier is available in the cascade (quota exhausted in any period or bucket, no enabled model in any tier of the cascade, or a reserve that cannot be computed): 429 `resource_exhausted`, quota scope `tokens`.
- The daily web search or code interpreter quota is exhausted for a request that uses the tool: 429 `resource_exhausted`, quota scope `web_search` or `code_interpreter`.
- The reserve re-check finds a bucket over its limit after the increments (a concurrent turn took the remaining quota): 429 `resource_exhausted`, quota scope `tokens`, the reserve transaction rolled back, and no downgrade is attempted at this point.
- The policy plugin cannot be resolved or fails: 500 `internal`.
- Every error above is a normal JSON Problem; no SSE stream is opened and no provider call is made.

**Steps**:
1. [ ] - `p1` - Chat User sends a new message, or retries or edits the last turn; the send pipeline of `cpt-cf-mini-chat-feature-message-streaming` has already passed admission, the replay check and the parallel-turn guard, so a replayed turn never reaches this flow - `inst-admit-request`
2. [ ] - `p1` - Run the preflight reserve algorithm for the user, the selected model and the request facts (message bytes, image count, ready documents, ready XLSX attachments, `web_search.enabled`) - `inst-admit-preflight`
3. [x] - `p1` - **IF** preflight returned a rejection - `inst-admit-rejected`
   1. [x] - `p1` - **RETURN** the Problem built by the quota rejection mapping algorithm (400 `FEATURE_DISABLED`, 429, or 500) and take no reserve - `inst-admit-return-rejection`
4. [ ] - `p1` - Hand the decision to the send pipeline for its remaining pre-provider steps: the `INPUT_TOO_LONG` check with the supplied input estimate, the image guards using the vision capability of the effective model, context assembly and provider resolution for the effective provider; every one of them runs before the reserve, so a failure there leaves no turn and no reserve - `inst-admit-pipeline-steps`
5. [ ] - `p1` - Run the reserve write algorithm inside the reserve transaction of the pipeline, which also creates the user message and the `running` turn on the send path, or fills the NULL preflight columns of the already committed turn on retry and edit - `inst-admit-reserve-write`
6. [x] - `p1` - **IF** the reserve write reported a bucket over its limit - `inst-admit-recheck-failed`
   1. [x] - `p1` - **RETURN** 429 `resource_exhausted` with quota scope `tokens` through the quota rejection mapping algorithm; the transaction is rolled back, so the send path leaves no user message, no turn and no reserve, while on retry and edit the already committed new turn is marked `failed` with `error_code` `quota_exceeded` by the turn mutation flow and no settlement or usage event follows - `inst-admit-return-recheck`
7. [ ] - `p1` - Start the provider call only after the reserve transaction committed, with `max_output_tokens_applied` as the hard output cap - `inst-admit-provider-call`
8. [x] - `p1` - **RETURN** the decision facts that the pipeline reports in the `done` event: `quota_decision` (`allow` or `downgrade`), `effective_model`, `selected_model`, and for a downgrade `downgrade_from` (equal to `selected_model`) and `downgrade_reason` - `inst-admit-return-decision`

### Chat User Requests a Tool Under Daily Quota

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-flow-quota-and-billing-tool-quota-request`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- A request that enables web search, or whose chat has a ready XLSX attachment, is admitted while the user's daily call count for that tool is below its daily quota.
- A request that does not use the tool is never rejected by that tool's quota, however many calls were used today.

**Error Scenarios**:
- The daily call count has reached `quota.web_search_daily_quota`: 429 `resource_exhausted` with quota scope `web_search`.
- The daily call count has reached `quota.code_interpreter_daily_quota`: 429 `resource_exhausted` with quota scope `code_interpreter`.

**Steps**:
1. [ ] - `p1` - Chat User sends a turn with `web_search.enabled` true, or into a chat with at least one ready code-interpreter (XLSX) attachment, through the send pipeline - `inst-toolreq-request`
2. [x] - `p1` - Run the preflight reserve algorithm, which runs the daily tool quota check for the effective model after the cascade picked it - `inst-toolreq-preflight`
3. [x] - `p1` - **IF** the tool quota check rejected the request - `inst-toolreq-rejected`
   1. [x] - `p1` - **RETURN** 429 `resource_exhausted` with the tool as quota scope through the quota rejection mapping algorithm; no reserve is written, no counter changes and no provider call is made - `inst-toolreq-return-429`
4. [x] - `p1` - Continue with the admission flow; the call counters of the turn are added to the daily `total` bucket row at settlement, never at preflight - `inst-toolreq-continue`
5. [ ] - `p1` - **RETURN** the admission result - `inst-toolreq-return`

### Chat User Turn Reaches a Terminal Outcome and Usage Is Published

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-flow-quota-and-billing-turn-settlement`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- A completed turn is settled on the provider-reported usage, a failed turn that reached the provider on the reported usage or the estimate, a cancelled or orphaned turn on the estimate, and a turn that never reached the provider releases its reserve at zero charge.
- Exactly one usage event per reserved turn is durably enqueued in the settlement transaction and delivered at least once to the model-policy plugin.

**Error Scenarios**:
- Another finalizer already moved the turn out of `running`: the call is an idempotent no-op with no quota change and no usage event.
- The credit computation or the policy snapshot of the turn's `policy_version_applied` fails: the whole transaction rolls back, the turn stays `running` and the caller handles it (the stream sends SSE `error` `finalization_failed`; the orphan watchdog retries on its next scan).
- The usage message cannot be enqueued: the whole transaction rolls back, so quota is never debited without a usage message.

**Steps**:
1. [ ] - `p1` - A terminal trigger fires for a turn that holds a reserve: provider `done` or `incomplete`, a provider terminal error, a client disconnect, a tool-limit breach, a message persistence failure, or the orphan watchdog timeout - `inst-settle-trigger`
2. [ ] - `p1` - The finalizing path opens its finalization transaction and calls the finalization settlement algorithm, supplying the turn row, the terminal trigger classification, the provider usage when known, the completed tool call counts and the period starts of the reserve - `inst-settle-call`
3. [x] - `p1` - **IF** the algorithm reported that the CAS was lost - `inst-settle-cas-lost`
   1. [x] - `p1` - **RETURN** without changing quota, enqueuing a usage message or emitting a terminal event; the Turn Status API is the authoritative source of the final state - `inst-settle-return-noop`
4. [x] - `p1` - Commit the finalization transaction, which contains the CAS state transition, the bucket updates and the usage message enqueue together - `inst-settle-commit`
5. [x] - `p1` - The usage outbox handler of the `outbox.queue_name` queue delivers the usage message to the model-policy plugin with `publish_usage` - `inst-settle-publish`
6. [ ] - `p1` - The downstream consumer deduplicates the delivery by `(tenant_id, turn_id, request_id)` or the equivalent `dedupe_key` and debits `actual_credits_micro` without recomputing it - `inst-settle-consumer`
7. [x] - `p1` - **RETURN** the settlement result (billing outcome, settlement method and committed credits) to the finalizing path, which emits the terminal SSE event only after this commit - `inst-settle-return`

### Admin Observes Cost Metrics and Quota Alerts

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-flow-quota-and-billing-admin-cost-observability`

**Actor**: `cpt-cf-mini-chat-actor-admin`

**Success Scenarios**:
- The admin reads the quota and cost series exported over OTLP with the configured prefix and low-cardinality labels, and can tell premium from standard usage, downgrades from allows, and overshoot frequency, without any tenant or user identifier and without chat content.

**Error Scenarios**:
- An alert named in the DESIGN is backed by a series that is declared but not recorded or not defined: the alert cannot fire, and the feature states this instead of inventing a recording.

**Steps**:
1. [ ] - `p1` - Admin opens the OTLP metrics of the gear, whose names use the prefix `metrics.prefix` (default `mini_chat`) - `inst-admin-open-metrics`
2. [ ] - `p1` - Read the quota series produced by the cost metrics algorithm: `mini_chat_quota_preflight`, `mini_chat_quota_reserve`, `mini_chat_quota_commit`, `mini_chat_quota_overshoot`, `mini_chat_quota_estimated_tokens`, `mini_chat_quota_actual_tokens` and `mini_chat_code_interpreter_calls` - `inst-admin-read-series`
3. [ ] - `p1` - **IF** the share of `mini_chat_quota_overshoot` against completed turns exceeds 5 percent - `inst-admin-overshoot-high`
   1. [ ] - `p1` - Review the catalog `estimation_budgets` (`image_token_budget`, `tool_surcharge_tokens`, `web_search_surcharge_tokens`, `safety_margin_pct`) and consider larger conservative margins - `inst-admin-tune-budgets`
4. [ ] - `p1` - Treat the usage dead-letter alerts and the `mini_chat_quota_negative` alert as not able to fire in P1, because their series are not defined or declared but not recorded - `inst-admin-unfirable-alerts`
5. [ ] - `p1` - **RETURN** the aggregated view; per-user and per-tenant attribution is available only in the usage records and audit events, never in metric labels - `inst-admin-return`

## 3. Processes / Business Logic (CDSL)

Internal functions and procedures that the flows above and the other features call. Credits are integers (`credits_micro`, one credit is `1_000_000` `credits_micro`) and floating-point arithmetic is never used in credit computation; the only floating-point comparison is the overshoot ratio of the settlement computation.

### Period Boundaries

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-and-billing-period-boundaries`

**Input**: a UTC instant, and optionally the `started_at` of a turn

**Output**: the `period_start` date of the `daily` and `monthly` periods, and the `next_reset` timestamp of each period. The rules are DESIGN 3.2 `Quota Period Reset Semantics` and 5.4.2 `Period Enforcement and Tier Downgrade`.

**Steps**:
1. [x] - `p1` - Compute the `daily` `period_start` as the UTC calendar date of the instant, and the `monthly` `period_start` as the first day of the UTC month of the instant; both are stored as DATE values in `quota_usage.period_start` - `inst-period-start`
2. [ ] - `p1` - Check against the normative examples: `2026-02-28T15:30:00Z` gives daily `2026-02-28` and monthly `2026-02-01`; `2026-02-28T23:59:59Z` gives daily `2026-02-28`; `2026-03-01T00:00:00Z` gives daily `2026-03-01` - `inst-period-examples`
3. [x] - `p1` - Make the computation deterministic, so that two requests in the same UTC second compute identical values and contend on the same `quota_usage` row - `inst-period-determinism`
4. [x] - `p1` - Compute `next_reset` as an RFC 3339 timestamp: 00:00 UTC of the next day for `daily`, and 00:00 UTC of the first day of the next month for `monthly` - `inst-period-next-reset`
5. [x] - `p1` - Compute the period starts once at preflight and carry them with the reserve; settlement targets exactly those rows and never recomputes them from the current clock, so a turn that started at 23:59:59 UTC and ended at 00:00:01 UTC is settled against the day-1 rows - `inst-period-boundary-invariant`
6. [x] - `p1` - Capture the preflight instant exactly once, from the application UTC clock, at the start of the preflight, and use that one instant for both the period start computation and the `chat_turns.started_at` written by the reserve write (the foundation schema has no column for the period starts, so `started_at` is the persisted carrier of the DESIGN 3.2 boundary invariant); never read the clock a second time for either purpose - `inst-period-single-instant`
7. [x] - `p1` - For a finalizer without the preflight context (the orphan watchdog), derive the starts from `chat_turns.started_at`: daily is the UTC date of `started_at`, monthly is the first day of that month; because `started_at` is the preflight instant of step 6, this derivation always yields the rows that were reserved, also when the settlement happens after a UTC midnight or month boundary - `inst-period-from-started-at`
8. [x] - `p1` - Support only the `daily` and `monthly` periods in the UTC calendar; per-tenant timezones, 4-hour windows and weekly windows are deferred to P2+ - `inst-period-p1-scope`
9. [x] - `p1` - **RETURN** the period starts and the next reset timestamps - `inst-period-return`

### Credit Arithmetic

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic`

**Input**: `input_tokens`, `output_tokens`, `in_mult` (`input_tokens_credit_multiplier_micro`) and `out_mult` (`output_tokens_credit_multiplier_micro`) of the model entry in the applicable policy snapshot

**Output**: `credits_micro`, or a computation error. The formula is DESIGN 5.3 and Appendix A.2: the input product divided by `1_000_000` rounded up, plus the output product divided by `1_000_000` rounded up.

**Steps**:
1. [x] - `p1` - Treat each multiplier as micro-credits per 1,000,000 tokens (a multiplier of `1_000_000_000` charges one credit per 1,000 tokens) - `inst-credit-unit`
2. [x] - `p1` - Reject a token count that is negative or above `10,000,000`, as an invalid token count - `inst-credit-token-bounds`
3. [x] - `p1` - Reject a multiplier of 0 as a zero-multiplier error, because it would make that usage free, and reject a multiplier above `10,000,000,000` as a separate out-of-range multiplier error; both checks run before any multiplication - `inst-credit-multiplier-bounds`
4. [x] - `p1` - Multiply each token count by its multiplier with checked arithmetic and report an overflow as an error - `inst-credit-checked-multiply`
5. [x] - `p1` - Divide each product by `1_000_000` rounding up, separately for input and output and never on the sum, then add the two components with checked arithmetic; the per-component rounding may differ from rounding the sum by up to 1 micro-credit, which is intentional - `inst-credit-ceil-per-component`
6. [x] - `p1` - Use only the total `input_tokens` and `output_tokens`; the cache read, cache write and reasoning breakdowns are telemetry and apply no discount - `inst-credit-totals-only`
7. [ ] - `p1` - Verify against the worked values: 1,000 input and 500 output tokens at `2_500_000_000` for both multipliers give `2_500_000 + 1_250_000 = 3_750_000`; at `1_000_000_000` they give `1_000_000 + 500_000 = 1_500_000`; 900 input and 300 output tokens at `1_000_000_000` give `1_200_000` - `inst-credit-worked-values`
8. [x] - `p1` - **IF** the computation failed - `inst-credit-failed`
   1. [x] - `p1` - At preflight, let the caller substitute `i64::MAX` for the reserve of that candidate, so that it fits no bucket and counts as unavailable (no 500 is returned); at settlement, map the error to an internal domain error that fails the finalization transaction and names the offending token count or multiplier in a warning - `inst-credit-error-handling`
9. [x] - `p1` - Do not validate provider-reported usage when the provider response is parsed; an out-of-range count is caught here at settlement - `inst-credit-no-parse-validation`
10. [x] - `p1` - **RETURN** `credits_micro` - `inst-credit-return`

### Policy Binding

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-and-billing-policy-binding`

**Input**: the tenant, the user, and for settlement the `policy_version_applied` of the turn

**Output**: the current policy version, the snapshot and the user limits for preflight and status; or the snapshot of the turn's applied version for settlement. The contract is DESIGN 5.2 (`5.2.1`, `5.2.2`, `5.2.6`, `5.2.9`).

**Steps**:
1. [x] - `p1` - At preflight and for the status endpoint, obtain the current policy version, the policy snapshot (model catalog with multipliers and `estimation_budgets`, the six kill switches) and the `UserLimits` of the user through the policy snapshot access algorithm of `cpt-cf-mini-chat-feature-gear-foundation`, keeping `UserLimits` apart from the shared snapshot - `inst-policy-current`
2. [x] - `p1` - Map the limits to buckets: bucket `total` takes `user_limits.standard` (`limit_daily_credits_micro`, `limit_monthly_credits_micro`) and is the global ceiling over all tiers, not a standard-only cap; bucket `tier:premium` takes `user_limits.premium` and is a sub-cap that premium spend counts against in addition to `total` - `inst-policy-limit-mapping`
3. [x] - `p1` - Persist the version bound at preflight as `chat_turns.policy_version_applied`, written once and immutable, and carry the same value in the usage event - `inst-policy-persist-version`
4. [x] - `p1` - At settlement, load only the snapshot of the turn's `policy_version_applied`, read the multipliers from the entry of the turn's persisted `effective_model` in that snapshot, and never read the current version, never re-run the cascade and never call CCM - `inst-policy-settlement-snapshot`
5. [x] - `p1` - Keep no snapshot cache, snapshot table or policy notification endpoint, and never call `check_user_license` (ADR-0008) - `inst-policy-no-cache`
6. [x] - `p1` - **IF** the plugin cannot be resolved or fails - `inst-policy-failed`
   1. [x] - `p1` - **RETURN** a plugin error: the caller maps it to 500 `internal` at preflight and for the status endpoint, and fails the finalization transaction at settlement - `inst-policy-return-error`
7. [x] - `p1` - **RETURN** the version, snapshot and limits, or the snapshot of the applied version - `inst-policy-return`

### Token Estimation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-and-billing-token-estimation`

**Input**: the `estimation_budgets` of a candidate catalog entry, the UTF-8 byte length of the current message, the image count, the chat facts (ready documents, ready XLSX attachments), the request flag `web_search.enabled`, the kill switches and the candidate's `tool_support`

**Output**: `estimated_text_tokens`, `prior_context_tokens`, the surcharges and `estimated_input_tokens`; this realizes `cpt-cf-mini-chat-algo-quota-billing-token-estimation` (DESIGN 5.5 and 5.4.1).

**Steps**:
1. [x] - `p1` - Estimate before context assembly: the estimate uses only the current message bytes, `prior_context_tokens` and the fixed surcharges, and does not measure the assembled context, retrieved chunks or history - `inst-est-before-context`
2. [x] - `p1` - Compute the text estimate with integer arithmetic: the base is the message byte length divided by `bytes_per_token_conservative` rounded up, plus `fixed_overhead_tokens`; `estimated_text_tokens` is the base multiplied by `100` plus `safety_margin_pct`, divided by `100` and rounded up, multiplying first; an empty message yields `fixed_overhead_tokens` before the margin; a `bytes_per_token_conservative` of 0 from a foreign snapshot is clamped to 1 - `inst-est-text`
3. [x] - `p1` - DB: SELECT the latest `messages` row of the chat with `role` equal to `assistant`, `deleted_at` empty and a non-zero `input_tokens` or `output_tokens`, ordered by `(created_at, id)` descending; `prior_context_tokens` is its `input_tokens` plus `output_tokens`, or 0 when there is none - `inst-est-prior-context`
4. [x] - `p1` - Compute `image_surcharge_tokens` as the number of images multiplied by `image_token_budget` - `inst-est-image`
5. [x] - `p1` - Add `tool_surcharge_tokens` once when the chat has at least one ready document, the candidate's `tool_support.file_search` is set and `disable_file_search` is off; add `web_search_surcharge_tokens` once when `web_search.enabled` is true and the candidate's `tool_support.web_search` is set; add `code_interpreter_surcharge_tokens` once when the chat has at least one ready XLSX attachment, the candidate's `tool_support.code_interpreter` is set and `disable_code_interpreter` is off; otherwise the contribution is 0, and the `search_knowledge` tool carries no surcharge - `inst-est-surcharges`
6. [x] - `p1` - Keep every surcharge a fixed per-turn addition that never scales with the number of provider tool calls, searches or retrieval passes, so that the same snapshot and the same enabled features always give the same reserve - `inst-est-fixed-surcharges`
7. [x] - `p1` - Take the budgets from the `estimation_budgets` of the model catalog entry (defaults apply when the entry omits the object, see Appendix B.5.2): each cascade candidate uses its own entry for its availability check, and the booked reserve, the input limit check and the context-assembly budget use the entry of the effective model; the gear configuration section `estimation_budgets` contributes only `minimal_generation_floor` - `inst-est-budget-source`
8. [x] - `p1` - Set `estimated_input_tokens` to `estimated_text_tokens` plus `prior_context_tokens` plus the image, tool, web search and code interpreter surcharges - `inst-est-sum`
9. [x] - `p1` - Provide the input limit check to the send pipeline: when the effective model's catalog `max_input_tokens` is not 0 and `estimated_text_tokens` of the current message exceeds it, report an input-too-long result, which the send pipeline maps to 400 `out_of_range` `INPUT_TOO_LONG`; a `max_input_tokens` of 0 means no separate limit - `inst-est-input-limit`
10. [x] - `p1` - Treat underestimation as unacceptable and overestimation as acceptable, and never persist the budgets or use them at settlement; only the persisted `minimal_generation_floor_applied` influences settlement - `inst-est-preflight-only`
11. [x] - `p1` - **RETURN** the estimate - `inst-est-return`

### Reserve Calculation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation`

**Input**: a candidate catalog entry, its estimate, `streaming.max_output_tokens` and `estimation_budgets.minimal_generation_floor` of the gear configuration

**Output**: `max_output_tokens_applied`, `estimated_input_tokens`, `reserve_tokens`, `reserved_credits_micro`, `credit_computation_failed` (true only when the credit computation failed) and `minimal_generation_floor_applied` of that candidate (DESIGN 5.4.1, 5.5.7 and 5.5.8).

**Steps**:
1. [x] - `p1` - Set `max_output_tokens_applied` to the smaller of the catalog `max_output_tokens` of the candidate and `streaming.max_output_tokens`; this value is always limited and is the hard output cap sent to the provider - `inst-reserve-max-output`
2. [x] - `p1` - Set `reserve_tokens` to `estimated_input_tokens` plus `max_output_tokens_applied` - `inst-reserve-tokens`
3. [x] - `p1` - Set `reserved_credits_micro` through the credit arithmetic algorithm applied to `estimated_input_tokens` and `max_output_tokens_applied` with the candidate's multipliers - `inst-reserve-credits`
4. [x] - `p1` - **IF** the credit computation failed (overflow, zero multiplier, out-of-range value) - `inst-reserve-compute-failed`
   1. [x] - `p1` - Log a warning and use `i64::MAX` as the reserve credits of this candidate (the sentinel of DESIGN 5.3 `Overflow Protection (Normative)`), so the candidate is unavailable and the cascade downgrades or rejects with 429 - `inst-reserve-substitute-max`
   2. [x] - `p1` - Set `credit_computation_failed` to true, so that the tier availability algorithm treats this candidate as explicitly unavailable instead of relying on the sentinel value fitting no bucket; a candidate with `credit_computation_failed` is never booked - `inst-reserve-mark-failed`
5. [x] - `p1` - Set `minimal_generation_floor_applied` to the smaller of the gear configuration `estimation_budgets.minimal_generation_floor` (default 50, greater than 0 and at most `streaming.max_output_tokens`) and `max_output_tokens_applied`; the floor comes from the gear configuration, never from the catalog entry - `inst-reserve-floor`
6. [x] - `p1` - Use the same formula for the availability check and for the booked reserve, so the booked reserve of the effective model equals the reserve that passed the check - `inst-reserve-same-formula`
7. [x] - `p1` - **RETURN** the reserve values - `inst-reserve-return`

### Tier Availability

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-and-billing-tier-availability`

**Input**: a tier, the candidate's `reserved_credits_micro` and `credit_computation_failed`, the user limits and the current period starts

**Output**: available or unavailable. The normative check is `bucket_available` of DESIGN 5.4.2, which supersedes the illustrative helper in `Two-Tier Rate Limiting & Throttling`.

**Steps**:
1. [x] - `p1` - Select the required buckets: the standard tier requires bucket `total`; the premium tier requires bucket `total` and bucket `tier:premium` - `inst-avail-buckets`
2. [x] - `p1` - Select the periods: `daily` and `monthly`, each with its own `period_start` - `inst-avail-periods`
3. [x] - `p1` - DB: read every required row (all periods and all required buckets) in a single transaction for a consistent snapshot, with at least READ COMMITTED and REPEATABLE READ recommended, using `SELECT ... FOR UPDATE` on PostgreSQL; a missing row counts as `spent_credits_micro` 0 and `reserved_credits_micro` 0 - `inst-avail-read-rows`
4. [x] - `p1` - **IF** the candidate's `credit_computation_failed` is true, **RETURN** unavailable at once, before any comparison and without booking anything, whatever the bucket rows contain; the `i64::MAX` sentinel is never added to a counter - `inst-avail-failed-candidate`
5. [x] - `p1` - **FOR EACH** required bucket and period - `inst-avail-for-each`
   1. [x] - `p1` - Compare in an overflow-safe way, per DESIGN 5.3 `Overflow Protection (Normative)`: the check passes when the candidate's `reserved_credits_micro` is at most `limit` minus `spent_credits_micro` minus `reserved_credits_micro`, evaluated in widened 128-bit signed arithmetic (or an equivalent saturating subtraction that cannot wrap), for the user limit of that bucket and period (`total` from the standard limits, `tier:premium` from the premium limits); this is the same condition as `spent + reserved + candidate <= limit` and never computes the 64-bit sum, so a large candidate on a bucket with non-zero spent can neither wrap nor panic, and a negative remainder makes the check fail - `inst-avail-compare`
6. [x] - `p1` - Treat the tier as available only when every check passes; one exhausted period or bucket makes the whole tier unavailable, and a daily pass with a monthly failure is unavailable - `inst-avail-all-must-pass`
7. [x] - `p1` - Apply no separate negative-threshold knob: availability uses only the comparison above (DESIGN B.5.4) - `inst-avail-no-threshold`
8. [ ] - `p1` - **RETURN** available or unavailable - `inst-avail-return`

### Downgrade Cascade

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade`

**Input**: the selected model id, the catalog, the kill switches, the user limits and the usage rows

**Output**: the effective model with its tier and decision, or a reject. The flow is DESIGN 4 `Downgrade Decision Flow` and `Effective Model Computed Once (P1 Determinism Rule)`, with the tier order fixed by the catalog rules of `cpt-cf-mini-chat-feature-gear-foundation`.

**Steps**:
1. [x] - `p1` - Look up the selected model: found and enabled gives its own tier and no downgrade reason; found but disabled gives its own tier and the reason `model_disabled`; not found gives the `premium` tier and the reason `model_disabled` - `inst-cascade-start`
2. [x] - `p1` - Build the cascade from the starting tier downward: premium gives premium then standard, standard gives standard only, and a standard model never cascades up to premium - `inst-cascade-order`
3. [x] - `p1` - **FOR EACH** tier in the cascade - `inst-cascade-for-each`
   1. [x] - `p1` - **IF** the tier is premium and `force_standard_tier` or `disable_premium_tier` is set, skip it and set the downgrade reason `force_standard_tier` when that switch is set, otherwise `disable_premium_tier`, unless a reason is already set - `inst-cascade-kill-switch`
   2. [x] - `p1` - Select the candidate among enabled models of the tier only: the selected model itself when it belongs to the tier, otherwise the model whose tenant preference has `is_default` true, otherwise the first enabled model of the tier in catalog order - `inst-cascade-candidate`
   3. [x] - `p1` - **IF** the tier has no enabled model, continue with the next tier; a tier with quota left but no enabled model counts as unavailable - `inst-cascade-no-model`
   4. [x] - `p1` - Compute the candidate reserve through the reserve calculation algorithm and evaluate the tier through the tier availability algorithm, passing the candidate's `credit_computation_failed` flag with its reserve; only this one candidate per tier is checked - `inst-cascade-check`
   5. [x] - `p1` - **IF** the tier is available, set `effective_model` to the candidate and stop; **ELSE** when the tier is premium and no reason is set yet, set the downgrade reason `premium_quota_exhausted`, and continue - `inst-cascade-pick`
4. [x] - `p1` - **IF** no tier is available - `inst-cascade-none`
   1. [x] - `p1` - **RETURN** a reject with quota scope `tokens`; this covers quota exhaustion and the case where no enabled model exists in any tier of the cascade - `inst-cascade-return-reject`
5. [x] - `p1` - Set the decision to `allow` when `effective_model` equals the selected model and no downgrade reason is set, and to `downgrade` otherwise, with `downgrade_from` equal to the selected model; the reasons are exactly `premium_quota_exhausted`, `force_standard_tier`, `disable_premium_tier` and `model_disabled` - `inst-cascade-decision`
6. [x] - `p1` - Expose with the decision the effective tier, the effective model's `provider_id` and whether its `multimodal_capabilities` contain `VISION_INPUT`, so that the send pipeline rejects an image request on a model without it with 400 `VISION_NOT_SUPPORTED` after the cascade and before any provider call, as the downgrade decision matrix requires, even when the selected model supports images - `inst-cascade-expose-capability`
7. [x] - `p1` - Resolve the effective model once at preflight and persist it as `chat_turns.effective_model`; settlement reads it and never runs the cascade again - `inst-cascade-once`
8. [x] - `p1` - **RETURN** the effective model, tier, decision and reason - `inst-cascade-return`

### Daily Tool Quota Check

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check`

**Input**: the effective model entry, the kill switches, the request facts, the daily `total` bucket row of the user and the configuration keys `quota.web_search_daily_quota` and `quota.code_interpreter_daily_quota`

**Output**: pass, or a rejection with the quota scope `web_search` or `code_interpreter`. The rules are DESIGN 4 `Web Search Quota Enforcement (P1 Determinism Rules)` and `Code Interpreter Quota Enforcement (P1 Determinism Rules)`.

**Steps**:
1. [x] - `p1` - **IF** `web_search.enabled` is true and the effective model's `tool_support.web_search` is set - `inst-toolq-web-applies`
   1. [x] - `p1` - Read `web_search_calls` of the daily `total` row (0 when the row is missing) and, when it is at least `quota.web_search_daily_quota` (default 75), **RETURN** a rejection with scope `web_search` - `inst-toolq-web-check`
2. [x] - `p1` - **IF** the code interpreter tool is included, meaning the chat has at least one ready XLSX attachment, the effective model's `tool_support.code_interpreter` is set and `disable_code_interpreter` is off - `inst-toolq-ci-applies`
   1. [x] - `p1` - Read `code_interpreter_calls` of the daily `total` row (0 when the row is missing) and, when it is at least `quota.code_interpreter_daily_quota` (default 50), **RETURN** a rejection with scope `code_interpreter` - `inst-toolq-ci-check`
3. [x] - `p1` - Never reject a request that does not use the tool, never check a tool the effective model does not support (the requested flag is still stored on the turn by the pipeline), and skip the code interpreter check when its kill switch silently omits the tool - `inst-toolq-gating`
4. [x] - `p1` - Check the web search quota first and the code interpreter quota second when both apply, so the rejection scope is deterministic - `inst-toolq-order`
5. [x] - `p1` - Apply no daily `file_search` limit (ADR-0007) and no refund when the per-message limit is breached later: that breach settles on the estimated path, and the surcharges reserved at preflight are not returned - `inst-toolq-scope`
6. [x] - `p1` - Add the call counts of the turn to the daily `total` row only at settlement; this check only reads - `inst-toolq-read-only`
7. [x] - `p1` - **RETURN** pass - `inst-toolq-return`

### Preflight Reserve

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve`

**Input**: the security context (tenant, user), the selected model id and the request facts (message bytes, image count, ready documents, ready XLSX attachments, `web_search.enabled`)

**Output**: a decision carrying the values that the send pipeline persists and uses, or a rejection. Preflight writes nothing; it is the decision phase of the two-phase counting of the quota service (DESIGN 3.2 and 5.4).

**Steps**:
1. [x] - `p1` - Resolve the policy through the policy binding algorithm and return a rejection of kind internal error when it fails - `inst-preflight-policy`
2. [x] - `p1` - **IF** `web_search.enabled` is true and the snapshot kill switch `disable_web_search` is on - `inst-preflight-web-kill`
   1. [x] - `p1` - **RETURN** a rejection of kind kill switch (400 `failed_precondition`, subject `web_search`, type `FEATURE_DISABLED`) before any estimation and before the quota cascade; the parameter is never silently ignored - `inst-preflight-return-kill`
3. [x] - `p1` - Capture the preflight instant once from the application UTC clock and compute the daily and monthly period starts from that same instant through the period boundaries algorithm; the instant is not read again during the preflight or the reserve write - `inst-preflight-periods`
4. [x] - `p1` - Resolve `prior_context_tokens` once for the chat - `inst-preflight-prior-context`
5. [x] - `p1` - Run the downgrade cascade, which estimates, computes the reserve and checks availability for each candidate - `inst-preflight-cascade`
6. [x] - `p1` - Record `mini_chat_quota_preflight` once with the labels `decision` (`allow`, `downgrade` or `reject`), `model` and `tier`, and, after an allow or downgrade, record `mini_chat_quota_estimated_tokens` with the `reserve_tokens` of the decision, before any reserve is written, so a request rejected later still records it - `inst-preflight-metrics`
7. [x] - `p1` - **IF** the cascade rejected - `inst-preflight-cascade-rejected`
   1. [x] - `p1` - End the read transaction without writing and **RETURN** a rejection of kind quota with scope `tokens` - `inst-preflight-return-quota`
8. [x] - `p1` - Run the daily tool quota check for the effective model and **RETURN** its rejection when it rejected - `inst-preflight-tool-quota`
9. [x] - `p1` - End the read transaction without writing any reserve; the reserve is written later by the reserve write algorithm - `inst-preflight-no-write`
10. [x] - `p1` - Make every product-level decision (block, downgrade, limit) here, in the domain service, before any request reaches OAGW; the quota service has no dependency on the provider or on OAGW, and mid-stream quota abort is not supported - `inst-preflight-before-outbound`
11. [x] - `p1` - **RETURN** the decision: `effective_model` with its tier, `provider_id` and `VISION_INPUT` capability; `quota_decision`, `downgrade_from` and `downgrade_reason`; `max_output_tokens_applied`, `reserve_tokens`, `reserved_credits_micro` and `minimal_generation_floor_applied` of the effective model; `policy_version_applied`; `estimated_text_tokens` for the input limit check; the preflight instant and the daily and monthly period starts computed from it; and the user limit of every bucket and period that was checked - `inst-preflight-return-decision`

### Reserve Write

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-and-billing-reserve-write`

**Input**: the preflight decision (which never carries a candidate whose credit computation failed), the turn identity (`tenant_id`, `user_id`, `chat_id`, `request_id`) and the caller's open reserve transaction

**Output**: the reserve committed with the caller's transaction, or a rejection with scope `tokens`. The rules are DESIGN 5.4.2 `TOCTOU` and 5.4.3, with the commit semantics of `Table: quota_usage`.

**Steps**:
1. [x] - `p1` - Run inside the reserve transaction of the caller, which is separate from the read transaction of the preflight and, on the send path, also creates the user message and the `running` turn, and on retry and edit follows the mutation commit - `inst-rw-transaction`
2. [x] - `p1` - Write the preflight columns of the turn once and never change them afterwards: `policy_version_applied`, `effective_model`, `reserve_tokens`, `max_output_tokens_applied`, `reserved_credits_micro` and `minimal_generation_floor_applied`; on the send path they are set on INSERT, on retry and edit the NULL columns of the new turn are filled in this same transaction - `inst-rw-turn-columns`
   1. [x] - `p1` - Write `chat_turns.started_at` of the turn as the preflight instant carried by the decision, which is the same instant that produced the daily and monthly period starts of the rows incremented below, so that deriving the period starts from `started_at` always yields the reserved rows; on the send path the value is set on INSERT, and on retry and edit the instant captured for the new turn's own preflight is the one written as its `started_at` (set in this same transaction when the new turn row was committed earlier by the mutation flow with another value, which is allowed because the turn is `running` and not yet reserved), never the instant of the mutation commit or of the original turn - `inst-rw-started-at`
3. [x] - `p1` - **FOR EACH** period in `daily` and `monthly` - `inst-rw-for-each-period`
   1. [x] - `p1` - DB: atomically add the turn's `reserved_credits_micro` to `quota_usage.reserved_credits_micro` of the row `(tenant_id, user_id, period_type, period_start, 'total')`, creating the row when absent, with a single UPDATE or a row lock so parallel streams cannot race - `inst-rw-increment-total`
   2. [x] - `p1` - **IF** the effective tier is premium, add the same amount to the row of bucket `tier:premium` for that period; a standard-tier turn updates bucket `total` only - `inst-rw-increment-premium`
4. [x] - `p1` - Re-read, in the same transaction and with a plain SELECT without locking, the user's bucket rows of the checked periods, and verify for every bucket and period of the decision that `spent_credits_micro` plus `reserved_credits_micro` is at most the user limit carried by the decision, comparing in widened 128-bit signed arithmetic (or the equivalent `reserved_credits_micro` at most `limit` minus `spent_credits_micro`) so that the sum cannot wrap, and treating a database arithmetic overflow of an increment as an over-limit bucket (rollback and 429, never a 500); the increments hold the row locks (PostgreSQL) or the write lock (SQLite), so the re-check sees every reserve committed before it - `inst-rw-recheck`
5. [x] - `p1` - **IF** any bucket is over its limit - `inst-rw-over-limit`
   1. [x] - `p1` - Roll back the whole transaction (the increments, the turn columns and the caller's rows), do not run the cascade again and do not downgrade, and **RETURN** a rejection with scope `tokens`, the same response as a preflight reject - `inst-rw-rollback`
6. [x] - `p1` - Record `mini_chat_quota_reserve` once per period with the label `period`, after the transaction commits - `inst-rw-metric`
7. [x] - `p1` - Guarantee that two concurrent turns of one user that both passed preflight cannot together book a reserve over a limit: the later reserve transaction is rejected - `inst-rw-concurrency`
8. [x] - `p1` - Allow the provider call only after this transaction committed, so a `running` turn with its reserve always exists before the first outbound byte; a crash after the commit and before the provider request is settled by the orphan watchdog on the estimated path - `inst-rw-before-provider`
9. [x] - `p1` - **RETURN** success - `inst-rw-return`

### Quota Rejection Mapping

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-and-billing-rejection-mapping`

**Input**: a rejection kind (kill switch, quota, internal error) and a quota scope

**Output**: the canonical Problem. The table is DESIGN 3.3 `Error Codes` and ADR-0004; the Problem shape and status mapping are provided by the error mapping algorithm of `cpt-cf-mini-chat-feature-gear-foundation`.

**Steps**:
1. [x] - `p1` - **IF** the kind is kill switch - `inst-reject-kill`
   1. [x] - `p1` - **RETURN** 400 `failed_precondition` with `violations[{subject: web_search, type: FEATURE_DISABLED}]` - `inst-reject-return-kill`
2. [x] - `p1` - **IF** the kind is quota - `inst-reject-quota`
   1. [x] - `p1` - **RETURN** 429 `resource_exhausted` with one `context.violations[]` entry `{subject: <quota_scope>, description: "quota_exceeded"}`, where the scope is `tokens` (cascade reject, no enabled model in any tier, uncomputable reserve, reserve re-check), `web_search` or `code_interpreter`; clients branch on `context.violations[0].subject`, never on `detail` - `inst-reject-return-quota`
3. [x] - `p1` - **IF** the kind is internal error, **RETURN** 500 `internal` with a generic detail and log the cause - `inst-reject-internal`
4. [x] - `p1` - Return every one of these as a JSON Problem before the SSE stream opens, so quota exhaustion never appears as an SSE `error` event; the mid-turn codes `web_search_calls_exceeded` and `code_interpreter_calls_exceeded` are SSE events of `cpt-cf-mini-chat-feature-message-streaming`, not 429 responses - `inst-reject-before-stream`
5. [x] - `p1` - Never return the scopes of the unimplemented per-user daily image quota and `image_bytes_exceeded` (ADR-0008) - `inst-reject-not-implemented-scopes`
6. [ ] - `p1` - Store `quota_exceeded` in `chat_turns.error_code` only for a retry or edit turn whose reserve re-check failed after the mutation commit; it is never sent over SSE and no settlement or usage event follows - `inst-reject-error-code-storage`
7. [x] - `p1` - Leave no `chat_turns` row with a reserve, no reserve and no usage event for any pre-reserve rejection; the optional zero-charge event for pre-reserve failures is not emitted - `inst-reject-no-side-effects`
8. [ ] - `p1` - Keep the three ownership tiers of the throttling design apart: this feature owns only the product quota; platform rate limits (per-user request rate, concurrent stream caps) stay in api_gateway, and a provider 429 passed through OAGW reaches the client as the SSE error `rate_limited` and is never mapped to a quota 429 - `inst-reject-ownership-tiers`

### Billing Outcome Derivation

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation`

**Input**: the terminal trigger classification, the internal terminal state, the `error_code` and whether usage is known

**Output**: `billing_outcome` and the settlement method. This is the single shared derivation of DESIGN 5.8 `Normative Billing Outcome Derivation (P1 Mandatory)` and `Internal Error Code Taxonomy (Normative)`, used by the stream finalization and the orphan finalization; no terminal path builds its own billing mapping.

**Steps**:
1. [x] - `p1` - Derive the outcome from the trigger classification and the error code, never from the `chat_turns.state` string alone - `inst-outcome-not-from-state`
2. [x] - `p1` - **MATCH** the terminal condition - `inst-outcome-match`
   - [x] - `p1` - **CASE** state `completed` (including a provider `incomplete`): `billing_outcome` `completed`, settlement method `actual`, also when the provider reported zero or no usage - `inst-outcome-completed`
   - [x] - `p1` - **CASE** state `failed` with `error_code` `provider_error`, `provider_timeout` or `rate_limited`: `failed`, `actual` when usage is known, otherwise `estimated` - `inst-outcome-provider-errors`
   - [x] - `p1` - **CASE** state `failed` with `error_code` `web_search_calls_exceeded`, `code_interpreter_calls_exceeded`, `agentic_iterations_exceeded`, `unexpected_tool_use` or `message_persistence_failed`: `failed`, `actual` when usage is known, otherwise `estimated`, never `released` because the provider was already called - `inst-outcome-tool-errors`
   - [x] - `p1` - **CASE** state `failed` with `error_code` `context_length_exceeded`, `validation_error`, `input_too_long` or `turn_setup_failed`: `failed`, `released` - `inst-outcome-pre-provider`
   - [x] - `p1` - **CASE** state `cancelled` (client disconnect): `aborted`, `estimated`, with the formula that ignores any usage because the cancel path carries none - `inst-outcome-cancelled`
   - [x] - `p1` - **CASE** state `failed` with `error_code` `orphan_timeout`: `aborted`, `estimated` - `inst-outcome-orphan`
   - [x] - `p1` - **CASE** state `failed` with `error_code` `quota_exceeded`: no billing outcome, because no reserve exists, so no settlement and no usage event - `inst-outcome-quota-exceeded`
   - [x] - `p1` - **DEFAULT** any other code: log a critical error with the code, classify as `failed` with `estimated` and flag `unknown_error_code` - `inst-outcome-unknown-code`
3. [x] - `p1` - Treat usage of a failed turn as known only when the usage object has at least one non-zero token field; an object with both fields 0, or missing, is unknown and takes the estimated path, which prevents a zero charge by an immediate disconnect; a completed turn is not subject to this rule - `inst-outcome-usage-known`
4. [x] - `p1` - Set the `terminal_state` of the usage event to the internal state (`completed`, `failed` or `cancelled`) and keep it separate from `billing_outcome`: the orphan watchdog sets state `failed` with `orphan_timeout` and emits the billing outcome `aborted` - `inst-outcome-terminal-state`
5. [x] - `p1` - Never choose `released` for a turn whose provider request may have started: no "started" flag exists, so a turn with a reserve that is found `running` is settled `estimated` - `inst-outcome-no-free-cancel`
6. [ ] - `p1` - **RETURN** the billing outcome and the settlement method - `inst-outcome-return`

### Settlement Computation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation`

**Input**: the persisted billing columns of the turn (`reserve_tokens`, `max_output_tokens_applied`, `reserved_credits_micro`, `minimal_generation_floor_applied`, `effective_model`, `policy_version_applied`), the derived billing outcome and settlement method, the provider usage when known, `quota.overshoot_tolerance_factor` and the snapshot of the applied policy version

**Output**: the committed credits, the charged tokens, the usage telemetry and the tool call counts to settle. The rules are DESIGN 5.4.4, 5.4.5, 5.5.14, 5.8 and 5.9; the computation reads persisted fields only, so a re-run on the same row always gives the same credits.

**Steps**:
1. [x] - `p1` - Derive `estimated_input_tokens` as `reserve_tokens` minus `max_output_tokens_applied`, and read `in_mult` and `out_mult` from the entry of `effective_model` in the snapshot of `policy_version_applied` - `inst-settle-compute-inputs`
2. [x] - `p1` - **MATCH** the settlement method - `inst-settle-compute-match`
   - [x] - `p1` - **CASE** `actual`: take `actual_input_tokens` and `actual_output_tokens` from the provider usage (a missing field inside a present usage object counts as 0, and a completed turn without any usage settles 0 credits), and compute `actual_credits_micro` with the credit arithmetic algorithm - `inst-settle-compute-actual`
   - [x] - `p1` - **CASE** `estimated`: set `charged_output_tokens` to `minimal_generation_floor_applied`, `charged_tokens` to the smaller of `reserve_tokens` and `estimated_input_tokens` plus `charged_output_tokens`, and `actual_credits_micro` to the credit arithmetic of `estimated_input_tokens` and `charged_output_tokens`; never charge the full `reserved_credits_micro` and never more than the reserve - `inst-settle-compute-estimated`
   - [x] - `p1` - **CASE** `released`: charge 0 tokens and 0 credits and release the whole reserve - `inst-settle-compute-released`
3. [x] - `p1` - **IF** the method is `actual` and the actual tokens (input plus output) exceed `reserve_tokens` - `inst-settle-overshoot`
   1. [x] - `p1` - Compute the overshoot factor as the actual tokens divided by `reserve_tokens` in floating point, never with integer division - `inst-settle-overshoot-factor`
   2. [x] - `p1` - **IF** the factor is at most `quota.overshoot_tolerance_factor` (default 1.10, range 1.0 to 1.5), commit the actual credits; **ELSE** commit `reserved_credits_micro` as `committed_credits_micro`, cap billing silently with no log line and no dedicated metric, and keep the turn `completed`, because a completed response is never retroactively cancelled - `inst-settle-overshoot-cap`
   3. [x] - `p1` - Record `mini_chat_quota_overshoot` for each of the two periods in both branches, and apply this rule to every actual settlement (completed turns, and failed turns whose provider usage is known); estimated and released settlements never exceed the reserve - `inst-settle-overshoot-metric`
4. [x] - `p1` - Add to `spent_credits_micro` the committed credits (equal to `actual_credits_micro`, or `reserved_credits_micro` when capped), and add to the `total` row telemetry the actual provider tokens, uncapped, only on an `actual` settlement; estimated and released settlements add 0 tokens - `inst-settle-committed-vs-actual`
5. [x] - `p1` - Add the completed `web_search` and `code_interpreter` call counts of the turn to the `total` row on `actual` and `estimated` settlements and add nothing on `released`; `file_search_calls`, `image_inputs`, `image_upload_bytes` and `rag_retrieval_calls` of `quota_usage` stay unchanged - `inst-settle-tool-counts`
6. [x] - `p1` - Ignore every estimation budget except the persisted floor, and read nothing from the current configuration or the current policy version - `inst-settle-determinism`
7. [x] - `p1` - **RETURN** the committed credits, charged tokens, telemetry and the internal overshoot flag (not exported) - `inst-settle-compute-return`

### Finalization Settlement

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement`

**Input**: the open finalization transaction of the caller, the turn row, the terminal trigger classification, the terminal-state-specific columns of the CAS update (supplied by the caller), the provider usage when known, the completed tool call counts, the selected model and the period starts of the reserve

**Output**: a settlement result, or the already-finalized no-op. This is the contract that `cpt-cf-mini-chat-feature-message-streaming` and the orphan watchdog call inside their finalization transactions (DESIGN 5.4.4, 5.7 `FinalizeTurn Invariant`, 5.8, 5.9).

**Steps**:
1. [x] - `p1` - DB: execute the CAS update of `chat_turns` to the terminal state `completed`, `failed` or `cancelled` with `completed_at`, guarded by `state` equal to `running` (the orphan watchdog adds its `deleted_at` and stale-progress predicates), using one conditional UPDATE and no in-memory lock, and read `rows_affected` - `inst-final-cas`
2. [x] - `p1` - **IF** `rows_affected` is 0 - `inst-final-cas-lost`
   1. [x] - `p1` - **RETURN** the already-finalized no-op: no quota change, no usage message, no state change, no metric and no further processing; every competing terminal signal (provider terminal, disconnect, orphan timeout) that arrives later loses in the same way, so the first terminal transition is authoritative and immutable - `inst-final-return-noop`
3. [x] - `p1` - Derive the billing outcome and the settlement method through the billing outcome derivation algorithm - `inst-final-derive`
4. [x] - `p1` - **IF** the turn holds no reserve, meaning its reserve columns or `requester_user_id` are NULL (a retry or edit turn that stayed `running` before its preflight columns were written) - `inst-final-no-reserve`
   1. [x] - `p1` - Skip the bucket updates with a warning, enqueue the usage event with `billing_outcome` `aborted`, `settlement_method` `estimated`, `actual_credits_micro` 0, `usage` null, empty `effective_model` and `selected_model` and `policy_version_applied` 0, leave every `quota_usage` row unchanged and **RETURN** the settlement result - `inst-final-no-reserve-event`
5. [x] - `p1` - Load the snapshot of the turn's `policy_version_applied` through the policy binding algorithm and run the settlement computation - `inst-final-compute`
6. [x] - `p1` - **IF** the snapshot or the computation failed - `inst-final-compute-failed`
   1. [x] - `p1` - Fail the transaction so that the CAS, the bucket updates and the enqueue all roll back, leave the turn `running` and report the failure to the caller - `inst-final-fail-transaction`
7. [x] - `p1` - Determine the tier of the turn from the persisted `effective_model` in the snapshot of `policy_version_applied`, not from the current catalog - `inst-final-tier`
8. [x] - `p1` - **FOR EACH** period in `daily` and `monthly`, targeting the rows of the persisted period starts - `inst-final-for-each-period`
   1. [x] - `p1` - DB: on the row of bucket `total` subtract the turn's `reserved_credits_micro` from `reserved_credits_micro`, add the committed credits to `spent_credits_micro`, add 1 to `calls` for every settlement method, add the actual tokens to `input_tokens` and `output_tokens` on an `actual` settlement only, and add the tool call counts as the settlement computation states - `inst-final-update-total`
   2. [x] - `p1` - **IF** the turn ran on the premium tier, on the row of bucket `tier:premium` subtract the same reserved amount, add the same committed credits and add 1 to `calls`, leaving tokens and tool counters untouched - `inst-final-update-premium`
9. [x] - `p1` - Build the usage event through the usage event construction algorithm and enqueue it in this same transaction to the usage queue; the message and the quota change commit together or not at all - `inst-final-enqueue`
10. [x] - `p1` - Release the reserve of the turn completely in this transaction, so that a `failed` turn never keeps an unsettled reserve (the reserve-uncommitted invariant) - `inst-final-reserve-released`
11. [x] - `p1` - After the caller commits, record `mini_chat_quota_commit` per period and `mini_chat_quota_actual_tokens` for `actual` settlements only, and `mini_chat_code_interpreter_calls` by the number of calls for `actual` settlements only - `inst-final-metrics`
12. [x] - `p1` - Never let a replay of a completed turn call this algorithm: a replay takes no reserve, updates no `quota_usage` row and enqueues no usage message - `inst-final-replay-excluded`
13. [x] - `p1` - **RETURN** the settlement result: billing outcome, settlement method, committed credits and the bucket facts needed for the quota warnings - `inst-final-return`

### Usage Event Construction

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-and-billing-usage-event`

**Input**: the turn row and the settlement result, or a system-task description (`tenant_id`, `chat_id`, summary model, token counts, `system_request_id`)

**Output**: one serialized `UsageEvent` JSON payload enqueued to the usage queue `outbox.queue_name` (default `mini-chat.usage_snapshot`), partitioned by `tenant_id`. The wire fields are DESIGN 5.6, 5.7 and Appendix A.3.

**Steps**:
1. [x] - `p1` - Normalize `tenant_id`, `turn_id` and `request_id` to the 32-character lowercase hexadecimal form without hyphens or braces, only for the `dedupe_key`; every UUID field of the JSON payload keeps the hyphenated RFC 4122 form, and database queries keep the native UUID type - `inst-event-normalize`
2. [x] - `p1` - Build the user-turn `dedupe_key` as `{tenant_id}/{turn_id}/{request_id}` in that normalized form from the persisted turn row (the tenant from the parent chat), so the tenant prefix check of the outbox enqueuer holds by construction - `inst-event-dedupe-user`
3. [x] - `p1` - **IF** a quota-bearing event would carry a missing `dedupe_key` - `inst-event-missing-key`
   1. [x] - `p1` - Log a critical error with the tenant, turn and request identifiers and fail the transaction so that it does not commit - `inst-event-missing-key-fail`
4. [x] - `p1` - Set the payload fields `tenant_id`, `user_id`, `chat_id`, `turn_id`, `request_id`, `effective_model` (the authoritative model for billing reconciliation, from `chat_turns.effective_model`), `selected_model` (from `chats.model`), `terminal_state`, `billing_outcome`, `usage`, `actual_credits_micro`, `settlement_method`, `policy_version_applied`, `web_search_calls`, `code_interpreter_calls`, `file_search_calls`, `timestamp` (UTC, RFC 3339), `requester_type` and `dedupe_key`; there is no `event_type` field - `inst-event-fields`
5. [x] - `p1` - Set `actual_credits_micro` to the committed credits, which is the authoritative billing amount that the consumer must use as is and never recompute, and set `usage` to the provider-reported `input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_write_input_tokens` and `reasoning_tokens` for an `actual` settlement, to null for an `estimated` settlement (the estimated split is not emitted), and to `input_tokens` 0 and `output_tokens` 0 for a `released` settlement - `inst-event-credits-usage`
6. [x] - `p1` - Omit from the payload `reserved_credits_micro`, `reserve_tokens`, `error_code`, `quota_decision`, `downgrade_from`, `downgrade_reason`, `completion_signal`, `committed_credits_micro` and `overshoot_capped`, and never place a provider file, response or vector store identifier in it - `inst-event-exclusions`
7. [x] - `p1` - For an orphan-finalized turn set `selected_model` to the effective model, and for a turn without reserve apply the placeholders of the finalization settlement algorithm - `inst-event-orphan-values`
8. [x] - `p1` - **IF** the event describes a system task (the thread-summary update) - `inst-event-system-task`
   1. [x] - `p1` - Set `requester_type` `system`, `billing_outcome` `system_task`, `settlement_method` `none`, `terminal_state` `completed`, `actual_credits_micro` 0, `policy_version_applied` 0, the three call counts 0, `usage` with the token counts of the summary call, `system_task_type` `thread_summary_update`, and omit `user_id` and `turn_id` entirely (absent, not null) - `inst-event-system-fields`
   2. [x] - `p1` - Build the `dedupe_key` as `{tenant_id}/{system_task_type}/{system_request_id}` with `tenant_id` and `system_request_id` in the 32-character hexadecimal form, where `system_request_id` is the server-generated UUID v4 persisted at the enqueue of the durable work and reused unchanged on every retry - `inst-event-dedupe-system`
   3. [x] - `p1` - Never touch `chat_turns`, `chat_turns.state` or any `quota_usage` row for a system task, never run it through the CAS finalization, and leave the enqueue itself to the feature that owns the thread-summary worker; the usage payload contract is defined here - `inst-event-system-isolation`
9. [x] - `p1` - Enqueue through the transactional enqueuer of `cpt-cf-mini-chat-feature-gear-foundation` inside the caller's transaction, relying on its dedupe prefix validation, and let an oversized payload surface through the error mapping of the calling operation - `inst-event-enqueue`
10. [x] - `p1` - Achieve exactly one logical enqueue per turn through the CAS winner of the finalization, because the shared outbox treats payloads as opaque bytes and enforces no Mini Chat uniqueness - `inst-event-one-per-turn`
11. [x] - `p1` - **RETURN** the enqueued payload - `inst-event-return`

### Usage Outbox Handler

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler`

**Input**: one message of the usage queue (payload and attempt count), delivered by the shared outbox with the shared default lease of 30 s

**Output**: a handler outcome `Ok`, `Retry` or `Reject(reason)`. The contract is DESIGN 5.6 `Shared Outbox Processing Model (P1)` and 5.2.2; delivery is at least once and the consumer is idempotent.

**Steps**:
1. [x] - `p1` - Register one leased handler for the usage queue; the queue registration, partitioning and lease come from `cpt-cf-mini-chat-feature-gear-foundation`, and retry backoff, reclaim, sequencing and vacuum stay with the shared outbox - `inst-uh-registration`
2. [x] - `p1` - Deserialize the payload as a `UsageEvent` - `inst-uh-deserialize`
3. [x] - `p1` - **IF** the payload cannot be deserialized - `inst-uh-corrupt`
   1. [x] - `p1` - **RETURN** `Reject(reason)`, so the shared outbox moves the message to its dead-letter store - `inst-uh-return-reject-corrupt`
4. [x] - `p1` - Resolve the `mini-chat-model-policy-plugin` instance through the model-policy gateway - `inst-uh-resolve-plugin`
5. [x] - `p1` - **TRY** - `inst-uh-try`
   1. [x] - `p1` - Call `publish_usage(payload)` on the plugin with the event unchanged, outside any database transaction - `inst-uh-publish`
6. [x] - `p1` - **CATCH** a plugin resolution failure or a transient publish error - `inst-uh-catch-transient`
   1. [x] - `p1` - **RETURN** `Retry`; no committed quota data is lost, and the DESIGN defines no attempt cap for this handler - `inst-uh-return-retry`
7. [x] - `p1` - **CATCH** a permanent publish error - `inst-uh-catch-permanent`
   1. [x] - `p1` - **RETURN** `Reject(reason)`; operators recover through the dead-letter operations of the shared outbox, and Mini Chat defines no separate dead row state - `inst-uh-return-reject-permanent`
8. [x] - `p1` - Accept that a crash after the plugin call and before the outbox records success redelivers the message, so the plugin or CCM consumer must deduplicate by `(tenant_id, turn_id, request_id)` for user turns and by `(tenant_id, system_task_type, system_request_id)` for system tasks, or by the equivalent `dedupe_key`; the handler itself performs no deduplication - `inst-uh-duplicate-delivery`
9. [x] - `p1` - **RETURN** `Ok` when the plugin accepted the event; the bundled `static_model_policy` plugin only logs the event - `inst-uh-return-ok`

### Quota Status Computation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-and-billing-status-computation`

**Input**: the user limits, the `quota_usage` rows of the current periods, `quota.warning_threshold_pct` (default 80, range 1 to 99) and the current UTC instant

**Output**: the `tiers` array of the quota status response. The rules are DESIGN 3.2 `Quota Warning Thresholds (P1)` and `Quota Status Endpoint (P1)`.

**Steps**:
1. [x] - `p1` - **FOR EACH** tier in the order `premium`, `total` (the tier `premium` reads bucket `tier:premium` and the premium limits, the tier `total` reads bucket `total` and the standard limits) - `inst-qs-for-each-tier`
   1. [x] - `p1` - **FOR EACH** period in the order `daily`, `monthly` - `inst-qs-for-each-period`
      1. [x] - `p1` - Skip the period when its limit is 0 or negative, so it appears neither in the status nor in the warnings - `inst-qs-skip-no-limit`
      2. [x] - `p1` - Read the row of the bucket for the current `period_start`, with `spent_credits_micro` 0 and `reserved_credits_micro` 0 when it does not exist - `inst-qs-row`
      3. [x] - `p1` - Set `used_credits_micro` to `spent_credits_micro` plus `reserved_credits_micro` (conservative: in-flight reserves count as used) - `inst-qs-used`
      4. [x] - `p1` - Set `remaining_credits_micro` to the limit minus `used_credits_micro`, not below 0 - `inst-qs-remaining`
      5. [x] - `p1` - Set `remaining_percentage` to the remaining credits multiplied by 100 and divided by the limit, as a floored integer from 0 to 100, using integer arithmetic wide enough not to overflow - `inst-qs-percentage`
      6. [x] - `p1` - Set `warning` to true when `remaining_percentage` is at most 100 minus `warning_threshold_pct`, and `exhausted` to true when `remaining_percentage` is 0, which already holds when less than 1 percent of the limit remains - `inst-qs-flags`
      7. [x] - `p1` - Set `next_reset` through the period boundaries algorithm - `inst-qs-next-reset`
2. [x] - `p1` - Omit a tier that has no listed period, and return an empty `tiers` array when no period has a positive limit - `inst-qs-omit-empty-tier`
3. [x] - `p1` - Keep tool quotas (web search, code interpreter), billing outcomes and settlement details out of the result - `inst-qs-exclusions`
4. [x] - `p1` - **RETURN** the tiers - `inst-qs-return`

### Quota Warnings Projection

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-and-billing-quota-warnings`

**Input**: the same inputs as the quota status computation, read after the settlement of the turn

**Output**: the `quota_warnings` entries that the streaming feature attaches to the `done` event: `tier`, `period`, `remaining_percentage`, `warning`, `exhausted` and, only when `warning` or `exhausted` is true, `next_reset`.

**Steps**:
1. [x] - `p1` - Compute one entry per tier and period with a positive limit exactly as the quota status computation does, so the SSE values and the `GET /v1/quota/status` values agree for the same rows - `inst-qw-same-computation`
2. [x] - `p1` - Read the buckets after the turn's settlement, so exhaustion caused by this turn is visible - `inst-qw-post-settlement`
3. [x] - `p1` - Set `next_reset` only on entries whose `warning` or `exhausted` is true and leave it absent otherwise - `inst-qw-next-reset`
4. [ ] - `p1` - Leave the decision to attach `quota_warnings` (present on turns finalized by the CAS winner as completed, absent on error, cancelled, replay and CAS-loser paths) to `cpt-cf-mini-chat-feature-message-streaming` - `inst-qw-attachment`
5. [x] - `p1` - **RETURN** the entries - `inst-qw-return`

### Cost Metrics

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-and-billing-cost-metrics`

**Input**: the preflight decisions, the reserve writes and the settlements of this feature

**Output**: the recorded OTLP series. The catalog is DESIGN 4 `Metrics (OTLP) and Alerts (P1)`; the PRD requirement is `cpt-cf-mini-chat-fr-cost-metrics`.

**Steps**:
1. [x] - `p1` - Use the prefix `metrics.prefix` (default `mini_chat`) and only low-cardinality labels from the allowed set (`model`, `tier`, `decision`, `period` with the values `daily` and `monthly`); never use `tenant_id`, `user_id`, `chat_id`, `request_id` or a free-form error string as a label - `inst-cm-naming`
2. [x] - `p1` - Record `mini_chat_quota_preflight` (counter; `decision`, `model`, `tier`) once per preflight, and `mini_chat_quota_estimated_tokens` (histogram) with the turn's `reserve_tokens` after an allow or downgrade and before the reserve is written, so a request rejected later still records it - `inst-cm-preflight`
3. [x] - `p1` - Record `mini_chat_quota_reserve` (counter; `period`) per period when the reserve is written - `inst-cm-reserve`
4. [x] - `p1` - Record `mini_chat_quota_commit` (counter; `period`) per period and `mini_chat_quota_actual_tokens` (histogram) only for `actual` settlements, `mini_chat_quota_overshoot` (counter; `period`) for `actual` settlements whose actual tokens exceed the reserve, and `mini_chat_code_interpreter_calls` (counter; `model`) by the number of calls only for `actual` settlements - `inst-cm-settlement`
5. [ ] - `p1` - Carry the cost attributes of every settled LLM request (model, input tokens, output tokens, file search, web search and code interpreter call counts) in the usage event and the quota series; tenant and user attribution stays in the usage records and audit events, and time to first token and total latency are recorded by `cpt-cf-mini-chat-feature-message-streaming` - `inst-cm-attribution`
6. [x] - `p1` - Keep `quota_preflight_v2`, `quota_tier_downgrade`, `quota_negative`, `quota_overshoot_tokens`, `quota_image_commit`, `credits_overflow` and `unknown_error_code` registered but unrecorded, define no `quota_reserved_tokens` or `quota_overshoot_exceeded` series, and treat the alerts that depend on `quota_negative` and on the usage dead-letter series as unable to fire - `inst-cm-not-recorded`
7. [ ] - `p1` - Provide the operator guidance that an overshoot count above 5 percent of completed turns calls for a review of the catalog estimation budgets - `inst-cm-guidance`
8. [ ] - `p1` - **RETURN** the recorded series - `inst-cm-return`

### Quota Configuration

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-and-billing-quota-configuration`

**Input**: the validated gear configuration of `cpt-cf-mini-chat-feature-gear-foundation` and the policy snapshot

**Output**: the values this feature reads. Validation at startup stays in the configuration algorithm of the foundation; this algorithm names the values and defaults the quota contracts depend on (DESIGN Appendix B.5.2, B.5.3, B.6, B.6.1, B.9.3).

**Steps**:
1. [x] - `p1` - Read `quota.overshoot_tolerance_factor` (default `1.10`, valid range `1.0` to `1.5`) and `quota.warning_threshold_pct` (default `80`, valid range `1` to `99`) - `inst-qc-overshoot-warning`
2. [x] - `p1` - Read `quota.web_search_daily_quota` (default `75`) and `quota.code_interpreter_daily_quota` (default `50`), each greater than 0; the per-message maxima `quota.web_search_max_calls_per_message` (default `2`) and `quota.code_interpreter_max_calls_per_message` (default `10`) are enforced mid-turn by `cpt-cf-mini-chat-feature-message-streaming` - `inst-qc-tool-quotas`
3. [x] - `p1` - Read `estimation_budgets.minimal_generation_floor` (default `50`, greater than 0 and at most `streaming.max_output_tokens`) and `streaming.max_output_tokens` from the gear configuration; read every other estimation budget from the catalog entry - `inst-qc-floor`
4. [x] - `p1` - Read the usage queue name `outbox.queue_name` (default `mini-chat.usage_snapshot`) and `outbox.num_partitions` (default 4) - `inst-qc-outbox`
5. [x] - `p1` - Read the kill switches `disable_premium_tier`, `force_standard_tier`, `disable_web_search`, `disable_file_search` and `disable_code_interpreter` from the snapshot; `disable_images` belongs to the image guards of the other features - `inst-qc-kill-switches`
6. [x] - `p1` - **RETURN** the values - `inst-qc-return`

## 4. States (CDSL)

### Turn Billing State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-quota-and-billing-turn-billing`

**States**: IN_PROGRESS, COMPLETED, FAILED, ABORTED

**Initial State**: IN_PROGRESS

The billing state maps to `chat_turns.state` as follows: IN_PROGRESS is `running`, COMPLETED is `completed`, FAILED is `failed`, and ABORTED is `cancelled` (client disconnect) or `failed` with `error_code` `orphan_timeout`. A turn enters IN_PROGRESS when the reserve is taken. Terminal states are immutable and a turn reaches exactly one of them. The terminal outcome taxonomy of DESIGN 5.7 is: `completed` ends with the SSE event `done`, `failed` ends with the SSE event `error`, and `cancelled` (the server-side cancellation after a client disconnect) has no terminal event because the stream is already gone; a disconnect is not a separate state, and a disconnect after the provider terminal event leaves the billing outcome of the provider terminal unchanged.

**Transitions**:
1. [x] - `p1` - **FROM** IN_PROGRESS **TO** COMPLETED **WHEN** the provider returned a terminal `response.completed` or `response.incomplete` and the CAS on `state` equal to `running` affected one row - `inst-bill-completed`
2. [x] - `p1` - **FROM** IN_PROGRESS **TO** FAILED **WHEN** the provider returned a terminal error, a tool limit was breached, the assistant message could not be persisted, or a pre-provider error occurred after the reserve was taken, and the CAS affected one row - `inst-bill-failed`
3. [x] - `p1` - **FROM** IN_PROGRESS **TO** ABORTED **WHEN** the stream ended without a provider terminal event (client disconnect, cancellation without provider confirmation, or the orphan watchdog timeout) and the CAS affected one row - `inst-bill-aborted`

### Turn Reserve Lifecycle State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-quota-and-billing-turn-reserve`

**States**: NO_RESERVE, RESERVED, SETTLED_ACTUAL, SETTLED_ESTIMATED, SETTLED_RELEASED

**Initial State**: NO_RESERVE

**Transitions**:
1. [x] - `p1` - **FROM** NO_RESERVE **TO** RESERVED **WHEN** the reserve write commits the bucket increments and the preflight columns of the turn together - `inst-reserve-state-reserved`
2. [x] - `p1` - **FROM** NO_RESERVE **TO** NO_RESERVE **WHEN** a preflight rejection or a reserve re-check rollback occurs, leaving no reserve, no settlement and no usage event - `inst-reserve-state-rejected`
3. [x] - `p1` - **FROM** RESERVED **TO** SETTLED_ACTUAL **WHEN** the CAS winner settles a completed turn, or a failed turn with known usage, on provider-reported usage - `inst-reserve-state-actual`
4. [x] - `p1` - **FROM** RESERVED **TO** SETTLED_ESTIMATED **WHEN** the CAS winner settles a cancelled or orphaned turn, or a failed turn that reached the provider without usable usage, with the estimated formula - `inst-reserve-state-estimated`
5. [x] - `p1` - **FROM** RESERVED **TO** SETTLED_RELEASED **WHEN** the CAS winner settles a turn that never reached the provider, charging 0 credits - `inst-reserve-state-released`

### Usage Event Delivery State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-quota-and-billing-usage-delivery`

**States**: ENQUEUED, RETRYING, DELIVERED, DEAD_LETTERED

**Initial State**: ENQUEUED

**Transitions**:
1. [x] - `p1` - **FROM** ENQUEUED **TO** DELIVERED **WHEN** the model-policy plugin accepts `publish_usage` - `inst-usage-state-delivered`
2. [x] - `p1` - **FROM** ENQUEUED **TO** RETRYING **WHEN** the plugin cannot be resolved or the publish fails transiently - `inst-usage-state-retrying`
3. [x] - `p1` - **FROM** ENQUEUED **TO** DEAD_LETTERED **WHEN** the payload cannot be deserialized or the plugin reports a permanent error - `inst-usage-state-dead-direct`
4. [x] - `p1` - **FROM** RETRYING **TO** DELIVERED **WHEN** a later attempt is accepted by the plugin - `inst-usage-state-retry-delivered`
5. [x] - `p1` - **FROM** RETRYING **TO** DEAD_LETTERED **WHEN** a later attempt reports a permanent error - `inst-usage-state-retry-dead`

## 5. Definitions of Done

### Credit Arithmetic and Overflow Protection

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-and-billing-credit-arithmetic`

The system **MUST** compute every credit amount with the one canonical integer function of DESIGN 5.3 (per-component round-up division by `1_000_000`), validate token counts (`0` to `10,000,000`) and multipliers (`1` to `10,000,000,000`, zero rejected) before multiplying, use checked arithmetic, and handle a failure by call site: at preflight the candidate reserve becomes `i64::MAX` (no 500), at settlement the finalization transaction fails.

- No floating-point arithmetic is used for credits; the overshoot ratio is the only floating-point comparison.
- The static model policy plugin rejects out-of-range multipliers at init (foundation); the credit computation re-checks them on every call.
- `mini_chat_credits_overflow` stays registered and unrecorded.

**Implements**:
- `cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: none
- DB: none
- Entities: `UserLimits` and tier definitions (multipliers read from `ModelCatalogEntry`)

### Period Boundaries and Bucket Rows

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-and-billing-period-buckets`

The system **MUST** key quota usage by `(tenant_id, user_id, period_type, period_start, bucket)` with one row per bucket (`total` for the overall cap across all tiers, `tier:premium` for the premium sub-cap), compute the daily and monthly `period_start` in UTC once at preflight from one captured instant that is also persisted as `chat_turns.started_at` in the reserve transaction, and settle against exactly the rows that were reserved.

- A premium turn is charged to both buckets and needs room in both; a standard turn is charged to `total` only; `tier:standard` rows are not required for enforcement.
- `calls`, `input_tokens` and `output_tokens` are telemetry and never used for enforcement; `file_search_calls`, `image_inputs`, `image_upload_bytes` and `rag_retrieval_calls` are not populated.
- The orphan watchdog derives the period starts from `chat_turns.started_at`; the stream path carries the preflight values. Both give the same rows because `started_at` is written in the reserve transaction as the one preflight instant from which the period starts were computed, on the send path and on retry and edit alike.

**Implements**:
- `cpt-cf-mini-chat-algo-quota-and-billing-period-boundaries`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: none
- DB: `cpt-cf-mini-chat-dbtable-quota-usage`
- DB Table: `cpt-cf-mini-chat-dbtable-quota-usage`
- Entities: `QuotaUsage`

### Token Estimation and Reserve Calculation

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-and-billing-estimation-reserve`

The system **MUST** estimate input tokens before context assembly from the current message bytes, `prior_context_tokens` and the fixed per-turn surcharges using the `estimation_budgets` of the catalog entry (each candidate's entry for the check, the effective model's entry for the booked reserve), compute the reserve as `estimated_input_tokens + max_output_tokens_applied` and `credits_micro(estimated_input_tokens, max_output_tokens_applied, in_mult, out_mult)`, cap `max_output_tokens_applied` at the smaller of the catalog `max_output_tokens` and `streaming.max_output_tokens`, and expose the input limit check for `INPUT_TOO_LONG`.

- The text estimate is `ceil((ceil(utf8_bytes / bytes_per_token_conservative) + fixed_overhead_tokens) * (100 + safety_margin_pct) / 100)` in integer arithmetic; the surcharges `image_token_budget` per image, `tool_surcharge_tokens`, `web_search_surcharge_tokens` and `code_interpreter_surcharge_tokens` apply once per turn under the `tool_support` and kill switch gates of the candidate; `search_knowledge` has no surcharge.
- `minimal_generation_floor_applied` is the smaller of the gear configuration floor (default 50) and `max_output_tokens_applied`.
- No estimation budget except the floor is persisted or used at settlement.

**Implements**:
- `cpt-cf-mini-chat-algo-quota-and-billing-token-estimation`
- `cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: none
- DB: `cpt-cf-mini-chat-dbtable-messages` (read of the latest assistant message for `prior_context_tokens`)
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`
- Entities: `ModelCatalogEntry`, `TurnExecution` billing columns

### Preflight Reserve and Tier Downgrade

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-and-billing-preflight-downgrade`

The system **MUST** decide at preflight, before any request reaches OAGW, the effective model through the premium-to-standard cascade: a tier is available only if every required bucket (`total` for standard, `total` and `tier:premium` for premium) passes in both the daily and the monthly period with the check `spent + reserved + this_request_reserve <= limit`, evaluated overflow-safe (as `this_request_reserve <= limit - spent - reserved` in widened 128-bit arithmetic, DESIGN 5.3), using the reserve of each candidate model, and a candidate whose credit computation failed is explicitly unavailable and never booked; when no tier is available the request is rejected with 429 `resource_exhausted` (scope `tokens`). The effective model is computed once, persisted and never recomputed at settlement.

- Kill switches: `force_standard_tier` and `disable_premium_tier` skip premium (reasons `force_standard_tier`, `disable_premium_tier`); `disable_web_search` with `web_search.enabled` is rejected with 400 `FEATURE_DISABLED` before the cascade.
- Downgrade reasons are `premium_quota_exhausted`, `force_standard_tier`, `disable_premium_tier` and `model_disabled`; a standard model never cascades up; a tier without an enabled model is skipped.
- The decision carries the vision capability and `provider_id` of the effective model so that the pipeline applies the downgrade decision matrix.
- Preflight writes nothing and has no dependency on the provider or OAGW.

**Implements**:
- `cpt-cf-mini-chat-flow-quota-and-billing-turn-admission`
- `cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve`
- `cpt-cf-mini-chat-algo-quota-and-billing-tier-availability`
- `cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: `POST /v1/chats/{id}/messages:stream`, retry and edit (as callers; the routes belong to other features)
- DB: `cpt-cf-mini-chat-dbtable-quota-usage`
- DB Table: `cpt-cf-mini-chat-dbtable-quota-usage`
- Entities: `QuotaUsage`, `UserLimits` and tier definitions, `ModelCatalogEntry`

### Reserve Write With Limit Re-Check

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-and-billing-reserve-write`

The system **MUST** write the reserve in the caller's reserve transaction: persist the six preflight columns once, add the turn's `reserved_credits_micro` to bucket `total` (and `tier:premium` for a premium turn) of the daily and monthly rows, re-read those rows in the same transaction and roll back with 429 `resource_exhausted` (scope `tokens`) when any bucket is over its limit, without a new cascade. The provider call may begin only after this transaction committed.

- On the send path the reserve, the user message and the `running` turn commit together; on retry and edit the NULL preflight columns of the already committed turn are filled in the same transaction as the increments.
- The `started_at` of the turn is the preflight instant that produced the period starts of the incremented rows (also on retry and edit), so the orphan derivation from `started_at` reaches exactly those rows.
- The limit re-check is overflow-safe and the reserve write never books a candidate whose credit computation failed.
- Two concurrent turns that both passed preflight cannot both book a reserve over a limit.
- A turn whose reserve write was rejected never takes a reserve and never produces a settlement or usage event.

**Implements**:
- `cpt-cf-mini-chat-algo-quota-and-billing-reserve-write`
- `cpt-cf-mini-chat-state-quota-and-billing-turn-reserve`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: none
- DB: `cpt-cf-mini-chat-dbtable-quota-usage`
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`
- Entities: `QuotaUsage`, `TurnExecution` billing columns

### Policy Version Persisted Per Turn

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-and-billing-policy-version`

The system **MUST** persist `policy_version_applied` on the turn at preflight (immutable), include the same value in the usage event, and settle with the multipliers of the snapshot of that version and the persisted `effective_model`, never with the current version and never by running the cascade again. This realizes `cpt-cf-mini-chat-dod-policy-version-per-turn`.

- Limits map to buckets as `total` from the standard limits and `tier:premium` from the premium limits; `UserLimits` stay apart from the shared snapshot.
- No snapshot cache, snapshot table or notify endpoint exists; the plugin is asked on every preflight, status read and settlement.

**Implements**:
- `cpt-cf-mini-chat-algo-quota-and-billing-policy-binding`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: none
- DB: `cpt-cf-mini-chat-dbtable-chat-turns`
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`
- Entities: `UserLimits` and tier definitions, `UsageEvent`

### Daily Web Search and Code Interpreter Quota (Preflight Part)

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-and-billing-tool-quota`

The system **MUST** reject at preflight with 429 `resource_exhausted` and quota scope `web_search` a request that enables web search on a model with web search support once the daily `web_search_calls` of the user has reached `quota.web_search_daily_quota` (default 75), and with scope `code_interpreter` a request that includes the code interpreter once the daily `code_interpreter_calls` has reached `quota.code_interpreter_daily_quota` (default 50), reading the counters from the daily `total` row and incrementing them only at settlement (actual and estimated, not released).

- A request that does not use the tool is never rejected by its quota; a model without the tool support is not checked; the code interpreter kill switch skips the check.
- The reservation carries the tool surcharge of the candidate model; there is no refund after a later per-message limit breach (that breach settles `estimated`).
- Tool-start detection and the per-message limits belong to `cpt-cf-mini-chat-feature-message-streaming`; there is no per-user daily `file_search` limit.

**Implements**:
- `cpt-cf-mini-chat-flow-quota-and-billing-tool-quota-request`
- `cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: `POST /v1/chats/{id}/messages:stream` (as caller)
- DB: `cpt-cf-mini-chat-dbtable-quota-usage`
- DB Table: `cpt-cf-mini-chat-dbtable-quota-usage`
- Entities: `QuotaUsage`

### Quota Rejection Contract

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-and-billing-rejection-contract`

The system **MUST** return every quota rejection as a JSON Problem before the SSE stream opens: 429 `resource_exhausted` with `violations[{subject: <quota_scope>, description: "quota_exceeded"}]` and the scope `tokens`, `web_search` or `code_interpreter`, 400 `failed_precondition` `FEATURE_DISABLED` for the web search kill switch, and 500 `internal` for a policy failure, leaving no reserve, no turn row with a reserve and no usage event.

- `quota_exceeded` is stored in `chat_turns.error_code` only for a retry or edit turn whose reserve re-check failed after the mutation commit.
- The per-user daily image quota and `image_bytes_exceeded` are never returned.

**Implements**:
- `cpt-cf-mini-chat-algo-quota-and-billing-rejection-mapping`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: `POST /v1/chats/{id}/messages:stream`, retry and edit (as callers)
- DB: none
- Entities: none

### Settlement Computation by Terminal Outcome

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-and-billing-settlement-outcomes`

The system **MUST** derive the billing outcome and settlement method once, in a shared derivation, from the terminal trigger and the `error_code` (table of the derivation algorithm), and settle with the persisted per-turn fields: `actual` from provider usage, `estimated` as `charged_tokens = min(reserve_tokens, estimated_input_tokens + minimal_generation_floor_applied)` with credits from `estimated_input_tokens` and `charged_output_tokens` equal to the floor, and `released` at zero, applying the overshoot tolerance (`quota.overshoot_tolerance_factor`) to every actual settlement.

- A completed turn is always `actual` (0 credits when no usage); a failed turn is `actual` only with at least one non-zero usage field; an aborted turn is always `estimated`; `released` only for the pre-provider codes; an unknown code is `failed` with `estimated` and a critical log.
- An overshoot above the tolerance caps committed credits at `reserved_credits_micro`, silently, and the turn stays `completed`.

**Implements**:
- `cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation`
- `cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation`
- `cpt-cf-mini-chat-state-quota-and-billing-turn-billing`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: none
- DB: `cpt-cf-mini-chat-dbtable-chat-turns`
- DB Table: `cpt-cf-mini-chat-dbtable-quota-usage`
- Entities: `TurnExecution` billing state, `QuotaUsage`

### CAS-Guarded Settlement With Exactly-One Usage Message

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-and-billing-cas-exactly-once`

The system **MUST** settle every reserved turn exactly once through a finalization transaction that performs the CAS on `chat_turns.state` equal to `running`, the bucket updates and the usage message enqueue together, with a lost CAS being a complete no-op, and **MUST** enqueue at most one usage message per turn. This realizes `cpt-cf-mini-chat-dod-cas-guarded-settlement-and-outbox`.

- Settlement per period and bucket: bucket `total` subtracts the turn's `reserved_credits_micro`, adds the committed credits, adds 1 to `calls`, adds the actual tokens on `actual` only and the tool call counts on `actual` and `estimated`; bucket `tier:premium` (premium turns) subtracts the reserve, adds the committed credits and adds 1 to `calls`.
- No reserve remains after a terminal transaction (reserve-uncommitted invariant); a failure of the computation, the snapshot or the enqueue rolls back all three parts.
- A turn without reserve columns (retry or edit turn left `running`) changes no `quota_usage` row and still emits the placeholder usage event.
- Replay takes no reserve, changes no row and enqueues nothing.

**Implements**:
- `cpt-cf-mini-chat-flow-quota-and-billing-turn-settlement`
- `cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: none
- DB: `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-quota-usage`
- DB Table: `cpt-cf-mini-chat-dbtable-quota-usage`
- Entities: `TurnExecution` billing state, `QuotaUsage`, `UsageEvent`

### Usage Event Payload and Dedupe Key

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-and-billing-usage-event`

The system **MUST** serialize the `UsageEvent` with the fields and omissions of the construction algorithm, carry `actual_credits_micro` as the committed and authoritative billing amount, `policy_version_applied` and both models, and build `dedupe_key` as `{tenant_id}/{turn_id}/{request_id}` for turns and `{tenant_id}/{system_task_type}/{system_request_id}` for system tasks with every UUID in 32-character lowercase hexadecimal form.

- System-task events carry `requester_type` `system`, `billing_outcome` `system_task`, `settlement_method` `none`, 0 credits, no `user_id` and no `turn_id`, and never touch `chat_turns` or `quota_usage`.
- The payload never contains provider identifiers, `reserved_credits_micro`, `reserve_tokens`, `error_code` or quota decision fields.

**Implements**:
- `cpt-cf-mini-chat-algo-quota-and-billing-usage-event`

**Constraints**: `cpt-cf-mini-chat-principle-tenant-scoped`

**Touches**:
- API: none
- DB: shared outbox tables (platform migrations)
- Entities: `UsageEvent`

### Reliable Usage Publication

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-and-billing-usage-publication`

The system **MUST** register a leased handler on the usage queue that delivers each `UsageEvent` to the model-policy plugin with `publish_usage`, returning `Ok` on success, `Retry` on a plugin resolution failure or transient publish error, and `Reject(reason)` on a payload that cannot be deserialized or a permanent publish error, so that delivery is at least once and the consumer deduplicates.

- The enqueue happens in the settlement transaction (exactly once per turn); redelivery after a crash between the plugin call and the recorded success is absorbed by the consumer.
- Rejected messages accumulate in the shared dead-letter store; the dead-letter alerts for usage cannot fire in P1 because their series are not defined.

**Implements**:
- `cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler`
- `cpt-cf-mini-chat-state-quota-and-billing-usage-delivery`

**Constraints**: `cpt-cf-mini-chat-principle-tenant-scoped`

**Touches**:
- API: none
- DB: shared outbox tables (platform migrations)
- Entities: `UsageEvent`

### Quota Status API

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-and-billing-status-api`

The system **MUST** serve `GET /v1/quota/status` (operation `mini_chat.get_quota_status`, tag `Mini Chat Quotas`) for the authenticated user: authorized by the action `read` on the `UserQuota` resource with the tenant and owner constraints, answering 200 with `tiers` (`premium`, `total`) and `warning_threshold_pct`, each period carrying `period`, `limit_credits_micro`, `used_credits_micro` (spent plus reserved), `remaining_credits_micro`, `remaining_percentage`, `next_reset`, `warning` and `exhausted`, and the responses 401, 403, 500 and 503 (with `Retry-After`) declared in the OpenAPI.

- Periods whose limit is 0 or negative are omitted; tool quotas, billing outcomes and settlement details are never exposed.
- The route is registered under `url_prefix` through the registration surface of the foundation and sits behind the license gate.

**Implements**:
- `cpt-cf-mini-chat-flow-quota-and-billing-quota-status`
- `cpt-cf-mini-chat-algo-quota-and-billing-status-computation`

**Constraints**: `cpt-cf-mini-chat-principle-tenant-scoped`

**Touches**:
- API: `GET /v1/quota/status`
- DB: `cpt-cf-mini-chat-dbtable-quota-usage`
- DB Table: `cpt-cf-mini-chat-dbtable-quota-usage`
- Entities: `QuotaUsage`, `UserLimits` and tier definitions

### Warning Thresholds and Quota Warnings Inputs

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-and-billing-warning-thresholds`

The system **MUST** apply `quota.warning_threshold_pct` (default 80, range 1 to 99): `warning` is true when `remaining_percentage` is at most 100 minus the threshold, `exhausted` is true when `remaining_percentage` is 0 (floored integer percentage), and the `quota_warnings` entries for the `done` event carry `tier`, `period`, `remaining_percentage`, `warning`, `exhausted` and, only when either flag is true, `next_reset`, computed from the same rows and rules as the status endpoint.

**Implements**:
- `cpt-cf-mini-chat-algo-quota-and-billing-quota-warnings`
- `cpt-cf-mini-chat-algo-quota-and-billing-quota-configuration`

**Constraints**: `cpt-cf-mini-chat-principle-tenant-scoped`

**Touches**:
- API: `GET /v1/quota/status`
- DB: `cpt-cf-mini-chat-dbtable-quota-usage`
- DB Table: `cpt-cf-mini-chat-dbtable-quota-usage`
- Entities: `QuotaUsage`

### Cost Metrics and Quota Alerts

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-and-billing-cost-metrics`

The system **MUST** record the series `mini_chat_quota_preflight`, `mini_chat_quota_reserve`, `mini_chat_quota_commit`, `mini_chat_quota_overshoot`, `mini_chat_quota_estimated_tokens`, `mini_chat_quota_actual_tokens` and `mini_chat_code_interpreter_calls` with the labels and recording rules of the cost metrics algorithm and only low-cardinality labels, carry the per-request cost attributes in the usage event, and keep the declared-but-unrecorded series unrecorded.

**Implements**:
- `cpt-cf-mini-chat-flow-quota-and-billing-admin-cost-observability`
- `cpt-cf-mini-chat-algo-quota-and-billing-cost-metrics`

**Constraints**: `cpt-cf-mini-chat-principle-tenant-scoped`

**Touches**:
- API: none
- DB: none
- Entities: `UsageEvent`

### Deferred and Not-Implemented Scope Stays Unimplemented

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-and-billing-deferred-scope`

The system **MUST NOT** implement the per-user daily image quota and image byte cap, the population of `file_search_calls`, `image_inputs` and `image_upload_bytes`, the per-user daily `file_search` limit, billing of knowledge-search agentic iterations, the policy snapshot cache, persistence and notify endpoint, the tenant operational bucket for system tasks, the `ai_chat` license feature, group chat usage attribution, the centralized policy gRPC interface, the credit status UI proxy and cross-service transactional guarantees, and **MUST** keep them recorded as not implemented (ADR-0003, ADR-0007, ADR-0008).

**Implements**:
- `cpt-cf-mini-chat-algo-quota-and-billing-quota-configuration`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: none
- DB: `cpt-cf-mini-chat-dbtable-quota-usage` (reserved columns stay 0)
- DB Table: `cpt-cf-mini-chat-dbtable-quota-usage`
- Entities: `QuotaUsage`

### Test Layering, Traceability and Feature-Local Deviations

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-and-billing-tests-and-deviations`

The system **MUST** cover every acceptance-criteria item that this feature owns with tests in the layers below, and **MUST** record each feature-local deviation with its rationale and review owner.

**Test layering**:
- Unit tests live in the gear crate (`gears/mini-chat/mini-chat/`) and cover pure logic: credit arithmetic (the worked values, per-component rounding, every bound, the zero-multiplier and out-of-range errors, checked overflow, the `i64::MAX` substitution, and the overflow-safe availability comparison: a bucket with non-zero spent and reserved credits and a candidate whose credit computation failed is unavailable with no wrap, no panic and no 500, also when `spent + reserved + i64::MAX` would exceed `i64::MAX`), period boundaries (the four normative examples, month and year ends, `next_reset`), text estimation (empty message, byte counts at the rounding edges, `bytes_per_token_conservative` of 0 clamped to 1), the surcharge gates (`tool_support`, kill switches, ready documents and XLSX), the cascade truth table (premium daily exhausted, standard monthly exhausted, both exhausted, kill switches, disabled and missing selected model, tier without an enabled model, standard never upgrades), the reserve numbers of DESIGN 5.10, the billing outcome table row by row (including the unknown code and the `quota_exceeded` case), settlement formulas (actual, estimated with the floor clamp, released, overshoot within and above tolerance with the DESIGN example of 11,000 and 500 tokens against a reserve of 10,000), the dedupe key and UUID normalization (user and system forms), the usage payload fields and omissions, the handler outcomes against a stubbed plugin (`Ok`, `Retry`, `Reject`), and the status and warning computation (flooring, the 1 percent case, skipped limits, `next_reset` presence). Database-backed unit tests use an in-memory SQLite database.
- Repository tests with SQLite seed `quota_usage` and `chat_turns` rows and cover the reserve write (increments of both buckets for a premium turn and of `total` only for a standard turn, the re-check rollback leaving no rows changed, two sequential reserves racing for the last credits), the settlement transaction (CAS win and CAS loss as a no-op, bucket arithmetic per settlement method, one usage message in the usage queue per settled turn, rollback of the whole transaction on a forced enqueue failure and on a forced snapshot failure, a period boundary straddled between reserve and settlement, and a preflight at 23:59:59.999 UTC with `started_at` equal to that instant and settlement after midnight, where both the normal settlement path with the carried period starts and the orphan path deriving the starts from `started_at` settle the day-1 rows, leave the day-2 rows untouched and leave no negative `reserved_credits_micro`, also for a retry and an edit turn whose `started_at` is its own preflight instant; and a re-check with a failed-computation candidate sentinel or near-`i64::MAX` counters that rolls back with 429 without wrapping), the replay and pre-reserve cases (no row, no message), the turn without reserve columns, and the daily tool counters (increment on `actual` and `estimated`, none on `released`).
- Integration and API tests exercise `GET /v1/quota/status` in-process through the router with a stubbed PDP, stubbed model-policy plugin and seeded `quota_usage` rows over SQLite, without a network: the 200 body for premium and total, daily and monthly, empty and partly used rows, the omitted zero limit, consistency with rows changed by the reserve write and settlement contracts, the isolation of another user's and another tenant's rows, and the 401, 403, 503 and 500 Problems.
- End-to-end verification of the contracts inside a real turn, stream, disconnect and orphan scenario belongs to `cpt-cf-mini-chat-feature-message-streaming`; no tests are added under `testing/e2e/suites/mini_chat/`.
- Every implemented CDSL step carries `@cpt-begin` and `@cpt-end` markers with its ID, phase token and `inst-*` ID (traceability mode FULL).

**Reliability and data integrity**: every reserve write and settlement is one atomic transaction (CAS, bucket updates and enqueue commit or roll back together), `quota_usage` updates are atomic per bucket row, and the first committed terminal transition is authoritative. **Security**: the status endpoint is scoped by tenant and owner predicates and fails closed through the PEP; reserve and settlement are runtime-owned internal operations keyed by the persisted turn row and carry no end-user access decision; payloads and metrics carry no provider identifiers and no tenant or user labels. **Observability**: the series of the cost metrics algorithm and the usage event; causes are logged and never message content. **Rollback**: a failed transaction leaves no partial state; schema rollback is owned by the foundation migrations. **Compile-time gates**: those of the foundation apply (workspace lints, the domain-model lint DE0309 on the new domain types, the example server build with the features `mini-chat,static-authn,static-authz,single-tenant,static-credstore`).

**Feature-local deviations** (review owner for every item: the mini-chat gear owners who maintain `gears/mini-chat/docs/`):

| Deviation | Rationale | Source |
|-----------|-----------|--------|
| The policy snapshot and user limits are asked from the plugin on every preflight, status read and settlement; there is no cache, table or notify endpoint | Harmless with the in-process static plugin (version 1); to be revisited before a remote CCM plugin | ADR-0008 |
| The availability check and the reserve write are separate transactions; the reserve transaction re-checks the limits after the increments and a rejected re-check is not downgraded | Resolves the TOCTOU gap without one long transaction; on retry and edit the new turn stays `failed` with `quota_exceeded` | ADR-0008 |
| Only one candidate model per tier is checked, and no negative-threshold knob exists | The DESIGN defines the cascade this way and records the knob as not implemented | DESIGN 4, B.5.4 |
| Estimation budgets come from the catalog entry; the gear `estimation_budgets` section is deprecated except `minimal_generation_floor` | Per-model budgets are implemented; the old keys are parsed and warned about | ADR-0008 |
| The overshoot cap is silent, and the payload has no `committed_credits_micro` or `overshoot_capped` field; `actual_credits_micro` carries the committed amount | The DESIGN records these as not implemented | DESIGN 5.4.5 |
| The estimated token split is not emitted: `usage` is null on estimated settlements | The DESIGN emits only provider-reported counts | DESIGN 5.7 |
| The `released` settlement of a turn that took a reserve but never reached the provider is specified but unreachable in P1 | Every pre-provider step runs before the reserve; a crash in the window is settled `estimated` by the orphan watchdog | DESIGN 5.7, 5.8 |
| The orphan watchdog uses a separate orphan finalization with a placeholder usage event when the turn has no reserve columns | Accepted P1 limitation of separate finalization paths | ADR-0010 |
| The optional zero-charge usage event for pre-reserve failures is not emitted | The DESIGN makes it optional and a rejected request leaves nothing behind | DESIGN 5.9 |
| System tasks (the thread summary) and knowledge-search agentic iterations make provider calls without a per-user reserve; a system task emits a `system_task` usage event with 0 credits and only the final knowledge-search iteration is settled | The reserve-before-execute criterion applies to user turns; the DESIGN excludes system tasks from per-user enforcement | ADR-0008, DESIGN 3.2 |
| The per-user daily image quota, the image counters, the per-user daily `file_search` limit, the tenant operational bucket and the `ai_chat` license feature are not implemented | Recorded P1 scope | ADR-0007, ADR-0008 |
| The series `quota_preflight_v2`, `quota_tier_downgrade`, `quota_negative`, `quota_overshoot_tokens`, `quota_image_commit`, `credits_overflow` and `unknown_error_code` are declared but unrecorded, so the alerts on `quota_negative` and on usage dead letters cannot fire | The DESIGN lists them as deferred or not defined | DESIGN 4 |
| The usage handler has no Mini Chat attempt cap, unlike the audit handler with its 120 attempts | The DESIGN specifies a cap only for the audit, cleanup and thread-summary handlers | DESIGN 5.6 |
| The tool quota check runs after the token cascade, for the effective model, web search before code interpreter | The DESIGN fixes only that both run at preflight before the stream opens; this order makes the rejection scope deterministic | DESIGN 3.2, 4 |
| The status computation clamps `remaining_credits_micro` at 0, lists tiers `premium` then `total` and periods `daily` then `monthly`, and omits a tier that has no listed period | The DESIGN and the OpenAPI fix the fields and the minimum 0 of `remaining_percentage`, not the order or the over-limit remainder | DESIGN 3.2, OpenAPI |
| The quota status tier names are `premium` and `total`, not `standard` | `total` is the overall cap mapped from the standard limits | DESIGN 3.2, 3.7 |
| The PostgreSQL `SELECT ... FOR UPDATE` locking behavior is covered by query construction tests only; concurrency ordering is exercised on SQLite | Tests run on SQLite; the SQLite write lock gives the same ordering for the re-check | DESIGN 5.4.2 |
| The Rust and SQL samples of the DESIGN are not reproduced; this document states the behavior in plain English | FEATURE documents carry no code snippets | FEATURE rules |
| Where `config/mini-chat.yaml` and the DESIGN differ, the DESIGN governs | `gears/mini-chat/docs/` is the source of truth | DECOMPOSITION 1 |

**Implements**:
- `cpt-cf-mini-chat-flow-quota-and-billing-quota-status`
- `cpt-cf-mini-chat-flow-quota-and-billing-turn-admission`
- `cpt-cf-mini-chat-flow-quota-and-billing-turn-settlement`
- `cpt-cf-mini-chat-flow-quota-and-billing-tool-quota-request`
- `cpt-cf-mini-chat-flow-quota-and-billing-admin-cost-observability`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`, `cpt-cf-mini-chat-principle-tenant-scoped`

**Touches**:
- API: `GET /v1/quota/status`
- DB: `cpt-cf-mini-chat-dbtable-quota-usage`, `cpt-cf-mini-chat-dbtable-chat-turns`
- DB Table: `cpt-cf-mini-chat-dbtable-quota-usage`
- Entities: `QuotaUsage`, `UsageEvent`

## 6. Acceptance Criteria

The first seven items are the DECOMPOSITION 2.3 "Acceptance Criteria Covered" and the matching items of `acceptance-criteria.md`; the eighth is the preflight part of the web search item that DECOMPOSITION 2.3 scopes to this feature. Each is both implemented and tested. The remaining items pin the details behind them.

- [ ] Quota is checked before any outbound provider call: a preflight rejection (429 `resource_exhausted` with scope `tokens`, `web_search` or `code_interpreter`, 400 `FEATURE_DISABLED`) is a JSON Problem, opens no SSE stream and leaves every `quota_usage` and `chat_turns` row unchanged, and the quota service has no dependency on the provider or on OAGW.
- [ ] Quota status reporting is accurate and consistent with actual usage: with seeded rows the response lists `premium` and `total` with `daily` and `monthly` periods, `used_credits_micro` equal to spent plus reserved, and `remaining_percentage`, `warning`, `exhausted` and `next_reset` per the rules; after a reserve write the used amount rises by the reserved credits, after settlement it equals the committed credits and no reserve remains, and another user's or tenant's rows never appear.
- [ ] The reserve-before-execute flow is enforced on every user-turn provider call: a `running` turn exists with `reserve_tokens`, `max_output_tokens_applied`, `reserved_credits_micro`, `policy_version_applied`, `effective_model` and `minimal_generation_floor_applied` set and the bucket rows incremented in one committed transaction before the provider call may begin, and a rejected or rolled-back reserve leaves no turn, no reserve and no usage event.
- [ ] A higher tier that is exhausted is downgraded: with premium exhausted in any period or bucket the effective model is the standard candidate with `quota_decision` `downgrade`, `downgrade_from` equal to the selected model and reason `premium_quota_exhausted`; the DESIGN 5.10 numbers hold (premium reserve `3_750_000` fails the `tier:premium` daily check `20_000_000 + 3_750_000 > 22_000_000`, the standard reserve `1_500_000` passes and is the booked reserve); `force_standard_tier`, `disable_premium_tier` and a disabled or missing selected model downgrade with their own reasons; a standard model never upgrades; with no tier available the answer is 429 `resource_exhausted` with scope `tokens`.
- [ ] Credits and tokens are accounted per model and tier: credits follow `ceil_div(input * in_mult, 1_000_000) + ceil_div(output * out_mult, 1_000_000)`, a standard turn changes bucket `total` only and a premium turn changes `total` and `tier:premium`, settlement of the DESIGN 5.10 turn (900 input and 300 output tokens at `1_000_000_000`) moves `1_200_000` to `spent_credits_micro` of the daily and monthly `total` rows and releases the `1_500_000` reserve, `calls` rises by 1 per settlement, and `input_tokens` and `output_tokens` rise only on actual settlements.
- [ ] Every terminal outcome settles exactly once using actual or estimated usage as appropriate: completed turns settle `actual`; failed turns settle `actual` with at least one non-zero usage field and `estimated` otherwise; cancelled and orphan-timeout turns settle `estimated` as `min(reserve_tokens, estimated_input_tokens + minimal_generation_floor_applied)`; pre-provider failure codes settle `released` at 0; a second finalizer of the same turn loses the CAS and changes nothing; no reserve remains after any terminal transaction; a replay takes no reserve, changes no row and enqueues nothing.
- [ ] Usage is published reliably and exactly once per turn: the settlement transaction enqueues exactly one `UsageEvent` in the queue `mini-chat.usage_snapshot` (the configured `outbox.queue_name`) with `dedupe_key` `{tenant_id}/{turn_id}/{request_id}` in 32-character lowercase hexadecimal form, a forced enqueue failure rolls back the quota change, the handler returns `Ok`, `Retry` or `Reject(reason)` as specified so delivery is at least once, and the consumer deduplicates by `(tenant_id, turn_id, request_id)`.
- [ ] Web search and code interpreter are quota-limited correctly at preflight: with the daily `web_search_calls` at `quota.web_search_daily_quota` (default 75) a request with `web_search.enabled` on a model with web search support gets 429 `resource_exhausted` with scope `web_search`, while a request without web search is admitted; the same holds for `code_interpreter_calls` at `quota.code_interpreter_daily_quota` (default 50) with scope `code_interpreter` for a chat with a ready XLSX attachment; the counters are incremented only at settlement and only for `actual` and `estimated` outcomes; a kill switch for web search gives 400 `FEATURE_DISABLED` before the cascade.
- [ ] Credit arithmetic is exact and protected: the per-component rounding differs from rounding the sum where the DESIGN says it may, token counts above `10,000,000` and multipliers of 0 or above `10,000,000,000` are rejected before multiplication, a preflight candidate with a failing computation counts as unavailable (no 500), and a settlement failure leaves the turn `running` with no partial change.
- [ ] A failed credit computation never books and never wraps: with a bucket holding non-zero `spent_credits_micro` and `reserved_credits_micro` and a candidate whose credit computation failed (reserve `i64::MAX`), the tier is unavailable, the availability comparison and the reserve re-check do not wrap, do not panic and return no 500, the cascade downgrades or rejects with 429 `resource_exhausted` (scope `tokens`), and no counter is incremented (DESIGN 5.3 `Overflow Protection (Normative)`).
- [ ] Period boundaries are UTC and stable: the daily and monthly `period_start` follow the four DESIGN examples, a reserve taken at 23:59:59 UTC and settled after midnight is settled against the day-1 rows, and the orphan derivation from `started_at` gives the same rows because `started_at` is the one instant captured at preflight and used for the period starts (also on retry and edit); a preflight at 23:59:59.999 UTC settled after midnight, through the normal path and through the orphan path, changes only the day-1 rows, leaves the day-2 rows untouched and leaves no negative `reserved_credits_micro`.
- [ ] The reserve write re-checks the limits: two reserves that individually pass preflight but together exceed a bucket leave the later one rejected with 429 scope `tokens` and the transaction rolled back, without a new cascade.
- [ ] The estimate is deterministic: for the same catalog entry, message, images, ready attachments and flags it returns the same `estimated_text_tokens`, `prior_context_tokens`, surcharges and reserve, a tool the candidate does not support or a kill switch that is on adds no surcharge, and `minimal_generation_floor_applied` is the smaller of the configured floor and `max_output_tokens_applied`.
- [ ] Overshoot is bounded: an actual settlement within `quota.overshoot_tolerance_factor` commits the actual credits, one beyond it commits `reserved_credits_micro` (the DESIGN example of 11,000 input and 500 output tokens against a reserve of 10,000 tokens and `2_500_000` credits commits `2_500_000` while the message keeps 11,000 and 500), and the turn stays `completed` in both cases.
- [ ] The usage event has the specified fields and omissions: `actual_credits_micro` is the committed amount, `usage` is the provider-reported counts, null (estimated) or zeros (released), both `effective_model` and `selected_model` and `policy_version_applied` are present, and no provider identifier, `reserved_credits_micro`, `reserve_tokens`, `error_code` or quota decision field appears; a system-task event has `requester_type` `system`, `billing_outcome` `system_task`, `settlement_method` `none`, 0 credits, no `user_id` or `turn_id` and the `{tenant_id}/thread_summary_update/{system_request_id}` key, and touches no `chat_turns` or `quota_usage` row.
- [ ] The quota status endpoint follows its contract: the PEP evaluates `read` on `UserQuota` with constraints required, a PDP deny gives 403 `AUTHZ_DENIED`, a PDP failure gives 503 with `Retry-After: 5`, a plugin failure gives 500, a PDP that returns only a tenant predicate still yields only the caller's rows, `warning_threshold_pct` equals the configured value, and the OpenAPI declares 200, 401, 403, 500 and 503.
- [ ] Warning thresholds are exact: with the threshold 80 a period with `remaining_percentage` 20 or less has `warning` true, 0 has `exhausted` true (also when less than 1 percent remains), a period with a limit of 0 or less is omitted from the status and the warnings, and the `quota_warnings` entries carry `next_reset` only when `warning` or `exhausted` is true.
- [ ] The cost metrics are recorded as specified: `mini_chat_quota_preflight`, `mini_chat_quota_reserve`, `mini_chat_quota_commit` (actual settlements only), `mini_chat_quota_overshoot`, `mini_chat_quota_estimated_tokens`, `mini_chat_quota_actual_tokens` (actual settlements only) and `mini_chat_code_interpreter_calls` (actual settlements only) appear with the configured prefix and no tenant or user label, and the declared-but-unrecorded series stay unrecorded.
- [ ] The deferred scope stays unimplemented: no per-user daily image quota, no population of `file_search_calls`, `image_inputs` or `image_upload_bytes`, no per-user daily `file_search` limit, no policy snapshot cache or notify endpoint, no tenant operational bucket and no group chat attribution.
- [ ] Unit tests live in the gear crate, repository and integration tests run in-process over SQLite without a network, no test is added under `testing/e2e/suites/mini_chat/`, and every implemented CDSL step has `@cpt-begin` and `@cpt-end` markers.
