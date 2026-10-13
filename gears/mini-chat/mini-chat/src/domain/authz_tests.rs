use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use authz_resolver_sdk::constraints::{Constraint, InPredicate, Predicate};
use authz_resolver_sdk::models::{
    EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::{PlatformSecurityContext, SecurityContext, pep_properties};
use uuid::Uuid;

use super::*;
use crate::domain::error::DomainError;
use crate::domain::ports::{AuthzPort, ChatAction};

const CHAT_TYPE: &str = "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~";
const MODEL_TYPE: &str = "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~";
const QUOTA_TYPE: &str = "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~";

#[derive(Clone, Copy)]
enum Mode {
    /// What the static authz plugin does: `in(owner_tenant_id, [tenant])` only.
    InTenant,
    /// Permit without any constraint.
    NoConstraints,
    Deny,
    Unavailable,
}

struct FakePdp {
    mode: Mode,
    seen: Mutex<Vec<EvaluationRequest>>,
}

#[async_trait]
impl AuthZResolverApi for FakePdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        let tenant = request
            .subject
            .properties
            .get("tenant_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            .expect("tenant_id");
        self.seen.lock().expect("lock").push(request);
        match self.mode {
            Mode::InTenant => Ok(EvaluationResponse {
                decision: true,
                context: EvaluationResponseContext {
                    constraints: vec![Constraint {
                        predicates: vec![Predicate::In(InPredicate::new(
                            pep_properties::OWNER_TENANT_ID,
                            [tenant],
                        ))],
                    }],
                    ..Default::default()
                },
            }),
            Mode::NoConstraints => Ok(EvaluationResponse {
                decision: true,
                context: EvaluationResponseContext::default(),
            }),
            Mode::Deny => Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            }),
            Mode::Unavailable => Err(CanonicalError::service_unavailable().create()),
        }
    }
}

fn setup(mode: Mode) -> (PolicyEnforcerAuthz, Arc<FakePdp>, SecurityContext) {
    let pdp = Arc::new(FakePdp {
        mode,
        seen: Mutex::new(Vec::new()),
    });
    let authz = PolicyEnforcerAuthz::new(PolicyEnforcer::new(pdp.clone()));
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::from_u128(0xA11CE))
        .subject_tenant_id(Uuid::from_u128(0x7E7A))
        .build()
        .expect("ctx");
    (authz, pdp, ctx)
}

fn last_request(pdp: &FakePdp) -> EvaluationRequest {
    pdp.seen
        .lock()
        .expect("lock")
        .last()
        .cloned()
        .expect("a PDP request")
}

#[tokio::test]
async fn chat_scope_contains_tenant_and_owner_predicates() {
    let (authz, _pdp, ctx) = setup(Mode::InTenant);
    let scope = authz
        .chat_scope(&ctx, ChatAction::Read, Some(Uuid::from_u128(1)))
        .await
        .expect("scope");
    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        vec![ctx.subject_tenant_id()]
    );
    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_ID),
        vec![ctx.subject_id()],
        "the owner predicate comes from ensure_owner, the PDP never emits it"
    );
}

#[tokio::test]
async fn chat_scope_does_not_cover_another_owner() {
    let (authz, _pdp, ctx) = setup(Mode::InTenant);
    let scope = authz
        .chat_scope(&ctx, ChatAction::List, None)
        .await
        .expect("scope");
    assert!(!scope.contains_uuid(pep_properties::OWNER_ID, Uuid::from_u128(0xB0B)));
}

#[tokio::test]
async fn every_chat_action_is_sent_by_name_and_owner_scoped() {
    for action in ChatAction::ALL {
        let (authz, pdp, ctx) = setup(Mode::InTenant);
        let chat_id = Uuid::from_u128(5);
        let scope = authz
            .chat_scope(&ctx, *action, Some(chat_id))
            .await
            .expect("scope");
        let req = last_request(&pdp);
        assert_eq!(req.action.name, action.as_str());
        assert_eq!(req.resource.resource_type, CHAT_TYPE);
        assert_eq!(req.resource.id, Some(chat_id));
        assert!(req.context.require_constraints);
        assert_eq!(
            req.context.supported_properties,
            ["owner_tenant_id", "owner_id", "id"]
        );
        assert_eq!(
            scope.all_uuid_values_for(pep_properties::OWNER_ID),
            vec![ctx.subject_id()],
            "{action:?}"
        );
    }
}

#[test]
fn chat_action_names_match_the_design_matrix() {
    let names: Vec<&str> = ChatAction::ALL.iter().map(|a| a.as_str()).collect();
    assert_eq!(
        names,
        [
            "create",
            "list",
            "read",
            "update",
            "delete",
            "list_messages",
            "send_message",
            "upload_attachment",
            "read_attachment",
            "delete_attachment",
            "read_turn",
            "retry_turn",
            "edit_turn",
            "delete_turn",
            "set_reaction",
            "delete_reaction",
        ]
    );
}

#[tokio::test]
async fn create_passes_owner_properties_without_resource_id() {
    let (authz, pdp, ctx) = setup(Mode::InTenant);
    authz
        .chat_scope(&ctx, ChatAction::Create, None)
        .await
        .expect("scope");
    let req = last_request(&pdp);
    assert_eq!(req.action.name, "create");
    assert_eq!(req.resource.id, None);
    assert_eq!(
        req.resource.properties.get("owner_tenant_id"),
        Some(&serde_json::json!(ctx.subject_tenant_id().to_string()))
    );
    assert_eq!(
        req.resource.properties.get("owner_id"),
        Some(&serde_json::json!(ctx.subject_id().to_string()))
    );
}

#[tokio::test]
async fn chat_scope_denied_is_authz_denied() {
    let (authz, _pdp, ctx) = setup(Mode::Deny);
    let err = authz
        .chat_scope(&ctx, ChatAction::Read, Some(Uuid::from_u128(1)))
        .await
        .expect_err("denied");
    assert!(matches!(err, DomainError::AuthzDenied), "{err:?}");
}

#[tokio::test]
async fn chat_scope_without_constraints_fails_closed_as_denied() {
    let (authz, _pdp, ctx) = setup(Mode::NoConstraints);
    let err = authz
        .chat_scope(&ctx, ChatAction::Read, Some(Uuid::from_u128(1)))
        .await
        .expect_err("compile failure");
    assert!(matches!(err, DomainError::AuthzDenied), "{err:?}");
}

#[tokio::test]
async fn chat_scope_evaluation_failure_is_authz_unavailable() {
    let (authz, _pdp, ctx) = setup(Mode::Unavailable);
    let err = authz
        .chat_scope(&ctx, ChatAction::Read, Some(Uuid::from_u128(1)))
        .await
        .expect_err("unavailable");
    assert!(matches!(err, DomainError::AuthzUnavailable), "{err:?}");
}

#[tokio::test]
async fn model_access_is_a_decision_only_check() {
    let (authz, pdp, ctx) = setup(Mode::NoConstraints);
    authz.model_access(&ctx, "list").await.expect("permitted");
    let req = last_request(&pdp);
    assert_eq!(req.action.name, "list");
    assert_eq!(req.resource.resource_type, MODEL_TYPE);
    assert!(!req.context.require_constraints);
    assert!(req.context.supported_properties.is_empty());
}

#[tokio::test]
async fn model_access_denied_and_unavailable_are_mapped() {
    let (authz, _pdp, ctx) = setup(Mode::Deny);
    let err = authz.model_access(&ctx, "read").await.expect_err("denied");
    assert!(matches!(err, DomainError::AuthzDenied), "{err:?}");

    let (authz, _pdp, ctx) = setup(Mode::Unavailable);
    let err = authz
        .model_access(&ctx, "read")
        .await
        .expect_err("unavailable");
    assert!(matches!(err, DomainError::AuthzUnavailable), "{err:?}");
}

#[tokio::test]
async fn quota_scope_uses_the_user_quota_type_and_owner_filter() {
    let (authz, pdp, ctx) = setup(Mode::InTenant);
    let scope = authz.quota_scope(&ctx).await.expect("scope");
    let req = last_request(&pdp);
    assert_eq!(req.action.name, "read");
    assert_eq!(req.resource.resource_type, QUOTA_TYPE);
    assert!(req.context.require_constraints);
    assert_eq!(
        req.context.supported_properties,
        ["owner_tenant_id", "owner_id"]
    );
    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        vec![ctx.subject_tenant_id()]
    );
    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_ID),
        vec![ctx.subject_id()]
    );
}

#[tokio::test]
async fn quota_scope_denied_is_authz_denied() {
    let (authz, _pdp, ctx) = setup(Mode::Deny);
    let err = authz.quota_scope(&ctx).await.expect_err("denied");
    assert!(matches!(err, DomainError::AuthzDenied), "{err:?}");
}

// ── PDP outage: fail closed with 503 + Retry-After (DESIGN section 3.8) ─────

/// The HTTP response a handler returns for `err` (handlers propagate the
/// `DomainError` with `?` into the canonical `Problem`).
fn http_response(err: DomainError) -> axum::response::Response {
    use axum::response::IntoResponse;
    toolkit_canonical_errors::Problem::from(CanonicalError::from(err)).into_response()
}

fn assert_503_retry_after_5(err: DomainError, what: &str) {
    assert!(
        matches!(err, DomainError::AuthzUnavailable),
        "{what}: {err:?}"
    );
    let resp = http_response(err);
    assert_eq!(
        resp.status(),
        http::StatusCode::SERVICE_UNAVAILABLE,
        "{what}"
    );
    assert_eq!(
        resp.headers()
            .get(http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("5"),
        "{what}"
    );
    assert_eq!(
        resp.headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json"),
        "{what}"
    );
}

#[tokio::test]
async fn pdp_outage_is_503_with_retry_after_on_every_operation() {
    let (authz, _pdp, ctx) = setup(Mode::Unavailable);
    for action in ChatAction::ALL {
        let chat_id = (*action != ChatAction::Create && *action != ChatAction::List)
            .then(|| Uuid::from_u128(9));
        let err = authz
            .chat_scope(&ctx, *action, chat_id)
            .await
            .expect_err("fail closed");
        assert_503_retry_after_5(err, action.as_str());
    }
    for action in ["list", "read"] {
        let err = authz
            .model_access(&ctx, action)
            .await
            .expect_err("fail closed");
        assert_503_retry_after_5(err, action);
    }
    let err = authz.quota_scope(&ctx).await.expect_err("fail closed");
    assert_503_retry_after_5(err, "quota");
}

/// Records the level of every event emitted by this module.
struct AuthzLogLevels(Arc<Mutex<Vec<tracing::Level>>>);

impl tracing::Subscriber for AuthzLogLevels {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        if event.metadata().target() == module_path!().trim_end_matches("::authz_tests") {
            self.0.lock().expect("lock").push(*event.metadata().level());
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[tokio::test]
async fn pdp_outage_is_logged_at_error_and_denial_at_warn() {
    let levels = Arc::new(Mutex::new(Vec::new()));
    // With a single live dispatcher, tracing computes a callsite's interest from the
    // *calling* thread's dispatcher, so a concurrently running test that hits the
    // callsite first caches `never` and this subscriber would miss the event. A second
    // live dispatcher forces tracing to consult every registered dispatcher instead.
    let _keep_registry_multi = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    let dispatch = tracing::Dispatch::new(AuthzLogLevels(levels.clone()));
    let _guard = tracing::dispatcher::set_default(&dispatch);
    tracing::callsite::rebuild_interest_cache();

    let (authz, _pdp, ctx) = setup(Mode::Unavailable);
    authz
        .chat_scope(&ctx, ChatAction::Read, Some(Uuid::from_u128(1)))
        .await
        .expect_err("unavailable");
    assert_eq!(*levels.lock().expect("lock"), [tracing::Level::ERROR]);

    levels.lock().expect("lock").clear();
    let (authz, _pdp, ctx) = setup(Mode::Deny);
    authz
        .chat_scope(&ctx, ChatAction::Read, Some(Uuid::from_u128(1)))
        .await
        .expect_err("denied");
    assert_eq!(*levels.lock().expect("lock"), [tracing::Level::WARN]);
}
