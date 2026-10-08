//! GTS plugin specs and resource type identifiers.

use toolkit::gts::PluginV1;
use toolkit_gts::gts_type_schema;

/// GTS plugin spec of the model policy plugin.
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~"),
    description = "Mini Chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

/// GTS plugin spec of the audit plugin.
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~"),
    description = "Mini Chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;

/// PEP resource type of chats (and every chat sub-resource).
pub const CHAT_PEP_RESOURCE_TYPE: &str = "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~";
/// PEP resource type of the Models API.
pub const MODEL_PEP_RESOURCE_TYPE: &str =
    "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~";
/// PEP resource type of the quota status API.
pub const USER_QUOTA_PEP_RESOURCE_TYPE: &str =
    "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~";

/// `context.resource_type` values of `not_found` errors.
pub const CHAT_RESOURCE_TYPE: &str = "gts.cf.core.mini_chat.chat.v1~";
pub const MESSAGE_RESOURCE_TYPE: &str = "gts.cf.core.mini_chat.message.v1~";
pub const TURN_RESOURCE_TYPE: &str = "gts.cf.core.mini_chat.turn.v1~";
pub const ATTACHMENT_RESOURCE_TYPE: &str = "gts.cf.core.mini_chat.attachment.v1~";
pub const MODEL_RESOURCE_TYPE: &str = "gts.cf.core.mini_chat.model.v1~";
