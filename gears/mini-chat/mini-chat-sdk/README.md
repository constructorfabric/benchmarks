# Mini Chat SDK

SDK crate for the `mini-chat` gear (multi-tenant AI chat). It defines the transport-agnostic contracts shared by the gear and its plugins: the model policy snapshot and user-limit models, the usage and audit event payloads, the canonical credit arithmetic (`credits_micro_checked`), the plugin client traits (`MiniChatModelPolicyPluginClientV1`, `MiniChatAuditPluginClientV1`) with their error types, and the GTS plugin specs and resource-type identifiers used for plugin discovery through the types registry.
