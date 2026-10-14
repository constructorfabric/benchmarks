# `mini-chat` SDK

SDK crate for the `mini-chat` gear. It holds the transport-agnostic contracts and no REST types:

- **`MiniChatModelPolicyPluginClientV1`** and **`MiniChatModelPolicyPluginSpecV1`**: the model-policy plugin
  (policy snapshot, user limits, usage publication).
- **`MiniChatAuditPluginClientV1`** and **`MiniChatAuditPluginSpecV1`**: the audit plugin.
- **`PolicySnapshot`**, **`ModelCatalogEntry`**, **`EstimationBudgets`**, **`ModelGeneralConfig`**,
  **`TierLimits`**, **`UserLimits`**, **`KillSwitches`**, **`UsageEvent`**, **`AuditEvent`**: shared types.
- **`ModelPolicyPluginError`**, **`PublishError`**, **`AuditPluginError`**: plugin error types.

Field-level definitions are in the mini-chat DESIGN (section 5.2 and Appendix B.2).
