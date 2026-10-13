//! Bundled plugin gears.
// @cpt-dod:cpt-cf-mini-chat-dod-gear-foundation-bundled-plugins:p1

use toolkit::context::GearCtx;

pub mod static_audit;
pub mod static_model_policy;

/// The raw `config` object of the gear section, or `None` when the gear has no
/// `config` object at all (an absent section uses the plugin defaults, a present
/// one is validated strictly).
pub(crate) fn raw_section(ctx: &GearCtx) -> Option<&serde_json::Value> {
    ctx.config_provider()
        .get_gear_config(ctx.gear_name())
        .and_then(serde_json::Value::as_object)
        .and_then(|section| section.get("config"))
}
