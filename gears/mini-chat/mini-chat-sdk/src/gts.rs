//! GTS plugin specifications of the mini-chat plugin contracts.
//!
//! The schemas reach types-registry through the toolkit-gts link-time inventory;
//! plugin gears register instances with `PluginV1::<Spec>::build_registration`.

use toolkit::gts::PluginV1;
use toolkit_gts::gts_type_schema;

/// Model policy plugin specification
/// (`gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_model_policy.plugin.v1~`).
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_model_policy.plugin.v1~"),
    description = "Mini-chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

/// Audit plugin specification
/// (`gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_audit.plugin.v1~`).
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_audit.plugin.v1~"),
    description = "Mini-chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;
