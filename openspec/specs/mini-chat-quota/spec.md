# mini-chat-quota Specification

## Purpose
Defines the `mini-chat` gear's models API, quota status API, reserve/settle quota enforcement, tier downgrade, credit accounting, usage publication (source: `gears/mini-chat/docs/`). Introduced by change `implement-mini-chat-gear`.

## Requirements

### Requirement: Models API
`GET /mini-chat/v1/models` SHALL return `{items:[{model_id, display_name, tier: standard|premium, multiplier_display, description?, multimodal_capabilities, context_window}]}` for enabled catalog entries only; `GET /models/{id}` SHALL return one such object or 404 (model resource type) for disabled or unknown models. Provider ids, provider model ids, multipliers and `is_default` SHALL NOT be exposed.

#### Scenario: Disabled model hidden
- **WHEN** the catalog has a disabled model
- **THEN** it is absent from the list and GET by id returns 404

### Requirement: Quota status API
`GET /quota/status` SHALL return `{tiers:[{tier: premium|total, periods:[{period: daily|monthly, limit_credits_micro, used_credits_micro (spent+reserved), remaining_credits_micro, remaining_percentage (floored, 0–100), next_reset, warning, exhausted}]}], warning_threshold_pct}` for the caller, skipping periods whose limit is ≤ 0. `warning` is `remaining_percentage <= 100 - warning_threshold_pct`; `exhausted` is `remaining_percentage == 0`.

#### Scenario: Status reflects usage
- **WHEN** a completed turn spent credits
- **THEN** `used_credits_micro` of the `total` daily period equals the spent credits

### Requirement: Credit arithmetic
Credit computation SHALL use `credits_micro(in, out, in_mult, out_mult) = ceil(in*in_mult/1e6) + ceil(out*out_mult/1e6)` with checked arithmetic (tokens ≤ 10,000,000; multipliers in 1..=10,000,000,000).

#### Scenario: Per-component rounding
- **WHEN** computing credits for 1 input and 1 output token with multiplier 1,000,000
- **THEN** the result is 2

### Requirement: Reserve before execute and tier cascade
Every provider call SHALL be preceded by a preflight that builds the cascade (`[premium, standard]` for premium/disabled/missing selected models, `[standard]` otherwise), skips premium under `disable_premium_tier`/`force_standard_tier`, picks the candidate model per tier (selected model, else tier default, else first enabled), and accepts the first tier where every bucket (`total`; plus `tier:premium` for premium) in every period (daily, monthly) satisfies `spent + reserved + candidate_reserve <= limit`. The reserve is `estimated_input_tokens + max_output_tokens_applied` tokens with `estimated_input_tokens = text estimate + prior_context_tokens + surcharges`. No available tier → 429 `resource_exhausted` `violations[{subject:"tokens", description:"quota_exceeded"}]` without provider call. Daily web search / code interpreter quotas → 429 with subject `web_search` / `code_interpreter`. The reserve is written (bucket rows incremented and re-checked) in the reserve transaction and persisted on `chat_turns` (`reserve_tokens`, `max_output_tokens_applied`, `reserved_credits_micro`, `policy_version_applied`, `effective_model`, `minimal_generation_floor_applied`).

#### Scenario: Premium exhausted downgrades
- **WHEN** the user's premium daily bucket is spent and a premium chat sends a message
- **THEN** the turn runs on the standard default model and `done` has `quota_decision: downgrade`, `downgrade_from` = selected model, `downgrade_reason: premium_quota_exhausted`

#### Scenario: All tiers exhausted
- **WHEN** the `total` daily bucket is spent
- **THEN** 429 with subject `tokens` and no provider request

### Requirement: Settlement exactly once
Each terminal outcome SHALL settle exactly once in the CAS-winning finalization transaction: completed → actual credits (capped at reserve when overshoot exceeds `quota.overshoot_tolerance_factor`); failed with non-zero usage → actual; failed without usage, cancelled, orphan → estimated `credits_micro(reserve_tokens − max_output_tokens_applied, minimal_generation_floor_applied)`. Settlement decrements `reserved_credits_micro`, increments `spent_credits_micro` and `calls` on the turn's buckets, token/tool telemetry on `total`, and enqueues exactly one `UsageEvent` (dedupe key `{tenant_hex}/{turn_hex}/{request_hex}`) and one audit event in the same transaction.

#### Scenario: Completed settlement
- **WHEN** a turn completes with usage 100/50 on a model with multipliers 1,000,000/3,000,000
- **THEN** `spent_credits_micro` increases by 250, `reserved_credits_micro` returns to its previous value, and one usage outbox row with `settlement_method: actual` exists

### Requirement: Usage publication
The usage outbox handler SHALL deliver each usage event to the model policy plugin's `publish_usage`, retry transient failures and reject malformed payloads.

#### Scenario: Usage delivered
- **WHEN** a turn completes
- **THEN** the policy plugin receives exactly one usage event for that turn
