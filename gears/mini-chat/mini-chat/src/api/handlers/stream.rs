//! `mini_chat.stream_message`: `POST {prefix}/v1/chats/{id}/messages:stream`.

use std::sync::Arc;

use axum::Extension;
use axum::response::Response;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;
use tracing::Instrument as _;
use uuid::Uuid;

use crate::api::dto::stream::StreamMessageRequest;
use crate::api::sse::into_sse_response;
use crate::api::state::AppServices;
use crate::domain::error::DomainError;

/// Sends a message and streams the answer as SSE (replays a completed turn of the same
/// `request_id`). The setup runs in a spawned task that is awaited, so a turn committed before
/// a client disconnect still gets its provider task (DESIGN 1163).
///
/// # Errors
/// Every setup rejection as a JSON problem (400, 403, 404, 409, 429, 500, 503); no stream is
/// opened then.
pub async fn stream_message(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    extract::Path(chat_id): extract::Path<Uuid>,
    extract::Json(req): extract::Json<StreamMessageRequest>,
) -> ApiResult<Response> {
    let stream = Arc::clone(&svc.stream);
    let setup =
        tokio::spawn(async move { stream.send(&ctx, chat_id, req.into()).await }.in_current_span());
    let events = setup
        .await
        .map_err(|e| DomainError::Internal(format!("stream setup task failed: {e}")))??;
    Ok(into_sse_response(events))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    use crate::domain::quota::{Bucket, Period};
    use crate::test_support::app::{
        NO_KILL_SWITCHES, PREMIUM_LIMITS, STANDARD_LIMITS, TestApp, TestResponse, ctx,
    };
    use crate::test_support::catalog::{no_vision_model, premium_model, test_catalog};
    use crate::test_support::stream::{
        SeedAttachment, answer, create_chat, message_attachments_of, messages_of, provider_calls,
        quota_rows, script_provider, seed_attachment, seed_spent, stream_uri, turns_of,
    };

    async fn post(app: &TestApp, who: &SecurityContext, chat: Uuid, body: Value) -> TestResponse {
        app.call("POST", &stream_uri(chat), who, Some(body)).await
    }

    fn field_violation(res: &TestResponse) -> (&str, &str) {
        let v = &res.json["context"]["field_violations"][0];
        (
            v["field"].as_str().unwrap_or_default(),
            v["reason"].as_str().unwrap_or_default(),
        )
    }

    fn assert_field(res: &TestResponse, status: u16, field: &str, reason: &str) {
        assert_eq!(res.status, status, "{}", res.json);
        assert_eq!(field_violation(res), (field, reason), "{}", res.json);
    }

    fn assert_feature_disabled(res: &TestResponse, subject: &str) {
        assert_eq!(res.status, 400, "{}", res.json);
        let v = &res.json["context"]["violations"][0];
        assert_eq!(v["type"], "FEATURE_DISABLED", "{}", res.json);
        assert_eq!(v["subject"], subject, "{}", res.json);
    }

    /// Nothing of a rejected send survives: no turn, no user message, no attachment link, no
    /// reserve.
    async fn assert_nothing_reserved(app: &TestApp, tenant: Uuid, user: Uuid, chat: Uuid) {
        assert!(turns_of(app, chat).await.is_empty());
        let messages = messages_of(app, chat).await;
        assert!(messages.is_empty(), "{messages:?}");
        let links = message_attachments_of(app, chat).await;
        assert!(links.is_empty(), "{links:?}");
        for row in quota_rows(app, tenant, user).await {
            assert_eq!(row.reserved_credits_micro, 0, "{row:?}");
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one scenario per rejection, each asserting the problem
    async fn preflight_rejections_open_no_stream() {
        let app = TestApp::builder().build().await;
        script_provider(&app, answer(&["never"], 1, 1));
        let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
        let who = ctx(tenant, user);
        let chat = create_chat(&app, &who, None).await;

        let res = post(&app, &who, chat, json!({"content": "  \n\t "})).await;
        assert_field(&res, 400, "content", "EMPTY_CONTENT");

        let dup = Uuid::new_v4();
        let res = post(
            &app,
            &who,
            chat,
            json!({"content": "hi", "attachment_ids": [dup, dup]}),
        )
        .await;
        assert_field(&res, 400, "attachment", "invalid_attachment");

        let stranger = ctx(tenant, Uuid::new_v4());
        let res = post(&app, &stranger, chat, json!({"content": "hi"})).await;
        assert_eq!(res.status, 404, "{}", res.json);

        // The chat model was removed from the catalog.
        let standard_chat = create_chat(&app, &who, Some("gpt-standard")).await;
        app.usage.set_catalog(
            test_catalog()
                .into_iter()
                .filter(|m| m.id != "gpt-standard")
                .collect(),
        );
        let res = post(&app, &who, standard_chat, json!({"content": "hi"})).await;
        assert_field(&res, 400, "model", "INVALID_MODEL");
        app.usage.set_catalog(test_catalog());

        app.usage.set_kill_switches(mini_chat_sdk::KillSwitches {
            disable_web_search: true,
            ..NO_KILL_SWITCHES
        });
        let res = post(
            &app,
            &who,
            chat,
            json!({"content": "hi", "web_search": {"enabled": true}}),
        )
        .await;
        assert_feature_disabled(&res, "web_search");
        app.usage.set_kill_switches(NO_KILL_SWITCHES);

        let mut images = Vec::new();
        for _ in 0..5 {
            images.push(seed_attachment(&app, SeedAttachment::image(tenant, chat, user)).await);
        }
        let res = post(
            &app,
            &who,
            chat,
            json!({"content": "hi", "attachment_ids": images}),
        )
        .await;
        assert_field(&res, 400, "image_count", "TOO_MANY_IMAGES");

        let novision_chat = create_chat(&app, &who, Some("gpt-mini-novision")).await;
        let image = seed_attachment(&app, SeedAttachment::image(tenant, novision_chat, user)).await;
        let res = post(
            &app,
            &who,
            novision_chat,
            json!({"content": "hi", "attachment_ids": [image]}),
        )
        .await;
        assert_field(&res, 400, "content_type", "VISION_NOT_SUPPORTED");

        app.usage.set_kill_switches(mini_chat_sdk::KillSwitches {
            disable_images: true,
            ..NO_KILL_SWITCHES
        });
        let res = post(
            &app,
            &who,
            chat,
            json!({"content": "hi", "attachment_ids": [images[0]]}),
        )
        .await;
        assert_feature_disabled(&res, "images");
        app.usage.set_kill_switches(NO_KILL_SWITCHES);

        let not_ready = seed_attachment(
            &app,
            SeedAttachment {
                status: "uploaded",
                ..SeedAttachment::document(tenant, chat, user)
            },
        )
        .await;
        let other_chat = create_chat(&app, &who, None).await;
        let elsewhere =
            seed_attachment(&app, SeedAttachment::document(tenant, other_chat, user)).await;
        let foreign_upload =
            seed_attachment(&app, SeedAttachment::document(tenant, chat, Uuid::new_v4())).await;
        for bad in [not_ready, elsewhere, foreign_upload] {
            let res = post(
                &app,
                &who,
                chat,
                json!({"content": "hi", "attachment_ids": [bad]}),
            )
            .await;
            assert_field(&res, 400, "attachment", "invalid_attachment");
            assert_nothing_reserved(&app, tenant, user, chat).await;
        }

        let tiny = mini_chat_sdk::TierLimits {
            limit_daily_credits_micro: 1,
            limit_monthly_credits_micro: 1,
        };
        app.usage.set_limits(tiny, tiny);
        let res = post(&app, &who, chat, json!({"content": "hi"})).await;
        assert_quota_exceeded(&res);

        assert!(provider_calls(&app).is_empty());
        assert_nothing_reserved(&app, tenant, user, chat).await;
    }

    fn assert_quota_exceeded(res: &TestResponse) {
        assert_eq!(res.status, 429, "{}", res.json);
        let v = &res.json["context"]["violations"][0];
        assert_eq!(v["subject"], "tokens", "{}", res.json);
        assert_eq!(v["description"], "quota_exceeded", "{}", res.json);
    }

    #[tokio::test]
    async fn spent_quota_rejects_with_429_and_reserves_nothing() {
        let app = TestApp::builder().build().await;
        script_provider(&app, answer(&["never"], 1, 1));
        let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
        let who = ctx(tenant, user);
        let chat = create_chat(&app, &who, Some("gpt-premium")).await;
        // Both tiers spent up to their daily limits (`total` covers the standard tier).
        for (bucket, spent) in [
            (Bucket::Premium, PREMIUM_LIMITS.limit_daily_credits_micro),
            (Bucket::Total, STANDARD_LIMITS.limit_daily_credits_micro),
        ] {
            seed_spent(&app, tenant, user, Period::Daily, bucket, spent).await;
        }

        let res = post(&app, &who, chat, json!({"content": "hi"})).await;

        assert_quota_exceeded(&res);
        assert!(provider_calls(&app).is_empty());
        assert_nothing_reserved(&app, tenant, user, chat).await;
    }

    #[tokio::test]
    async fn input_too_long_counts_utf8_bytes() {
        let mut small = no_vision_model("small");
        small.max_input_tokens = 50;
        small.estimation_budgets.fixed_overhead_tokens = 0;
        small.estimation_budgets.safety_margin_pct = 0;
        assert_eq!(small.estimation_budgets.bytes_per_token_conservative, 4);
        let app = TestApp::builder().catalog(vec![small]).build().await;
        script_provider(&app, answer(&["ok"], 1, 1));
        let who = ctx(Uuid::new_v4(), Uuid::new_v4());
        let chat = create_chat(&app, &who, Some("small")).await;

        // 52 characters, 208 UTF-8 bytes: 52 tokens.
        let res = post(&app, &who, chat, json!({"content": "\u{1F600}".repeat(52)})).await;
        assert_field(&res, 400, "content", "INPUT_TOO_LONG");
        assert!(provider_calls(&app).is_empty());

        // 200 ASCII bytes: exactly 50 tokens.
        let frames = app
            .stream(
                "POST",
                &stream_uri(chat),
                &who,
                json!({"content": "a".repeat(200)}),
            )
            .await
            .expect("200 bytes fit");
        assert_eq!(frames.last().unwrap().event, "done");
    }

    #[tokio::test]
    async fn vision_rejected_after_downgrade() {
        let mut premium = premium_model("gpt-premium");
        premium.preference = Some(mini_chat_sdk::ModelPreference {
            is_default: true,
            sort_order: 0,
        });
        let app = TestApp::builder()
            .catalog(vec![premium, no_vision_model("std-novision")])
            .build()
            .await;
        script_provider(&app, answer(&["never"], 1, 1));
        let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
        let who = ctx(tenant, user);
        let chat = create_chat(&app, &who, Some("gpt-premium")).await;
        seed_spent(
            &app,
            tenant,
            user,
            Period::Daily,
            Bucket::Premium,
            PREMIUM_LIMITS.limit_daily_credits_micro,
        )
        .await;
        let image = seed_attachment(&app, SeedAttachment::image(tenant, chat, user)).await;

        let res = post(
            &app,
            &who,
            chat,
            json!({"content": "what is this?", "attachment_ids": [image]}),
        )
        .await;
        assert_field(&res, 400, "content_type", "VISION_NOT_SUPPORTED");
        assert!(provider_calls(&app).is_empty());
        assert_nothing_reserved(&app, tenant, user, chat).await;
    }
}
