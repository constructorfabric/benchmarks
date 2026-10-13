#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use authz_resolver_sdk::constraints::{Constraint, EqPredicate, InPredicate, Predicate};
use authz_resolver_sdk::models::{
    DenyReason, EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use authz_resolver_sdk::{AuthZResolverApi, TenantMode};
use mini_chat_sdk::resource_types;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::{
    AccessScope, PlatformSecurityContext, ScopeConstraint, ScopeFilter, SecurityContext,
    pep_properties,
};
use uuid::Uuid;

use super::*;
use crate::domain::error::{DomainError, ResourceKind};

const TENANT: Uuid = Uuid::from_u128(0x7e7a);
const USER: Uuid = Uuid::from_u128(0xa11c);
const OTHER_USER: Uuid = Uuid::from_u128(0xb0b);
const OTHER_TENANT: Uuid = Uuid::from_u128(0xf0f0);

fn ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(USER)
        .subject_tenant_id(TENANT)
        .token_scopes(vec!["ai:mini_chat".to_owned()])
        .build()
        .unwrap()
}

/// What the fake PDP answers.
#[derive(Clone)]
enum Outcome {
    Allow(Vec<Constraint>),
    Deny,
    Fail,
}

/// Fake PDP: answers with a fixed outcome and records every request.
struct FakePdp {
    outcome: Outcome,
    requests: Mutex<Vec<EvaluationRequest>>,
}

impl FakePdp {
    fn new(outcome: Outcome) -> Arc<Self> {
        Arc::new(Self {
            outcome,
            requests: Mutex::new(Vec::new()),
        })
    }

    fn last_request(&self) -> EvaluationRequest {
        self.requests
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("PDP called")
    }

    fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

#[async_trait]
impl AuthZResolverApi for FakePdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.requests.lock().unwrap().push(request);
        match &self.outcome {
            Outcome::Allow(constraints) => Ok(EvaluationResponse {
                decision: true,
                context: EvaluationResponseContext {
                    constraints: constraints.clone(),
                    ..Default::default()
                },
            }),
            Outcome::Deny => Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext {
                    deny_reason: Some(DenyReason {
                        error_code: "secret_policy".to_owned(),
                        details: Some("internal deny details".to_owned()),
                    }),
                    ..Default::default()
                },
            }),
            Outcome::Fail => Err(CanonicalError::service_unavailable()
                .with_detail("pdp down")
                .create()),
        }
    }
}

fn eq(property: &str, value: Uuid) -> Predicate {
    Predicate::Eq(EqPredicate::new(property, value))
}

fn owner_constraints() -> Vec<Constraint> {
    vec![Constraint {
        predicates: vec![
            eq(pep_properties::OWNER_TENANT_ID, TENANT),
            eq(pep_properties::OWNER_ID, USER),
        ],
    }]
}

fn service(pdp: &Arc<FakePdp>) -> AuthzService {
    AuthzService::from_client(Arc::clone(pdp) as Arc<dyn AuthZResolverApi>)
}

fn subject_scope() -> AccessScope {
    AccessScope::single(ScopeConstraint::new(vec![
        ScopeFilter::eq(pep_properties::OWNER_ID, USER),
        ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, TENANT),
    ]))
}

/// The scope admits only the subject: every constraint names exactly the
/// subject tenant and owner.
fn assert_confined(scope: &AccessScope) {
    assert!(!scope.is_unconstrained());
    assert!(!scope.constraints().is_empty());
    for constraint in scope.constraints() {
        for (property, value) in [
            (pep_properties::OWNER_TENANT_ID, TENANT),
            (pep_properties::OWNER_ID, USER),
        ] {
            let filters: Vec<_> = constraint
                .filters()
                .iter()
                .filter(|f| f.property() == property)
                .collect();
            assert_eq!(filters.len(), 1, "{property} in {constraint:?}");
            assert_eq!(filters[0].uuid_values(), vec![value]);
        }
    }
}

// ── Actions and resource descriptors ─────────────────────────────────────

#[test]
fn actions_use_the_matrix_wire_names() {
    let names: Vec<&str> = Action::ALL.iter().map(|a| a.as_str()).collect();
    assert_eq!(
        names,
        [
            "create",
            "list",
            "read",
            "delete",
            "update",
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
    assert!(Action::List.is_model_action());
    assert!(Action::Read.is_model_action());
    assert!(!Action::Create.is_model_action());
}

#[test]
fn resource_descriptors_use_the_pep_gts_types() {
    assert_eq!(
        CHAT_RESOURCE.name(),
        "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~"
    );
    assert_eq!(
        CHAT_RESOURCE.supported_properties(),
        ["owner_tenant_id", "owner_id", "id"]
    );
    assert_eq!(
        MODEL_RESOURCE.name(),
        resource_types::MODEL_AUTHZ_RESOURCE_TYPE
    );
    assert!(MODEL_RESOURCE.supported_properties().is_empty());
    assert_eq!(
        USER_QUOTA_RESOURCE.name(),
        resource_types::USER_QUOTA_AUTHZ_RESOURCE_TYPE
    );
    assert_eq!(
        USER_QUOTA_RESOURCE.supported_properties(),
        ["owner_tenant_id", "owner_id"]
    );
}

// ── Chat ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn chat_allow_with_constraints_returns_the_subject_scope() {
    let pdp = FakePdp::new(Outcome::Allow(owner_constraints()));
    let chat_id = Uuid::from_u128(0xc4a7);

    let scope = service(&pdp)
        .authorize_chat(&ctx(), Action::Read, Some(chat_id))
        .await
        .unwrap();

    assert_confined(&scope);
    assert_eq!(scope.constraints().len(), 1);

    let request = pdp.last_request();
    assert_eq!(request.action.name, "read");
    assert_eq!(request.resource.resource_type, CHAT_RESOURCE.name());
    assert_eq!(request.resource.id, Some(chat_id));
    assert!(request.resource.properties.is_empty());
    assert_eq!(request.subject.id, USER);
    assert_eq!(
        request.subject.properties["tenant_id"],
        serde_json::json!(TENANT.to_string())
    );
    assert!(request.context.require_constraints);
    assert_eq!(request.context.token_scopes, ["ai:mini_chat"]);
    assert!(request.context.capabilities.is_empty());
    assert_eq!(
        request.context.supported_properties,
        ["owner_tenant_id", "owner_id", "id"]
    );
    let tenant_context = request.context.tenant_context.expect("tenant context");
    assert_eq!(tenant_context.mode, TenantMode::RootOnly);
    assert_eq!(tenant_context.root_id, Some(TENANT));
}

#[tokio::test]
async fn chat_create_sends_owner_properties_without_resource_id() {
    let pdp = FakePdp::new(Outcome::Allow(owner_constraints()));

    let scope = service(&pdp)
        .authorize_chat(&ctx(), Action::Create, None)
        .await
        .unwrap();

    assert_confined(&scope);
    let request = pdp.last_request();
    assert_eq!(request.action.name, "create");
    assert_eq!(request.resource.id, None);
    assert_eq!(
        request.resource.properties["owner_tenant_id"],
        serde_json::json!(TENANT.to_string())
    );
    assert_eq!(
        request.resource.properties["owner_id"],
        serde_json::json!(USER.to_string())
    );
    assert!(request.context.require_constraints);
}

#[tokio::test]
async fn chat_tenant_only_constraint_gets_the_owner_predicate() {
    let pdp = FakePdp::new(Outcome::Allow(vec![Constraint {
        predicates: vec![eq(pep_properties::OWNER_TENANT_ID, TENANT)],
    }]));

    let scope = service(&pdp)
        .authorize_chat(&ctx(), Action::List, None)
        .await
        .unwrap();

    assert_confined(&scope);
    assert_eq!(scope, subject_scope());
}

#[tokio::test]
async fn chat_owner_only_constraint_gets_the_tenant_predicate() {
    let pdp = FakePdp::new(Outcome::Allow(vec![Constraint {
        predicates: vec![eq(pep_properties::OWNER_ID, USER)],
    }]));

    let scope = service(&pdp)
        .authorize_chat(&ctx(), Action::SendMessage, Some(Uuid::from_u128(1)))
        .await
        .unwrap();

    assert_confined(&scope);
}

#[tokio::test]
async fn chat_constraint_for_a_foreign_owner_is_dropped() {
    let pdp = FakePdp::new(Outcome::Allow(vec![
        Constraint {
            predicates: vec![
                eq(pep_properties::OWNER_TENANT_ID, TENANT),
                eq(pep_properties::OWNER_ID, OTHER_USER),
            ],
        },
        Constraint {
            predicates: vec![
                eq(pep_properties::OWNER_TENANT_ID, TENANT),
                eq(pep_properties::OWNER_ID, USER),
            ],
        },
    ]));

    let scope = service(&pdp)
        .authorize_chat(&ctx(), Action::List, None)
        .await
        .unwrap();

    assert_eq!(scope.constraints().len(), 1);
    assert_confined(&scope);
    assert!(!scope.contains_uuid(pep_properties::OWNER_ID, OTHER_USER));
}

#[tokio::test]
async fn chat_in_predicate_is_narrowed_to_the_subject() {
    let pdp = FakePdp::new(Outcome::Allow(vec![Constraint {
        predicates: vec![
            eq(pep_properties::OWNER_TENANT_ID, TENANT),
            Predicate::In(InPredicate::new(
                pep_properties::OWNER_ID,
                [USER, OTHER_USER],
            )),
        ],
    }]));

    let scope = service(&pdp)
        .authorize_chat(&ctx(), Action::Read, Some(Uuid::from_u128(2)))
        .await
        .unwrap();

    assert_confined(&scope);
    assert!(!scope.contains_uuid(pep_properties::OWNER_ID, OTHER_USER));
}

#[tokio::test]
async fn chat_constraints_only_for_foreign_data_are_denied() {
    let pdp = FakePdp::new(Outcome::Allow(vec![Constraint {
        predicates: vec![eq(pep_properties::OWNER_TENANT_ID, OTHER_TENANT)],
    }]));

    let err = service(&pdp)
        .authorize_chat(&ctx(), Action::List, None)
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        DomainError::PermissionDenied {
            resource: ResourceKind::Chat
        }
    ));
}

#[tokio::test]
async fn chat_deny_is_permission_denied_without_details() {
    let pdp = FakePdp::new(Outcome::Deny);

    let err = service(&pdp)
        .authorize_chat(&ctx(), Action::Delete, Some(Uuid::from_u128(3)))
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        DomainError::PermissionDenied {
            resource: ResourceKind::Chat
        }
    ));
    let problem = toolkit_canonical_errors::Problem::from(CanonicalError::from(err));
    assert_eq!(problem.status, Some(403));
    assert_eq!(problem.context["reason"], serde_json::json!("AUTHZ_DENIED"));
    let wire = serde_json::to_string(&problem).unwrap();
    assert!(!wire.contains("internal deny details"));
    assert!(!wire.contains("secret_policy"));
}

#[tokio::test]
async fn chat_pdp_error_is_pdp_unavailable() {
    let pdp = FakePdp::new(Outcome::Fail);

    let err = service(&pdp)
        .authorize_chat(&ctx(), Action::List, None)
        .await
        .unwrap_err();

    assert!(matches!(err, DomainError::PdpUnavailable));
    let problem = toolkit_canonical_errors::Problem::from(CanonicalError::from(err));
    assert_eq!(problem.status, Some(503));
}

#[tokio::test]
async fn chat_empty_constraints_are_denied() {
    let pdp = FakePdp::new(Outcome::Allow(Vec::new()));

    let err = service(&pdp)
        .authorize_chat(&ctx(), Action::List, None)
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        DomainError::PermissionDenied {
            resource: ResourceKind::Chat
        }
    ));
}

#[tokio::test]
async fn chat_unknown_property_is_denied() {
    let pdp = FakePdp::new(Outcome::Allow(vec![Constraint {
        predicates: vec![eq("project_id", Uuid::from_u128(9))],
    }]));

    let err = service(&pdp)
        .authorize_chat(&ctx(), Action::List, None)
        .await
        .unwrap_err();

    assert!(matches!(err, DomainError::PermissionDenied { .. }));
}

#[tokio::test]
async fn every_chat_action_is_sent_with_its_name() {
    for action in Action::ALL {
        let pdp = FakePdp::new(Outcome::Allow(owner_constraints()));
        service(&pdp)
            .authorize_chat(&ctx(), action, Some(Uuid::from_u128(4)))
            .await
            .unwrap();
        assert_eq!(pdp.last_request().action.name, action.as_str());
    }
}

// ── Model ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn model_allow_without_constraints_passes_as_permission_only() {
    let pdp = FakePdp::new(Outcome::Allow(Vec::new()));

    service(&pdp)
        .authorize_model(&ctx(), Action::List)
        .await
        .unwrap();

    let request = pdp.last_request();
    assert_eq!(request.action.name, "list");
    assert_eq!(
        request.resource.resource_type,
        resource_types::MODEL_AUTHZ_RESOURCE_TYPE
    );
    assert_eq!(request.resource.id, None);
    assert!(!request.context.require_constraints);
    assert!(request.context.supported_properties.is_empty());
    assert_eq!(request.context.token_scopes, ["ai:mini_chat"]);
}

#[tokio::test]
async fn model_deny_is_permission_denied_on_the_model_resource() {
    let pdp = FakePdp::new(Outcome::Deny);

    let err = service(&pdp)
        .authorize_model(&ctx(), Action::Read)
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        DomainError::PermissionDenied {
            resource: ResourceKind::Model
        }
    ));
}

#[tokio::test]
async fn model_pdp_error_is_pdp_unavailable() {
    let pdp = FakePdp::new(Outcome::Fail);

    let err = service(&pdp)
        .authorize_model(&ctx(), Action::List)
        .await
        .unwrap_err();

    assert!(matches!(err, DomainError::PdpUnavailable));
}

#[tokio::test]
async fn model_constraints_fail_closed() {
    let pdp = FakePdp::new(Outcome::Allow(owner_constraints()));

    let err = service(&pdp)
        .authorize_model(&ctx(), Action::List)
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        DomainError::PermissionDenied {
            resource: ResourceKind::Model
        }
    ));
}

#[tokio::test]
async fn model_rejects_actions_outside_the_models_api_without_calling_the_pdp() {
    let pdp = FakePdp::new(Outcome::Allow(Vec::new()));

    let err = service(&pdp)
        .authorize_model(&ctx(), Action::Create)
        .await
        .unwrap_err();

    assert!(matches!(err, DomainError::Internal(_)));
    assert_eq!(pdp.calls(), 0);
}

// ── User quota ───────────────────────────────────────────────────────────

#[tokio::test]
async fn user_quota_read_returns_the_subject_scope() {
    let pdp = FakePdp::new(Outcome::Allow(vec![Constraint {
        predicates: vec![eq(pep_properties::OWNER_TENANT_ID, TENANT)],
    }]));

    let scope = service(&pdp).authorize_user_quota(&ctx()).await.unwrap();

    assert_confined(&scope);
    let request = pdp.last_request();
    assert_eq!(request.action.name, "read");
    assert_eq!(
        request.resource.resource_type,
        resource_types::USER_QUOTA_AUTHZ_RESOURCE_TYPE
    );
    assert!(request.context.require_constraints);
    assert_eq!(
        request.context.supported_properties,
        ["owner_tenant_id", "owner_id"]
    );
}

#[tokio::test]
async fn user_quota_empty_constraints_are_denied() {
    let pdp = FakePdp::new(Outcome::Allow(Vec::new()));

    let err = service(&pdp)
        .authorize_user_quota(&ctx())
        .await
        .unwrap_err();

    assert!(matches!(err, DomainError::PermissionDenied { .. }));
}

#[tokio::test]
async fn user_quota_pdp_error_is_pdp_unavailable() {
    let pdp = FakePdp::new(Outcome::Fail);

    let err = service(&pdp)
        .authorize_user_quota(&ctx())
        .await
        .unwrap_err();

    assert!(matches!(err, DomainError::PdpUnavailable));
}

// ── Scope confinement ────────────────────────────────────────────────────

#[test]
fn confine_turns_allow_all_into_the_subject_scope() {
    let scope = confine_to_subject(&AccessScope::allow_all(), TENANT, USER);

    assert_eq!(scope, subject_scope());
}

#[test]
fn confine_keeps_deny_all() {
    assert!(confine_to_subject(&AccessScope::deny_all(), TENANT, USER).is_deny_all());
}

#[test]
fn confine_keeps_unrelated_filters() {
    let chat_id = Uuid::from_u128(0xc0);
    let scope = AccessScope::single(ScopeConstraint::new(vec![
        ScopeFilter::eq(pep_properties::RESOURCE_ID, chat_id),
        ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, TENANT),
    ]));

    let confined = confine_to_subject(&scope, TENANT, USER);

    assert_confined(&confined);
    assert!(confined.contains_uuid(pep_properties::RESOURCE_ID, chat_id));
}
