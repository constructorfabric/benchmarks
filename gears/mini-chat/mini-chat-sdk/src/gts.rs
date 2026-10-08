//! GTS plugin specifications of the mini-chat plugins.
//!
//! Plugin instances register under these types in the types-registry; the
//! gear lists the instances of a type, picks one by vendor and priority and
//! resolves its scoped client from the `ClientHub`.

use toolkit::gts::PluginV1;
use toolkit_gts::gts_type_schema;

/// Model-policy plugin specification (`MiniChatModelPolicyPluginClientV1`).
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy.v1~"),
    description = "Mini-chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

/// Audit plugin specification (`MiniChatAuditPluginClientV1`).
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit.v1~"),
    description = "Mini-chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;
