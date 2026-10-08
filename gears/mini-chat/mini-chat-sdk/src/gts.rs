//! GTS declarations for the mini-chat plugin specs.
//!
//! Plugin specs nest under the toolkit base,
//! `gts.cf.toolkit.plugins.plugin.v1~cf.core.<spec>.plugin.v1~`. Each
//! `#[gts_type_schema]` struct submits its schema to the link-time inventory the
//! types-registry loads at boot.

use toolkit::gts::PluginV1;
use toolkit_gts::gts_type_schema;

#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_model_policy.plugin.v1~"),
    description = "Mini-chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_audit.plugin.v1~"),
    description = "Mini-chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;

#[cfg(test)]
mod tests {
    use super::*;
    use ::gts::GtsSchema;

    #[test]
    fn plugin_spec_type_ids() {
        assert_eq!(
            <MiniChatModelPolicyPluginSpecV1 as GtsSchema>::TYPE_ID,
            "gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_model_policy.plugin.v1~"
        );
        assert_eq!(
            <MiniChatAuditPluginSpecV1 as GtsSchema>::TYPE_ID,
            "gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_audit.plugin.v1~"
        );
    }
}
