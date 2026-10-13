## ADDED Requirements

### Requirement: SDK crate packaging
The workspace SHALL contain the crate `gears/mini-chat/mini-chat-sdk` with package name `cf-gears-mini-chat-sdk` and library name `mini_chat_sdk`. It SHALL be a workspace member and the `cf-gears-mini-chat` gear SHALL depend on it.

#### Scenario: Server builds with the SDK
- **WHEN** `cargo build --bin cf-gears-example-server --no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore` runs
- **THEN** the build succeeds and the gear crate links `mini_chat_sdk`

### Requirement: Model policy plugin contract
The SDK SHALL define `MiniChatModelPolicyPluginClientV1` (async trait) with operations to read the current policy version, the policy snapshot for a version (model catalog + kill switches), the user limits for `(user_id, policy_version)`, `publish_usage(UsageEvent)`, and `check_user_license` (default body returns `active: false`), plus a GTS plugin spec `MiniChatModelPolicyPluginSpecV1`. `ModelCatalogEntry` SHALL deserialize the catalog format of `config/mini-chat.yaml` (`id`, `provider_model_id`, `display_name`, `description`, `provider_id`, `provider_display_name`, `icon`, `tier` (`premium`/`standard`, case-insensitive), `enabled` (default false), `system_prompt`, `thread_summary_prompt`, `multimodal_capabilities`, `context_window`, `max_output_tokens`, `max_input_tokens`, both credit multipliers, `multiplier_display`, `estimation_budgets` (defaults when absent), `max_num_results`, `web_search_context_size` (default `low`), `max_tool_calls` (default 2), `general_config` (`max_file_size_mb`, `api_params`, `features`, `tool_support`, `supported_endpoints`), `preference` (`is_default`, `sort_order`)). `KillSwitches` SHALL contain `disable_premium_tier`, `force_standard_tier`, `disable_web_search`, `disable_file_search`, `disable_images`, `disable_code_interpreter`. `UserLimits` SHALL contain `standard` and `premium` `TierLimits` with `limit_daily_credits_micro` and `limit_monthly_credits_micro`.

#### Scenario: Catalog entry from config
- **WHEN** the catalog entry `gpt-4.1` of `config/mini-chat.yaml` is deserialized
- **THEN** it yields tier `premium`, `enabled = true`, `tool_support.web_search = true` and the configured estimation budgets

### Requirement: Audit plugin contract
The SDK SHALL define `MiniChatAuditPluginClientV1` with an `emit` operation accepting audit events (`TurnAuditEvent` with `event_type` `turn_completed`/`turn_failed`, and mutation events `turn_retry`, `turn_edit`, `turn_delete`), an error type distinguishing `Transient`, `PluginTimeout` and `Permanent`, and a GTS plugin spec `MiniChatAuditPluginSpecV1`.

#### Scenario: Audit error classification
- **WHEN** a plugin returns `Transient` or `PluginTimeout`
- **THEN** the outbox audit handler treats it as retryable, and `Permanent` as a rejection

### Requirement: Usage event model
The SDK SHALL define `UsageEvent` serialized with fields `tenant_id`, `user_id` (omitted for system tasks), `chat_id`, `turn_id` (omitted for system tasks), `request_id`, `effective_model`, `selected_model`, `terminal_state`, `billing_outcome`, `usage` (`input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_write_input_tokens`, `reasoning_tokens`, or `null`), `actual_credits_micro`, `settlement_method`, `policy_version_applied`, `web_search_calls`, `code_interpreter_calls`, `file_search_calls`, `timestamp`, `requester_type`, `dedupe_key`, `system_task_type` (omitted for user turns).

#### Scenario: User turn event omits system fields
- **WHEN** a user-turn usage event is serialized
- **THEN** `system_task_type` is absent and `dedupe_key` is `{tenant_hex}/{turn_hex}/{request_hex}`

### Requirement: Bundled static plugins
The gear crate SHALL register the gears `static-mini-chat-model-policy-plugin` (module `infra::plugins::static_model_policy`) and `static-mini-chat-audit-plugin` (module `infra::plugins::static_audit`). The policy plugin SHALL serve a fixed snapshot (policy version 1) from its `model_catalog` and `kill_switches` config, per-user limits `default_standard_limits` (default daily 100_000_000, monthly 1_000_000_000) and `default_premium_limits` (default daily 50_000_000, monthly 500_000_000), reject unknown top-level keys and any entry whose credit multiplier is outside `1..=10_000_000_000`, and only log published usage. The audit plugin SHALL log events when `enabled` (default true). Both SHALL register a GTS instance with `vendor` (default `constructorfabric`) and `priority` (default 100).

#### Scenario: Static policy serves configured catalog
- **WHEN** the gear asks the static policy plugin for the snapshot
- **THEN** it receives version 1 with the configured catalog and kill switches (all `false` when omitted)
