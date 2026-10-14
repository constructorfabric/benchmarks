//! Image guards (`cpt-cf-mini-chat-algo-message-streaming-image-guards`).

use mini_chat_sdk::KillSwitches;

use crate::domain::error::DomainError;

/// The image guards of one request: no image passes (also on a model without `VISION_INPUT`);
/// with images, `disable_images` is `FeatureDisabled { images }` (checked before the capability),
/// and an effective model without `VISION_INPUT` is `VisionNotSupported`. Images are never
/// dropped and no other model is chosen.
///
/// # Errors
///
/// [`DomainError::FeatureDisabled`] or [`DomainError::VisionNotSupported`].
pub fn check_image_guards(
    image_count: u32,
    kill_switches: &KillSwitches,
    vision_input: bool,
) -> Result<(), DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-image-guards:p1:inst-img-no-images
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-image-guards:p1:inst-img-return-pass-text
    // a text-only request is never rejected, whatever the model or the kill switch
    if image_count == 0 {
        return Ok(());
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-image-guards:p1:inst-img-return-pass-text
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-image-guards:p1:inst-img-no-images
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-image-guards:p1:inst-img-kill-switch
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-image-guards:p1:inst-img-return-disabled
    if kill_switches.disable_images {
        return Err(DomainError::FeatureDisabled { subject: "images" });
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-image-guards:p1:inst-img-return-disabled
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-image-guards:p1:inst-img-kill-switch
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-image-guards:p1:inst-img-no-vision
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-image-guards:p1:inst-img-return-vision
    // the capability of the effective model (after the downgrade cascade), also when the
    // selected model has it
    if !vision_input {
        return Err(DomainError::VisionNotSupported);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-image-guards:p1:inst-img-return-vision
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-image-guards:p1:inst-img-no-vision
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-image-guards:p1:inst-img-no-silent-drop
    // images are never dropped from the request and no other model is chosen: the guard only
    // passes or rejects
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-image-guards:p1:inst-img-no-silent-drop
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-image-guards:p1:inst-img-return-pass
    Ok(())
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-image-guards:p1:inst-img-return-pass
}
