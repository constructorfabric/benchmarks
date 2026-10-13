# mini-chat-quota Specification

## Purpose
Credit-based quota status, preflight enforcement, reservation and settlement of the mini-chat gear.

## Requirements

### Requirement: Quota status reporting
`GET /v1/quota/status` SHALL return `{tiers:[{tier: premium|total, periods:[{period: daily|monthly, limit_credits_micro, used_credits_micro (spent+reserved), remaining_credits_micro, remaining_percentage (floored), next_reset, warning, exhausted}]}], warning_threshold_pct}` for the caller, where `total` uses the standard limits and `premium` the premium limits; periods with limit ≤ 0 are omitted; `warning` when `remaining_percentage <= 100 - warning_threshold_pct`; `exhausted` when it is 0.

#### Scenario: Status reflects a completed turn
- **WHEN** a standard-tier turn settled 1234 credits_micro
- **THEN** the `total` daily and monthly `used_credits_micro` increase by 1234 and premium is unchanged

### Requirement: Reserve before execute
Every provider call for a user turn SHALL be preceded by a reserve: `estimated_input_tokens = ceil((ceil(bytes/bpt)+overhead)*(100+margin)/100) + prior_context_tokens + image/tool/web/ci surcharges`, `reserve_tokens = estimated_input_tokens + max_output_tokens_applied`, `reserved_credits_micro = credits_micro(...)`, persisted on `chat_turns` with `policy_version_applied`, `effective_model`, `minimal_generation_floor_applied`, and added to `quota_usage.reserved_credits_micro` of `total` (and `tier:premium` for premium) daily and monthly rows in the same transaction as the turn insert, with a re-check that rolls back on overflow (429 `tokens`). When no tier fits the request SHALL be rejected with 429 before any provider call.

#### Scenario: All tiers exhausted
- **WHEN** the user's `total` daily bucket is spent up to the limit
- **THEN** sending returns 429 `resource_exhausted` with `violations[0].subject = tokens` and the provider is not called

### Requirement: Tier downgrade
The cascade SHALL be `[premium, standard]` for a premium (or missing) selected model and `[standard]` for standard; premium is skipped under `disable_premium_tier`/`force_standard_tier`; a candidate is the selected model, else the tier's default, else its first enabled model; availability requires `spent + reserved + this_reserve <= limit` for every required bucket and period. Downgrade reasons: `premium_quota_exhausted`, `force_standard_tier`, `disable_premium_tier`, `model_disabled`.

#### Scenario: Premium exhausted
- **WHEN** a premium chat's `tier:premium` daily bucket is exhausted
- **THEN** `done` has `quota_decision = downgrade`, `downgrade_reason = premium_quota_exhausted`, `downgrade_from` = selected model and `effective_model` = a standard model, and the assistant message `model` is that standard model

### Requirement: Credit accounting per model and tier
`credits_micro(in, out, im, om) = ceil(in*im/1e6) + ceil(out*om/1e6)` (checked arithmetic). Settlement SHALL release `turn_reserved_credits_micro` and add committed credits to `spent_credits_micro`, `calls += 1` for `total` and, for premium turns, `tier:premium`; `input_tokens`/`output_tokens` telemetry only on `total` for actual settlements; overshoot above `quota.overshoot_tolerance_factor` caps the charge at the reserve.

#### Scenario: Actual settlement
- **WHEN** a standard turn with multipliers 1_000_000/3_000_000 reports usage 100/50
- **THEN** `spent_credits_micro` of the `total` rows grows by 250 and `reserved_credits_micro` returns to its previous value

### Requirement: Every terminal outcome settles exactly once
Completed turns settle `actual`; failed turns with non-zero usage `actual`, otherwise `estimated` (`credits_micro(estimated_input, floor)`); cancelled and orphaned turns `estimated`. Settlement, CAS state transition and the usage outbox enqueue SHALL be one transaction, performed only by the CAS winner.

#### Scenario: Cancelled turn charged estimate
- **WHEN** a client disconnects mid-stream
- **THEN** exactly one usage outbox message with `billing_outcome = aborted`, `settlement_method = estimated` is enqueued and the reserve is released

### Requirement: Usage published reliably once per turn
Usage events (SDK `UsageEvent` JSON with `dedupe_key = {tenant_hex}/{turn_hex}/{request_hex}`) SHALL be enqueued to `outbox.queue_name` (default `mini-chat.usage_snapshot`) and delivered by the usage handler to the policy plugin's `publish_usage`.

#### Scenario: One event per turn
- **WHEN** a turn completes and is replayed twice
- **THEN** the usage queue contains exactly one message for the turn
