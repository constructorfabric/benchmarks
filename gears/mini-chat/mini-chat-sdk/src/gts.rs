//! GTS (Global Type System) declarations for the mini-chat SDK.
//!
//! **Plugin specs.** Mini-chat plugin specs nest under the toolkit plugin base
//! `gts.cf.toolkit.plugins.plugin.v1~`, like every other platform plugin
//! (credstore, authn/authz/tenant/license resolvers):
//!
//! ```text
//! gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~<vendor>.<package>.<name>.v1
//! gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~<vendor>.<package>.<name>.v1
//! ```
//!
//! Each `#[gts_type_schema(...)]` spec submits its schema to the link-time
//! inventory the types-registry loads at boot. A plugin registers a
//! `PluginV1<Spec>` instance (vendor + priority) and its scoped client under
//! that instance id; the gear lists instances matching
//! `"{Spec::gts_type_id()}*"` and selects one by vendor and priority.
//!
//! **Resource types.** [`resource_types`] holds the GTS resource-type ids of
//! the gear's PEP (DESIGN section 3.8) and canonical error contract
//! (ADR-0004), so the gear and its tests share a single source of truth.

use toolkit_gts::{PluginV1, gts_type_schema};

/// GTS type of mini-chat model policy plugin instances
/// ([`MiniChatModelPolicyPluginClientV1`](crate::MiniChatModelPolicyPluginClientV1)).
///
/// # Instance ID Format
///
/// ```text
/// gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~<vendor>.<package>.<name>.v1
/// ```
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~"),
    description = "Mini Chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

/// GTS type of mini-chat audit plugin instances
/// ([`MiniChatAuditPluginClientV1`](crate::MiniChatAuditPluginClientV1)).
///
/// # Instance ID Format
///
/// ```text
/// gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~<vendor>.<package>.<name>.v1
/// ```
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~"),
    description = "Mini Chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;

/// GTS resource-type ids used by the mini-chat gear.
///
/// - `*_AUTHZ_RESOURCE_TYPE`: PEP resource types (DESIGN section 3.8 "Resource
///   Type", "Model Resource Type", "User Quota Resource Type").
/// - `*_RESOURCE_TYPE`: canonical-error `context.resource_type` values of
///   `not_found` and resource-scoped errors (ADR-0004).
pub mod resource_types {
    use toolkit_gts::gts_id;

    /// PEP resource type of a chat; messages, attachments and thread summaries
    /// are authorized through their parent chat.
    pub const CHAT_AUTHZ_RESOURCE_TYPE: &str =
        gts_id!("cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~");

    /// PEP resource type of the Models API (permission-only, no constraints).
    pub const MODEL_AUTHZ_RESOURCE_TYPE: &str =
        gts_id!("cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~");

    /// PEP resource type of `GET /v1/quota/status` (tenant + owner constraints).
    pub const USER_QUOTA_AUTHZ_RESOURCE_TYPE: &str =
        gts_id!("cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~");

    /// Error resource type of a chat.
    pub const CHAT_RESOURCE_TYPE: &str = gts_id!("cf.core.mini_chat.chat.v1~");

    /// Error resource type of a message.
    pub const MESSAGE_RESOURCE_TYPE: &str = gts_id!("cf.core.mini_chat.message.v1~");

    /// Error resource type of a turn.
    pub const TURN_RESOURCE_TYPE: &str = gts_id!("cf.core.mini_chat.turn.v1~");

    /// Error resource type of an attachment.
    pub const ATTACHMENT_RESOURCE_TYPE: &str = gts_id!("cf.core.mini_chat.attachment.v1~");

    /// Error resource type of a catalog model.
    pub const MODEL_RESOURCE_TYPE: &str = gts_id!("cf.core.mini_chat.model.v1~");
}

#[cfg(test)]
mod tests {
    use super::resource_types;
    use super::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
    use gts::GtsSchema;

    const MODEL_POLICY_SPEC_ID: &str =
        "gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~";
    const AUDIT_SPEC_ID: &str =
        "gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~";

    #[test]
    fn plugin_spec_type_ids_follow_platform_convention() {
        assert_eq!(
            MiniChatModelPolicyPluginSpecV1::TYPE_ID,
            MODEL_POLICY_SPEC_ID
        );
        assert_eq!(MiniChatAuditPluginSpecV1::TYPE_ID, AUDIT_SPEC_ID);
    }

    #[test]
    fn plugin_specs_are_registered_in_the_gts_inventory() {
        let registered =
            toolkit_gts::all_inventory_type_schemas().expect("inventory schemas parse");
        for id in [MODEL_POLICY_SPEC_ID, AUDIT_SPEC_ID] {
            let want = serde_json::json!(format!("gts://{id}"));
            assert!(
                registered.iter().any(|schema| schema["$id"] == want),
                "{id} must be registered"
            );
        }
    }

    #[test]
    fn resource_type_ids_match_design() {
        assert_eq!(
            resource_types::CHAT_AUTHZ_RESOURCE_TYPE,
            "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~"
        );
        assert_eq!(
            resource_types::MODEL_AUTHZ_RESOURCE_TYPE,
            "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~"
        );
        assert_eq!(
            resource_types::USER_QUOTA_AUTHZ_RESOURCE_TYPE,
            "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~"
        );
        assert_eq!(
            resource_types::CHAT_RESOURCE_TYPE,
            "gts.cf.core.mini_chat.chat.v1~"
        );
        assert_eq!(
            resource_types::MESSAGE_RESOURCE_TYPE,
            "gts.cf.core.mini_chat.message.v1~"
        );
        assert_eq!(
            resource_types::TURN_RESOURCE_TYPE,
            "gts.cf.core.mini_chat.turn.v1~"
        );
        assert_eq!(
            resource_types::ATTACHMENT_RESOURCE_TYPE,
            "gts.cf.core.mini_chat.attachment.v1~"
        );
        assert_eq!(
            resource_types::MODEL_RESOURCE_TYPE,
            "gts.cf.core.mini_chat.model.v1~"
        );
    }
}
