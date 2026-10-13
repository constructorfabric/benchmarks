## ADDED Requirements

### Requirement: Reserve before execute
Every provider call of a user turn SHALL be preceded by a quota preflight and a reserve. The preflight SHALL compute per cascade candidate `estimated_input_tokens = ceil((ceil(utf8_bytes / bytes_per_token_conservative) + fixed_overhead_tokens) * (100 + safety_margin_pct) / 100) + prior_context_tokens + images * image_token_budget + surcharges` (tool/web search/code interpreter surcharges only when the tool is sent with that model), `max_output_tokens_applied = min(catalog max_output_tokens, streaming.max_output_tokens)`, `reserve_tokens = estimated_input_tokens + max_output_tokens_applied` and `reserved_credits_micro = credits_micro(estimated_input_tokens, max_output_tokens_applied, in_mult, out_mult)`. The reserve transaction SHALL increment `reserved_credits_micro` on the daily and monthly `total` rows (and `tier:premium` rows for premium turns), re-check `spent + reserved <= limit` and roll back with 429 on violation, and persist `reserve_tokens`, `max_output_tokens_applied`, `reserved_credits_micro`, `policy_version_applied`, `effective_model` and `minimal_generation_floor_applied = min(config floor, max_output_tokens_applied)` on the turn.

#### Scenario: Reserve recorded
- **WHEN** a turn starts
- **THEN** its `chat_turns` row has non-null reserve columns and the user's daily and monthly `total` rows have `reserved_credits_micro` equal to the turn's reserve while it runs

#### Scenario: All tiers exhausted
- **WHEN** the user's `total` bucket is exhausted
- **THEN** the send returns 429 `resource_exhausted` with `violations[0].subject = tokens` and the provider is not called

### Requirement: Tier cascade and downgrade
The cascade SHALL start at the selected model's tier (premium for a missing model) and try `[premium, standard]` or `[standard]`; premium is skipped under `force_standard_tier`/`disable_premium_tier`; the candidate is the selected model when enabled in that tier, else the tier's enabled `is_default` model, else its first enabled model; a premium candidate needs room in `total` and `tier:premium` for all periods, a standard one in `total`. The first available candidate is the effective model; `quota_decision = downgrade` with `downgrade_reason` when it differs from the selected model or a reason was set (`premium_quota_exhausted`, `force_standard_tier`, `disable_premium_tier`, `model_disabled`).

#### Scenario: Premium exhausted
- **WHEN** the `tier:premium` daily bucket of a user is exhausted and they send in a premium-model chat
- **THEN** the turn runs on the standard model and `done` has `quota_decision: downgrade`, `downgrade_from: <premium>`, `downgrade_reason: premium_quota_exhausted`; the assistant message `model` is the standard model

### Requirement: Credit arithmetic
Credits SHALL be computed as `ceil_div(input_tokens * in_mult, 1_000_000) + ceil_div(output_tokens * out_mult, 1_000_000)` with checked arithmetic, token counts ≤ 10,000,000 and multipliers in `1..=10_000_000_000`, using the multipliers of the turn's effective model in the snapshot of `policy_version_applied`. Premium turns SHALL be charged to both `total` and `tier:premium`, standard turns to `total` only.

#### Scenario: Credits for completed turn
- **WHEN** a standard turn (in_mult 1_000_000, out_mult 3_000_000) completes with 100 input and 50 output tokens
- **THEN** 250 micro-credits are added to `spent_credits_micro` of the daily and monthly `total` rows

### Requirement: Settlement exactly once
Every turn that took a reserve SHALL be settled exactly once in the CAS-winning finalization transaction, together with exactly one usage outbox event (`dedupe_key = {tenant}/{turn}/{request}` in simple hex) and one audit event: completed → `actual` (overshoot above `quota.overshoot_tolerance_factor` capped at the reserve); failed with non-zero usage → `actual`; failed without usage, cancelled and orphan → `estimated` with `credits_micro(reserve_tokens - max_output_tokens_applied, minimal_generation_floor_applied, ...)`. Settlement SHALL release the turn's reserve, add committed credits, `calls += 1`, token telemetry (actual only, `total` row) and web search / code interpreter calls (`total` row). Billing outcome SHALL be `completed`, `failed` or `aborted` (cancelled / `orphan_timeout`).

#### Scenario: Cancelled turn settlement
- **WHEN** a client disconnects mid-stream
- **THEN** the turn's reserve is released, the estimated charge is added to `spent_credits_micro` and one usage event with `billing_outcome = aborted`, `settlement_method = estimated` is enqueued

#### Scenario: Orphan watchdog
- **WHEN** a running turn's `last_progress_at` is older than `orphan_watchdog.timeout_secs`
- **THEN** the watchdog CAS-finalizes it as `failed` with `error_code = orphan_timeout`, settles estimated and enqueues one `aborted` usage event; a second scan does nothing

### Requirement: Tool quotas
With `web_search.enabled` on a model supporting web search the preflight SHALL reject with 429 subject `web_search` when the daily `total` row's `web_search_calls >= quota.web_search_daily_quota`; ready XLSX attachments on a model supporting code interpreter likewise with subject `code_interpreter` and `quota.code_interpreter_daily_quota`. During the stream more `web_search` (`code_interpreter`) tool starts than `quota.web_search_max_calls_per_message` (`code_interpreter_max_calls_per_message`) SHALL cancel the provider stream, finalize `failed` with `web_search_calls_exceeded` (`code_interpreter_calls_exceeded`), settle estimated and send that SSE error code. Completed web search calls SHALL be counted into `quota_usage.web_search_calls`.

#### Scenario: Web search daily quota
- **WHEN** the user's daily web search calls reached the quota and they send with `web_search.enabled = true`
- **THEN** the response is 429 with `violations[0].subject = web_search`

### Requirement: Quota status reporting
`GET /v1/quota/status` SHALL return `{tiers: [{tier: premium|total, periods: [{period: daily|monthly, limit_credits_micro, used_credits_micro (spent + reserved), remaining_credits_micro, remaining_percentage (floored, 0-100), next_reset, warning, exhausted}]}], warning_threshold_pct}` for the caller, skipping periods with limit ≤ 0; `warning` iff `remaining_percentage <= 100 - warning_threshold_pct`, `exhausted` iff `remaining_percentage == 0`; `next_reset` is next UTC midnight (daily) or the 1st of next month (monthly). `done.quota_warnings` SHALL carry the same per-tier/period flags, with `next_reset` only when warning or exhausted.

#### Scenario: Status after usage
- **WHEN** a user has spent credits in a completed turn
- **THEN** `used_credits_micro` of the `total` daily period equals the `spent_credits_micro + reserved_credits_micro` of that row
