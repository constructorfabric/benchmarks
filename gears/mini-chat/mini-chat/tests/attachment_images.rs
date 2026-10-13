//! Images as model input: `input_image` items of the provider request and the image guards of
//! send, retry and edit (`VISION_NOT_SUPPORTED`, `TOO_MANY_IMAGES`, the `disable_images` kill
//! switch).
//!
//! Source of truth: DESIGN "Internal: Multimodal Input Format (Responses API)" (the user item is
//! `[input_text, input_image{file_id}...]`), 3.6 context assembly step 8 (images are appended to
//! the user message of the current request only; earlier images are never implicitly reused),
//! "Model Capability Constraint (Images)" and 3.6 "Images after a downgrade" (`VISION_INPUT` is
//! judged on the effective model, never silently dropped), 2.2 `disable_images`, 3.3 REST error
//! table (`TOO_MANY_IMAGES` field `image_count`, `invalid_attachment`), 5.5.5 (image surcharge
//! `num_images * image_token_budget`) and 3.9 retry / edit ("same image guards as a new message").

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

mod attachment_support;
mod chat_support;
mod common;
mod quota_support;
mod stream_support;

use std::collections::BTreeSet;

use attachment_support::{
    AttRow, AttachmentHarness, AttachmentOptions, LimitsSpec, PREMIUM, STANDARD, all_image_parts,
    assert_problem, find_model, image_file_ids, last_user_content, quota_models, ref_reserve,
};
use chat_support::{assert_field_violation, assert_precondition};
use serde_json::json;
use stream_support::{assert_400_reason, contains_provider_identifier};
use uuid::Uuid;

fn premium_reserve(text: &str, surcharge: u64) -> i64 {
    let reserve = ref_reserve(
        &find_model(&quota_models(), PREMIUM),
        32_768,
        text,
        0,
        surcharge,
    );
    i64::try_from(reserve.2).unwrap()
}

async fn images(h: &AttachmentHarness, chat: Uuid, n: usize) -> Vec<Uuid> {
    let mut ids = Vec::new();
    for i in 0..n {
        ids.push(h.image(chat, &format!("photo{i}.png")).await);
    }
    ids
}

async fn files_of(h: &AttachmentHarness, ids: &[Uuid]) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    for id in ids {
        files.insert(h.provider_file_of(*id).await);
    }
    files
}

fn assert_vision_rejected(resp: &common::Resp) {
    let problem = assert_400_reason(resp, "invalid_argument", "VISION_NOT_SUPPORTED");
    assert_field_violation(&problem, "content_type", "VISION_NOT_SUPPORTED");
}

// ═════════════════════════════════════════════════════════════════════════════
// the provider request
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn an_image_follows_the_text_as_an_input_image_with_its_provider_file_id() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let image = h.image(chat, "photo.png").await;
    let file = h.provider_file_of(image).await;

    let sent = h.say_with(chat, "What is in this image?", &[image]).await;
    assert_eq!(sent.terminal().event, "done", "{:?}", sent.names());
    let request = h.oagw.last_request();
    assert_eq!(
        last_user_content(&request),
        vec![
            json!({"type": "input_text", "text": "What is in this image?"}),
            json!({"type": "input_image", "file_id": file}),
        ],
        "[input_text, input_image{{file_id}}] of DESIGN \"Multimodal Input Format\""
    );
    assert!(
        request.tool_types().is_empty(),
        "an image is not a retrieval document"
    );
    assert!(
        !contains_provider_identifier(&sent.blob()),
        "the provider file id never reaches the client: {}",
        sent.blob()
    );
    assert!(h.stores_of(chat).await.is_empty());
}

#[tokio::test]
async fn several_images_are_appended_as_further_input_images() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let ids = images(&h, chat, 3).await;
    let expected = files_of(&h, &ids).await;

    let sent = h.say_with(chat, "compare them", &ids).await;
    assert_eq!(sent.terminal().event, "done");
    let request = h.oagw.last_request();
    let parts = last_user_content(&request);
    assert_eq!(parts.len(), 4, "text + three images: {parts:?}");
    assert_eq!(
        parts[0],
        json!({"type": "input_text", "text": "compare them"})
    );
    assert_eq!(
        image_file_ids(&request)
            .into_iter()
            .collect::<BTreeSet<_>>(),
        expected
    );
}

#[tokio::test]
async fn only_the_image_of_a_mixed_attachment_list_becomes_model_input() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let doc = h.doc(chat, "doc.pdf").await;
    let image = h.image(chat, "photo.png").await;
    let image_file = h.provider_file_of(image).await;
    let doc_file = h.provider_file_of(doc).await;

    let sent = h.say_with(chat, "use both", &[doc, image]).await;
    assert_eq!(sent.terminal().event, "done");
    let request = h.oagw.last_request();
    assert_eq!(image_file_ids(&request), vec![image_file]);
    assert_eq!(last_user_content(&request).len(), 2);
    assert!(!request.body.to_string().contains(&doc_file));
    assert!(
        request.tool("file_search").is_some(),
        "the document is searched, not sent"
    );
}

#[tokio::test]
async fn images_of_earlier_turns_are_never_reused_implicitly() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let image = h.image(chat, "photo.png").await;
    h.say_with(chat, "what is this?", &[image]).await;

    let sent = h.say(chat, "and what colour is it?").await;
    assert_eq!(sent.terminal().event, "done");
    let request = h.oagw.last_request();
    assert!(
        all_image_parts(&request).is_empty(),
        "no input_image anywhere: {}",
        request.body["input"]
    );
    // the history keeps the earlier question as text
    assert!(
        request
            .conversation_texts()
            .iter()
            .any(|t| t == "user:what is this?"),
        "{:?}",
        request.conversation_texts()
    );
}

#[tokio::test]
async fn an_image_can_be_attached_again_to_a_later_message() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let image = h.image(chat, "photo.png").await;
    let file = h.provider_file_of(image).await;
    h.say_with(chat, "first", &[image]).await;

    let sent = h.say_with(chat, "look again", &[image]).await;
    assert_eq!(sent.terminal().event, "done");
    let request = h.oagw.last_request();
    assert_eq!(
        image_file_ids(&request),
        vec![file],
        "only the explicit re-attachment counts"
    );
    assert_eq!(
        all_image_parts(&request).len(),
        1,
        "the earlier turn's image is not repeated"
    );
}

#[tokio::test]
async fn images_are_charged_per_image_in_the_reserve() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let ids = images(&h, chat, 2).await;

    let sent = h.say_with(chat, "look", &ids).await;
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert_eq!(
        turn.reserve_tokens,
        Some(premium_reserve("look", 2 * 1000)),
        "image_surcharge_tokens = num_images * image_token_budget"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// capability guard
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn an_image_on_a_model_without_vision_is_400_vision_not_supported_before_any_provider_call() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat_on(STANDARD).await;
    // the upload of an image to this chat is refused already; a ready image row can still exist
    let image = h.seed(AttRow::image(chat, "old.png")).await;

    let before = h.row_counts().await;
    let resp = h.reject_with(chat, "what is this?", &[image]).await;
    assert_vision_rejected(&resp);
    assert_eq!(h.row_counts().await, before, "nothing is stored");
    h.oagw.assert_no_provider_call();
}

#[tokio::test]
async fn the_vision_check_is_made_on_the_effective_model_after_a_kill_switch_downgrade() {
    for switch in ["force_standard_tier", "disable_premium_tier"] {
        let h = AttachmentHarness::new().await;
        let chat = h.seed_chat().await;
        let image = h.image(chat, "photo.png").await;
        h.policy.set_kill_switches(json!({ switch: true }));

        let before = h.row_counts().await;
        let resp = h.reject_with(chat, "what is this?", &[image]).await;
        assert_vision_rejected(&resp);
        assert_eq!(h.row_counts().await, before, "{switch}");
        h.oagw.assert_no_provider_call();

        // the same message without the image is served by the standard model
        let sent = h.say(chat, "no image this time").await;
        assert_eq!(sent.terminal().event, "done", "{switch}");
        assert_eq!(
            h.oagw.last_request().model(),
            stream_support::provider_model(STANDARD)
        );
    }
}

#[tokio::test]
async fn the_vision_check_follows_a_quota_downgrade() {
    // premium credits exhausted from the start: the cascade lands on the standard model
    let h = AttachmentHarness::with(AttachmentOptions {
        limits: LimitsSpec::new(1_000_000_000_000, 10_000_000_000_000, 10, 10),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let image = h.image(chat, "photo.png").await;

    let resp = h.reject_with(chat, "what is this?", &[image]).await;
    assert_vision_rejected(&resp);
    h.oagw.assert_no_provider_call();
    // an image-free message of the same chat is downgraded and served
    let sent = h.say(chat, "plain").await;
    assert_eq!(sent.terminal().event, "done");
    assert_eq!(
        h.oagw.last_request().model(),
        stream_support::provider_model(STANDARD)
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// image count and kill switch
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn more_images_than_rag_max_images_per_message_is_400_too_many_images() {
    let h = AttachmentHarness::with_config(json!({"rag": {"max_images_per_message": 2}})).await;
    let chat = h.seed_chat().await;
    let ids = images(&h, chat, 3).await;

    let ok = h.say_with(chat, "two are fine", &ids[..2]).await;
    assert_eq!(ok.terminal().event, "done");

    let before = h.row_counts().await;
    let resp = h.reject_with(chat, "three are not", &ids).await;
    let problem = assert_problem(&resp, 400, "out_of_range");
    assert_field_violation(&problem, "image_count", "TOO_MANY_IMAGES");
    assert_eq!(
        h.row_counts().await,
        before,
        "a rejection leaves nothing behind"
    );
    assert_eq!(h.oagw.request_count(), 1, "no second provider call");
}

#[tokio::test]
async fn the_default_image_limit_is_four_and_documents_do_not_count() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let ids = images(&h, chat, 5).await;
    let docs = vec![h.doc(chat, "a.pdf").await, h.doc(chat, "b.pdf").await];

    let mut with_docs: Vec<Uuid> = ids[..4].to_vec();
    with_docs.extend(&docs);
    let ok = h
        .say_with(chat, "four images and two documents", &with_docs)
        .await;
    assert_eq!(ok.terminal().event, "done", "{:?}", ok.names());
    assert_eq!(all_image_parts(&h.oagw.last_request()).len(), 4);

    let resp = h.reject_with(chat, "five images", &ids).await;
    let problem = assert_problem(&resp, 400, "out_of_range");
    assert_field_violation(&problem, "image_count", "TOO_MANY_IMAGES");
}

#[tokio::test]
async fn more_attachment_ids_than_documents_plus_images_allow_is_invalid_attachment() {
    let h = AttachmentHarness::with_config(
        json!({"rag": {"max_documents_per_chat": 1, "max_images_per_message": 1}}),
    )
    .await;
    let chat = h.seed_chat().await;
    let doc = h.doc(chat, "a.pdf").await;
    let image = h.image(chat, "a.png").await;
    let extra = h.seed(AttRow::document(chat, "b.pdf")).await;

    let resp = h.reject_with(chat, "q", &[doc, image, extra]).await;
    let problem = assert_problem(&resp, 400, "invalid_argument");
    assert_field_violation(&problem, "attachment", "invalid_attachment");
    h.oagw.assert_no_provider_call();
}

#[tokio::test]
async fn an_image_message_is_refused_while_disable_images_is_on() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let image = h.image(chat, "photo.png").await;
    let doc = h.doc(chat, "doc.pdf").await;
    h.policy.set_kill_switches(json!({"disable_images": true}));

    let before = h.row_counts().await;
    let resp = h.reject_with(chat, "what is this?", &[image]).await;
    let problem = assert_problem(&resp, 400, "failed_precondition");
    assert_precondition(&problem, "images", "FEATURE_DISABLED");
    assert_eq!(
        h.row_counts().await,
        before,
        "the stream is rejected before it opens"
    );
    h.oagw.assert_no_provider_call();

    // documents and plain text are not affected
    let sent = h.say_with(chat, "about the document", &[doc]).await;
    assert_eq!(sent.terminal().event, "done");
}

// ═════════════════════════════════════════════════════════════════════════════
// retry and edit apply the same guards to the carried-forward images
// ═════════════════════════════════════════════════════════════════════════════

async fn turn_with_image(h: &AttachmentHarness) -> (Uuid, Uuid, Uuid) {
    let chat = h.seed_chat().await;
    let image = h.image(chat, "photo.png").await;
    let sent = h.say_with(chat, "what is this?", &[image]).await;
    assert_eq!(sent.terminal().event, "done");
    (chat, sent.request_id(), image)
}

async fn assert_turn_untouched(
    h: &AttachmentHarness,
    request_id: Uuid,
    before: (usize, usize, usize, usize),
) {
    let turn = h.turn_of(request_id).await.unwrap();
    assert!(
        turn.deleted_at.is_none() && turn.replaced_by_request_id.is_none(),
        "the turn is not replaced"
    );
    assert_eq!(
        h.row_counts().await,
        before,
        "a rejected mutation changes nothing"
    );
}

#[tokio::test]
async fn retry_and_edit_are_refused_when_the_effective_model_lost_vision() {
    let h = AttachmentHarness::new().await;
    let (chat, request_id, _image) = turn_with_image(&h).await;
    h.policy
        .set_kill_switches(json!({"force_standard_tier": true}));
    let before = h.row_counts().await;

    let retry = h.reject_retry(chat, request_id).await;
    assert_vision_rejected(&retry);
    assert_turn_untouched(&h, request_id, before).await;
    let edit = h.reject_edit(chat, request_id, "describe it").await;
    assert_vision_rejected(&edit);
    assert_turn_untouched(&h, request_id, before).await;
    assert_eq!(
        h.oagw.request_count(),
        1,
        "no provider call for the rejected mutations"
    );
}

#[tokio::test]
async fn retry_and_edit_are_refused_while_disable_images_is_on() {
    let h = AttachmentHarness::new().await;
    let (chat, request_id, _image) = turn_with_image(&h).await;
    h.policy.set_kill_switches(json!({"disable_images": true}));
    let before = h.row_counts().await;

    for resp in [
        h.reject_retry(chat, request_id).await,
        h.reject_edit(chat, request_id, "describe it").await,
    ] {
        let problem = assert_problem(&resp, 400, "failed_precondition");
        assert_precondition(&problem, "images", "FEATURE_DISABLED");
        assert_turn_untouched(&h, request_id, before).await;
    }
    assert_eq!(h.oagw.request_count(), 1);
}

#[tokio::test]
async fn a_turn_without_images_is_not_blocked_by_the_image_switches() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let first = h.say(chat, "plain").await;
    h.policy.set_kill_switches(json!({"disable_images": true}));

    let retried = h.retry(chat, first.request_id()).await;
    assert_eq!(retried.terminal().event, "done", "{:?}", retried.names());
}

#[tokio::test]
async fn a_retry_with_a_working_vision_model_resends_the_image() {
    let h = AttachmentHarness::new().await;
    let (chat, request_id, image) = turn_with_image(&h).await;
    let file = h.provider_file_of(image).await;

    let retried = h.retry(chat, request_id).await;
    assert_eq!(retried.terminal().event, "done", "{:?}", retried.names());
    assert_eq!(image_file_ids(&h.oagw.last_request()), vec![file]);
}
