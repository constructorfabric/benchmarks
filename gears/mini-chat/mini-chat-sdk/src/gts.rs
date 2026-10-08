//! GTS declarations of the mini-chat SDK: plugin specifications and the
//! resource type identifiers used by authorization and canonical errors.

use toolkit::gts::PluginV1;
use toolkit_gts::{gts_id, gts_type_schema};

/// Canonical-error resource type of a chat.
pub const CHAT_RESOURCE_TYPE: &str = gts_id!("cf.core.mini_chat.chat.v1~");
/// Canonical-error resource type of a message.
pub const MESSAGE_RESOURCE_TYPE: &str = gts_id!("cf.core.mini_chat.message.v1~");
/// Canonical-error resource type of a turn.
pub const TURN_RESOURCE_TYPE: &str = gts_id!("cf.core.mini_chat.turn.v1~");
/// Canonical-error resource type of an attachment.
pub const ATTACHMENT_RESOURCE_TYPE: &str = gts_id!("cf.core.mini_chat.attachment.v1~");
/// Canonical-error resource type of a model.
pub const MODEL_RESOURCE_TYPE: &str = gts_id!("cf.core.mini_chat.model.v1~");

/// PEP resource type of the chat (sub-resources inherit its decision).
pub const CHAT_AUTHZ_RESOURCE_TYPE: &str =
    gts_id!("cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~");
/// PEP resource type of the read-only model catalog.
pub const MODEL_AUTHZ_RESOURCE_TYPE: &str =
    gts_id!("cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~");
/// PEP resource type of the per-user quota status.
pub const USER_QUOTA_AUTHZ_RESOURCE_TYPE: &str =
    gts_id!("cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~");

/// Plugin specification of the mini-chat model policy plugin.
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_model_policy.plugin.v1~"),
    description = "Mini Chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

/// Plugin specification of the mini-chat audit plugin.
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_audit.plugin.v1~"),
    description = "Mini Chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;
