//! Deprecated `estimation_budgets.*` keys (DESIGN B.5.2): parsed, unused, and
//! reported by name when set to a non-default value so init can warn.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

use mini_chat::config::MiniChatConfig;

const CREDENTIALS: &str = "client_credentials:\n  client_id: mini-chat\n  client_secret: s3cr3t\n";

fn load(extra: &str) -> MiniChatConfig {
    serde_saphyr::from_str(&format!("{CREDENTIALS}{extra}")).unwrap()
}

#[test]
fn defaults_report_no_deprecated_keys() {
    assert!(load("").deprecated_estimation_budget_keys().is_empty());
}

#[test]
fn the_floor_is_not_deprecated() {
    let cfg = load("estimation_budgets:\n  minimal_generation_floor: 80\n");
    assert!(cfg.deprecated_estimation_budget_keys().is_empty());
}

#[test]
fn non_default_deprecated_keys_are_reported_by_name() {
    let cfg = load(
        "estimation_budgets:\n  bytes_per_token_conservative: 3\n  image_token_budget: 2000\n  safety_margin_pct: 10\n",
    );
    assert_eq!(
        cfg.deprecated_estimation_budget_keys(),
        vec!["bytes_per_token_conservative", "image_token_budget"]
    );
}

#[test]
fn deprecated_keys_are_not_validated() {
    let cfg = load("estimation_budgets:\n  safety_margin_pct: 0\n  fixed_overhead_tokens: 0\n");
    cfg.validate().unwrap();
}
