//! Image guards (slice S1-setup-path of `cpt-cf-mini-chat-feature-message-streaming`): the
//! kill switch, the vision capability of the effective model and the text-only pass.
//!
//! Algorithm `cpt-cf-mini-chat-algo-message-streaming-image-guards`; constraint
//! `cpt-cf-mini-chat-constraint-model-image-capability`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::KillSwitches;

use super::check_image_guards;
use crate::domain::error::DomainError;

fn switches(disable_images: bool) -> KillSwitches {
    KillSwitches {
        disable_images,
        ..KillSwitches::default()
    }
}

#[test]
fn a_text_only_request_passes_on_any_model() {
    for vision in [true, false] {
        assert!(check_image_guards(0, &switches(false), vision).is_ok());
    }
}

#[test]
fn a_text_only_request_passes_even_when_images_are_disabled() {
    assert!(check_image_guards(0, &switches(true), false).is_ok());
    assert!(check_image_guards(0, &switches(true), true).is_ok());
}

#[test]
fn images_on_a_vision_model_pass() {
    assert!(check_image_guards(1, &switches(false), true).is_ok());
    assert!(check_image_guards(4, &switches(false), true).is_ok());
}

#[test]
fn images_while_the_kill_switch_is_on_are_feature_disabled() {
    for vision in [true, false] {
        let result = check_image_guards(1, &switches(true), vision);
        assert!(
            matches!(
                result,
                Err(DomainError::FeatureDisabled { subject: "images" })
            ),
            "vision={vision}: {result:?}"
        );
    }
}

#[test]
fn images_on_a_model_without_vision_are_not_supported() {
    let result = check_image_guards(2, &switches(false), false);
    assert!(
        matches!(result, Err(DomainError::VisionNotSupported)),
        "{result:?}"
    );
}

#[test]
fn the_kill_switch_is_checked_before_the_capability() {
    // both apply: the kill switch wins
    let result = check_image_guards(1, &switches(true), false);
    assert!(
        matches!(result, Err(DomainError::FeatureDisabled { .. })),
        "{result:?}"
    );
}
