//! GTS plugin specifications of the mini-chat gear.
//!
//! The mini-chat gear registers both schemas in types-registry; plugin gears
//! register instances of them and a scoped `ClientHub` client under the
//! instance id.

use toolkit::gts::PluginV1;
use toolkit_gts::gts_type_schema;

/// GTS type of model-policy plugin instances (`mini-chat-model-policy-plugin`).
///
/// The plugin serves the versioned policy snapshot (model catalog, kill
/// switches), the per-user credit limits and receives usage events.
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_model_policy.plugin.v1~"),
    description = "Mini Chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

/// GTS type of audit plugin instances (`mini-chat-audit-plugin`).
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_audit.plugin.v1~"),
    description = "Mini Chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;
