//! GTS declarations: plugin specs and resource type ids.

use toolkit::gts::PluginV1;
use toolkit_gts::{gts_id, gts_type_schema};

/// Model-policy plugin specification.
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~"),
    description = "Mini Chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

/// Audit plugin specification.
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~"),
    description = "Mini Chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;

/// PEP resource type of a chat.
pub const CHAT_RESOURCE_TYPE: &str = gts_id!("cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~");
/// PEP resource type of a model (permission-only).
pub const MODEL_RESOURCE_TYPE: &str =
    gts_id!("cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~");
/// PEP resource type of the user quota.
pub const USER_QUOTA_RESOURCE_TYPE: &str =
    gts_id!("cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~");

/// Canonical-error `resource_type` values (ADR-0004).
pub mod error_resource_types {
    use toolkit_gts::gts_id;

    pub const CHAT: &str = gts_id!("cf.core.mini_chat.chat.v1~");
    pub const MESSAGE: &str = gts_id!("cf.core.mini_chat.message.v1~");
    pub const TURN: &str = gts_id!("cf.core.mini_chat.turn.v1~");
    pub const ATTACHMENT: &str = gts_id!("cf.core.mini_chat.attachment.v1~");
    pub const MODEL: &str = gts_id!("cf.core.mini_chat.model.v1~");
}
