# Mini Chat SDK

SDK crate of the `mini-chat` gear. It defines the transport-agnostic contracts the gear
shares with its plugins:

- **`MiniChatModelPolicyPluginClientV1`** - model-policy plugin: current policy version,
  policy snapshot by version, per-user limits, usage publication and the (unused) user
  license check.
- **`MiniChatAuditPluginClientV1`** - audit plugin: turn and turn-mutation audit events.
- **`MiniChatModelPolicyPluginSpecV1`** / **`MiniChatAuditPluginSpecV1`** - GTS plugin
  specifications used to register and discover plugin instances through the types registry.
- **Models** - `PolicySnapshot`, `ModelCatalogEntry`, `EstimationBudgets`,
  `ModelGeneralConfig`, `KillSwitches`, `UserLimits`, `TierLimits`, `UsageEvent` and the audit
  event types.
- **Errors** - `ModelPolicyPluginError`, `PublishError` and `AuditPluginError`.

The nested catalog types (`ModelCatalogEntry`, `EstimationBudgets`, `ModelGeneralConfig`,
`TierLimits`) ignore unknown keys; `KillSwitches` requires every switch so that a snapshot with
a missing or renamed switch fails to deserialize instead of being read as `false`.
