//! Pure chat-management rules: title validation and chat model resolution.
//!
//! Feature `cpt-cf-mini-chat-feature-chat-management`: algorithms
//! `cpt-cf-mini-chat-algo-chat-management-title-validation` and
//! `cpt-cf-mini-chat-algo-chat-management-model-resolution`; definition of done
//! `cpt-cf-mini-chat-dod-chat-management-create` (title and model rules).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::{ModelCatalogEntry, ModelTier, PolicySnapshot};
use serde_json::json;

use super::{TITLE_MAX_CHARS, resolve_chat_model, validate_title};
use crate::domain::error::DomainError;
use crate::test_support::{entry, entry_json, snapshot};

fn chars(c: char, n: usize) -> String {
    std::iter::repeat_n(c, n).collect()
}

// ── title validation ─────────────────────────────────────────────────────────

#[test]
fn an_absent_title_is_an_untitled_chat() {
    assert_eq!(validate_title(None).unwrap(), None);
}

#[test]
fn a_valid_title_is_stored_trimmed() {
    let cases = vec![
        ("Q3 plan", "Q3 plan"),
        ("  padded  ", "padded"),
        ("\t\n tabbed and newlined \r\n", "tabbed and newlined"),
        ("inner  spaces  stay", "inner  spaces  stay"),
        ("\u{00A0}nbsp\u{3000}", "nbsp"),
        ("x", "x"),
    ];
    for (raw, expected) in cases {
        assert_eq!(
            validate_title(Some(raw)).unwrap().as_deref(),
            Some(expected),
            "{raw:?}"
        );
    }
}

#[test]
fn an_empty_or_whitespace_only_title_is_invalid() {
    for raw in ["", " ", "   \t\r\n  ", "\u{00A0}", "\u{3000}\u{2003}"] {
        assert!(
            matches!(validate_title(Some(raw)), Err(DomainError::InvalidTitle)),
            "{raw:?}"
        );
    }
}

#[test]
fn the_length_limit_is_255_characters_after_trimming() {
    assert_eq!(TITLE_MAX_CHARS, 255);
    let at_limit = chars('a', 255);
    let over_limit = chars('a', 256);
    assert_eq!(
        validate_title(Some(&at_limit)).unwrap().as_deref(),
        Some(at_limit.as_str())
    );
    assert!(matches!(
        validate_title(Some(&over_limit)),
        Err(DomainError::InvalidTitle)
    ));

    // the limit applies to the trimmed value: padding does not count
    let padded = format!("   {at_limit}   ");
    assert_eq!(
        validate_title(Some(&padded)).unwrap().as_deref(),
        Some(at_limit.as_str())
    );
    let padded_over = format!("   {over_limit}   ");
    assert!(matches!(
        validate_title(Some(&padded_over)),
        Err(DomainError::InvalidTitle)
    ));
}

#[test]
fn the_length_is_counted_in_characters_not_bytes() {
    // 255 two-byte characters are 510 bytes and still valid; 256 are not
    let two_byte = chars('\u{00E9}', 255);
    assert_eq!(
        validate_title(Some(&two_byte)).unwrap().as_deref(),
        Some(two_byte.as_str())
    );
    assert!(matches!(
        validate_title(Some(&chars('\u{00E9}', 256))),
        Err(DomainError::InvalidTitle)
    ));
    // a four-byte character counts once
    let four_byte = chars('\u{1F600}', 255);
    assert_eq!(
        validate_title(Some(&four_byte)).unwrap().as_deref(),
        Some(four_byte.as_str())
    );
    assert!(matches!(
        validate_title(Some(&chars('\u{1F600}', 256))),
        Err(DomainError::InvalidTitle)
    ));
}

#[test]
fn a_present_but_empty_title_is_invalid_even_on_create() {
    // only an absent or null title means "untitled"; an explicit empty string is rejected
    assert!(matches!(
        validate_title(Some("")),
        Err(DomainError::InvalidTitle)
    ));
}

// ── model resolution ─────────────────────────────────────────────────────────

fn flagged(id: &str, tier: ModelTier, enabled: bool) -> ModelCatalogEntry {
    entry(
        id,
        tier,
        enabled,
        &json!({ "preference": { "is_default": true, "sort_order": 1 } }),
    )
}

fn plain(id: &str, tier: ModelTier, enabled: bool) -> ModelCatalogEntry {
    entry(id, tier, enabled, &json!({}))
}

fn assert_invalid_model(outcome: Result<String, DomainError>, what: &str) {
    match outcome {
        Err(DomainError::InvalidModel { .. }) => {}
        other => panic!("{what}: {other:?}"),
    }
}

fn catalog() -> PolicySnapshot {
    snapshot(vec![
        plain("gpt-standard", ModelTier::Standard, true),
        flagged("gpt-premium", ModelTier::Premium, true),
        plain("gpt-hidden", ModelTier::Standard, false),
    ])
}

#[test]
fn a_supplied_enabled_model_is_used_as_it_is() {
    let snap = catalog();
    for id in ["gpt-standard", "gpt-premium"] {
        assert_eq!(resolve_chat_model(&snap, Some(id)).unwrap(), id);
    }
}

#[test]
fn a_supplied_disabled_unknown_or_empty_model_is_invalid() {
    let snap = catalog();
    for id in ["gpt-hidden", "never-existed", "", " ", "GPT-STANDARD"] {
        assert_invalid_model(resolve_chat_model(&snap, Some(id)), id);
    }
}

#[test]
fn an_entry_that_omits_enabled_counts_as_disabled() {
    let mut value = entry_json("omits-enabled", "standard", true);
    value.as_object_mut().unwrap().remove("enabled");
    let omitting: ModelCatalogEntry = serde_json::from_value(value).unwrap();
    let snap = snapshot(vec![
        omitting,
        plain("gpt-standard", ModelTier::Standard, true),
    ]);
    assert_invalid_model(resolve_chat_model(&snap, Some("omits-enabled")), "supplied");
    // and it is never the default either
    assert_eq!(resolve_chat_model(&snap, None).unwrap(), "gpt-standard");
}

#[test]
fn an_absent_model_resolves_to_the_enabled_default_entry() {
    // the flagged entry wins over an earlier enabled one, whatever the tier
    assert_eq!(resolve_chat_model(&catalog(), None).unwrap(), "gpt-premium");
}

#[test]
fn the_first_flagged_default_in_catalog_order_wins() {
    let snap = snapshot(vec![
        plain("first", ModelTier::Premium, true),
        flagged("second", ModelTier::Standard, true),
        flagged("third", ModelTier::Premium, true),
    ]);
    assert_eq!(resolve_chat_model(&snap, None).unwrap(), "second");
}

#[test]
fn a_disabled_default_entry_is_skipped() {
    let snap = snapshot(vec![
        flagged("disabled-default", ModelTier::Premium, false),
        plain("enabled-plain", ModelTier::Standard, true),
    ]);
    assert_eq!(resolve_chat_model(&snap, None).unwrap(), "enabled-plain");
}

#[test]
fn without_a_flag_the_first_enabled_entry_is_the_default_whatever_its_tier() {
    let snap = snapshot(vec![
        plain("off", ModelTier::Premium, false),
        plain("standard-first", ModelTier::Standard, true),
        plain("premium-second", ModelTier::Premium, true),
    ]);
    assert_eq!(resolve_chat_model(&snap, None).unwrap(), "standard-first");

    let snap = snapshot(vec![
        plain("premium-first", ModelTier::Premium, true),
        plain("standard-second", ModelTier::Standard, true),
    ]);
    assert_eq!(resolve_chat_model(&snap, None).unwrap(), "premium-first");
}

#[test]
fn a_catalog_without_an_enabled_entry_cannot_resolve_a_default() {
    assert_invalid_model(resolve_chat_model(&snapshot(vec![]), None), "empty catalog");
    let all_disabled = snapshot(vec![
        plain("a", ModelTier::Standard, false),
        flagged("b", ModelTier::Premium, false),
    ]);
    assert_invalid_model(resolve_chat_model(&all_disabled, None), "all disabled");
}
