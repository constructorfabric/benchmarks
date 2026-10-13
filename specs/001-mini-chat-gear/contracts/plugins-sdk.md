# SDK contract (`mini_chat_sdk`)

## Plugin traits (resolved through types-registry by `vendor`, lowest `priority` wins)

```rust
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    async fn get_current_policy_version(&self, user_id: Uuid) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError>;
    async fn get_policy_snapshot(&self, user_id: Uuid, policy_version: u64) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError>;
    async fn get_user_limits(&self, user_id: Uuid, policy_version: u64) -> Result<UserLimits, MiniChatModelPolicyPluginError>;
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError>;
    async fn check_user_license(&self, ...) -> ... { default: active = false }
}

#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    async fn emit_turn_audit(&self, event: TurnAuditEvent) -> Result<(), MiniChatAuditPluginError>;
    async fn emit_turn_retry_audit(&self, event: TurnRetryAuditEvent) -> Result<(), MiniChatAuditPluginError>;
    async fn emit_turn_edit_audit(&self, event: TurnEditAuditEvent) -> Result<(), MiniChatAuditPluginError>;
    async fn emit_turn_delete_audit(&self, event: TurnDeleteAuditEvent) -> Result<(), MiniChatAuditPluginError>;
}
```

Errors: `PublishError::{Transient, Permanent}`; `MiniChatAuditPluginError::{Transient, Permanent,
PluginTimeout}`.

GTS plugin specs: `MiniChatModelPolicyPluginSpecV1`, `MiniChatAuditPluginSpecV1` (schemas registered
by the host gear, instances by the plugin gears with `vendor` + `priority`).

## Policy types

- `PolicySnapshot { policy_version, model_catalog: Vec<ModelCatalogEntry>, kill_switches: KillSwitches }`
- `ModelCatalogEntry` — fields per DESIGN §3 "Model Catalog Configuration" / A.1; `tier`
  accepts `premium|standard` in any case (`Premium` in YAML).
- `EstimationBudgets` (defaults 4 / 100 / 10 / 1000 / 500 / 500 / 1000 / 50).
- `ModelGeneralConfig { type, available_from, max_file_size_mb, api_params, features, tool_support, supported_endpoints }`.
- `ModelToolSupport { web_search, file_search, image_generation, code_interpreter, mcp }`.
- `KillSwitches { disable_premium_tier, force_standard_tier, disable_web_search, disable_file_search, disable_images, disable_code_interpreter }` (all required in the SDK type).
- `UserLimits { user_id, policy_version, standard: TierLimits, premium: TierLimits }`,
  `TierLimits { limit_daily_credits_micro, limit_monthly_credits_micro }`.

## Event types

- `UsageEvent` — DESIGN A.3 payload (user turns and system tasks; `user_id`, `turn_id`,
  `system_task_type` skipped when `None`; `usage` serialized as `null` when unknown).
- `UsageTokens { input_tokens, output_tokens, cache_read_input_tokens, cache_write_input_tokens, reasoning_tokens }`.
- `TurnAuditEvent { event_type: turn_completed|turn_failed, tenant_id, user_id, chat_id, turn_id,
  request_id, selected_model, effective_model, usage, latency_ms, ttft_ms, tool_calls {web_search_calls,
  file_search_calls}, policy_decisions {quota: {decision, downgrade_from, downgrade_reason}}, error_code,
  prompt/response/attachments/license/quota_scope empty, timestamp }`.
- `TurnRetryAuditEvent` / `TurnEditAuditEvent { actor_user_id, chat_id, original_request_id, new_request_id, timestamp, tenant_id }`,
  `TurnDeleteAuditEvent { actor_user_id, chat_id, request_id, timestamp, tenant_id }`.
