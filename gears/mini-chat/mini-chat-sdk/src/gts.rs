//! GTS plugin specs of the mini-chat plugin extension points.
//!
//! Both specs nest under the toolkit plugin base
//! (`gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.<spec>.v1~`). Each
//! `#[gts_type_schema(...)]` struct submits its schema to the link-time
//! inventory that the types-registry loads at boot; the gear resolves plugin
//! instances through `<Spec>::gts_type_id()` and the vendor match.

use toolkit::gts::PluginV1;
use toolkit_gts::gts_type_schema;

/// Plugin spec of the model-policy plugin (`MiniChatModelPolicyPluginClientV1`).
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~"),
    description = "Mini-chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

/// Plugin spec of the audit plugin (`MiniChatAuditPluginClientV1`).
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~"),
    description = "Mini-chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;
