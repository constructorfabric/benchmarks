---
description: "Quota and billing feature of the mini-chat gear: integer credit arithmetic, per-turn policy version binding, preflight token estimation, the daily and monthly bucket cascade with tier downgrade and 429 rejection, the reserve write with limit re-check, CAS-guarded settlement and exactly-once usage events, system-task usage events, tool quotas, the quota status API with warnings, and cost metrics."
---

# Feature: Quota and Billing

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-featstatus-quota-billing-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-mini-chat-feature-quota-billing`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Get Quota Status](#get-quota-status)
  - [Quota Steps of a Chat Turn](#quota-steps-of-a-chat-turn)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Credit Computation](#credit-computation)
  - [Period Keys and Reset Times](#period-keys-and-reset-times)
  - [Policy Resolution](#policy-resolution)
  - [Reserve Estimation](#reserve-estimation)
  - [Tier Availability Check](#tier-availability-check)
  - [Preflight Evaluation](#preflight-evaluation)
  - [Reserve Write and Limit Re-Check](#reserve-write-and-limit-re-check)
  - [Billing Outcome Derivation](#billing-outcome-derivation)
  - [Quota Settlement](#quota-settlement)
  - [Turn Usage Event](#turn-usage-event)
  - [CAS-Guarded Turn Finalization](#cas-guarded-turn-finalization)
  - [System-Task Usage Event](#system-task-usage-event)
  - [Per-Turn Tool Call Limit](#per-turn-tool-call-limit)
  - [Quota View and Warnings](#quota-view-and-warnings)
  - [Quota Metrics Recording](#quota-metrics-recording)
- [4. States (CDSL)](#4-states-cdsl)
  - [Turn Billing State Machine](#turn-billing-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Credit Arithmetic](#credit-arithmetic)
  - [Policy Version Binding](#policy-version-binding)
  - [Token Estimation](#token-estimation)
  - [Preflight and Tier Cascade](#preflight-and-tier-cascade)
  - [Reserve Write](#reserve-write)
  - [Settlement and Finalization](#settlement-and-finalization)
  - [Usage Event Publication](#usage-event-publication)
  - [Tool Quotas](#tool-quotas)
  - [Quota Status API and Warnings](#quota-status-api-and-warnings)
  - [Cost Metrics](#cost-metrics)
  - [Deferred Quota Capabilities](#deferred-quota-capabilities)
  - [Cross-Cutting Quality and Verification](#cross-cutting-quality-and-verification)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

## 1. Feature Context

### 1.1 Overview

This feature delivers the quota service of the `mini-chat` gear (DESIGN §5). It owns the integer credit arithmetic (`credits_micro`, per-component ceiling, overflow protection) and the policy version bound to every turn. It owns the preflight token estimate and the reserve size, and the daily and monthly enforcement over the `total` and `tier:premium` buckets on UTC calendar periods. It owns the premium to standard downgrade cascade with kill switches and model disablement, the 429 rejection when no tier is available, and the reserve write with its in-transaction limit re-check. It owns the settlement and finalization contract: the terminal outcome taxonomy, the billing outcome derivation, the actual, estimated and released settlement formulas, the overshoot cap, the bounded debit of aborted streams, and the CAS-guarded finalization helper that settles quota and enqueues exactly one usage event in one transaction. It also owns the system-task usage event, the daily web-search and code-interpreter quotas and per-turn limits, the quota warnings and `GET /v1/quota/status`, and the billing group of the cost metrics.

The feature exposes a quota service API (preflight evaluation, reserve write, turn finalization, system-task usage event, per-turn tool limit check, quota warnings) and one REST endpoint. It does not decide when a turn finalizes or with which terminal outcome: `cpt-cf-mini-chat-feature-streaming-turns` owns the turn state machine and calls these helpers, `cpt-cf-mini-chat-feature-turn-mutations` calls them for retry and edit, and `cpt-cf-mini-chat-feature-background-workers` calls the orphan variant of the finalization helper and the system-task usage helper.

**Implementation slices** (implement in this order; each slice is independently testable):

| # | Slice | CDSL elements |
|---|-------|---------------|
| 1 | Credit arithmetic, period keys and policy resolution | `cpt-cf-mini-chat-algo-quota-billing-credits-micro`, `cpt-cf-mini-chat-algo-quota-billing-period-keys`, `cpt-cf-mini-chat-algo-quota-billing-resolve-policy` |
| 2 | Estimation, availability and preflight | `cpt-cf-mini-chat-algo-quota-billing-estimate-reserve`, `cpt-cf-mini-chat-algo-quota-billing-tier-availability`, `cpt-cf-mini-chat-algo-quota-billing-preflight` |
| 3 | Reserve write and the quota steps of a turn | `cpt-cf-mini-chat-algo-quota-billing-reserve-write`, `cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps` |
| 4 | Settlement, usage event and CAS-guarded finalization | `cpt-cf-mini-chat-algo-quota-billing-billing-outcome`, `cpt-cf-mini-chat-algo-quota-billing-settle`, `cpt-cf-mini-chat-algo-quota-billing-turn-usage-event`, `cpt-cf-mini-chat-algo-quota-billing-finalize-turn`, `cpt-cf-mini-chat-state-quota-billing-turn-billing` |
| 5 | System-task usage and per-turn tool limits | `cpt-cf-mini-chat-algo-quota-billing-system-task-usage`, `cpt-cf-mini-chat-algo-quota-billing-tool-call-limit` |
| 6 | Quota status, warnings and metrics | `cpt-cf-mini-chat-algo-quota-billing-quota-view`, `cpt-cf-mini-chat-flow-quota-billing-get-quota-status`, `cpt-cf-mini-chat-algo-quota-billing-record-metrics` |

**Out of scope** (owned by other features): the send-message, retry and edit pipelines that call the helpers, the turn state machine and the choice of terminal outcome, assistant message persistence, the turn audit event, the SSE `done` / `error` events and the placement of `quota_warnings` in `done` (`cpt-cf-mini-chat-feature-streaming-turns`, `cpt-cf-mini-chat-feature-turn-mutations`). Also out of scope: the tool-enablement decision in the provider request and the mid-turn counting of tool start events, the vision check, `INPUT_TOO_LONG` and context assembly (`cpt-cf-mini-chat-feature-streaming-turns`), and orphan watchdog scheduling and the thread-summary handler that call the helpers (`cpt-cf-mini-chat-feature-background-workers`). Not implemented per `cpt-cf-mini-chat-adr-quota-policy-scope`: the image-input daily quota, the image counters, the per-message image byte cap, policy snapshot caching and `POST /internal/policy:notify`, and the tenant operational bucket, audit and kill-switch checks for system tasks. The request gate, PEP, error mapping, schema, outbox queues, usage handler and event encoding come from `cpt-cf-mini-chat-feature-gear-foundation` and are referenced, not redefined.

### 1.2 Purpose

Every provider call costs money, and the user has a single credit wallet for chat. The gear must therefore decide before any outbound call whether the user may run the turn, and on which model. It must book a worst-case reserve so that parallel turns cannot overspend, and settle every reserved turn exactly once from provider usage or a deterministic estimate. It must hand one idempotent usage event per settled turn to the billing system, and show users how much quota remains before they hit a wall.

**Requirements**: `cpt-cf-mini-chat-fr-quota-enforcement`, `cpt-cf-mini-chat-fr-quota-billing-architecture`, `cpt-cf-mini-chat-fr-web-search`, `cpt-cf-mini-chat-fr-code-interpreter`, `cpt-cf-mini-chat-fr-group-chats`, `cpt-cf-mini-chat-fr-cost-metrics`, `cpt-cf-mini-chat-nfr-cost-control`

`cpt-cf-mini-chat-fr-web-search` and `cpt-cf-mini-chat-fr-code-interpreter` are covered here only for their quota facet: the daily quotas, the per-turn limits as quota decisions, the reserve surcharges and the call counters. Tool enablement, guards and citations belong to `cpt-cf-mini-chat-feature-streaming-turns`. `cpt-cf-mini-chat-fr-group-chats` is traced only to confirm its P2 status: every bucket is keyed by the requesting user, which keeps the per-sender attribution model of `cpt-cf-mini-chat-adr-group-chat-usage-attribution` open. `cpt-cf-mini-chat-fr-cost-metrics` is covered for its billing group (token and credit values per request, billing outcome, quota metrics); latency and tool-call metrics belong to `cpt-cf-mini-chat-feature-streaming-turns`.

**Principles**: `cpt-cf-mini-chat-principle-tenant-scoped`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-mini-chat-actor-chat-user` | Calls `GET /v1/quota/status`; every turn the user sends, retries or edits is preflighted, reserved and settled against the user's own buckets |
| Turn pipeline (stream service of `cpt-cf-mini-chat-feature-streaming-turns`, mutation path of `cpt-cf-mini-chat-feature-turn-mutations`) | Calls the preflight evaluation, the reserve write, the CAS-guarded finalization (stream variant), the per-turn tool limit check and the quota warnings computation |
| Orphan watchdog (`cpt-cf-mini-chat-feature-background-workers`) | Calls the orphan variant of the CAS-guarded finalization for stale `running` turns |
| Thread-summary handler (`cpt-cf-mini-chat-feature-background-workers`) | Calls the system-task usage event helper in the transaction that upserts the summary |
| Model policy plugin (via the model-policy gateway) | Supplies the current policy version, the policy snapshot (catalog, multipliers, estimation budgets, kill switches) and the user limits; receives `publish_usage` from the foundation usage handler |
| ToolKit runtime (gear host) | Registers `GET {url_prefix}/v1/quota/status`; runs the authentication and license middleware; hosts the shared outbox that stores and delivers usage events |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — `cpt-cf-mini-chat-fr-quota-enforcement`, `cpt-cf-mini-chat-fr-quota-billing-architecture`, `cpt-cf-mini-chat-fr-web-search`, `cpt-cf-mini-chat-fr-code-interpreter`, `cpt-cf-mini-chat-fr-cost-metrics`, `cpt-cf-mini-chat-nfr-cost-control`, the quota status API of PRD §7
- **Design**: [DESIGN.md](../DESIGN.md) — §2.2 "Downgrade Decision Matrix", "Model Locked Per Chat", "Quota Before Outbound"; §3.2 quota service (`cpt-cf-mini-chat-component-quota-service`), "Quota Period Reset Semantics", "Quota Warning Thresholds (P1)", "Quota Status Endpoint (P1)", "System Task Isolation Invariant (P1)", "System Task Attribution Rules (Normative)"; §3.3 endpoints table, `event: done` (`quota_warnings`), REST error mapping; §3.6 `cpt-cf-mini-chat-seq-send-message` (quota steps); §3.7 `chat_turns`, `quota_usage`, "Bucket Semantics and Naming Clarification"; §3.8 "User Quota Resource Type" and the authorization matrix; §4 "Web Search Configuration", "Code Interpreter Configuration", their quota enforcement rules, "Two-Tier Rate Limiting & Throttling" (`cpt-cf-mini-chat-design-throttling-tiers`), "Downgrade Decision Flow", "Effective Model Computed Once", "Metrics (OTLP) and Alerts (P1)"; §5.1 to §5.13, including `cpt-cf-mini-chat-flow-quota-enforced-chat-turn`, `cpt-cf-mini-chat-algo-quota-billing-token-estimation`, `cpt-cf-mini-chat-design-quota-usage-accounting`, `cpt-cf-mini-chat-design-nonterminal-reconciliation`, `cpt-cf-mini-chat-design-terminal-error-reconciliation`; Appendix A.1 to A.3; Appendix B.2, B.4, B.5, B.6, B.6.1
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) entry 2.3
- **Acceptance checklist**: [acceptance-criteria.md](../acceptance-criteria.md) — owned items: Principles & Constraints "Quota is checked before any outbound provider call"; Quota Status API; Quota Enforcement (all three items); Settlement & Finalization (both items); Web Search "Web search tool use is reported, cited, accounted, and quota-limited correctly" (accounting and quota-limit part; reporting and citations are owned by `cpt-cf-mini-chat-feature-streaming-turns`)
- **ADRs**: `cpt-cf-mini-chat-adr-quota-policy-scope`, `cpt-cf-mini-chat-adr-group-chat-usage-attribution`, `cpt-cf-mini-chat-adr-runtime-consistency-limitations`, `cpt-cf-mini-chat-adr-document-retrieval-scope`, `cpt-cf-mini-chat-adr-canonical-error-contract`
- **Foundation elements reused by reference** (from `cpt-cf-mini-chat-feature-gear-foundation`, not redefined here): request gate `cpt-cf-mini-chat-flow-gear-foundation-request-gate`; PEP `cpt-cf-mini-chat-algo-gear-foundation-authorize` (user-quota resource); chat-scoped access `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access`; DomainError to `Problem` mapping `cpt-cf-mini-chat-algo-gear-foundation-map-error` (row "Quota exhausted" and row "Feature disabled by kill switch"); plugin resolution `cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin`; outbox queues `cpt-cf-mini-chat-algo-gear-foundation-start-outbox`; usage handler `cpt-cf-mini-chat-algo-gear-foundation-handle-usage`; event encoding `cpt-cf-mini-chat-algo-gear-foundation-encode-events`; configuration `cpt-cf-mini-chat-algo-gear-foundation-load-config` (`quota`, `streaming.max_output_tokens`, `estimation_budgets.minimal_generation_floor`, `outbox.queue_name`); metric instruments `cpt-cf-mini-chat-algo-gear-foundation-register-metrics`; schema `cpt-cf-mini-chat-dod-gear-foundation-schema`
- **Dependencies**: `cpt-cf-mini-chat-feature-gear-foundation`

**Source precedence**: DESIGN is authoritative for details. Where DESIGN delegates response schemas to the generated OpenAPI (`docs/api/api.json`), the schemas `QuotaStatusResponse`, `QuotaTierStatus`, `QuotaPeriodStatus`, `QuotaTier`, `QuotaPeriod` and `QuotaWarning` apply. The gear docs win over `config/mini-chat.yaml` and platform docs (DECOMPOSITION task-level overrides).

**REST contract** (path under `{url_prefix}`, default `/mini-chat`; requires authentication and the license feature `gts.cf.core.lic.feat.v1~cf.core.global.base.v1`):

| Method and path | PEP resource, action, `resource.id`, `require_constraints` | Request | Success | Domain errors (besides gate and PEP errors) |
|---|---|---|---|---|
| `GET /v1/quota/status` | `gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~`, `read`, absent, `true` | none (no query parameters are defined; any query string is ignored) | 200, `QuotaStatusResponse` | 500 `internal` (policy plugin missing, failing or timing out; database error) |

`QuotaStatusResponse` has exactly `tiers` (array) and `warning_threshold_pct` (integer, the configured `quota.warning_threshold_pct`). Each `tiers[]` entry (`QuotaTierStatus`) has `tier` (`premium` | `total`) and `periods` (array). Each `periods[]` entry (`QuotaPeriodStatus`) has exactly `period` (`daily` | `monthly`), `limit_credits_micro`, `used_credits_micro`, `remaining_credits_micro` (64-bit integers), `remaining_percentage` (integer 0 to 100), `next_reset` (RFC 3339 UTC date-time), `warning` and `exhausted` (booleans). Tool quotas (web search, code interpreter), billing outcomes and settlement details are not exposed. A `quota_warnings` entry (`QuotaWarning`) has `tier`, `period`, `remaining_percentage`, `warning`, `exhausted` and `next_reset`, which is present only when `warning` or `exhausted` is `true`.

**Feature-local deviations and interpretations** (review owner: mini-chat gear maintainers):

| Topic | Deviation or interpretation | Rationale |
|-------|-----------------------------|-----------|
| Status arithmetic | `used_credits_micro` = `spent_credits_micro` + `reserved_credits_micro` of the bucket row, not clamped (it may exceed the limit after an overshoot); `remaining_credits_micro` = the limit minus used, clamped at 0; `remaining_percentage` = remaining × 100 / limit, floored, computed in 128-bit integers and clamped to `0..=100`; a missing bucket row counts as 0 spent and 0 reserved | DESIGN names the fields, says "0-100" and "floored integer division" but not how an over-limit bucket is shown; a negative remaining value would contradict the `minimum: 0` of the OpenAPI percentage and the "remaining" meaning |
| Status ordering and empty tiers | `tiers` is ordered `premium`, then `total`; `periods` is ordered `daily`, then `monthly`; a period whose limit is `<= 0` is omitted; a tier whose periods are all omitted is still listed with an empty `periods` array | DESIGN fixes the contents but not the order; the OpenAPI requires `periods` on every tier entry; a stable order makes clients and tests deterministic |
| Limits used by the status endpoint | The status endpoint reads the current policy version and the user limits of that version; it does not read the policy snapshot | The status shape needs only the limits and the bucket rows; DESIGN §5.2.8 says user limits are needed only at preflight |
| Limits and periods of `quota_warnings` | The warnings are computed after the finalization commit from the user limits captured by the turn's preflight and the bucket rows of the current UTC periods at computation time | Reusing the preflight limits avoids a plugin call on the terminal path; the current periods make `next_reset` meaningful when a turn straddles midnight |
| Both premium kill switches set | When `force_standard_tier` and `disable_premium_tier` are both `true`, the downgrade reason is `force_standard_tier` | DESIGN lists both reasons for the premium skip without an order; one fixed reason keeps `done.downgrade_reason` deterministic |
| Several `is_default` models in a tier | The candidate is the first enabled model of the tier in catalog order whose `preference.is_default` is `true` | DESIGN says "the model whose preference has `is_default: true`" without a tiebreaker; catalog order is the order DESIGN uses elsewhere |
| `prior_context_tokens` source | `input_tokens + output_tokens` of the chat's assistant message with `deleted_at IS NULL` and `input_tokens > 0` or `output_tokens > 0` that is latest by `created_at`, then `id`, read at preflight; 0 when none exists | DESIGN defines "most recent non-deleted assistant message with non-zero token counts"; `created_at, id` is the strict message order of the Messages API |
| "Ready document" for the file search surcharge | A non-deleted attachment of the chat with `for_file_search = true` and `status = ready`; "ready code-interpreter attachment" is a non-deleted attachment with `for_code_interpreter = true` and `status = ready`; the turn pipeline supplies both flags | These are the same predicates that later decide whether `file_search` and `code_interpreter` are sent, as DESIGN §5.5.6 requires ("the same gates that later build the tool list") |
| Preflight metric on rejection | `mini_chat_quota_preflight` is recorded once per cascade outcome: `decision` `allow` or `downgrade` with the effective model and its tier, or `decision` `reject` with the selected model id and the starting tier; tool-quota rejections do not record a second preflight count | DESIGN names the labels `decision`, `model`, `tier` without values for a rejection that has no effective model |
| Missing bucket row at settlement | Settlement writes the bucket rows with an upsert on the unique key; a missing row is created with the same deltas applied to zero values | The orphan path derives the period from `chat_turns.started_at`, which can fall on the next day when preflight ran just before midnight; failing the finalization would leave the turn `running` forever |
| Unknown error code | Classified `failed` with `settlement_method` `estimated`, logged at `error` with the code; `mini_chat_unknown_error_code` stays unrecorded | DESIGN §5.8 requires the classification and lists the metric as declared but not recorded |
| `usage` field of a turn event | Actual settlement: the provider usage object with the five token fields (missing fields as 0), or `null` when a completed turn received no usage object; estimated settlement: `null`; released settlement: all five fields 0 | DESIGN: `usage` is `null` when the provider reported no usage and the estimated split is not emitted; a failed turn with an all-zero usage object is "usage unknown" and therefore estimated |
| Turn without `requester_user_id` | The finalization helper (orphan and stream variants) checks `requester_user_id` after the CAS. When it is NULL the helper logs at `error`, skips settlement (no `quota_usage` row changes) and skips the usage enqueue (no event is encoded or enqueued). It still returns `won`, so the CAS commits and the turn is finalized. A turn with NULL reserve fields and a non-NULL `requester_user_id` keeps the DESIGN behaviour: a warning and an `aborted` / `estimated` event with 0 credits | DESIGN says to skip settlement with a warning and still enqueue the usage event. But the SDK `UsageEvent` invariant requires `user_id` for `requester_type = user`, so the encoding would fail and roll back the CAS. The turn would then stay `running` and the watchdog would retry it forever. The case is reachable only through corrupt data, hence the `error` level |
| Deterministic settlement fallback | Some settlement errors cannot succeed on retry: the snapshot of `policy_version_applied` is unavailable (plugin error or timeout on the fetch before the transaction), `effective_model` is missing from that snapshot, or a credit computation fails (overflow, invalid token count, invalid multiplier). In these cases the CAS winner does not roll back. It settles `estimated` from persisted columns only: charged tokens are `min(reserve_tokens, estimated_input_tokens + minimal_generation_floor_applied)` as in DESIGN §5.8, and the committed credits are `chat_turns.reserved_credits_micro`. The premium tier comes from the snapshot entry when it exists; otherwise from the persisted `quota_usage.reserved_credits_micro` of the turn's `tier:premium` rows (see `cpt-cf-mini-chat-algo-quota-billing-settle`). The usage event carries `settlement_method` `estimated` and the case is logged at `error`. A `released` settlement stays `released` with 0 credits | DESIGN §5.4.4 forbids `reserved_credits_micro` as the charge on the normal estimated path, and §5.7 says no turn may permanently evade billing. Without multipliers, the §5.8 credit amount cannot be computed. Leaving the turn `running` would loop forever: the watchdog would hit the same error on every scan. `reserved_credits_micro` is the only persisted credit value and is already booked against every limit, so committing it cannot exceed a limit. The result is deterministic for the same row. The overcharge is bounded by the output share above the floor and is visible in the `error` log for reconciliation. With no tier column on `chat_turns`, the reserved premium rows are the only persisted trace of the tier. A standard turn can be taken for premium only when other in-flight premium reserves cover its reserve in both periods |
| Cost record per request | Every settlement writes one structured log record `quota settled` with chat, turn and request ids, effective model, actual input and output tokens, `reserved_credits_micro`, committed credits, released credits (`reserved_credits_micro` minus committed, at least 0), billing outcome, settlement method and tool call counts; tenant and user attribution stays in the usage event | `cpt-cf-mini-chat-fr-cost-metrics` requires the values "for every LLM request" and forbids tenant and user labels; DESIGN defines no metric instrument for credits |

**External-call timeouts** (foundation gear constants, reused unchanged; every call to a dependency that can stall is bounded):

| Constant | Value | Call in this feature | Outcome on timeout |
|----------|-------|----------------------|--------------------|
| `PDP_EVALUATION_TIMEOUT` | 5 s | PEP evaluation of `GET /v1/quota/status` (user-quota resource) | 503 `service_unavailable`, `Retry-After: 5`, `context.retry_after_seconds = 5` |
| `REGISTRY_QUERY_TIMEOUT` | 10 s | Model-policy plugin resolution in `cpt-cf-mini-chat-algo-quota-billing-resolve-policy` | Preflight and status: 500 `internal`, nothing reserved; settlement snapshot fetch (before the finalization transaction opens): the snapshot is `unavailable` and the CAS winner settles with the deterministic estimated fallback |
| `POLICY_PLUGIN_CALL_TIMEOUT` | 10 s | `get_current_policy_version`, `get_policy_snapshot`, `get_user_limits` at preflight; `get_current_policy_version`, `get_user_limits` at status; `get_policy_snapshot` of `policy_version_applied` before the finalization transaction opens | Preflight: 500 `internal`, no stream is opened and no reserve is written; status: 500 `internal`; settlement: logged at `error`, the snapshot is `unavailable` and the CAS winner settles with the deterministic estimated fallback; no row or write lock is held during the call |
| `PUBLISH_USAGE_TIMEOUT` | 20 s | `publish_usage` in the foundation usage handler (unchanged) | `Retry` |

Database statements run inside the caller's transaction (preflight read transaction, reserve transaction, finalization transaction) or the status request. No plugin call runs while a write transaction is open. The reserve and finalization transactions follow the transaction discipline of `cpt-cf-mini-chat-algo-quota-billing-reserve-write` and `cpt-cf-mini-chat-algo-quota-billing-finalize-turn`: write-first, canonical `quota_usage` row order, and a bounded retry on `SQLITE_BUSY` or a PostgreSQL serialization or deadlock failure. Any other database failure, or a retry budget that runs out, rolls the transaction back. It then surfaces as 500 `internal`, or as a failed finalization on the terminal path. No other external system is called by this feature.

**Audit**: this feature emits no audit event. The turn audit event (with `policy_decisions.quota.decision` taken from the preflight decision) is built and enqueued by the turn pipeline in the same finalization transaction.

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (human or external system) and describe the end-to-end flow of a use case. Each flow has a triggering actor and shows how the system responds to actor actions.

**Use cases**: `cpt-cf-mini-chat-usecase-send-message` (quota steps only: preflight, reserve, settlement and usage event; the end-to-end flow belongs to `cpt-cf-mini-chat-feature-streaming-turns`).

### Get Quota Status

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-quota-billing-get-quota-status`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- 200 with `tiers` `premium` and `total`, each with its `daily` and `monthly` entries, and `warning_threshold_pct`; a user without any usage gets `used_credits_micro` 0 and `remaining_percentage` 100 in every period with a positive limit.
- Usage settled or reserved by the user's turns is reflected immediately (`used_credits_micro` includes in-flight reserves).

**Error Scenarios**:
- Gate and PEP errors of `cpt-cf-mini-chat-flow-gear-foundation-request-gate`: 401, 403 `LICENSE_FEATURE_REQUIRED`, 403 `AUTHZ_DENIED`, 503 with `Retry-After: 5`.
- Model-policy plugin missing, failing or timing out, or a database error: 500 `internal`.

**Steps**:
1. [x] - `p1` - User calls `GET {url_prefix}/v1/quota/status` through the request gate (see `cpt-cf-mini-chat-flow-gear-foundation-request-gate`); the request has no body and any query string is ignored - `inst-call-status`
2. [x] - `p1` - Authorize the user-quota resource type `gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~`, action `read`, no `resource.id`, `require_constraints` `true`, declared properties `owner_tenant_id` and `owner_id` (see `cpt-cf-mini-chat-algo-gear-foundation-authorize`); the compiled scope is restricted to the subject tenant and the subject user - `inst-authorize-status`
3. [x] - `p1` - Resolve the current policy version and the user limits of the caller (see `cpt-cf-mini-chat-algo-quota-billing-resolve-policy`, status mode); **IF** it returns an error, **RETURN** 500 `internal` - `inst-resolve-limits`
4. [x] - `p1` - Compute the period keys and reset times for the current UTC time (see `cpt-cf-mini-chat-algo-quota-billing-period-keys`) - `inst-status-periods`
5. [x] - `p1` - DB: SELECT `quota_usage` through Secure ORM with the compiled scope (tenant column `tenant_id`, owner column `user_id`), filtered by (`period_type` `daily`, `period_start` the daily start) or (`period_type` `monthly`, `period_start` the monthly start) and `bucket` in `total`, `tier:premium` - `inst-select-buckets`
6. [x] - `p1` - Build the status view from the limits, the rows, `quota.warning_threshold_pct` and the reset times (see `cpt-cf-mini-chat-algo-quota-billing-quota-view`, status mode) - `inst-build-status`
7. [x] - `p1` - **RETURN** 200 with `{ tiers, warning_threshold_pct }` - `inst-return-status`

### Quota Steps of a Chat Turn

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- A turn whose tier is available is reserved before any provider call, runs on the effective model with `max_output_tokens_applied` as hard cap, and is settled exactly once with one usage event, all in the finalization transaction.
- A premium turn whose premium buckets are exhausted runs on the standard candidate (`downgrade`, `premium_quota_exhausted`).

**Error Scenarios**:
- No tier available, or a reserve that no longer fits at the re-check: 429 `resource_exhausted` with `violations[{subject: tokens, description: quota_exceeded}]`, no SSE stream, no provider call.
- Daily web-search or code-interpreter quota exhausted for a turn that uses the tool: 429 with subject `web_search` or `code_interpreter`.
- `web_search.enabled` with `disable_web_search`: 400 `failed_precondition` `violations[{subject: web_search, type: FEATURE_DISABLED}]`.
- Policy plugin failure or timeout: 500 `internal`.

**Steps**:
1. [ ] - `p1` - User sends a message (`POST /v1/chats/{id}/messages:stream`), or retries or edits a turn; the turn pipeline of `cpt-cf-mini-chat-feature-streaming-turns` authorizes, loads the chat, applies idempotency and the parallel-turn guard, and rejects a chat model that is missing from the catalog with 400 `INVALID_MODEL`, all before any quota step - `inst-user-sends`
2. [x] - `p1` - Turn pipeline calls the preflight evaluation with the tenant, the user, the chat id, `chats.model` as selected model, the UTF-8 byte length of the message content, the image count, `web_search.enabled` and the two ready-attachment flags (see `cpt-cf-mini-chat-algo-quota-billing-preflight`); on retry and edit this runs before the mutation commit - `inst-call-preflight`
3. [ ] - `p1` - **IF** preflight returns an error - `inst-if-preflight-error`
   1. [ ] - `p1` - **RETURN** the JSON `Problem` of `cpt-cf-mini-chat-algo-gear-foundation-map-error` (400 `FEATURE_DISABLED`, 429 `quota_exceeded` with subject `tokens`, `web_search` or `code_interpreter`, or 500); no SSE stream is opened, no turn row and no reserve exist, and no provider request is made - `inst-return-preflight-error`
4. [ ] - `p1` - Turn pipeline runs its own pre-provider steps with the preflight decision (vision check of the effective model, image kill switch, input limit, context assembly with the effective model's estimation budgets, provider resolution of the effective model's `provider_id`) - `inst-pipeline-checks`
5. [x] - `p1` - Turn pipeline opens the reserve transaction write-first (no read before the first write; every read it needs ran before the transaction opened) and calls the reserve write (see `cpt-cf-mini-chat-algo-quota-billing-reserve-write`): send mode on the send path, where the pipeline then inserts the user message, the `message_attachments` rows and the `chat_turns` row with state `running` and the six returned preflight values; fill mode on retry and edit, as the last setup step after the mutation commit - `inst-reserve-transaction`
6. [x] - `p1` - **IF** the reserve write returns the token quota error - `inst-if-reserve-rejected`
   1. [x] - `p1` - Turn pipeline rolls the reserve transaction back and **RETURN** 429 `quota_exceeded` with subject `tokens`; on the send path nothing is left behind; on retry and edit the pipeline moves the already committed new turn to `failed` with `error_code` `quota_exceeded` through a plain CAS, without settlement and without a usage event - `inst-return-reserve-rejected`
7. [ ] - `p1` - Turn pipeline commits the reserve transaction, records the reserve metrics (see `cpt-cf-mini-chat-algo-quota-billing-record-metrics`) and only then sends the provider request through OAGW with `max_output_tokens_applied` as the output cap - `inst-call-provider`
8. [x] - `p1` - **WHILE** the provider streams, the turn pipeline checks each `web_search` and `code_interpreter` start event against the per-turn limit (see `cpt-cf-mini-chat-algo-quota-billing-tool-call-limit`) - `inst-check-tool-limits`
9. [x] - `p1` - On the terminal trigger chosen by the turn pipeline (provider done or incomplete, provider error, tool limit breach, client disconnect), the pipeline first lets the helper fetch the snapshot of the turn's `policy_version_applied` (the preflight decision's `policy_version`), then opens its finalization transaction write-first and calls the stream variant of the CAS-guarded finalization inside it (see `cpt-cf-mini-chat-algo-quota-billing-finalize-turn`) - `inst-call-finalize`
10. [x] - `p1` - **IF** the finalization reports a lost CAS - `inst-if-cas-lost`
    1. [x] - `p1` - **RETURN** without settlement, usage event or quota metrics (another finalizer owns the turn) - `inst-return-cas-lost`
11. [x] - `p1` - Turn pipeline commits the finalization transaction (re-running it within the bounded retry of `cpt-cf-mini-chat-algo-quota-billing-finalize-turn` on a lock or serialization conflict) and records the settlement metrics (see `cpt-cf-mini-chat-algo-quota-billing-record-metrics`) - `inst-commit-finalization`
12. [ ] - `p1` - **IF** the committed state is `completed` - `inst-if-completed`
    1. [x] - `p1` - Compute the quota warnings with the user limits captured by the preflight (see `cpt-cf-mini-chat-algo-quota-billing-quota-view`, warnings mode) for the `done` event - `inst-compute-warnings`
13. [ ] - `p1` - **RETURN** the settled turn - `inst-return-settled`

## 3. Processes / Business Logic (CDSL)

Internal system functions and procedures that do not interact with actors directly. Examples: database layer operations, authorization logic, middleware, validation routines, library functions, background jobs. These are reusable building blocks called by Actor Flows or other processes.

### Credit Computation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-billing-credits-micro`

**Input**: `input_tokens`, `output_tokens`, `in_mult` (`input_tokens_credit_multiplier_micro`), `out_mult` (`output_tokens_credit_multiplier_micro`), all integers

**Output**: Credits in `credits_micro` (64-bit integer; 1 credit is 1_000_000 `credits_micro`), or a credit error naming the offending value

**Steps**:
1. [x] - `p1` - **IF** `input_tokens` is negative or greater than 10_000_000 - `inst-check-input-tokens`
   1. [x] - `p1` - **RETURN** invalid token count error naming `input_tokens` and its value - `inst-return-input-tokens`
2. [x] - `p1` - **IF** `output_tokens` is negative or greater than 10_000_000 - `inst-check-output-tokens`
   1. [x] - `p1` - **RETURN** invalid token count error naming `output_tokens` and its value - `inst-return-output-tokens`
3. [x] - `p1` - **IF** `in_mult` or `out_mult` is 0 - `inst-check-zero-mult`
   1. [x] - `p1` - **RETURN** zero multiplier error naming the multiplier (a zero multiplier would make the usage free) - `inst-return-zero-mult`
4. [x] - `p1` - **IF** `in_mult` or `out_mult` is greater than 10_000_000_000 or does not fit a signed 64-bit integer - `inst-check-mult-range`
   1. [x] - `p1` - **RETURN** invalid multiplier error naming the multiplier and its value - `inst-return-mult-range`
5. [x] - `p1` - Multiply `input_tokens` by `in_mult` and `output_tokens` by `out_mult` with checked 64-bit multiplication; **IF** either product overflows, **RETURN** multiplication overflow error - `inst-multiply`
6. [x] - `p1` - Divide each product by 1_000_000 separately, rounding up (a non-zero remainder adds 1); the ceiling is applied per component, never to the sum - `inst-ceil-per-component`
7. [x] - `p1` - Add the two components with checked addition; **IF** it overflows, **RETURN** addition overflow error - `inst-add-components`
8. [x] - `p1` - **RETURN** the sum; floating-point arithmetic is never used - `inst-return-credits`

### Period Keys and Reset Times

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-billing-period-keys`

**Input**: A UTC instant (the current time at preflight and status, `chat_turns.started_at` on the orphan path)

**Output**: Daily and monthly `period_start` DATE values and their next reset instants

**Steps**:
1. [x] - `p1` - Convert the instant to UTC; the daily `period_start` is its UTC calendar date (for example `2026-02-28T23:59:59Z` gives `2026-02-28`, `2026-03-01T00:00:00Z` gives `2026-03-01`) - `inst-daily-start`
2. [x] - `p1` - The monthly `period_start` is the first day of that UTC month (for example `2026-02-28T15:30:00Z` gives `2026-02-01`) - `inst-monthly-start`
3. [x] - `p1` - The daily next reset is midnight UTC of the day after the daily start; the monthly next reset is midnight UTC of the first day of the following month; both are rendered as RFC 3339 with the `Z` offset (for example `2026-03-01T00:00:00Z`) - `inst-next-reset`
4. [x] - `p1` - Use no tenant timezone and no other period type (`4h` and `weekly` are P2+) - `inst-utc-only`
5. [x] - `p1` - **RETURN** the two period starts and the two reset instants - `inst-return-periods`

### Policy Resolution

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-billing-resolve-policy`

**Input**: User id, mode (`preflight`, `status` or `settlement`), and for settlement the turn's `policy_version_applied`

**Output**: Policy version, policy snapshot and user limits (preflight); policy version and user limits (status); the snapshot of the given version (settlement); or an internal error

**Steps**:
1. [x] - `p1` - Resolve the model-policy plugin (see `cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin`); **IF** no plugin is registered or resolution fails (including `REGISTRY_QUERY_TIMEOUT`), **RETURN** internal error - `inst-resolve-plugin`
2. [x] - `p1` - **IF** mode is `settlement` - `inst-if-settlement-mode`
   1. [x] - `p1` - API: `get_policy_snapshot(user_id, policy_version_applied)` bounded by `POLICY_PLUGIN_CALL_TIMEOUT` (10 s); never the current version; called before the finalization transaction opens, never inside it - `inst-snapshot-by-version`
   2. [x] - `p1` - **RETURN** the snapshot, or internal error on plugin error or timeout (the finalization helper turns this error into an `unavailable` snapshot, see `cpt-cf-mini-chat-algo-quota-billing-finalize-turn`) - `inst-return-settlement-snapshot`
3. [x] - `p1` - API: `get_current_policy_version(user_id)` bounded by `POLICY_PLUGIN_CALL_TIMEOUT` - `inst-current-version`
4. [x] - `p1` - **IF** mode is `preflight` - `inst-if-preflight-mode`
   1. [x] - `p1` - API: `get_policy_snapshot(user_id, version)` bounded by `POLICY_PLUGIN_CALL_TIMEOUT` - `inst-current-snapshot`
5. [x] - `p1` - API: `get_user_limits(user_id, version)` bounded by `POLICY_PLUGIN_CALL_TIMEOUT` (no tenant id is passed; limits are keyed by user and version) - `inst-user-limits`
6. [x] - `p1` - **IF** any call returns a plugin error or its timeout elapses - `inst-if-policy-failed`
   1. [x] - `p1` - Log the cause and **RETURN** internal error (no partial result is used) - `inst-return-policy-error`
7. [x] - `p1` - Keep nothing in a cache: every preflight and status call repeats these calls (`cpt-cf-mini-chat-adr-quota-policy-scope`) - `inst-no-cache`
8. [x] - `p1` - **RETURN** the version, the snapshot (preflight only) and the user limits - `inst-return-policy`

### Reserve Estimation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-billing-estimate-reserve`

**Input**: Candidate catalog entry, kill switches, message UTF-8 byte length, image count, `web_search_enabled`, `has_ready_file_search_documents`, `has_ready_code_interpreter_files`, `prior_context_tokens`, `streaming.max_output_tokens`, `estimation_budgets.minimal_generation_floor` (gear configuration)

**Output**: `estimated_input_tokens`, `max_output_tokens_applied`, `reserve_tokens`, `reserved_credits_micro`, `minimal_generation_floor_applied` of the candidate

**Steps**:
1. [x] - `p1` - Read the `estimation_budgets` of the candidate catalog entry (the decoded defaults apply when the entry omits them); never read the deprecated gear-configuration budget fields - `inst-read-budgets`
2. [x] - `p1` - Set the bytes-per-token ratio to `bytes_per_token_conservative`, or 1 when it is 0 - `inst-clamp-ratio`
3. [x] - `p1` - Compute the base estimate as the message byte length divided by the ratio rounded up, plus `fixed_overhead_tokens` (an empty message gives `fixed_overhead_tokens`) - `inst-base-estimate`
4. [x] - `p1` - Compute `estimated_text_tokens` as the base estimate multiplied by (100 plus `safety_margin_pct`) and then divided by 100 rounded up, in integer arithmetic (for example 1000 bytes, ratio 4, overhead 100, margin 10 give 385) - `inst-text-tokens`
5. [x] - `p1` - Compute `image_surcharge_tokens` as the image count multiplied by `image_token_budget` - `inst-image-surcharge`
6. [x] - `p1` - Add `tool_surcharge_tokens` only when `has_ready_file_search_documents`, the candidate's `general_config.tool_support.file_search` is `true` and `disable_file_search` is `false`; otherwise 0 - `inst-tool-surcharge`
7. [x] - `p1` - Add `web_search_surcharge_tokens` only when `web_search_enabled` and the candidate's `tool_support.web_search` is `true`; otherwise 0 - `inst-web-surcharge`
8. [x] - `p1` - Add `code_interpreter_surcharge_tokens` only when `has_ready_code_interpreter_files`, the candidate's `tool_support.code_interpreter` is `true` and `disable_code_interpreter` is `false`; otherwise 0; `search_knowledge` has no surcharge - `inst-ci-surcharge`
9. [x] - `p1` - Compute `estimated_input_tokens` as the sum of `estimated_text_tokens`, `prior_context_tokens` and the image, file search, web search and code interpreter surcharges, each applied once per turn, with checked arithmetic - `inst-estimated-input`
10. [x] - `p1` - Set `max_output_tokens_applied` to the smaller of the candidate's `max_output_tokens` and `streaming.max_output_tokens` - `inst-max-output`
11. [x] - `p1` - Set `reserve_tokens` to `estimated_input_tokens` plus `max_output_tokens_applied` - `inst-reserve-tokens`
12. [x] - `p1` - Set `minimal_generation_floor_applied` to the smaller of the gear configuration `estimation_budgets.minimal_generation_floor` and `max_output_tokens_applied` (the snapshot value of the floor is never read) - `inst-floor-applied`
13. [x] - `p1` - Compute `reserved_credits_micro` with `estimated_input_tokens`, `max_output_tokens_applied` and the candidate's two multipliers (see `cpt-cf-mini-chat-algo-quota-billing-credits-micro`) - `inst-reserve-credits`
14. [x] - `p1` - **IF** the sum overflows or the credit computation returns an error - `inst-if-estimate-error`
    1. [x] - `p1` - Log a warning naming the candidate model and the error, and set `reserved_credits_micro` to the largest signed 64-bit value so that the candidate cannot fit any bucket (the cascade then skips it; no 500 is returned) - `inst-max-reserve`
15. [x] - `p1` - **RETURN** the five values - `inst-return-estimate`

### Tier Availability Check

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-billing-tier-availability`

**Input**: Tier (`premium` or `standard`), the preflight bucket rows, the user limits, the period keys, the candidate's `reserved_credits_micro`

**Output**: Available or unavailable

**Steps**:
1. [x] - `p1` - Set the required buckets: `total` for a standard candidate; `total` and then `tier:premium` for a premium candidate - `inst-required-buckets`
2. [x] - `p1` - **FOR EACH** required bucket in that order, and within it for `daily` and then `monthly` - `inst-for-each-bucket-period`
   1. [x] - `p1` - Take the limit: bucket `total` uses `user_limits.standard` (the global ceiling across all tiers), bucket `tier:premium` uses `user_limits.premium` (premium sub-cap); `limit_daily_credits_micro` for `daily`, `limit_monthly_credits_micro` for `monthly` - `inst-limit-mapping`
   2. [x] - `p1` - Take `spent_credits_micro` and `reserved_credits_micro` of the row keyed by tenant, user, period type, period start and bucket; a missing row counts as 0 and 0 - `inst-row-values`
   3. [x] - `p1` - **IF** `spent_credits_micro` plus `reserved_credits_micro` plus the candidate reserve is greater than the limit, or the checked addition overflows - `inst-if-bucket-over`
      1. [x] - `p1` - **RETURN** unavailable (the remaining checks are skipped) - `inst-return-unavailable`
3. [x] - `p1` - **RETURN** available - `inst-return-available`

### Preflight Evaluation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-billing-preflight`

**Input**: Tenant id, user id, chat id, selected model (`chats.model`), message UTF-8 byte length, image count, `web_search_enabled`, `has_ready_file_search_documents`, `has_ready_code_interpreter_files`, current UTC time

**Output**: Preflight decision (effective model and its catalog entry, effective tier, effective `provider_id`, `quota_decision` `allow` or `downgrade`, `downgrade_from` and `downgrade_reason` when downgraded, `policy_version`, the snapshot, the user limits, the daily and monthly period starts, the five reserve values of the effective model, whether `web_search` may be sent, whether `code_interpreter` may be sent, whether the effective model has `VISION_INPUT`), or a domain error

**Steps**:
1. [x] - `p1` - Resolve the policy version, snapshot and user limits for the user (see `cpt-cf-mini-chat-algo-quota-billing-resolve-policy`, preflight mode); **IF** it returns an error, **RETURN** internal error - `inst-preflight-policy`
2. [x] - `p1` - **IF** `web_search_enabled` and the snapshot kill switch `disable_web_search` is `true` - `inst-if-web-disabled`
   1. [x] - `p1` - **RETURN** feature-disabled error with subject `web_search` (400 `failed_precondition`, `FEATURE_DISABLED`), before any estimate or quota read - `inst-return-web-disabled`
3. [x] - `p1` - Compute the period keys for the current time (see `cpt-cf-mini-chat-algo-quota-billing-period-keys`) - `inst-preflight-periods`
4. [x] - `p1` - DB: SELECT the chat's latest `messages` row with `role` `assistant`, `deleted_at IS NULL` and `input_tokens` or `output_tokens` greater than 0, ordered by `created_at` and then `id` descending, through the tenant-scoped query on the already authorized chat; `prior_context_tokens` is its `input_tokens` plus `output_tokens`, or 0 when there is none - `inst-prior-context`
5. [x] - `p1` - DB: open a read transaction and SELECT the user's `quota_usage` rows for buckets `total` and `tier:premium` and both period keys in one statement. On PostgreSQL the statement uses `FOR UPDATE`; on SQLite it is a plain read. Rows are ordered in the canonical quota row order: `period_type` `daily` before `monthly`, then bucket `total` before `tier:premium`. Plain text order would put `tier:premium` first, so the bucket order is explicit, and PostgreSQL row locks are taken in the same order as every reserve and settlement write. The transaction writes nothing and never upgrades to a write transaction. The single statement returns the four rows from one consistent read. The result is advisory; the in-transaction re-check of the reserve write is authoritative - `inst-read-buckets`
6. [x] - `p1` - Look up the selected model in the snapshot catalog - `inst-lookup-selected`
7. [x] - `p1` - **MATCH** the lookup result - `inst-match-selected`
   - [x] - `p1` - **CASE** found with `enabled` `true`: the starting tier is its tier; no downgrade reason - `inst-case-selected-enabled`
   - [x] - `p1` - **CASE** found with `enabled` `false`: the starting tier is its tier; downgrade reason `model_disabled` - `inst-case-selected-disabled`
   - [x] - `p1` - **CASE** not found (defensive; the turn pipeline rejects a removed chat model with 400 `INVALID_MODEL` earlier): the starting tier is `premium`; downgrade reason `model_disabled` - `inst-case-selected-missing`
8. [x] - `p1` - Build the cascade from the starting tier downward: `premium` then `standard` for a premium start, only `standard` for a standard start (a standard model never cascades up) - `inst-build-cascade`
9. [x] - `p1` - **FOR EACH** tier in the cascade - `inst-for-each-tier`
   1. [x] - `p1` - **IF** the tier is `premium` and `force_standard_tier` is `true` - `inst-if-force-standard`
      1. [x] - `p1` - Set the downgrade reason to `force_standard_tier` unless one is already set, and continue with the next tier - `inst-skip-force-standard`
   2. [x] - `p1` - **IF** the tier is `premium` and `disable_premium_tier` is `true` - `inst-if-premium-disabled`
      1. [x] - `p1` - Set the downgrade reason to `disable_premium_tier` unless one is already set, and continue with the next tier - `inst-skip-premium-disabled`
   3. [x] - `p1` - Select the candidate among the tier's catalog entries with `enabled` `true`: the selected model when it is one of them; otherwise the first one in catalog order with `preference.is_default` `true`; otherwise the first one in catalog order - `inst-select-candidate`
   4. [x] - `p1` - **IF** the tier has no enabled entry - `inst-if-no-candidate`
      1. [x] - `p1` - Continue with the next tier (a tier with quota but no enabled model is unavailable) - `inst-skip-empty-tier`
   5. [x] - `p1` - Estimate the candidate's reserve with its own budgets, multipliers and tool support (see `cpt-cf-mini-chat-algo-quota-billing-estimate-reserve`) - `inst-estimate-candidate`
   6. [x] - `p1` - Check the tier with the candidate's `reserved_credits_micro` (see `cpt-cf-mini-chat-algo-quota-billing-tier-availability`) - `inst-check-candidate`
   7. [x] - `p1` - **IF** available - `inst-if-candidate-available`
      1. [x] - `p1` - Set the effective model to the candidate and stop the cascade (only one candidate per tier is checked) - `inst-set-effective`
   8. [x] - `p1` - **ELSE IF** the tier is `premium` - `inst-else-premium-unavailable`
      1. [x] - `p1` - Set the downgrade reason to `premium_quota_exhausted` unless one is already set - `inst-reason-premium-exhausted`
10. [x] - `p1` - **IF** no effective model was set (quota exhausted, or no enabled model in any tier of the cascade) - `inst-if-no-tier`
    1. [x] - `p1` - End the read transaction without writing, record the `reject` preflight count (see `cpt-cf-mini-chat-algo-quota-billing-record-metrics`) and **RETURN** quota exceeded with subject `tokens` (429 `resource_exhausted`, `violations[{subject: tokens, description: quota_exceeded}]`) - `inst-return-tokens-exhausted`
11. [x] - `p1` - Set `quota_decision` to `allow` when the effective model equals the selected model and no downgrade reason is set, otherwise to `downgrade` with `downgrade_from` the selected model and the downgrade reason - `inst-set-decision`
12. [x] - `p1` - Record the preflight decision count and the effective model's `reserve_tokens` as estimated tokens before any later rejection (see `cpt-cf-mini-chat-algo-quota-billing-record-metrics`) - `inst-record-preflight`
13. [x] - `p1` - **IF** `web_search_enabled` and the effective model's `tool_support.web_search` is `true` - `inst-if-web-quota`
    1. [x] - `p1` - **IF** `web_search_calls` of the daily `total` row (0 when absent) is at least `quota.web_search_daily_quota` - `inst-if-web-exhausted`
       1. [x] - `p1` - End the read transaction and **RETURN** quota exceeded with subject `web_search` (429) - `inst-return-web-exhausted`
14. [x] - `p1` - **IF** `has_ready_code_interpreter_files`, the effective model's `tool_support.code_interpreter` is `true` and `disable_code_interpreter` is `false` - `inst-if-ci-quota`
    1. [x] - `p1` - **IF** `code_interpreter_calls` of the daily `total` row (0 when absent) is at least `quota.code_interpreter_daily_quota` - `inst-if-ci-exhausted`
       1. [x] - `p1` - End the read transaction and **RETURN** quota exceeded with subject `code_interpreter` (429) - `inst-return-ci-exhausted`
15. [x] - `p1` - Commit the read transaction without writing any reserve (the reserve is written later, see `cpt-cf-mini-chat-algo-quota-billing-reserve-write`) - `inst-commit-read`
16. [x] - `p1` - **RETURN** the preflight decision; the effective model is computed once here and never recomputed at settlement - `inst-return-decision`

### Reserve Write and Limit Re-Check

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-billing-reserve-write`

**Input**: The caller's open reserve transaction, the preflight decision, mode (`send` or `fill` with the new turn id), tenant id, user id, current time

**Output**: The six preflight column values (`reserve_tokens`, `max_output_tokens_applied`, `reserved_credits_micro`, `policy_version_applied`, `effective_model`, `minimal_generation_floor_applied`), or quota exceeded with subject `tokens`

**Transaction discipline** (normative): the reserve transaction is write-first. Its first statement is the daily `total` UPSERT of step 3; on SQLite the caller may instead begin it `IMMEDIATE`. Either way no read runs in the transaction before the first write. Every read the pipeline needs (chat, idempotency, preflight, prior context) runs before the transaction opens. Every `quota_usage` statement follows the canonical row order: `period_type` `daily` before `monthly`, then bucket `total` before `tier:premium`. A lock or serialization conflict is retried a bounded number of times (step 7).

**Steps**:
1. [x] - `p1` - The caller opens the reserve transaction write-first: no statement precedes the UPSERT of step 3 (on SQLite the transaction may instead be begun `IMMEDIATE`), so the transaction never upgrades from a read to a write lock - `inst-reserve-write-first`
2. [x] - `p1` - Scope every `quota_usage` statement by the turn's `tenant_id` and the requesting `user_id` (tenant and owner predicates; no further PDP call, the chat was authorized by the turn pipeline) - `inst-reserve-scope`
3. [x] - `p1` - **FOR EACH** period key of the decision in the canonical order (daily, then monthly) - `inst-for-each-reserve-period`
   1. [x] - `p1` - DB: UPSERT `quota_usage` on the unique key (`tenant_id`, `user_id`, `period_type`, `period_start`, bucket `total`): a new row gets a new UUID `id`, `reserved_credits_micro` equal to the turn reserve and 0 in every other counter; an existing row gets its `reserved_credits_micro` increased by the turn reserve; `updated_at` is set to the current time - `inst-upsert-total`
   2. [x] - `p1` - **IF** the effective tier is `premium`, DB: UPSERT the bucket `tier:premium` row of the same period in the same way, after the `total` row of that period; a standard turn writes no `tier:premium` row - `inst-upsert-premium`
4. [x] - `p1` - DB: SELECT again, without locking, the rows of every bucket and period of the decision, ordered in the canonical row order (the increments hold the row locks on PostgreSQL and the write lock on SQLite, so the re-check sees every reserve committed before it) - `inst-reread-buckets`
5. [x] - `p1` - **FOR EACH** re-read row - `inst-for-each-recheck`
   1. [x] - `p1` - **IF** its `spent_credits_micro` plus `reserved_credits_micro` is greater than the limit of its bucket and period in the decision's user limits - `inst-if-recheck-over`
      1. [x] - `p1` - **RETURN** quota exceeded with subject `tokens`; the caller rolls back the whole reserve transaction; the cascade is not run again and the request is not downgraded - `inst-return-recheck-over`
6. [x] - `p1` - **IF** mode is `fill` - `inst-if-fill-mode`
   1. [x] - `p1` - DB: UPDATE `chat_turns` SET the six preflight columns WHERE `id` is the new turn, `state` is `running` and `reserve_tokens IS NULL` (the columns are written once and never changed afterwards) - `inst-fill-turn`
   2. [x] - `p1` - **IF** 0 rows are updated, **RETURN** internal error (the caller rolls back) - `inst-if-fill-missed`
7. [x] - `p1` - **IF** any statement of the reserve transaction, including the caller's own statements and the commit, fails with `SQLITE_BUSY` ("database is locked"), a PostgreSQL serialization failure (SQLSTATE `40001`) or a deadlock (SQLSTATE `40P01`) - `inst-if-reserve-conflict`
   1. [x] - `p1` - Report a retryable conflict. The caller rolls back and re-runs the whole reserve transaction from step 1, with at most 3 attempts in total and a short backoff between them (for example 20 ms, then 100 ms). A rolled-back attempt leaves no bucket increment and no turn row behind - `inst-retry-reserve`
   2. [x] - `p1` - **IF** the third attempt also conflicts, **RETURN** internal error (500 `internal`, nothing is reserved; never 429) - `inst-return-reserve-conflict`
8. [x] - `p1` - **RETURN** the six values; in `send` mode the caller writes them in the `chat_turns` INSERT of the same transaction, so the turn row and the bucket increments commit together before any provider call - `inst-return-reserve`

### Billing Outcome Derivation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-billing-billing-outcome`

**Input**: Terminal classification chosen by the caller (`completed`; `failed` with `error_code`; `cancelled`; orphan timeout), optional provider usage

**Output**: `terminal_state` (`completed`, `failed`, `cancelled`), `billing_outcome` (`completed`, `failed`, `aborted`), `settlement_method` (`actual`, `estimated`, `released`)

**Steps**:
1. [x] - `p1` - Derive the outcome from the terminal classification passed by the finalization path, never by re-reading `chat_turns.state` - `inst-derive-from-trigger`
2. [x] - `p1` - **MATCH** the classification - `inst-match-classification`
   - [x] - `p1` - **CASE** `completed` (provider `response.completed` or `response.incomplete`): `completed`, `completed`, `actual`, also when the provider reported zero or no usage - `inst-case-completed`
   - [x] - `p1` - **CASE** `failed` with `provider_error`, `provider_timeout`, `rate_limited`, `web_search_calls_exceeded`, `code_interpreter_calls_exceeded`, `agentic_iterations_exceeded`, `unexpected_tool_use` or `message_persistence_failed`: `failed`, `failed`, and `actual` when the usage object has `input_tokens` or `output_tokens` greater than 0, otherwise `estimated` (never `released`, the provider was called) - `inst-case-post-provider`
   - [x] - `p1` - **CASE** `failed` with `context_length_exceeded`, `validation_error`, `input_too_long` or `turn_setup_failed`: `failed`, `failed`, `released` - `inst-case-pre-provider`
   - [x] - `p1` - **CASE** `cancelled` (client disconnect): `cancelled`, `aborted`, `estimated`; any usage passed is ignored - `inst-case-cancelled`
   - [x] - `p1` - **CASE** orphan timeout (`failed` with `orphan_timeout`): `failed`, `aborted`, `estimated` - `inst-case-orphan`
   - [x] - `p1` - **DEFAULT** (any other `error_code`, including `quota_exceeded`, which never reaches settlement because no reserve exists): `failed`, `failed`, `estimated`, and log an error naming the unknown code - `inst-case-unknown-code`
3. [x] - `p1` - **RETURN** the three values - `inst-return-outcome`

### Quota Settlement

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-billing-settle`

**Input**: The caller's open finalization transaction, the persisted turn row (reserve fields, `effective_model`, `policy_version_applied`, `tenant_id`, `requester_user_id`), the billing outcome and settlement method, optional provider usage, completed web search and code interpreter call counts, the daily and monthly period starts of the turn, the snapshot of `policy_version_applied` fetched before the transaction opened or `unavailable`, `quota.overshoot_tolerance_factor`, current time

**Output**: Settlement result: the settlement method applied (`estimated` after the deterministic fallback), committed credits, actual input and output tokens used for telemetry, whether the overshoot cap applied (internal only), or an internal error (database failures only)

**Steps**:
1. [x] - `p1` - Derive `estimated_input_tokens` as `chat_turns.reserve_tokens` minus `chat_turns.max_output_tokens_applied`; take `turn_reserved_credits_micro` from `chat_turns.reserved_credits_micro`; never read estimation budgets or configuration that may have changed since preflight - `inst-derive-estimated-input`
2. [x] - `p1` - Look up `chat_turns.effective_model` in the snapshot of `policy_version_applied` and take its two multipliers and its tier; **IF** the snapshot is `unavailable` or the entry is missing, log at `error` naming the turn and the cause, skip steps 3 to 5, and continue with the deterministic fallback of step 6; no internal error is returned - `inst-lookup-effective`
3. [x] - `p1` - **MATCH** the settlement method - `inst-match-method`
   - [x] - `p1` - **CASE** `actual`: take `input_tokens` and `output_tokens` from the usage object (a missing field is 0; no usage object on a completed turn means 0 and 0); compute `actual_credits_micro` from them (see `cpt-cf-mini-chat-algo-quota-billing-credits-micro`); the committed credits are `actual_credits_micro`; the telemetry tokens are these counts - `inst-case-actual`
   - [x] - `p1` - **CASE** `estimated`: the charged output tokens are `chat_turns.minimal_generation_floor_applied`; the charged tokens are the smaller of `reserve_tokens` and `estimated_input_tokens` plus the floor; the committed credits are computed from `estimated_input_tokens` and the floor; the telemetry tokens are 0 and 0 - `inst-case-estimated`
   - [x] - `p1` - **CASE** `released`: the committed credits are 0 and the telemetry tokens are 0 and 0 - `inst-case-released`
4. [x] - `p1` - **IF** the method is `actual` and the actual tokens (`input_tokens` plus `output_tokens`) are greater than `reserve_tokens` - `inst-if-overshoot`
   1. [x] - `p1` - Compute the overshoot factor as the actual tokens divided by `reserve_tokens` in floating point (never integer division) - `inst-overshoot-factor`
   2. [x] - `p1` - **IF** the factor is at most `quota.overshoot_tolerance_factor` (default 1.10), keep the committed credits at `actual_credits_micro` - `inst-within-tolerance`
   3. [x] - `p1` - **ELSE** set the committed credits to `turn_reserved_credits_micro` and the internal overshoot-cap flag (not exported, no log line); the turn state is never changed by the cap - `inst-cap-at-reserve`
5. [x] - `p1` - **IF** a credit computation returns an error (multiplication or addition overflow, invalid token count, zero or invalid multiplier) - `inst-if-settle-error`
   1. [x] - `p1` - Log at `error` with the error, naming the offending token count or multiplier, and continue with the deterministic fallback of step 6. Retrying cannot succeed, so the transaction is not rolled back and the turn does not stay `running` - `inst-return-settle-error`
6. [x] - `p1` - **IF** step 2 or step 5 selected the deterministic fallback and the method is `actual` or `estimated` - `inst-if-fallback`
   1. [x] - `p1` - Set the settlement method to `estimated`; the billing outcome and terminal state stay unchanged. Set the charged tokens to the smaller of `reserve_tokens` and `estimated_input_tokens` plus `minimal_generation_floor_applied` (DESIGN §5.8). Set the committed credits to `turn_reserved_credits_micro` and the telemetry tokens to 0 and 0. Only the persisted columns `reserve_tokens`, `max_output_tokens_applied`, `reserved_credits_micro` and `minimal_generation_floor_applied` are read; no multiplier is read, no credit computation runs and the overshoot cap is not evaluated. The same turn row always gives the same committed credits - `inst-fallback-estimated`
   2. [x] - `p1` - Log at `error` the record `quota settlement fallback` with the chat, turn and request ids, `effective_model`, `policy_version_applied`, the cause (snapshot unavailable, model missing from the snapshot, or the credit error), the original settlement method and the committed credits - `inst-fallback-log`
   3. [x] - `p1` - A `released` settlement never takes the fallback: its committed credits stay 0 and it needs no multiplier - `inst-fallback-not-released`
7. [x] - `p1` - Determine whether the turn is premium. Use the tier of the snapshot entry when the entry exists, which includes the credit-error case. When the snapshot is `unavailable` or lacks the entry, use the persisted bucket rows instead. DB: SELECT, without locking, the `tier:premium` rows of the turn's daily and monthly period starts in the canonical order. The turn is premium when both rows exist and each `reserved_credits_micro` is at least `turn_reserved_credits_micro`; otherwise it is standard. A premium turn's own reserve stays in both rows until this settlement, so a premium turn is never treated as standard while its periods match the preflight periods. The plain read leaves the canonical lock order of step 8 intact - `inst-fallback-tier`
8. [x] - `p1` - **FOR EACH** period start of the turn in the canonical order (daily, then monthly; never recomputed from the current clock), writing the `total` row before the `tier:premium` row of the same period - `inst-for-each-settle-period`
   1. [x] - `p1` - DB: UPSERT the bucket `total` row: `reserved_credits_micro` decreased by `turn_reserved_credits_micro`, `spent_credits_micro` increased by the committed credits, `calls` increased by 1, `input_tokens` and `output_tokens` increased by the telemetry tokens, `web_search_calls` and `code_interpreter_calls` increased by the turn's completed call counts on `actual` and `estimated` settlements (by 0 on `released`), `updated_at` set to the current time; a missing row is created with these deltas applied to zero - `inst-settle-total`
   2. [x] - `p1` - **IF** step 7 found the turn premium, DB: UPSERT the bucket `tier:premium` row: `reserved_credits_micro` decreased by `turn_reserved_credits_micro`, `spent_credits_micro` increased by the committed credits, `calls` increased by 1, `updated_at` set; token and tool counters of this bucket stay unchanged - `inst-settle-premium`
9. [x] - `p1` - Never update `file_search_calls`, `rag_retrieval_calls`, `image_inputs` or `image_upload_bytes` - `inst-reserved-counters`
10. [x] - `p1` - **RETURN** the settlement result with the settlement method applied - `inst-return-settlement`

### Turn Usage Event

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-billing-turn-usage-event`

**Input**: The caller's open finalization transaction, the turn row (with a non-NULL `requester_user_id`), the selected model, the billing outcome values with the settlement method applied by the settlement, the committed credits, optional provider usage, completed web search, code interpreter and file search call counts, current time

**Output**: One usage message enqueued, or an internal error

**Steps**:
1. [x] - `p1` - Build the SDK `UsageEvent` with `tenant_id` (`chat_turns.tenant_id`), `user_id` (`chat_turns.requester_user_id`), `chat_id`, `turn_id` (`chat_turns.id`), `request_id` (`chat_turns.request_id`), `effective_model` (`chat_turns.effective_model`), `selected_model`, `terminal_state`, `billing_outcome`, `settlement_method`, `actual_credits_micro` equal to the committed credits, `policy_version_applied` (`chat_turns.policy_version_applied`), `web_search_calls`, `code_interpreter_calls`, `file_search_calls`, `timestamp` (current time, RFC 3339), `requester_type` `user`, and no `system_task_type` - `inst-build-turn-event`
2. [x] - `p1` - Set `usage`: on `actual` the provider usage with `input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_write_input_tokens`, `reasoning_tokens` (missing fields 0), or `null` when no usage object was received; on `estimated` `null`; on `released` all five fields 0 - `inst-set-usage`
3. [x] - `p1` - Set `dedupe_key` to the simple lowercase hex forms (32 characters, no hyphens) of `tenant_id`, `turn_id` and `request_id` joined by `/`, built from the persisted turn row so it is never null and always starts with the tenant hex - `inst-dedupe-key`
4. [x] - `p1` - Never include `reserved_credits_micro`, `reserve_tokens`, `committed_credits_micro`, `overshoot_capped`, `error_code`, `completion_signal`, `quota_decision`, `downgrade_from` or `downgrade_reason` - `inst-excluded-fields`
5. [x] - `p1` - Encode the event (see `cpt-cf-mini-chat-algo-gear-foundation-encode-events`). The finalization helper never calls this algorithm for a turn whose `requester_user_id` is NULL: that corrupt-data case is skipped before any enqueue (see `cpt-cf-mini-chat-algo-quota-billing-finalize-turn`), so a missing `user_id` never reaches the encoder. **IF** encoding still fails, **RETURN** internal error - `inst-encode-turn-event`
6. [x] - `p1` - DB: enqueue the payload to the outbox queue `outbox.queue_name` (default `mini-chat.usage_snapshot`) partitioned by `tenant_id`, inside the same transaction as the CAS and the settlement - `inst-enqueue-turn-event`
7. [x] - `p1` - **RETURN** success; delivery to `publish_usage` happens later in `cpt-cf-mini-chat-algo-gear-foundation-handle-usage` - `inst-return-turn-event`

### CAS-Guarded Turn Finalization

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-billing-finalize-turn`

**Input**: Turn id, variant (`stream` with the preflight period starts, the selected model and the preflight decision's `policy_version`, or `orphan` with the stale-progress cutoff and the turn's `policy_version_applied` returned by the watchdog scan, read outside any transaction), terminal classification, the terminal columns chosen by the caller (`state`, `error_code`, `error_detail`, `assistant_message_id`, `provider_response_id`), optional provider usage, completed tool call counts, current time, and the caller's finalization transaction, which the caller opens only after step 1

**Output**: `lost` (no-op), or `won` with the billing outcome values and the settlement result (none for the corrupt-data branch without `requester_user_id`), or an internal error that rolls the transaction back

**Transaction discipline** (normative): no plugin call runs inside the finalization transaction. The policy version is immutable once written, so the snapshot of `policy_version_applied` is fetched before the transaction opens and passed in. The finalization transaction is write-first: its first statement is the `chat_turns` CAS UPDATE of step 3, or on SQLite the caller begins it `IMMEDIATE`. No read runs in it before the first write. The settlement writes `quota_usage` rows in the canonical order: `period_type` `daily` before `monthly`, then bucket `total` before `tier:premium`. A lock or serialization conflict is retried a bounded number of times (step 11).

**Steps**:
1. [x] - `p1` - Before the caller opens the finalization transaction, **IF** the given `policy_version_applied` is not NULL, fetch its snapshot (see `cpt-cf-mini-chat-algo-quota-billing-resolve-policy`, settlement mode); no transaction is open, so no row or write lock is held during the plugin call - `inst-prefetch-snapshot`
   1. [x] - `p1` - **IF** the fetch fails or its timeout elapses, log at `error` with the turn id and the cause, and mark the snapshot `unavailable`; finalization continues and settles with the deterministic fallback of `cpt-cf-mini-chat-algo-quota-billing-settle` - `inst-snapshot-unavailable`
2. [x] - `p1` - The caller opens the finalization transaction write-first: no statement precedes the CAS UPDATE of step 3, or on SQLite the transaction is begun `IMMEDIATE` - `inst-write-first-finalize`
3. [x] - `p1` - DB: UPDATE `chat_turns` SET `state`, `completed_at` and `updated_at` to the current time, and the caller's terminal columns, WHERE `id` is the turn and `state` is `running`; the `orphan` variant adds `deleted_at IS NULL` and `last_progress_at` (or `started_at` when it is NULL) at or before the cutoff and sets `state` `failed`, `error_code` `orphan_timeout` - `inst-cas-update`
4. [x] - `p1` - **IF** 0 rows are affected - `inst-if-cas-zero`
   1. [x] - `p1` - **RETURN** `lost`: no quota change, no usage event, no metric; the caller stops (first terminal wins) - `inst-return-lost`
5. [x] - `p1` - DB: SELECT the turn row (preflight columns, `tenant_id`, `chat_id`, `request_id`, `requester_user_id`, `started_at`, completed call counts) inside the same transaction - `inst-load-turn`
6. [x] - `p1` - Derive the billing outcome (see `cpt-cf-mini-chat-algo-quota-billing-billing-outcome`) - `inst-derive-outcome`
7. [x] - `p1` - **IF** `requester_user_id` is NULL (corrupt data: a user turn always has one) - `inst-if-no-requester`
   1. [x] - `p1` - Log at `error` with the turn, chat and tenant ids. Skip settlement (no `quota_usage` row changes), skip the usage enqueue (no event is built, encoded or enqueued) and skip the cost record. **RETURN** `won` with the outcome and no settlement result, so the CAS commits with the caller's steps and the turn is finalized rather than rescanned forever - `inst-skip-no-requester`
8. [x] - `p1` - **IF** the variant is `orphan` - `inst-if-orphan`
   1. [x] - `p1` - Use no usage, set the selected model to `chat_turns.effective_model`, take the tool call counts from `web_search_completed_count`, `code_interpreter_completed_count` and `file_search_completed_count`, and derive the period starts from `chat_turns.started_at` (see `cpt-cf-mini-chat-algo-quota-billing-period-keys`) - `inst-orphan-inputs`
9. [x] - `p1` - **IF** any of the six preflight columns is NULL (a retry or edit turn left `running` before its reserve) - `inst-if-no-reserve`
   1. [x] - `p1` - Log a warning, skip settlement, change no `quota_usage` row, and enqueue the usage event with `billing_outcome` `aborted`, `settlement_method` `estimated`, `actual_credits_micro` 0, `usage` `null`, `effective_model` and `selected_model` empty strings and `policy_version_applied` 0 (see `cpt-cf-mini-chat-algo-quota-billing-turn-usage-event`) - `inst-skip-settlement`
10. [x] - `p1` - **ELSE** - `inst-else-has-reserve`
    1. [x] - `p1` - Take the snapshot prefetched in step 1; no plugin call is made inside the transaction. **IF** it is `unavailable`, or its version differs from the loaded `policy_version_applied`, pass `unavailable` to the settlement - `inst-load-settlement-snapshot`
    2. [x] - `p1` - Settle the quota (see `cpt-cf-mini-chat-algo-quota-billing-settle`). An unavailable snapshot, a missing effective model and a credit error never fail it: it takes the deterministic estimated fallback. **IF** it fails (database error), **RETURN** its error - `inst-settle`
    3. [x] - `p1` - Enqueue the usage event with the committed credits and the settlement method returned by the settlement (`estimated` after a fallback) (see `cpt-cf-mini-chat-algo-quota-billing-turn-usage-event`); **IF** it fails, **RETURN** its error - `inst-enqueue-usage`
11. [x] - `p1` - **IF** any statement of the finalization transaction, including the caller's own statements and the commit, fails with `SQLITE_BUSY`, a PostgreSQL serialization failure (SQLSTATE `40001`) or a deadlock (SQLSTATE `40P01`) - `inst-if-finalize-conflict`
    1. [x] - `p1` - Report a retryable conflict. The caller rolls back and re-runs the whole finalization transaction from step 2, reusing the prefetched snapshot, with at most 3 attempts in total and a short backoff between them (for example 20 ms, then 100 ms). A re-run whose CAS finds the turn already terminal returns `lost` - `inst-retry-finalize`
    2. [x] - `p1` - **IF** the third attempt also conflicts, **RETURN** internal error. The turn stays `running` for the orphan watchdog; the stream path reports `finalization_failed` or its original error code - `inst-return-finalize-conflict`
12. [x] - `p1` - Write the per-request cost record `quota settled` (see `cpt-cf-mini-chat-algo-quota-billing-record-metrics`) - `inst-cost-record`
13. [x] - `p1` - **RETURN** `won` with the outcome and the settlement result; the caller adds its own steps (assistant message, audit event, thread-summary task) to the same transaction and commits; a failure of any step rolls back the CAS, the settlement and the usage event together - `inst-return-won`

### System-Task Usage Event

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-billing-system-task-usage`

**Input**: The caller's open transaction (the one that upserts the thread summary), `tenant_id` (`chats.tenant_id`), `chat_id`, the stable `system_request_id` stored in the thread-summary outbox payload, the summary model id, optional provider usage, current time

**Output**: One usage message enqueued, or an internal error

**Steps**:
1. [x] - `p1` - Build the SDK `UsageEvent` with `tenant_id`, `chat_id`, `request_id` the `system_request_id`, `effective_model` and `selected_model` the summary model id, `terminal_state` `completed`, `billing_outcome` `system_task`, `settlement_method` `none`, `actual_credits_micro` 0, `policy_version_applied` 0, `web_search_calls`, `code_interpreter_calls` and `file_search_calls` 0, `timestamp`, `requester_type` `system`, `system_task_type` `thread_summary_update`, and `usage` the provider token counts (or `null` when none were reported) - `inst-build-system-event`
2. [x] - `p1` - Omit `user_id` and `turn_id` (absent, not `null`) - `inst-omit-user-turn`
3. [x] - `p1` - Set `dedupe_key` to the simple hex `tenant_id`, `thread_summary_update` and the simple hex `system_request_id` joined by `/`; the same `system_request_id` is reused on every retry of the same outbox message - `inst-system-dedupe-key`
4. [x] - `p1` - Never read or write `quota_usage` or `chat_turns`, and never pass through the turn CAS - `inst-no-user-debit`
5. [x] - `p1` - Encode the event and DB: enqueue it to `outbox.queue_name` partitioned by `tenant_id` inside the caller's transaction; **IF** encoding fails, **RETURN** internal error - `inst-enqueue-system-event`
6. [x] - `p1` - **RETURN** success - `inst-return-system-event`

### Per-Turn Tool Call Limit

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-billing-tool-call-limit`

**Input**: Tool (`web_search` or `code_interpreter`), the number of start events of that tool in the turn including the new one

**Output**: `within` or `exceeded` with the error code

**Steps**:
1. [x] - `p1` - Take the limit `quota.web_search_max_calls_per_message` (default 2) for `web_search` or `quota.code_interpreter_max_calls_per_message` (default 10) for `code_interpreter` - `inst-take-limit`
2. [x] - `p1` - **IF** the start count is greater than the limit - `inst-if-limit-exceeded`
   1. [x] - `p1` - **RETURN** `exceeded` with `web_search_calls_exceeded` or `code_interpreter_calls_exceeded`; the turn pipeline cancels the provider stream and finalizes the turn as `failed` with that code and no usage, so the settlement is `estimated` and the preflight surcharges are not refunded - `inst-return-exceeded`
3. [x] - `p1` - **RETURN** `within` - `inst-return-within`

### Quota View and Warnings

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-billing-quota-view`

**Input**: Mode (`status` or `warnings`), user limits, the bucket rows of the current daily and monthly periods, `quota.warning_threshold_pct`, the two next reset instants; in warnings mode the rows are read after the finalization commit with the turn's tenant and user scope

**Output**: Status mode: the `tiers` array of `QuotaStatusResponse`; warnings mode: the `quota_warnings` array

**Steps**:
1. [x] - `p1` - **FOR EACH** tier in the order `premium` (bucket `tier:premium`, limits `user_limits.premium`) and `total` (bucket `total`, limits `user_limits.standard`) - `inst-for-each-view-tier`
   1. [x] - `p1` - **FOR EACH** period in the order `daily` and `monthly` - `inst-for-each-view-period`
      1. [x] - `p1` - **IF** the limit of the period is 0 or negative, skip the period (it appears neither in the status nor in the warnings) - `inst-skip-zero-limit`
      2. [x] - `p1` - Set used to `spent_credits_micro` plus `reserved_credits_micro` of the row (0 when the row is missing) - `inst-view-used`
      3. [x] - `p1` - Set remaining to the limit minus used, or 0 when that is negative - `inst-view-remaining`
      4. [x] - `p1` - Set `remaining_percentage` to remaining multiplied by 100 and divided by the limit, floored, computed in 128-bit integers and kept within 0 to 100 - `inst-view-percentage`
      5. [x] - `p1` - Set `warning` when `remaining_percentage` is at most 100 minus `warning_threshold_pct`, and `exhausted` when `remaining_percentage` is 0 (less than 1% of the limit remains) - `inst-view-flags`
      6. [x] - `p1` - Set `next_reset` to the daily or monthly next reset instant - `inst-view-reset`
2. [x] - `p1` - **IF** mode is `status` - `inst-if-status-mode`
   1. [x] - `p1` - **RETURN** one entry per tier with `tier` and `periods`, each period with `period`, `limit_credits_micro`, `used_credits_micro`, `remaining_credits_micro`, `remaining_percentage`, `next_reset`, `warning`, `exhausted` - `inst-return-status-view`
3. [x] - `p1` - **RETURN** one warnings entry per tier and period with `tier`, `period`, `remaining_percentage`, `warning`, `exhausted`, and `next_reset` only when `warning` or `exhausted` is `true` - `inst-return-warnings`

### Quota Metrics Recording

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-quota-billing-record-metrics`

**Input**: A preflight decision or rejection, a committed reserve, or a committed settlement result with its turn

**Output**: Recorded instruments and the per-request cost record

**Steps**:
1. [x] - `p1` - After the preflight cascade, count `mini_chat_quota_preflight` once with `decision` `allow` or `downgrade`, `model` the effective model id and `tier` its tier, or with `decision` `reject`, `model` the selected model id and `tier` the starting tier - `inst-count-preflight`
2. [x] - `p1` - After an `allow` or `downgrade`, record the effective model's `reserve_tokens` in `mini_chat_quota_estimated_tokens`, before the reserve is written (a request rejected later still records it) - `inst-record-estimated`
3. [x] - `p1` - After the reserve transaction commits, count `mini_chat_quota_reserve` once with `period` `daily` and once with `period` `monthly` - `inst-count-reserve`
4. [x] - `p1` - **IF** a committed settlement was `actual` - `inst-if-actual-metrics`
   1. [x] - `p1` - Count `mini_chat_quota_commit` once per period, record the actual tokens in `mini_chat_quota_actual_tokens`, and add the turn's code interpreter call count to `mini_chat_code_interpreter_calls` with `model` the effective model - `inst-actual-metrics`
   2. [x] - `p1` - **IF** the actual tokens exceed `reserve_tokens`, count `mini_chat_quota_overshoot` once per period, within tolerance and capped alike - `inst-overshoot-metric`
5. [x] - `p1` - Write one structured log record `quota settled` per won finalization (except the corrupt-data branch without `requester_user_id`, which settles nothing) with chat, turn and request ids, effective model, actual input and output tokens, `reserved_credits_micro`, committed credits, released credits (`reserved_credits_micro` minus committed, at least 0), billing outcome, settlement method and tool call counts - `inst-cost-log`
6. [x] - `p1` - Use only the low-cardinality labels `decision`, `model`, `tier`, `period`; never `tenant_id`, `user_id`, `chat_id` or `request_id`; never record the declared-but-unrecorded `mini_chat_quota_preflight_v2`, `mini_chat_quota_tier_downgrade`, `mini_chat_quota_negative`, `mini_chat_quota_overshoot_tokens`, `mini_chat_quota_image_commit`, `mini_chat_credits_overflow` or `mini_chat_unknown_error_code` - `inst-label-rules`
7. [x] - `p1` - **RETURN** - `inst-return-metrics`

## 4. States (CDSL)

### Turn Billing State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-quota-billing-turn-billing`

**States**: `pre_reserve`, `in_progress`, `completed`, `failed`, `aborted`

**Initial State**: `pre_reserve`

The billing state is derived by the finalization path, not stored; `in_progress` maps to `chat_turns.state` `running` with the reserve fields set, `completed` to `completed`, `failed` to `failed` (except `orphan_timeout`), and `aborted` to `cancelled` or to `failed` with `orphan_timeout`. Pre-reserve failures never leave `pre_reserve` and produce no usage event. A turn whose `requester_user_id` is NULL (corrupt data) still reaches its terminal state through the CAS winner, but nothing is settled and no usage event is enqueued; the case is logged at `error` and the turn is not rescanned. When the snapshot of `policy_version_applied` is unavailable, the effective model is missing from it, or a credit computation fails, the CAS winner still reaches the terminal state. It settles with the deterministic estimated fallback of `cpt-cf-mini-chat-algo-quota-billing-settle` and never stays in `in_progress`.

**Transitions**:
1. [x] - `p1` - **FROM** `pre_reserve` **TO** `in_progress` **WHEN** the reserve transaction commits the bucket increments together with the turn row (send) or the filled preflight columns (retry and edit) - `inst-to-in-progress`
2. [x] - `p1` - **FROM** `in_progress` **TO** `completed` **WHEN** the CAS winner finalizes a provider `done` or `incomplete` response; settlement `actual` - `inst-to-completed`
3. [x] - `p1` - **FROM** `in_progress` **TO** `failed` **WHEN** the CAS winner finalizes a post-provider error code; settlement `actual` when usage with a non-zero count was reported, otherwise `estimated` - `inst-to-failed-post`
4. [x] - `p1` - **FROM** `in_progress` **TO** `failed` **WHEN** the CAS winner finalizes a pre-provider error code after the reserve; settlement `released` (no P1 code path reaches it) - `inst-to-failed-pre`
5. [x] - `p1` - **FROM** `in_progress` **TO** `aborted` **WHEN** the CAS winner finalizes a client disconnect (`cancelled`) or the orphan watchdog CAS finalizes an `orphan_timeout`; settlement `estimated` - `inst-to-aborted`
6. [x] - `p1` - **FROM** `completed`, `failed` or `aborted` **TO** the same state **WHEN** a later terminal signal loses the CAS; nothing is settled or enqueued (terminal states are immutable) - `inst-terminal-noop`

## 5. Definitions of Done

Specific implementation tasks derived from flows/algorithms above.

### Credit Arithmetic

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-billing-credit-arithmetic`

The system **MUST** compute every credit amount (reserve, actual, estimated, overshoot cap) through one checked function implementing `credits_micro` of DESIGN §5.3 and A.2. The function takes integer inputs only, applies the ceiling division by 1_000_000 per component (input and output separately), checks token counts against `0..=10_000_000`, rejects zero multipliers and multipliers above 10_000_000_000 as separate errors, and uses checked multiplication and addition. Floating point is never used in credit computation (only for the overshoot factor). At preflight an error makes the candidate unavailable through a reserve of the largest signed 64-bit value. At settlement it switches the CAS winner to the deterministic estimated fallback (committed credits `reserved_credits_micro`, logged at `error`), so the turn is finalized and not left `running`.

**Implements**:
- `cpt-cf-mini-chat-algo-quota-billing-credits-micro`

**Touches**:
- Design: `cpt-cf-mini-chat-design-quota-usage-accounting`
- Entities: `ModelCatalogEntry` (multipliers)

### Policy Version Binding

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-billing-policy-binding`

The system **MUST** resolve the current policy version, the snapshot and the user limits through the model-policy plugin at every preflight, without a cache (`cpt-cf-mini-chat-adr-quota-policy-scope`). It **MUST** persist `policy_version_applied`, `effective_model` and the other preflight columns on `chat_turns` once, never changed afterwards. Settlement **MUST** read the multipliers and the effective model's tier only from the snapshot of `policy_version_applied`, fetched before the finalization transaction opens, and never re-run the cascade. The only exception is the deterministic fallback of `cpt-cf-mini-chat-algo-quota-billing-settle`, which reads only persisted columns when that snapshot or its entry is unavailable. and the usage event **MUST** carry the same `policy_version_applied` (DESIGN `cpt-cf-mini-chat-dod-policy-version-per-turn`). Each plugin call is bounded by `POLICY_PLUGIN_CALL_TIMEOUT`.

**Implements**:
- `cpt-cf-mini-chat-algo-quota-billing-resolve-policy`
- `cpt-cf-mini-chat-algo-quota-billing-reserve-write`
- `cpt-cf-mini-chat-algo-quota-billing-settle`

**Constraints**: `cpt-cf-mini-chat-constraint-model-locked-per-chat`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns` (`policy_version_applied`, `effective_model`)
- Design: `cpt-cf-mini-chat-flow-quota-enforced-chat-turn`
- Entities: `PolicySnapshot`, `UserLimits`, ChatTurn

### Token Estimation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-billing-token-estimation`

The system **MUST** estimate the reserve of every cascade candidate as specified in `cpt-cf-mini-chat-algo-quota-billing-estimate-reserve` (DESIGN `cpt-cf-mini-chat-algo-quota-billing-token-estimation`). The estimate uses the candidate's catalog `estimation_budgets` (zero `bytes_per_token_conservative` clamped to 1) and the text estimate from the current message bytes with the safety margin applied by integer arithmetic. It adds `prior_context_tokens` from the latest non-deleted assistant message with non-zero counts, the per-image surcharge, and the file search, web search and code interpreter surcharges, each gated by the candidate's `tool_support` and the kill switches and added once per turn. `max_output_tokens_applied` is the smaller of the catalog `max_output_tokens` and `streaming.max_output_tokens`, and `minimal_generation_floor_applied` is the smaller of the configured floor and `max_output_tokens_applied`. The booked reserve is the effective model's estimate, so it equals the reserve that passed the check. Estimation budgets other than the floor never influence settlement.

**Implements**:
- `cpt-cf-mini-chat-algo-quota-billing-estimate-reserve`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-messages` (read: `input_tokens`, `output_tokens` of the latest assistant message)
- Config: `streaming.max_output_tokens`, `estimation_budgets.minimal_generation_floor`
- Entities: `EstimationBudgets`

### Preflight and Tier Cascade

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-billing-preflight`

The system **MUST** provide the preflight evaluation of `cpt-cf-mini-chat-algo-quota-billing-preflight` in the domain-layer quota service and call it before any request reaches OAGW (DESIGN `cpt-cf-mini-chat-dod-preflight-reserve-and-downgrade`). It reads the four bucket rows (`total` and `tier:premium`, daily and monthly UTC calendar periods) with one statement in a read transaction that never writes or upgrades. The statement uses `FOR UPDATE` on PostgreSQL and orders rows in the canonical quota row order (`daily` before `monthly`, then `total` before `tier:premium`). Its result is advisory; the reserve re-check is authoritative. It rejects `web_search.enabled` under `disable_web_search` with 400 `FEATURE_DISABLED` before estimation, and runs the premium to standard cascade with the `force_standard_tier` / `disable_premium_tier` skip, the `model_disabled` start rules, one candidate per tier, and availability as spent plus reserved plus the candidate reserve at most the limit in every required bucket and period (`total` mapped to `user_limits.standard`, `tier:premium` to `user_limits.premium`). The decision is `allow` or `downgrade` with the reasons `premium_quota_exhausted`, `force_standard_tier`, `disable_premium_tier`, `model_disabled`. When no tier is available it returns 429 `resource_exhausted` with `context.violations[0]` `{subject: tokens, description: quota_exceeded}` and writes nothing. The decision carries the effective model, tier, provider, vision capability and the five reserve values.

**Implements**:
- `cpt-cf-mini-chat-algo-quota-billing-preflight`
- `cpt-cf-mini-chat-algo-quota-billing-tier-availability`
- `cpt-cf-mini-chat-algo-quota-billing-period-keys`
- `cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`, `cpt-cf-mini-chat-constraint-model-locked-per-chat`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-quota-usage` (read)
- Components: `cpt-cf-mini-chat-component-quota-service`
- Design: `cpt-cf-mini-chat-design-throttling-tiers`
- Sequence: `cpt-cf-mini-chat-seq-send-message` (preflight step)
- Entities: QuotaUsage, `KillSwitches`, `ModelCatalogEntry`

### Reserve Write

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-billing-reserve-write`

The system **MUST** write the reserve in the caller's reserve transaction as specified in `cpt-cf-mini-chat-algo-quota-billing-reserve-write`, before any provider call. The write upserts `reserved_credits_micro` into the `total` rows of both periods, and into the `tier:premium` rows only for a premium effective tier, keyed by the preflight period starts. It then re-reads the rows without locking and rolls back with 429 `quota_exceeded` (`tokens`) when any bucket of the decision is over its limit, so concurrent turns cannot together book reserves over a limit, and a rejected request is not downgraded. It returns the six preflight values `reserve_tokens`, `max_output_tokens_applied`, `reserved_credits_micro`, `policy_version_applied`, `effective_model`, `minimal_generation_floor_applied` for the `chat_turns` INSERT (send), or writes them once into a retry or edit turn whose columns are still NULL (fill). The column names and types are those of DESIGN §3.7 and `cpt-cf-mini-chat-dod-gear-foundation-schema`. The reserve transaction **MUST** follow the transaction discipline of `cpt-cf-mini-chat-algo-quota-billing-reserve-write`. It is write-first: the daily `total` UPSERT is its first statement, or on SQLite it is begun `IMMEDIATE`. Writes follow the canonical row order. A lock or serialization conflict is retried at most 3 attempts in total with a short backoff, then the request fails with 500 `internal`.

**Implements**:
- `cpt-cf-mini-chat-algo-quota-billing-reserve-write`
- `cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps`
- `cpt-cf-mini-chat-state-quota-billing-turn-billing`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-quota-usage`, `cpt-cf-mini-chat-dbtable-chat-turns` (six preflight columns)
- Components: `cpt-cf-mini-chat-component-quota-service`, `cpt-cf-mini-chat-component-chat-store`
- Sequence: `cpt-cf-mini-chat-seq-send-message` (reserve transaction)
- Entities: QuotaUsage, ChatTurn

### Settlement and Finalization

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-billing-settlement`

The system **MUST** provide the CAS-guarded finalization helper of `cpt-cf-mini-chat-algo-quota-billing-finalize-turn` (stream and orphan variants) as the only path that settles a reserved turn (DESIGN `cpt-cf-mini-chat-dod-cas-guarded-settlement-and-outbox`, `cpt-cf-mini-chat-design-nonterminal-reconciliation`, `cpt-cf-mini-chat-design-terminal-error-reconciliation`). The helper runs, in the caller's transaction, a conditional UPDATE on `state = 'running'`, and on `rows_affected = 1` it derives the billing outcome from the terminal classification (the exhaustive table of `cpt-cf-mini-chat-algo-quota-billing-billing-outcome`) and settles. Completed turns settle `actual` (overshoot within `quota.overshoot_tolerance_factor` committed, beyond it capped at `reserved_credits_micro`, the turn staying `completed`). Failed turns settle `actual` only with a non-zero usage count, otherwise `estimated`. Aborted turns settle `estimated` with the credits of `estimated_input_tokens` (`reserve_tokens` minus `max_output_tokens_applied`) and `minimal_generation_floor_applied`. Pre-provider codes settle `released` with 0. When the snapshot of `policy_version_applied` (fetched before the transaction opens) is unavailable, the effective model is missing from it, or a credit computation fails, the CAS winner settles `estimated` with the deterministic fallback. That fallback commits `reserved_credits_micro`, derives the premium tier from the persisted `tier:premium` reserve rows when the snapshot cannot give it, and logs at `error`. A turn without `requester_user_id` is finalized with an `error` log, no settlement and no usage event. The finalization transaction follows the transaction discipline of `cpt-cf-mini-chat-algo-quota-billing-finalize-turn`. The bucket updates of `cpt-cf-mini-chat-algo-quota-billing-settle` apply to the preflight period rows. The helper then enqueues exactly one usage event (none only in the corrupt-data case without `requester_user_id`). On `rows_affected = 0` it does nothing. The helper accepts no system task (it requires a `chat_turns` row in `running`).

**Implements**:
- `cpt-cf-mini-chat-algo-quota-billing-finalize-turn`
- `cpt-cf-mini-chat-algo-quota-billing-billing-outcome`
- `cpt-cf-mini-chat-algo-quota-billing-settle`
- `cpt-cf-mini-chat-state-quota-billing-turn-billing`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns` (CAS columns, read of preflight columns), `cpt-cf-mini-chat-dbtable-quota-usage`
- Components: `cpt-cf-mini-chat-component-quota-service`, `cpt-cf-mini-chat-component-chat-store`, `cpt-cf-mini-chat-component-orphan-watchdog` (caller of the orphan variant)
- Design: `cpt-cf-mini-chat-design-quota-usage-accounting`
- Entities: QuotaUsage, ChatTurn

### Usage Event Publication

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-billing-usage-events`

The system **MUST** enqueue one SDK `UsageEvent` per won finalization (except a turn whose `requester_user_id` is NULL, which is finalized with an `error` log and no event) in the same transaction as the CAS and the settlement, with the fields of `cpt-cf-mini-chat-algo-quota-billing-turn-usage-event`, `actual_credits_micro` equal to the committed credits, and `dedupe_key` `{tenant_id}/{turn_id}/{request_id}` in 32-character lowercase hex. It **MUST** enqueue one system-task event per thread-summary commit as specified in `cpt-cf-mini-chat-algo-quota-billing-system-task-usage`, with `requester_type` `system`, `billing_outcome` `system_task`, `settlement_method` `none`, `actual_credits_micro` 0, no `user_id` and `turn_id`, and `dedupe_key` `{tenant_id}/thread_summary_update/{system_request_id}`, without touching user buckets. Both are enqueued to `outbox.queue_name` partitioned by `tenant_id` and delivered at least once to `publish_usage` by `cpt-cf-mini-chat-algo-gear-foundation-handle-usage`. No quota change can commit without its usage message (`cpt-cf-mini-chat-fr-quota-billing-architecture`).

**Implements**:
- `cpt-cf-mini-chat-algo-quota-billing-turn-usage-event`
- `cpt-cf-mini-chat-algo-quota-billing-system-task-usage`
- `cpt-cf-mini-chat-algo-quota-billing-finalize-turn`

**Touches**:
- DB: shared outbox tables (queue `mini-chat.usage_snapshot`)
- Entities: `UsageEvent`

### Tool Quotas

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-billing-tool-quotas`

The system **MUST** check the daily web search quota (`quota.web_search_daily_quota`, default 75) only when the request enables web search and the effective model supports it. It **MUST** check the daily code interpreter quota (`quota.code_interpreter_daily_quota`, default 50) only when the chat has ready code-interpreter attachments, the effective model supports the tool and `disable_code_interpreter` is off. Both are checked at preflight against the daily `total` row with 429 subjects `web_search` and `code_interpreter`, never reported as `tokens` (`cpt-cf-mini-chat-design-web-search`, `cpt-cf-mini-chat-design-code-interpreter-config`). The system **MUST** provide the per-turn limit decision of `cpt-cf-mini-chat-algo-quota-billing-tool-call-limit` (`web_search_max_calls_per_message` default 2, `code_interpreter_max_calls_per_message` default 10). It **MUST** count completed web search and code interpreter calls into the daily `total` row on actual and estimated settlements. No file search daily limit exists (`cpt-cf-mini-chat-adr-document-retrieval-scope`).

**Implements**:
- `cpt-cf-mini-chat-algo-quota-billing-preflight`
- `cpt-cf-mini-chat-algo-quota-billing-tool-call-limit`
- `cpt-cf-mini-chat-algo-quota-billing-settle`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-quota-usage` (`web_search_calls`, `code_interpreter_calls`)
- Config: `quota.web_search_max_calls_per_message`, `quota.web_search_daily_quota`, `quota.code_interpreter_max_calls_per_message`, `quota.code_interpreter_daily_quota`

### Quota Status API and Warnings

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-billing-quota-status`

The system **MUST** serve `GET /v1/quota/status` (`cpt-cf-mini-chat-interface-public-api`) under `url_prefix` as specified in `cpt-cf-mini-chat-flow-quota-billing-get-quota-status`. The route requires authentication and the license feature `gts.cf.core.lic.feat.v1~cf.core.global.base.v1` (`cpt-cf-mini-chat-constraint-license-gate`). Authorization uses the user-quota resource, action `read`, `require_constraints` `true`, with owner and tenant predicates. The response is the `QuotaStatusResponse` shape and arithmetic of the REST contract and `cpt-cf-mini-chat-algo-quota-billing-quota-view`. The route is registered with OpenAPI operation `mini_chat.get_quota_status`, tag `Mini Chat Quotas`, response 200 `QuotaStatusResponse`, and the 401, 403, 500 and 503 (`Retry-After`) problem responses. The system **MUST** provide the warnings computation of the same algorithm for the `done` event (`quota_warnings`), which the turn pipeline places only on CAS-winning completed turns.

**Implements**:
- `cpt-cf-mini-chat-flow-quota-billing-get-quota-status`
- `cpt-cf-mini-chat-algo-quota-billing-quota-view`
- `cpt-cf-mini-chat-algo-quota-billing-period-keys`

**Constraints**: `cpt-cf-mini-chat-constraint-license-gate`

**Touches**:
- API: `GET /v1/quota/status`
- DB Table: `cpt-cf-mini-chat-dbtable-quota-usage` (read)
- Design: `cpt-cf-mini-chat-design-authz-pep` (user-quota resource)
- Entities: QuotaUsage, `UserLimits`

### Cost Metrics

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-billing-cost-metrics`

The system **MUST** record the quota instruments of DESIGN "Metric series" as specified in `cpt-cf-mini-chat-algo-quota-billing-record-metrics`: `mini_chat_quota_preflight{decision,model,tier}`, `mini_chat_quota_reserve{period}`, `mini_chat_quota_commit{period}` (actual only), `mini_chat_quota_overshoot{period}`, `mini_chat_quota_estimated_tokens`, `mini_chat_quota_actual_tokens` and `mini_chat_code_interpreter_calls{model}` (actual only). It **MUST** write one `quota settled` record per settlement with the token and credit values of the request, and **MUST NOT** record any declared-but-unrecorded quota instrument or use a tenant, user, chat or request id as a label (`cpt-cf-mini-chat-fr-cost-metrics`, `cpt-cf-mini-chat-nfr-cost-control`).

**Implements**:
- `cpt-cf-mini-chat-algo-quota-billing-record-metrics`

**Touches**:
- Observability: `mini_chat_*` meter (instruments registered by `cpt-cf-mini-chat-algo-gear-foundation-register-metrics`)

### Deferred Quota Capabilities

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-billing-deferred`

The system **MUST NOT** implement in P1 (`cpt-cf-mini-chat-adr-quota-policy-scope`, `cpt-cf-mini-chat-adr-group-chat-usage-attribution`): the per-user daily image-input quota, the image byte cap, any write to `image_inputs`, `image_upload_bytes`, `file_search_calls` or `rag_retrieval_calls`, a policy snapshot or user-limits cache, `POST /internal/policy:notify`, a configurable negative downgrade threshold, a tenant operational bucket, an audit event or kill-switch check for system tasks, an `overshoot_capped` or `committed_credits_micro` payload field, the optional pre-reserve usage event, and group-chat attribution. Every quota bucket stays keyed by the requesting user.

**Implements**:
- `cpt-cf-mini-chat-algo-quota-billing-settle`
- `cpt-cf-mini-chat-algo-quota-billing-system-task-usage`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-quota-usage` (reserved counters stay 0)

### Cross-Cutting Quality and Verification

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-quota-billing-quality-gates`

The system **MUST** meet these cross-cutting requirements and completion signals:

- **Security**: the status endpoint is fail-closed (deny 403 `AUTHZ_DENIED`, PDP failure 503 `Retry-After: 5`) and returns only the caller's own buckets; every `quota_usage` statement carries tenant and owner predicates (`cpt-cf-mini-chat-principle-tenant-scoped`); usage events and responses contain no provider identifiers; billing outcomes and settlement details are never exposed by the API.
- **Reliability**: every plugin call is bounded (10 s) and the PDP call 5 s. Preflight failures leave no reserve. A finalization that fails on a database error, or exhausts its conflict retries, leaves the turn `running` for the orphan watchdog. Settlement errors that cannot succeed on retry never do so: an unavailable snapshot, a missing effective model or a credit error makes the CAS winner settle with the deterministic estimated fallback. A NULL `requester_user_id` finalizes the turn without settlement or usage event. Settlement is deterministic from persisted columns and the versioned snapshot (or, on the fallback, from persisted columns alone), so a repeated computation gives the same `actual_credits_micro`.
- **Data integrity**: the CAS guard admits exactly one finalizer per turn. Quota settlement and the usage enqueue commit in one transaction, and the six preflight columns are written once. The reserve re-check prevents concurrent overspend. Settlement targets the preflight period rows. **Transaction discipline** (normative):
  - The reserve and finalization transactions are write-first. The first statement is a write: the daily `total` `quota_usage` UPSERT for the reserve, the `chat_turns` CAS UPDATE for finalization. Alternatively, on SQLite, the transaction is begun `IMMEDIATE`. Either way no read precedes the first write. The reads the pipeline needs, and the version-pinned snapshot fetch, happen before the transaction opens, and no plugin call runs inside it.
  - The preflight read transaction writes nothing and never upgrades to a write transaction.
  - `quota_usage` rows are locked and written in one canonical order: `period_type` `daily` before `monthly`, then bucket `total` before `tier:premium`. This applies to the preflight `FOR UPDATE` read (PostgreSQL), the reserve upserts, the re-read and the settlement upserts.
  - On `SQLITE_BUSY`, or a PostgreSQL serialization failure (`40001`) or deadlock (`40P01`), the caller re-runs the whole transaction at most 3 attempts in total with a short backoff. It then fails: 500 `internal` on the reserve; on finalization the turn is left `running` for the orphan watchdog.
- **Observability**: the instruments and the `quota settled` record of `cpt-cf-mini-chat-dod-quota-billing-cost-metrics`; warnings name the model on an unusable candidate and the offending value on a settlement credit error; unknown error codes are logged at `error`.
- **Rollback**: the feature adds no migration; rolling it back removes the route and the quota service, after which no turn can be reserved; existing `quota_usage` rows and enqueued usage messages remain valid.
- **Test layering**: unit tests cover `credits_micro` (per-component rounding, every error kind), estimation, every cascade branch and downgrade reason, availability per bucket and period, the billing outcome table row by row, the three settlement formulas, the overshoot cap boundary, the usage event fields and dedupe keys, the view arithmetic and the tool limits, all with fake plugins. Integration tests on SQLite and PostgreSQL cover the preflight read, the reserve upsert and re-check under two concurrent reserves, two concurrent reserves on a file-backed SQLite database in WAL mode that produce no "database is locked" 500, the bounded conflict retry, the deterministic settlement fallback (snapshot unavailable, model missing, credit overflow), the finalization of a turn without `requester_user_id`, the CAS race between two finalizers, the bucket rows after each settlement path, the outbox row, and `GET /v1/quota/status` through the REST layer with a mock PDP. No live provider is called.
- **Compile-time gates**: domain code depends on repository and policy ports, not on SeaORM entities; `make dylint` passes.
- **Completion signals**: automated tests exist and pass for every owned acceptance item listed in §1.4, and `make gear-ci GEAR=mini-chat`, `make dylint` and `cfs validate` pass. Every implemented CDSL instruction of this feature is wrapped in `@cpt-begin:{cpt-id}:p1:inst-{id}` / `@cpt-end:{cpt-id}:p1:inst-{id}` code markers (FULL traceability), and every flow, algorithm, state and definition-of-done ID of this feature appears in code. The checkboxes of this document and the featstatus are set to `[x]` only when their markers and tests exist.

**Implements**:
- `cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps`
- `cpt-cf-mini-chat-algo-quota-billing-finalize-turn`
- `cpt-cf-mini-chat-algo-quota-billing-reserve-write`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- Build: `make gear-ci GEAR=mini-chat`, `make dylint`, `cfs validate`

## 6. Acceptance Criteria

- [ ] Principles & Constraints — "Quota is checked before any outbound provider call": with a mock provider, a send whose tiers are all exhausted returns 429 `resource_exhausted` with `context.violations[0]` `{subject: "tokens", description: "quota_exceeded"}` and no SSE stream. A send that exhausts the daily web search or code interpreter quota returns 429 with subject `web_search` or `code_interpreter`. In each case the provider receives no request, and no `chat_turns` row, user message or `quota_usage` change is left behind. For an allowed send the reserve transaction (bucket increments and `chat_turns` row with its six preflight columns) is committed before the first provider request.
- [ ] Quota Status API — "Quota status reporting is accurate and consistent with actual usage": `GET /v1/quota/status` returns `tiers` `premium` then `total`, each with `daily` then `monthly` entries carrying exactly `period`, `limit_credits_micro`, `used_credits_micro`, `remaining_credits_micro`, `remaining_percentage`, `next_reset`, `warning`, `exhausted`, plus `warning_threshold_pct`. Limits come from `user_limits.premium` and `user_limits.standard`, and `used_credits_micro` equals `spent_credits_micro` plus `reserved_credits_micro` of the bucket row (0 without a row). With limit 100_000_000 and used 80_000_001, remaining is 19_999_999, `remaining_percentage` 19 and `warning` `true` at threshold 80; with used 99_000_001 `exhausted` is `true`; with used above the limit remaining is 0. `next_reset` is the next UTC midnight (daily) or the first of the next month at UTC midnight (monthly). A period with limit 0 is omitted. After a completed turn the `used_credits_micro` grows by exactly the committed credits. Another user's or tenant's buckets never appear. PDP deny gives 403 and PDP failure 503 with `Retry-After: 5`.
- [ ] Quota Enforcement — "Reserve-before-execute quota flow enforced on every provider call": every send, retry and edit runs preflight then the reserve write before the provider call. Two concurrent sends that each pass preflight but together exceed a limit yield one success and one 429 `quota_exceeded` (`tokens`) from the re-check, without downgrade. Two concurrent reserves on a file-backed SQLite database in WAL mode both complete, or one gets 429, and neither returns a "database is locked" 500. A conflict that persists through 3 attempts gives 500 `internal` with nothing reserved. On retry and edit the rejected new turn ends `failed` with `error_code` `quota_exceeded` and no usage event. Rows are keyed by the preflight `period_start` (`daily` the UTC date, `monthly` the first of the UTC month).
- [ ] Quota Enforcement — "Tier downgrade applied when a higher tier is exhausted": with the DESIGN §5.10 data (premium multipliers 2_500_000_000, standard 1_000_000_000, premium daily limit 22_000_000 with 20_000_000 spent, total daily 60_000_000 with 25_000_000 spent, estimated input 1000 tokens, `max_output_tokens_applied` 500), the premium reserve 3_750_000 fails `tier:premium` daily, the turn runs on the standard model with `quota_decision` `downgrade` and `downgrade_reason` `premium_quota_exhausted`, and `reserved_credits_micro` 1_500_000 is booked on the `total` daily and monthly rows only. `force_standard_tier` and `disable_premium_tier` give the reasons of the same names. A disabled selected model gives `model_disabled`. A standard selection never runs on premium. No enabled model in any tier gives 429.
- [ ] Quota Enforcement — "Credits and tokens are accounted correctly per model and tier": `credits_micro(1, 1, 1, 1)` is 2 (per-component ceiling). Zero multipliers, multipliers above 10_000_000_000 and token counts above 10_000_000 are errors, which make a candidate unavailable at preflight and switch settlement to the deterministic estimated fallback. A message of 1000 bytes with budgets 4 / 100 / 10 estimates 385 text tokens. `max_output_tokens_applied` is `min(catalog max_output_tokens, streaming.max_output_tokens)`. The surcharges appear only when the candidate's `tool_support` and the kill switches allow the tool. After the §5.10 turn settles with actual 900 input and 300 output tokens, the `total` daily and monthly rows have `spent_credits_micro` +1_200_000, `reserved_credits_micro` back to their prior value, `calls` +1, `input_tokens` +900, `output_tokens` +300. A premium turn also moves `tier:premium` spent, reserved and calls, but not its token counters.
- [ ] Settlement & Finalization — "Every terminal outcome settles quota exactly once, using actual or estimated usage as appropriate": completed settles `actual` (0 credits without usage). A provider error with non-zero usage settles `actual`, and without usage or with all-zero usage settles `estimated`. A client disconnect settles `aborted` / `estimated`. The orphan watchdog settles `aborted` / `estimated` with `terminal_state` `failed`, `selected_model` equal to `effective_model`, and periods from `started_at`. Estimated credits for `reserve_tokens` 1500, `max_output_tokens_applied` 500, floor 50 and multipliers 1_000_000_000 are 1_050_000. Actual 11_000 + 500 tokens against `reserve_tokens` 10_000 (factor 1.15 > 1.10) commits `reserved_credits_micro`, keeps the turn `completed`, and still adds 11_000 / 500 to the token counters; a factor of exactly 1.10 commits the actual credits. Two finalizers racing on one turn produce one settlement and one usage event; the loser changes nothing. A retry or edit turn left `running` without reserve fields is finalized by the watchdog with no `quota_usage` change and an `aborted` / `estimated` event with 0 credits, empty model fields and `policy_version_applied` 0. A stale turn whose `requester_user_id` is NULL is finalized `failed` / `orphan_timeout` by the watchdog. That run logs at `error`, changes no `quota_usage` row, writes no outbox message, and leaves the turn out of the next scan. Settlement uses the snapshot of `policy_version_applied` even after the plugin's current version changed.
- [ ] Settlement & Finalization — deterministic fallback, missing snapshot (feature-local deviation): a fake plugin fails or times out `get_policy_snapshot` for the turn's `policy_version_applied`. A completed stream turn is then still finalized `completed`, and a stale turn `failed` / `orphan_timeout`. The plugin call happens before the finalization transaction opens. The event has `settlement_method` `estimated` and `actual_credits_micro` equal to `chat_turns.reserved_credits_micro`. The `total` rows get `spent_credits_micro` + `reserved_credits_micro`, `reserved_credits_micro` back to the prior value, `calls` +1 and token counters +0. The `tier:premium` rows are settled only for a premium turn, detected from its reserved premium rows. One `error` log names the cause. A snapshot without the turn's `effective_model` gives the same result. Running the scenario twice on copies of the same row commits the same credits.
- [ ] Settlement & Finalization — deterministic fallback, credit overflow (feature-local deviation): a completed turn reports usage with `input_tokens` 10_000_001, or the snapshot multipliers make the actual credit computation overflow. The turn is still finalized `completed` with `settlement_method` `estimated`. It commits `reserved_credits_micro`, takes the tier from the snapshot entry, writes one outbox message with `usage` `null`, and logs at `error` naming the offending value. The transaction is not rolled back and the turn is not left `running`.
- [ ] Settlement & Finalization — "Usage is published reliably and exactly once per turn": every won finalization of a turn with a `requester_user_id` writes exactly one outbox message on `mini-chat.usage_snapshot` in the same transaction as the bucket updates; forcing that transaction to fail leaves neither the quota change nor the message. The payload has `tenant_id`, `user_id`, `chat_id`, `turn_id`, `request_id`, `effective_model`, `selected_model`, `terminal_state`, `billing_outcome`, `usage`, `actual_credits_micro` (the committed credits), `settlement_method`, `policy_version_applied`, `web_search_calls`, `code_interpreter_calls`, `file_search_calls`, `timestamp`, `requester_type` `user`, `dedupe_key` `{tenant hex}/{turn hex}/{request hex}`, and no `event_type`, `reserved_credits_micro` or `error_code`. The plugin's `publish_usage` receives the same event. A thread-summary commit enqueues one `system_task` / `none` event with 0 credits, `policy_version_applied` 0, no `user_id` or `turn_id`, `system_task_type` `thread_summary_update` and `dedupe_key` `{tenant hex}/thread_summary_update/{system request hex}`, and changes no `quota_usage` row.
- [ ] Web Search (accounting and quota-limit part) — "Web search tool use is reported, cited, accounted, and quota-limited correctly": `web_search.enabled` under `disable_web_search` returns 400 `failed_precondition` `violations[{subject: web_search, type: FEATURE_DISABLED}]` before any quota read. With `web_search_calls` of the daily `total` row at 75, a web-search send on a model with `tool_support.web_search` returns 429 subject `web_search`, while a send without web search, or on a model without web search support, is not affected. The reserve includes `web_search_surcharge_tokens` only when the tool is sent. A third web search start in one turn (limit 2) gives `web_search_calls_exceeded`, settled `failed` / `estimated` without refund. Completed web search calls are added to `web_search_calls` of the `total` daily and monthly rows and to the usage event.
- [ ] Cost metrics: `mini_chat_quota_preflight` is counted with `decision` `allow`, `downgrade` or `reject`. `mini_chat_quota_reserve` is counted per period after each reserve commit, and `mini_chat_quota_commit` and `mini_chat_quota_actual_tokens` only for actual settlements. `mini_chat_quota_overshoot` is counted per period when actual tokens exceed the reserve. No metric carries a tenant, user, chat or request id label, and every settlement writes one `quota settled` record.
- [ ] Quality gates: `make gear-ci GEAR=mini-chat`, `make dylint` and `cfs validate` pass, and every implemented CDSL ID of this feature has `@cpt-begin` / `@cpt-end` markers.
