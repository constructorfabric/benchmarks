# mini-chat-sdk Specification

## Purpose
Defines the `mini-chat` gear's sDK crate with plugin contracts and shared policy/usage/audit types (source: `gears/mini-chat/docs/`). Introduced by change `implement-mini-chat-gear`.

## Requirements

### Requirement: SDK crate exists and is a workspace member
The workspace SHALL contain the crate `gears/mini-chat/mini-chat-sdk` with package name `cf-gears-mini-chat-sdk` and library name `mini_chat_sdk`; it SHALL be listed in the root workspace members and the gear crate `cf-gears-mini-chat` SHALL depend on it.

#### Scenario: Server builds with the SDK
- **WHEN** `cargo build --bin cf-gears-example-server --no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore` is run
- **THEN** the build succeeds and `mini_chat_sdk` is compiled as a dependency of `mini_chat`

### Requirement: Model policy plugin contract
The SDK SHALL define `MiniChatModelPolicyPluginClientV1` with operations to get the current policy version, get a `PolicySnapshot` by version, get `UserLimits` for a user and version, publish a `UsageEvent`, and `check_user_license` (default body returns inactive). It SHALL define the GTS plugin spec `MiniChatModelPolicyPluginSpecV1` used to resolve the plugin through types-registry by vendor and priority.

#### Scenario: Snapshot types round-trip
- **WHEN** a `PolicySnapshot` with a `ModelCatalogEntry` (id, provider_model_id, display_name, description, provider_id, tier, enabled, multimodal_capabilities, context_window, max_output_tokens, max_input_tokens, credit multipliers, multiplier_display, estimation_budgets, max_num_results, web_search_context_size, max_tool_calls, general_config, preference, system_prompt, thread_summary_prompt) and `KillSwitches` is serialized to JSON and back
- **THEN** the value is unchanged, `enabled` defaults to `false` when omitted, and a `KillSwitches` JSON missing any switch fails to deserialize

### Requirement: Audit plugin contract
The SDK SHALL define `MiniChatAuditPluginClientV1` (emit turn audit events and turn mutation audit events) with error kinds `Transient`, `Permanent` and `PluginTimeout`, and the GTS plugin spec `MiniChatAuditPluginSpecV1`.

#### Scenario: Audit event serialization
- **WHEN** a `turn_retry` mutation event is serialized
- **THEN** it carries `event_type`, `actor_user_id`, `chat_id`, `original_request_id`, `new_request_id` and `timestamp`

### Requirement: Usage event type
The SDK SHALL define `UsageEvent` with fields `tenant_id`, `user_id` (omitted for system tasks), `chat_id`, `turn_id` (omitted for system tasks), `request_id`, `effective_model`, `selected_model`, `terminal_state`, `billing_outcome`, `usage` (nullable: input/output/cache_read_input/cache_write_input/reasoning tokens), `actual_credits_micro`, `settlement_method`, `policy_version_applied`, `web_search_calls`, `code_interpreter_calls`, `file_search_calls`, `timestamp`, `requester_type`, `dedupe_key`, `system_task_type` (omitted for user turns).

#### Scenario: System task usage event omits user fields
- **WHEN** a thread-summary usage event is serialized
- **THEN** `user_id` and `turn_id` are absent, `requester_type` is `system`, `billing_outcome` is `system_task`, `settlement_method` is `none`
