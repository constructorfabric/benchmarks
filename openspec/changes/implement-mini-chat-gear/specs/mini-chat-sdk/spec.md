## ADDED Requirements

### Requirement: SDK crate packaging
The workspace SHALL contain the crate `gears/mini-chat/mini-chat-sdk` with package name `cf-gears-mini-chat-sdk` and library name `mini_chat_sdk`; it SHALL be a workspace member and the `cf-gears-mini-chat` gear SHALL depend on it.

#### Scenario: Gear builds against the SDK
- **WHEN** the example server is built with `--features mini-chat,static-authn,static-authz,single-tenant,static-credstore`
- **THEN** the build succeeds and the gear crate uses the plugin traits and models from `mini_chat_sdk`

### Requirement: Model policy plugin contract
The SDK SHALL define the GTS plugin spec `MiniChatModelPolicyPluginSpecV1` and the trait `MiniChatModelPolicyPluginClientV1` with `get_current_policy_version(user_id)`, `get_policy_snapshot(user_id, policy_version)`, `get_user_limits(user_id, policy_version)`, `publish_usage(UsageEvent)` and `check_user_license` (default body returns inactive), plus the models `PolicySnapshot { policy_version, model_catalog, kill_switches }`, `ModelCatalogEntry`, `EstimationBudgets`, `ModelGeneralConfig` (api params, features, tool support, supported endpoints, `max_file_size_mb`), `KillSwitches` (all six switches required), `TierLimits`, `UserLimits` and `UsageEvent`.

#### Scenario: Catalog entry defaults
- **WHEN** a catalog entry omits `enabled`, `multimodal_capabilities`, `estimation_budgets`, `web_search_context_size`, `max_tool_calls` and `preference`
- **THEN** it deserializes with `enabled = false`, an empty capability list, default estimation budgets (4 bytes/token, 100 overhead, 10% margin, 1000 image, 500 tool, 500 web search, 1000 code interpreter, floor 50), `low`, `2` and no preference

#### Scenario: Usage event serialization for system tasks
- **WHEN** a `UsageEvent` for a system task is serialized
- **THEN** `user_id` and `turn_id` are absent (not `null`), `system_task_type` is present, and for user turns `system_task_type` is absent

### Requirement: Audit plugin contract
The SDK SHALL define `MiniChatAuditPluginSpecV1` and `MiniChatAuditPluginClientV1` with a method that accepts audit events (`turn_completed`, `turn_failed`, `turn_retry`, `turn_edit`, `turn_delete`) and returns transient, timeout or permanent errors.

#### Scenario: Audit event types
- **WHEN** a turn mutation audit event is built
- **THEN** it carries `actor_user_id`, `chat_id`, the original and new request ids (retry/edit) or the request id (delete) and a timestamp
