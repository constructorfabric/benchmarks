#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use crate::api::rest::dto::ModelTierDto;
use crate::domain::error::resource_types;
use crate::domain::service::chats::test_rows::{env_with_pdp, problem};
use crate::domain::service::test_support::{
    DenyPdp, FailingPdp, TestEnv, TestOptions, ctx_a1, default_catalog, model,
};

#[tokio::test]
async fn lists_enabled_models_in_catalog_order() {
    let env = TestEnv::default_env().await;
    let list = env.services.models.list(&ctx_a1()).await.unwrap();
    let ids: Vec<&str> = list.items.iter().map(|m| m.model_id.as_str()).collect();
    assert_eq!(ids, vec!["gpt-premium", "gpt-standard"]);
    let p = &list.items[0];
    assert_eq!(p.display_name, "GPT-PREMIUM");
    assert_eq!(p.tier, ModelTierDto::Premium);
    assert_eq!(p.multiplier_display, "1x");
    assert_eq!(p.description.as_deref(), Some("gpt-premium description"));
    assert_eq!(p.multimodal_capabilities, vec!["VISION_INPUT".to_owned()]);
    assert_eq!(p.context_window, 128_000);
    assert_eq!(list.items[1].tier, ModelTierDto::Standard);

    let json = serde_json::to_value(&list).unwrap();
    let item = json["items"][0].as_object().unwrap();
    let mut keys: Vec<&str> = item.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "context_window",
            "description",
            "display_name",
            "model_id",
            "multimodal_capabilities",
            "multiplier_display",
            "tier"
        ],
        "no provider / pricing / default fields"
    );
    env.shutdown().await;
}

#[tokio::test]
async fn get_model_and_404_for_disabled_or_unknown() {
    let env = TestEnv::default_env().await;
    let m = env
        .services
        .models
        .get(&ctx_a1(), "gpt-standard")
        .await
        .unwrap();
    assert_eq!(m.model_id, "gpt-standard");
    for id in ["gpt-disabled", "nope", ""] {
        let p = problem(env.services.models.get(&ctx_a1(), id).await.unwrap_err());
        assert_eq!(p["status"], 404);
        assert_eq!(p["context"]["resource_type"], resource_types::MODEL);
    }
    env.shutdown().await;
}

#[tokio::test]
async fn empty_description_is_omitted() {
    let mut catalog = default_catalog();
    let mut m = model("plain", "standard");
    m.description = String::new();
    catalog.push(m);
    let env = TestEnv::new(TestOptions {
        catalog,
        ..Default::default()
    })
    .await;
    let dto = env.services.models.get(&ctx_a1(), "plain").await.unwrap();
    assert!(dto.description.is_none());
    let json = serde_json::to_value(&dto).unwrap();
    assert!(json.get("description").is_none());
    env.shutdown().await;
}

#[tokio::test]
async fn pep_denial_and_failure() {
    let env = env_with_pdp(Arc::new(DenyPdp)).await;
    assert_eq!(
        problem(env.services.models.list(&ctx_a1()).await.unwrap_err())["status"],
        403
    );
    assert_eq!(
        problem(
            env.services
                .models
                .get(&ctx_a1(), "gpt-standard")
                .await
                .unwrap_err()
        )["status"],
        403
    );
    env.shutdown().await;
    let env = env_with_pdp(Arc::new(FailingPdp)).await;
    assert_eq!(
        problem(env.services.models.list(&ctx_a1()).await.unwrap_err())["status"],
        503
    );
    assert_eq!(
        problem(
            env.services
                .models
                .get(&ctx_a1(), "gpt-standard")
                .await
                .unwrap_err()
        )["status"],
        503
    );
    env.shutdown().await;
}
