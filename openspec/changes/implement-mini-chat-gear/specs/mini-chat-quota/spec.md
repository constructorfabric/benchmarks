## ADDED Requirements

### Requirement: Quota checked before any provider call
The quota preflight SHALL run before any outbound provider call; rejection returns 429 `resource_exhausted` with `violations[{subject:"tokens"|"web_search"|"code_interpreter", description:"quota_exceeded"}]` and no provider request is made.

#### Scenario: All tiers exhausted
- **WHEN** a user's `total` bucket is spent to the daily limit
- **THEN** sending a message returns 429 with subject `tokens` and the mock provider receives no request

### Requirement: Reserve before execute
For the effective model the gear SHALL compute `estimated_input_tokens = ceil((ceil(bytes/bpt)+overhead)*(100+margin)/100) + prior_context_tokens + image/tool/web/code surcharges`, `reserve_tokens = estimated_input_tokens + max_output_tokens_applied`, `reserved_credits_micro = credits_micro(estimated_input, max_output_applied, in_mult, out_mult)` (per-component ceil division by 1,000,000), persist them with `policy_version_applied`, `effective_model` and `minimal_generation_floor_applied = min(config floor, max_output_applied)` on the turn, and increment `quota_usage.reserved_credits_micro` of bucket `total` (and `tier:premium` for premium) for the daily and monthly rows in the same transaction as the turn insert, re-checking limits after the increment.

#### Scenario: Reserve rows
- **WHEN** a standard-tier turn starts
- **THEN** daily and monthly `total` rows have `reserved_credits_micro` increased by the turn's `reserved_credits_micro` and no `tier:premium` row changes

### Requirement: Tier downgrade
The cascade SHALL start at the selected model's tier (premium for a missing model), skip premium under `force_standard_tier`/`disable_premium_tier`, pick the tier candidate (selected model, else tier default, else first enabled), and accept it if every required bucket satisfies `spent + reserved + this_reserve <= limit` for daily and monthly; `done` reports `quota_decision = "downgrade"`, `downgrade_from = selected_model` and `downgrade_reason` in {`premium_quota_exhausted`, `force_standard_tier`, `disable_premium_tier`, `model_disabled`}.

#### Scenario: Premium exhausted
- **WHEN** the premium daily bucket is exhausted and a premium chat sends a message
- **THEN** the turn runs on the standard model and `done` has `downgrade_reason = "premium_quota_exhausted"`

### Requirement: Credits accounted per model and tier
Settlement SHALL use the multipliers of the effective model from the turn's policy version: completed → actual usage (`settlement_method = actual`, always — also when the provider reported no usage object, which then charges 0; overshoot above `quota.overshoot_tolerance_factor` capped at the reserve); failed with a usage object that has at least one non-zero token count → actual (a missing or all-zero usage object is "usage unknown"); failed without usage, cancelled, orphan → estimated `credits_micro(reserve_tokens − max_output_applied, floor_applied)`. Settlement decrements reserved and increments spent, `calls += 1`, token counters (actual only, `total` bucket) and web search / code interpreter calls (actual and estimated, `total` bucket).

#### Scenario: Completed settlement
- **WHEN** a standard turn with multipliers 1,000,000/3,000,000 completes with usage 1000/200
- **THEN** `spent_credits_micro` of the daily and monthly `total` rows increase by 1600, reserved returns to its prior value and `calls`, `input_tokens`, `output_tokens` increase by 1, 1000, 200

### Requirement: Every terminal outcome settles exactly once
Exactly one CAS-winning finalization per reserved turn SHALL settle quota and enqueue one usage event (dedupe key `{tenant}/{turn}/{request}` in simple UUID form, `billing_outcome` completed/failed/aborted, `settlement_method` actual/estimated) in the same transaction; replays and CAS losers never settle.

#### Scenario: Replay does not settle
- **WHEN** a completed turn is replayed
- **THEN** `quota_usage` is unchanged and no outbox row is added

### Requirement: Tool quotas
Daily web search (`quota.web_search_daily_quota`) and code interpreter (`quota.code_interpreter_daily_quota`) quotas SHALL be checked at preflight only when the tool is sent; per-turn limits (`web_search_max_calls_per_message`, `code_interpreter_max_calls_per_message`) are enforced mid-stream by cancelling the provider stream, finalizing `failed` with `web_search_calls_exceeded`/`code_interpreter_calls_exceeded` (estimated settlement) and sending that SSE error.

#### Scenario: Web search per-turn limit
- **WHEN** the provider starts 3 web searches with the per-turn limit 2
- **THEN** the client receives `error {code:"web_search_calls_exceeded"}` and the turn is `failed`

### Requirement: Quota status API
`GET /v1/quota/status` SHALL return `{tiers:[{tier:"premium"|"total", periods:[{period, limit_credits_micro, used_credits_micro = spent+reserved, remaining_credits_micro, remaining_percentage (floored), next_reset, warning, exhausted}]}], warning_threshold_pct}`, skipping periods with limit ≤ 0; `done.quota_warnings` carries the same flags with `next_reset` only when warning or exhausted.

#### Scenario: Status after usage
- **WHEN** a user has spent 60% of the daily total limit with threshold 80
- **THEN** the daily `total` entry shows `remaining_percentage = 40`, `warning = false`, `exhausted = false`

### Requirement: Models API
`GET /v1/models` SHALL list enabled catalog entries as `{model_id, display_name, tier, multiplier_display, description?, multimodal_capabilities, context_window}`; `GET /v1/models/{id}` returns one or 404 (model resource type) for disabled/unknown ids; provider ids, multipliers and `is_default` are never exposed.

#### Scenario: Disabled model hidden
- **WHEN** the catalog has a disabled model
- **THEN** it is absent from the list and its GET returns 404

### Requirement: Reserved quota counters stay unpopulated
The `quota_usage` columns `file_search_calls`, `image_inputs` and `image_upload_bytes` SHALL stay 0 (ADR-0007, ADR-0008); completed `file_search` calls are counted only in `chat_turns.file_search_completed_count` and the `file_search_calls` field of the usage and audit events.

#### Scenario: File search call accounting
- **WHEN** a turn with an image attachment completes after one provider `file_search` call
- **THEN** the turn's `file_search_completed_count` and the usage event's `file_search_calls` are 1, while `quota_usage.file_search_calls`, `image_inputs` and `image_upload_bytes` remain 0

#### Scenario: Failed turn with zero usage
- **WHEN** a provider stream fails with `usage {input_tokens: 0, output_tokens: 0}`
- **THEN** the turn settles `estimated` with a non-zero charge

