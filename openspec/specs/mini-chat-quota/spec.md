# mini-chat-quota Specification

## Purpose
Credit-based quota enforcement: preflight cascade, reserve, settlement, usage events, quota status and web search quotas.

## Requirements

### Requirement: Quota before outbound
Every provider call of a user turn SHALL be preceded by a quota preflight and a reserve written in the same transaction as the turn insert; no provider request SHALL be issued when the preflight or the reserve re-check rejects.

#### Scenario: Exhausted user
- **WHEN** the user's `total` daily bucket has no room for the reserve
- **THEN** the send returns 429 `resource_exhausted` with `violations[0].subject = tokens`, `description = quota_exceeded`, and the provider receives nothing

### Requirement: Reserve estimation and credits
`credits_micro(in, out, in_mult, out_mult) = ceil(in*in_mult/1e6) + ceil(out*out_mult/1e6)` with checked arithmetic (tokens ≤ 10_000_000, multipliers in `1..=10_000_000_000`). The reserve of a candidate model SHALL be: `estimated_text_tokens = ceil((ceil(utf8_bytes / bytes_per_token_conservative) + fixed_overhead_tokens) * (100 + safety_margin_pct) / 100)` + `prior_context_tokens` (input+output of the latest non-deleted assistant message with non-zero usage) + `image_token_budget * images` + surcharges for `file_search` (ready document, tool support, kill switch off), `web_search` (requested, tool support) and `code_interpreter` (ready XLSX, tool support, kill switch off); `max_output_tokens_applied = min(catalog max_output_tokens, streaming.max_output_tokens)`; `reserve_tokens = estimated_input + max_output_tokens_applied`; `reserved_credits_micro = credits_micro(estimated_input, max_output_tokens_applied, ...)`. The turn SHALL persist `reserve_tokens`, `max_output_tokens_applied`, `reserved_credits_micro`, `policy_version_applied`, `effective_model`, `minimal_generation_floor_applied = min(estimation_budgets.minimal_generation_floor, max_output_tokens_applied)`.

#### Scenario: Reserve persisted
- **WHEN** a turn starts
- **THEN** its `chat_turns` row holds the reserve columns and `quota_usage` `total` daily and monthly rows have `reserved_credits_micro` increased by `reserved_credits_micro`

### Requirement: Downgrade cascade
The cascade SHALL start at the selected model's tier (premium when the model is missing; reason `model_disabled` when disabled or missing), skip premium under `force_standard_tier`/`disable_premium_tier` (reason accordingly), pick per tier the selected model if enabled in that tier, else the enabled `is_default` model of the tier, else the first enabled model of the tier, and check availability `spent + reserved + candidate_reserve <= limit` for `total` (standard limits) and, for premium, also `tier:premium` (premium limits) in daily and monthly periods (UTC). Premium unavailable SHALL set reason `premium_quota_exhausted`. No available tier SHALL return 429 with subject `tokens`. The `done` event SHALL report `quota_decision = downgrade` with `downgrade_from = selected_model` and the reason.

#### Scenario: Premium exhausted
- **WHEN** the premium daily bucket is full and the chat model is premium
- **THEN** the turn runs on the standard model and `done` has `quota_decision: downgrade`, `downgrade_reason: premium_quota_exhausted`

### Requirement: Settlement exactly once
Finalization SHALL, in one transaction with the CAS, settle the reserve on the turn's persisted `period_start` rows: release `reserved_credits_micro`, add committed credits to `spent_credits_micro`, `calls += 1` (both buckets for premium turns, `total` only otherwise), add actual `input_tokens`/`output_tokens` to `total` on actual settlements and tool call counts on actual/estimated settlements. Completed turns settle `actual` (capped at the reserve when `actual_tokens / reserve_tokens > quota.overshoot_tolerance_factor`); failed turns with non-zero usage settle `actual`, otherwise `estimated` with `credits_micro(reserve_tokens - max_output_tokens_applied, minimal_generation_floor_applied, ...)`; cancelled and orphan turns settle `estimated`. The same transaction SHALL enqueue exactly one `UsageEvent` (billing outcome `completed`/`failed`/`aborted`, `dedupe_key = {tenant_hex}/{turn_hex}/{request_hex}`) and one audit event.

#### Scenario: Completed settlement
- **WHEN** a turn completes with usage 100/50 on a model with multipliers 1_000_000/3_000_000
- **THEN** `spent_credits_micro` grows by 250, `reserved_credits_micro` returns to its prior value and one usage outbox row with `settlement_method: actual` exists

#### Scenario: Cancelled settlement
- **WHEN** the client disconnects mid-stream
- **THEN** the settlement is `estimated`, `billing_outcome` `aborted`, and the reserve is released

### Requirement: Quota status API
`GET /v1/quota/status` SHALL return `{tiers: [{tier: "premium"|"total", periods: [{period, limit_credits_micro, used_credits_micro (spent+reserved), remaining_credits_micro, remaining_percentage (floored), next_reset, warning, exhausted}]}], warning_threshold_pct}`, skipping periods with limit ≤ 0, with `warning` when `remaining_percentage <= 100 - warning_threshold_pct` and `exhausted` when it is 0.

#### Scenario: After usage
- **WHEN** a turn consumed credits
- **THEN** `used_credits_micro` of `total` daily equals the bucket's spent + reserved

### Requirement: Web search
`web_search.enabled` SHALL include the `web_search` tool and guard only when the effective model supports it; the daily quota `quota.web_search_daily_quota` SHALL be checked at preflight against the daily `total` row's `web_search_calls` (429 subject `web_search`); more than `quota.web_search_max_calls_per_message` started calls SHALL abort the turn with SSE `error` `web_search_calls_exceeded` (turn `failed`, estimated settlement); completed calls SHALL be counted in `chat_turns.web_search_completed_count`, the usage event and `quota_usage.web_search_calls`; web citations SHALL be emitted. The same rules SHALL apply to code interpreter with `quota.code_interpreter_*`.

#### Scenario: Per-message limit
- **WHEN** the provider starts three web searches with the default limit 2
- **THEN** the client receives `error` with `web_search_calls_exceeded`
