//! `GET {prefix}/v1/models` and `GET {prefix}/v1/models/{id}`.

use std::sync::Arc;

use axum::Extension;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;

use crate::api::dto::models::{ModelDto, ModelListDto};
use crate::api::state::AppServices;

/// `mini_chat.list_models`: enabled models of the caller's policy snapshot, in catalog order.
///
/// # Errors
/// 403 / 503 from the PDP, 500 when the policy plugin fails.
pub async fn list_models(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
) -> ApiResult<Json<ModelListDto>> {
    let items = svc.models.list_visible(&ctx).await?;
    Ok(Json(ModelListDto {
        items: items.into_iter().map(ModelDto::from).collect(),
    }))
}

/// `mini_chat.get_model`: one enabled model; disabled or unknown ids are 404 (model resource).
///
/// # Errors
/// 404 model, 403 / 503 from the PDP, 500 when the policy plugin fails.
pub async fn get_model(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    extract::Path(id): extract::Path<String>,
) -> ApiResult<Json<ModelDto>> {
    let model = svc.models.get_visible(&ctx, &id).await?;
    Ok(Json(model.into()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use crate::test_support::app::{TestApp, ctx};
    use crate::test_support::pdp::PdpMode;

    const MODEL_RESOURCE: &str = "gts.cf.core.mini_chat.model.v1~";

    fn user() -> toolkit_security::SecurityContext {
        ctx(Uuid::new_v4(), Uuid::new_v4())
    }

    #[tokio::test]
    async fn list_shows_only_enabled_without_internal_fields() {
        let app = TestApp::builder().build().await;
        let res = app.call("GET", "/mini-chat/v1/models", &user(), None).await;
        assert_eq!(res.status, 200, "{}", res.json);

        let items = res.json["items"].as_array().expect("items");
        let ids: Vec<&str> = items
            .iter()
            .map(|m| m["model_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["gpt-premium", "gpt-standard", "gpt-mini-novision"]);
        for item in items {
            for key in [
                "provider_id",
                "provider_model_id",
                "input_tokens_credit_multiplier_micro",
                "output_tokens_credit_multiplier_micro",
                "is_default",
                "preference",
                "max_output_tokens",
                "enabled",
            ] {
                assert!(item.get(key).is_none(), "{key} exposed in {item}");
            }
        }
        assert_eq!(
            items[0],
            json!({
                "model_id": "gpt-premium",
                "display_name": "gpt-premium display",
                "tier": "premium",
                "multiplier_display": "",
                "multimodal_capabilities": ["VISION_INPUT"],
                "context_window": 128_000,
            }),
            "empty description is omitted"
        );
        assert_eq!(items[1]["tier"], "standard");
        assert_eq!(items[2]["multimodal_capabilities"], json!([]));
    }

    #[tokio::test]
    async fn get_returns_projection_with_description() {
        let mut model = crate::test_support::catalog::standard_model("gpt-x");
        model.description = "Fast answers".to_owned();
        model.multiplier_display = "1x".to_owned();
        let app = TestApp::builder().catalog(vec![model]).build().await;
        let res = app
            .call("GET", "/mini-chat/v1/models/gpt-x", &user(), None)
            .await;
        assert_eq!(res.status, 200, "{}", res.json);
        assert_eq!(
            res.json,
            json!({
                "model_id": "gpt-x",
                "display_name": "gpt-x display",
                "tier": "standard",
                "multiplier_display": "1x",
                "description": "Fast answers",
                "multimodal_capabilities": ["VISION_INPUT"],
                "context_window": 128_000,
            })
        );
    }

    #[tokio::test]
    async fn get_disabled_or_unknown_is_404_model() {
        let app = TestApp::builder().build().await;
        for id in ["gpt-disabled", "nope"] {
            let res = app
                .call("GET", &format!("/mini-chat/v1/models/{id}"), &user(), None)
                .await;
            assert_eq!(res.status, 404, "{id}: {}", res.json);
            assert_eq!(res.json["context"]["resource_type"], MODEL_RESOURCE, "{id}");
        }
    }

    #[tokio::test]
    async fn policy_snapshot_failure_is_500() {
        let app = TestApp::builder().build().await;
        app.usage.fail_snapshots(true);
        for uri in ["/mini-chat/v1/models", "/mini-chat/v1/models/gpt-standard"] {
            let res = app.call("GET", uri, &user(), None).await;
            assert_eq!(res.status, 500, "{uri}: {}", res.json);
            assert!(res.headers.get("retry-after").is_none(), "{uri}");
        }
    }

    #[tokio::test]
    async fn pdp_deny_is_403_and_failure_503() {
        let app = TestApp::builder().pdp(PdpMode::Deny).build().await;
        for uri in ["/mini-chat/v1/models", "/mini-chat/v1/models/gpt-standard"] {
            let res = app.call("GET", uri, &user(), None).await;
            assert_eq!(res.status, 403, "{uri}: {}", res.json);
            assert_eq!(res.json["context"]["reason"], "AUTHZ_DENIED", "{uri}");
            assert_eq!(
                res.json["context"]["resource_type"], MODEL_RESOURCE,
                "{uri}"
            );
        }

        let app = TestApp::builder().pdp(PdpMode::Fail).build().await;
        for uri in ["/mini-chat/v1/models", "/mini-chat/v1/models/gpt-standard"] {
            let res = app.call("GET", uri, &user(), None).await;
            assert_eq!(res.status, 503, "{uri}: {}", res.json);
            assert_eq!(res.headers["retry-after"], "5", "{uri}");
        }

        // The PDP was asked about the model resource with the list / read actions.
        let actions: Vec<String> = app
            .pdp
            .requests()
            .iter()
            .map(|r| format!("{} {}", r.resource.resource_type, r.action.name))
            .collect();
        assert_eq!(
            actions,
            [
                format!("{} list", mini_chat_sdk::MODEL_RESOURCE_TYPE),
                format!("{} read", mini_chat_sdk::MODEL_RESOURCE_TYPE),
            ]
        );
    }
}
