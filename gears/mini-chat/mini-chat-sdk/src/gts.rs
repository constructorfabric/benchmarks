//! GTS plugin specs and resource-type identifiers of the mini-chat gear.

use toolkit::gts::PluginV1;
use toolkit_gts::{gts_id, gts_type_schema};

/// GTS type of model policy plugin instances.
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~"),
    description = "Mini-Chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

/// GTS type of audit plugin instances.
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~"),
    description = "Mini-Chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;

/// Authorization resource type of a chat.
pub const CHAT_RESOURCE_TYPE: &str = gts_id!("cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~");
/// Authorization resource type of a model.
pub const MODEL_RESOURCE_TYPE: &str =
    gts_id!("cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~");
/// Authorization resource type of a user quota.
pub const USER_QUOTA_RESOURCE_TYPE: &str =
    gts_id!("cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~");
