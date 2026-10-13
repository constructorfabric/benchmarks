//! GTS plugin specifications of the mini-chat plugin capabilities.
//!
//! Plugin instances register under
//! `gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.<spec>.v1~<instance>`
//! and the gear selects one by vendor and priority.

use toolkit::gts::PluginV1;
use toolkit_gts::gts_type_schema;

/// Model policy plugin specification: policy snapshots (model catalog, kill
/// switches), per-user limits and usage publication.
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~"),
    description = "Mini Chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

/// Audit plugin specification: receives turn and turn-mutation audit events.
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~"),
    description = "Mini Chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;
