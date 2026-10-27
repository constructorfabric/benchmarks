//! GTS schema definitions for mini-chat plugins.
//!
//! The mini-chat gear defines two plugin extension points. Plugins register
//! instances of these types with the types-registry to be discovered by the
//! gear:
//!
//! - [`MiniChatModelPolicyPluginSpecV1`] — model policy plugins
//!   (`mini-chat-model-policy-plugin`)
//! - [`MiniChatAuditPluginSpecV1`] — audit plugins

use toolkit::gts::PluginV1;
use toolkit_gts::gts_type_schema;

/// GTS type definition for mini-chat model policy plugin instances.
///
/// Each plugin registers an instance of this type with its vendor-specific
/// instance ID. The gear discovers plugins by querying the types-registry
/// for instances matching this schema.
///
/// # Instance ID Format
///
/// ```text
/// gts.cf.toolkit.plugins.plugin.v1~<vendor>.<package>.`mini_chat_model_policy`.plugin.v1~
/// ```
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_model_policy.plugin.v1~"),
    description = "Mini Chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

/// GTS type definition for mini-chat audit plugin instances.
///
/// Each plugin registers an instance of this type with its vendor-specific
/// instance ID. The gear discovers plugins by querying the types-registry
/// for instances matching this schema.
///
/// # Instance ID Format
///
/// ```text
/// gts.cf.toolkit.plugins.plugin.v1~<vendor>.<package>.`mini_chat_audit`.plugin.v1~
/// ```
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_audit.plugin.v1~"),
    description = "Mini Chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;
