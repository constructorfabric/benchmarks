#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of `cpt-cf-mini-chat-algo-gear-foundation-visible-models` and the
//! domain side of `cpt-cf-mini-chat-flow-gear-foundation-list-models` /
//! `-get-model`.

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::{MiniChatModelPolicyPluginClientV1, ModelTier, PolicyPluginError};
use serde_json::json;

use super::{ModelView, ModelsService};
use crate::domain::authz::MODEL_RESOURCE_TYPE;
use crate::domain::error::{DomainError, ResourceKind};
use crate::test_doubles::{
    Behavior, FakePdp, FakePolicyPlugin, POLICY_VERSION, PdpReply, PluginWorld, USER, entry,
    entry_json, models_service, user_ctx,
};

struct Fixture {
    world: PluginWorld,
    plugin: Arc<FakePolicyPlugin>,
    pdp: Arc<FakePdp>,
    service: Arc<ModelsService>,
}

fn catalog() -> Vec<mini_chat_sdk::ModelCatalogEntry> {
    let mut first = entry_json("model-a", true);
    first["preference"] = json!({ "is_default": false, "sort_order": 30 });
    let second = entry_json("model-b", false);
    let mut third = entry_json("model-c", true);
    third["preference"] = json!({ "is_default": true, "sort_order": 1 });
    third["tier"] = json!("standard");
    third["description"] = json!("");
    third["multiplier_display"] = json!("1x");
    third["multimodal_capabilities"] = json!([]);
    third["context_window"] = json!(32_000);
    let fourth = entry_json("model-d", true);
    [first, second, third, fourth]
        .into_iter()
        .map(entry)
        .collect()
}

fn fixture_with(reply: PdpReply, catalog: Vec<mini_chat_sdk::ModelCatalogEntry>) -> Fixture {
    let plugin = FakePolicyPlugin::with_catalog(catalog);
    let world = PluginWorld::new();
    world.register_policy(Arc::clone(&plugin) as Arc<dyn MiniChatModelPolicyPluginClientV1>);
    let pdp = FakePdp::new(reply);
    let service = models_service(
        &pdp,
        world
            .policy_gateway()
            .with_call_timeout(Duration::from_millis(50)),
    );
    Fixture {
        world,
        plugin,
        pdp,
        service,
    }
}

fn fixture() -> Fixture {
    fixture_with(PdpReply::Allow, catalog())
}

fn ids(views: &[ModelView]) -> Vec<&str> {
    views.iter().map(|view| view.model_id.as_str()).collect()
}

// ---- inst-filter-enabled -----------------------------------------------------------------------

#[tokio::test]
async fn only_enabled_entries_are_visible_in_catalog_order() {
    let fixture = fixture();

    let views = fixture
        .service
        .visible_models(&user_ctx())
        .await
        .expect("visible");

    assert_eq!(ids(&views), vec!["model-a", "model-c", "model-d"]);
}

#[tokio::test]
async fn an_empty_catalog_has_no_visible_models() {
    let fixture = fixture_with(PdpReply::Allow, Vec::new());

    let views = fixture
        .service
        .visible_models(&user_ctx())
        .await
        .expect("visible");

    assert!(views.is_empty());
}

#[tokio::test]
async fn a_catalog_without_enabled_entries_has_no_visible_models() {
    let fixture = fixture_with(
        PdpReply::Allow,
        vec![
            entry(entry_json("off-1", false)),
            entry(entry_json("off-2", false)),
        ],
    );

    let views = fixture
        .service
        .visible_models(&user_ctx())
        .await
        .expect("visible");

    assert!(views.is_empty());
}

// ---- inst-project-fields / inst-non-exposure -----------------------------------------------------

#[tokio::test]
async fn the_projection_carries_the_seven_documented_fields() {
    let fixture = fixture();

    let views = fixture
        .service
        .visible_models(&user_ctx())
        .await
        .expect("visible");

    assert_eq!(
        views[0],
        ModelView {
            model_id: "model-a".to_owned(),
            display_name: "Model model-a".to_owned(),
            tier: ModelTier::Premium,
            multiplier_display: "2x".to_owned(),
            description: Some("About model-a".to_owned()),
            multimodal_capabilities: vec!["VISION_INPUT".to_owned(), "RAG".to_owned()],
            context_window: 128_000,
        }
    );
    assert_eq!(views[1].tier, ModelTier::Standard);
    assert_eq!(views[1].multiplier_display, "1x");
    assert_eq!(views[1].context_window, 32_000);
    assert!(views[1].multimodal_capabilities.is_empty());
}

#[tokio::test]
async fn an_empty_description_is_omitted() {
    let fixture = fixture();

    let views = fixture
        .service
        .visible_models(&user_ctx())
        .await
        .expect("visible");

    assert_eq!(views[1].model_id, "model-c");
    assert_eq!(views[1].description, None);
    assert_eq!(views[0].description.as_deref(), Some("About model-a"));
}

#[test]
fn the_projection_has_no_field_beyond_the_seven() {
    // Exhaustive destructuring: adding a field to `ModelView` (for example
    // `provider_id` or a credit multiplier) breaks this test at compile time.
    let ModelView {
        model_id,
        display_name,
        tier,
        multiplier_display,
        description,
        multimodal_capabilities,
        context_window,
    } = ModelView {
        model_id: "m".to_owned(),
        display_name: "M".to_owned(),
        tier: ModelTier::Standard,
        multiplier_display: "1x".to_owned(),
        description: None,
        multimodal_capabilities: Vec::new(),
        context_window: 1,
    };
    let _ = (
        model_id,
        display_name,
        tier,
        multiplier_display,
        description,
        multimodal_capabilities,
        context_window,
    );
}

#[tokio::test]
async fn no_prompt_or_provider_detail_is_reachable_from_the_projection() {
    let fixture = fixture();

    let views = fixture
        .service
        .visible_models(&user_ctx())
        .await
        .expect("visible");

    assert_eq!(views.len(), 3);
    let rendered = format!("{views:?}");
    for secret in [
        "SECRET-SYSTEM-PROMPT",
        "SECRET-SUMMARY-PROMPT",
        "provider-model-a",
        "openai",
        "OpenAI",
        "icon.svg",
    ] {
        assert!(!rendered.contains(secret), "{secret} leaked: {rendered}");
    }
}

// ---- inst-read-snapshot -----------------------------------------------------------------------------

#[tokio::test]
async fn the_snapshot_is_read_for_the_user_at_the_current_version() {
    let fixture = fixture();

    fixture
        .service
        .visible_models(&user_ctx())
        .await
        .expect("visible");

    assert_eq!(
        fixture.plugin.snapshot_requests.lock().unwrap().clone(),
        vec![(USER, POLICY_VERSION)]
    );
}

#[tokio::test]
async fn nothing_is_cached_between_calls() {
    let fixture = fixture();
    let first = fixture
        .service
        .visible_models(&user_ctx())
        .await
        .expect("first");
    assert_eq!(first.len(), 3);

    *fixture.plugin.catalog.lock().unwrap() = vec![entry(entry_json("only", true))];
    let second = fixture
        .service
        .visible_models(&user_ctx())
        .await
        .expect("second");

    assert_eq!(ids(&second), vec!["only"]);
    assert_eq!(fixture.plugin.snapshot_requests.lock().unwrap().len(), 2);
}

// ---- inst-resolve-catalog-plugin / inst-read-snapshot failures ----------------------------------------

fn assert_policy_error(result: Result<Vec<ModelView>, DomainError>) {
    match result {
        Err(DomainError::PolicyResolution(_)) => {}
        other => panic!("expected PolicyResolution (500 internal), got {other:?}"),
    }
}

#[tokio::test]
async fn an_unregistered_plugin_is_an_internal_error() {
    let pdp = FakePdp::new(PdpReply::Allow);
    let world = PluginWorld::new();
    let service = models_service(&pdp, world.policy_gateway());

    assert_policy_error(service.visible_models(&user_ctx()).await);
}

#[tokio::test]
async fn a_registry_failure_is_an_internal_error() {
    let fixture = fixture();
    fixture.world.registry.fail_list("registry down");

    assert_policy_error(fixture.service.visible_models(&user_ctx()).await);
}

#[tokio::test]
async fn an_instance_without_a_client_is_an_internal_error() {
    let pdp = FakePdp::new(PdpReply::Allow);
    let world = PluginWorld::new();
    world.add_policy_instance();
    let service = models_service(&pdp, world.policy_gateway());

    assert_policy_error(service.visible_models(&user_ctx()).await);
}

#[tokio::test]
async fn a_failing_version_call_is_an_internal_error() {
    let fixture = fixture();
    fixture
        .plugin
        .set_version(Behavior::Fail(PolicyPluginError::Internal(
            "boom".to_owned(),
        )));

    assert_policy_error(fixture.service.visible_models(&user_ctx()).await);
    assert!(fixture.plugin.snapshot_requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_failing_snapshot_call_is_an_internal_error() {
    let fixture = fixture();
    fixture
        .plugin
        .set_snapshot(Behavior::Fail(PolicyPluginError::NotFound(
            "gone".to_owned(),
        )));

    assert_policy_error(fixture.service.visible_models(&user_ctx()).await);
}

#[tokio::test]
async fn a_version_call_that_never_answers_is_an_internal_error() {
    let fixture = fixture();
    fixture.plugin.set_version(Behavior::Hang);

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        fixture.service.visible_models(&user_ctx()),
    )
    .await
    .expect("the plugin call is bounded");

    assert_policy_error(result);
}

#[tokio::test]
async fn a_snapshot_call_that_never_answers_is_an_internal_error() {
    let fixture = fixture();
    fixture.plugin.set_snapshot(Behavior::Hang);

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        fixture.service.visible_models(&user_ctx()),
    )
    .await
    .expect("the plugin call is bounded");

    assert_policy_error(result);
}

// ---- list ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn list_authorizes_the_list_action_on_the_model_resource_decision_only() {
    let fixture = fixture();

    let views = fixture
        .service
        .list_models(&user_ctx())
        .await
        .expect("list");

    assert_eq!(ids(&views), vec!["model-a", "model-c", "model-d"]);
    let requests = fixture.pdp.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].action.name, "list");
    assert_eq!(requests[0].resource.resource_type, MODEL_RESOURCE_TYPE);
    assert_eq!(requests[0].resource.id, None);
    assert!(!requests[0].context.require_constraints);
}

#[tokio::test]
async fn list_is_denied_before_the_catalog_is_read() {
    let fixture = fixture_with(PdpReply::Deny, catalog());

    let result = fixture.service.list_models(&user_ctx()).await;

    assert!(
        matches!(result, Err(DomainError::AuthzDenied)),
        "{result:?}"
    );
    assert!(fixture.plugin.snapshot_requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn list_maps_a_pdp_failure_to_pdp_unavailable_before_the_catalog_is_read() {
    let fixture = fixture_with(PdpReply::Failure, catalog());

    let result = fixture.service.list_models(&user_ctx()).await;

    assert!(
        matches!(result, Err(DomainError::PdpUnavailable)),
        "{result:?}"
    );
    assert!(fixture.plugin.snapshot_requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn list_reports_a_policy_failure_as_an_internal_error() {
    let fixture = fixture();
    fixture
        .plugin
        .set_snapshot(Behavior::Fail(PolicyPluginError::Internal(
            "boom".to_owned(),
        )));

    assert_policy_error(fixture.service.list_models(&user_ctx()).await);
}

// ---- get ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn get_authorizes_the_read_action_on_the_model_resource_decision_only() {
    let fixture = fixture();

    let view = fixture
        .service
        .get_model(&user_ctx(), "model-c")
        .await
        .expect("enabled model");

    assert_eq!(view.model_id, "model-c");
    assert_eq!(view.tier, ModelTier::Standard);
    let requests = fixture.pdp.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].action.name, "read");
    assert_eq!(requests[0].resource.resource_type, MODEL_RESOURCE_TYPE);
    assert_eq!(requests[0].resource.id, None);
    assert!(!requests[0].context.require_constraints);
}

#[tokio::test]
async fn get_of_an_unknown_model_is_not_found_with_the_model_resource() {
    let fixture = fixture();

    let result = fixture
        .service
        .get_model(&user_ctx(), "no-such-model")
        .await;

    assert!(
        matches!(
            result,
            Err(DomainError::NotFound {
                resource: ResourceKind::Model
            })
        ),
        "{result:?}"
    );
}

#[tokio::test]
async fn get_of_a_disabled_model_is_indistinguishable_from_an_unknown_one() {
    let fixture = fixture();

    let disabled = fixture.service.get_model(&user_ctx(), "model-b").await;
    let unknown = fixture.service.get_model(&user_ctx(), "model-z").await;

    assert!(matches!(
        disabled,
        Err(DomainError::NotFound {
            resource: ResourceKind::Model
        })
    ));
    assert_eq!(format!("{disabled:?}"), format!("{unknown:?}"));
}

#[tokio::test]
async fn the_model_id_is_an_opaque_case_sensitive_string() {
    let weird = "Team A/Model:v1 \u{2713}";
    let fixture = fixture_with(
        PdpReply::Allow,
        vec![
            entry(entry_json(weird, true)),
            entry(entry_json("lower", true)),
        ],
    );

    let found = fixture
        .service
        .get_model(&user_ctx(), weird)
        .await
        .expect("found");
    let wrong_case = fixture.service.get_model(&user_ctx(), "LOWER").await;

    assert_eq!(found.model_id, weird);
    assert!(
        matches!(wrong_case, Err(DomainError::NotFound { .. })),
        "{wrong_case:?}"
    );
}

#[tokio::test]
async fn get_is_denied_before_the_catalog_is_read() {
    let fixture = fixture_with(PdpReply::Deny, catalog());

    let result = fixture.service.get_model(&user_ctx(), "model-a").await;

    assert!(
        matches!(result, Err(DomainError::AuthzDenied)),
        "{result:?}"
    );
    assert!(fixture.plugin.snapshot_requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn get_maps_a_pdp_failure_to_pdp_unavailable() {
    let fixture = fixture_with(PdpReply::Failure, catalog());

    let result = fixture.service.get_model(&user_ctx(), "model-a").await;

    assert!(
        matches!(result, Err(DomainError::PdpUnavailable)),
        "{result:?}"
    );
}

#[tokio::test]
async fn get_reports_a_policy_failure_as_an_internal_error() {
    let fixture = fixture();
    fixture.plugin.set_version(Behavior::Hang);

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        fixture.service.get_model(&user_ctx(), "model-a"),
    )
    .await
    .expect("bounded");

    assert!(
        matches!(result, Err(DomainError::PolicyResolution(_))),
        "{result:?}"
    );
}
