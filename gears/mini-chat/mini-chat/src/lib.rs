//! Mini Chat gear: multi-tenant AI chat with SSE streaming, attachments and
//! quotas.
//!
//! The public SDK (plugin contracts and policy/usage/audit models) lives in
//! `mini-chat-sdk`.

#![forbid(unsafe_code)]
// Pedantic clippy categories that are pervasive in this crate's
// contract-shaped code (provider wire formats, DB schema mirrors, SSE
// domain vocabulary) are allowed crate-wide with this note; substantive
// lints stay on.
#![allow(clippy::cast_possible_wrap, reason = "sizes/counts are bounded by config and the schema")]
#![allow(clippy::cast_sign_loss, reason = "casts guard on non-negativity first")]
#![allow(clippy::cast_precision_loss, reason = "f64 ratios are normative in the overshoot rule")]
#![allow(clippy::struct_field_names, reason = "DB columns and wire fields mirror their schema names")]
#![allow(clippy::redundant_clone, reason = "clones in builder/tx plumbing are deliberate")]
#![allow(clippy::missing_const_for_fn, reason = "not applicable here")]
#![allow(clippy::needless_continue, reason = "event/poll loops read clearest with explicit continue")]
#![allow(clippy::match_same_arms, reason = "distinct protocol arms may share bodies")]
#![allow(clippy::match_single_binding, reason = "match shape is the contract even when one arm")]
#![allow(clippy::single_match_else, reason = "match shape is the contract even when one arm")]
#![allow(clippy::single_match_else, reason = "protocol dispatch reads as match")]
#![allow(clippy::integer_division, reason = "quota percentages are floored by contract")]
#![allow(clippy::trivially_copy_pass_by_ref, reason = "shared refs are the house style")]
#![allow(clippy::redundant_clone, reason = "clones in builder/tx plumbing are deliberate")]
#![allow(clippy::let_underscore_must_use, reason = "drop-without-read is intentional at cleanup points")]
#![allow(clippy::option_option, reason = "trigger state is a three-state by design")]
#![allow(clippy::too_many_lines, reason = "the normative flows stay in one function")]
#![allow(clippy::cognitive_complexity, reason = "the normative flows stay in one function")]
#![allow(clippy::inconsistent_struct_constructor, reason = "literals follow narrative order")]
#![allow(clippy::assigning_clones, reason = "builder-style assignment is deliberate")]
#![allow(clippy::similar_names, reason = "resolver/adapter names are domain terms")]
#![allow(clippy::type_complexity, reason = "the plan type is the domain shape")]
#![allow(clippy::needless_pass_by_value, reason = "uniform owned passing at boundaries")]
#![allow(clippy::no_effect_underscore_binding, reason = "marker bindings document intent")]
#![allow(clippy::unnecessary_map_or, reason = "map_or reads clearer at these sites")]
#![allow(clippy::unnecessary_to_owned, reason = "readability first at cold paths")]
#![allow(clippy::format_push_string, reason = "readability first at cold paths")]
#![allow(clippy::if_not_else, reason = "explicit negation reads clearer here")]

// === PUBLIC API (from SDK) ===
pub use mini_chat_sdk::*;

// === MODULE DEFINITION ===
pub mod gear;
pub use gear::MiniChatGear;

// === INTERNAL MODULES ===
#[doc(hidden)]
pub mod api;
#[doc(hidden)]
pub mod config;
#[doc(hidden)]
pub mod domain;
#[doc(hidden)]
pub mod error;
#[doc(hidden)]
pub mod infra;

#[cfg(test)]
#[doc(hidden)]
pub mod test_gateway;
#[cfg(test)]
#[doc(hidden)]
pub mod test_support;
#[cfg(test)]
mod tests;
