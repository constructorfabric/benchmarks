//! GTS type definitions for mini-chat plugin instances.
//!
//! Plugins register a `PluginV1<Spec>` instance with types-registry; the gear
//! discovers it by listing instances of the spec type and selecting by vendor.
//!
//! Instance id format:
//! `gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.<kind>_plugin.v1~<vendor>.<pkg>.<name>.plugin.v1`

use toolkit::gts::PluginV1;
use toolkit_gts::gts_type_schema;

/// GTS type of model-policy plugin instances.
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~"),
    description = "Mini Chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

/// GTS type of audit plugin instances.
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~"),
    description = "Mini Chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;
