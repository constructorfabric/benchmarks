#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of `cpt-cf-mini-chat-algo-gear-foundation-authorize` and
//! `cpt-cf-mini-chat-dod-gear-foundation-authz-pep`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverApi;
use authz_resolver_sdk::constraints::{Constraint, EqPredicate, InPredicate, Predicate};
use authz_resolver_sdk::models::{
    EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::{PlatformSecurityContext, SecurityContext, pep_properties};
use uuid::Uuid;

use super::{
    AuthzService, CHAT_RESOURCE_TYPE, ChatAction, MODEL_RESOURCE_TYPE, ModelAction,
    PDP_EVALUATION_TIMEOUT, USER_QUOTA_RESOURCE_TYPE,
};
use crate::domain::error::DomainError;

const SUBJECT_USER_TYPE: &str = "gts.cf.core.security.subject_user.v1~";

fn tenant() -> Uuid {
    Uuid::from_u128(0x7E)
}

fn user() -> Uuid {
    Uuid::from_u128(0x55)
}

fn ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(user())
        .subject_type(SUBJECT_USER_TYPE)
        .subject_tenant_id(tenant())
        .token_scopes(vec!["ai:mini_chat".to_owned()])
        .build()
        .expect("ctx")
}

/// What the fake PDP answers.
enum Reply {
    Decision {
        decision: bool,
        constraints: Vec<Constraint>,
    },
    Failure,
    Hang,
}

struct FakePdp {
    reply: Reply,
    seen: Mutex<Vec<EvaluationRequest>>,
}

impl FakePdp {
    fn new(reply: Reply) -> Arc<Self> {
        Arc::new(Self {
            reply,
            seen: Mutex::new(Vec::new()),
        })
    }

    fn allow(constraints: Vec<Constraint>) -> Arc<Self> {
        Self::new(Reply::Decision {
            decision: true,
            constraints,
        })
    }

    fn deny() -> Arc<Self> {
        Self::new(Reply::Decision {
            decision: false,
            constraints: Vec::new(),
        })
    }

    fn requests(&self) -> Vec<EvaluationRequest> {
        self.seen.lock().unwrap().clone()
    }

    fn only_request(&self) -> EvaluationRequest {
        let seen = self.requests();
        assert_eq!(seen.len(), 1, "exactly one PDP call expected");
        seen.into_iter().next().unwrap()
    }
}

#[async_trait]
impl AuthZResolverApi for FakePdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.seen.lock().unwrap().push(request);
        match &self.reply {
            Reply::Decision {
                decision,
                constraints,
            } => Ok(EvaluationResponse {
                decision: *decision,
                context: EvaluationResponseContext {
                    constraints: constraints.clone(),
                    ..Default::default()
                },
            }),
            Reply::Failure => Err(CanonicalError::service_unavailable().create()),
            Reply::Hang => {
                std::future::pending::<()>().await;
                unreachable!("the PDP fake never answers")
            }
        }
    }
}

/// The shape of the static authz plugin: `In(owner_tenant_id, [tenant])`.
fn tenant_only() -> Vec<Constraint> {
    vec![Constraint {
        predicates: vec![Predicate::In(InPredicate::new(
            pep_properties::OWNER_TENANT_ID,
            [tenant()],
        ))],
    }]
}

fn tenant_and_owner() -> Vec<Constraint> {
    vec![Constraint {
        predicates: vec![
            Predicate::Eq(EqPredicate::new(pep_properties::OWNER_TENANT_ID, tenant())),
            Predicate::Eq(EqPredicate::new(pep_properties::OWNER_ID, user())),
        ],
    }]
}

fn service(pdp: &Arc<FakePdp>) -> AuthzService {
    AuthzService::new(Arc::clone(pdp) as Arc<dyn AuthZResolverApi>)
}

const ALL_CHAT_ACTIONS: [(ChatAction, &str); 16] = [
    (ChatAction::Create, "create"),
    (ChatAction::List, "list"),
    (ChatAction::Read, "read"),
    (ChatAction::Update, "update"),
    (ChatAction::Delete, "delete"),
    (ChatAction::ListMessages, "list_messages"),
    (ChatAction::SendMessage, "send_message"),
    (ChatAction::UploadAttachment, "upload_attachment"),
    (ChatAction::ReadAttachment, "read_attachment"),
    (ChatAction::DeleteAttachment, "delete_attachment"),
    (ChatAction::ReadTurn, "read_turn"),
    (ChatAction::RetryTurn, "retry_turn"),
    (ChatAction::EditTurn, "edit_turn"),
    (ChatAction::DeleteTurn, "delete_turn"),
    (ChatAction::SetReaction, "set_reaction"),
    (ChatAction::DeleteReaction, "delete_reaction"),
];

// ---- constants -----------------------------------------------------------------

#[test]
fn resource_types_and_timeout_match_the_design() {
    assert_eq!(
        CHAT_RESOURCE_TYPE,
        "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~"
    );
    assert_eq!(
        MODEL_RESOURCE_TYPE,
        "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~"
    );
    assert_eq!(
        USER_QUOTA_RESOURCE_TYPE,
        "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~"
    );
    assert_eq!(PDP_EVALUATION_TIMEOUT, Duration::from_secs(5));
}

#[test]
fn action_names_match_the_design() {
    for (action, name) in ALL_CHAT_ACTIONS {
        assert_eq!(action.as_str(), name);
    }
    assert_eq!(ModelAction::List.as_str(), "list");
    assert_eq!(ModelAction::Read.as_str(), "read");
}

// ---- request building ------------------------------------------------------------

#[tokio::test]
async fn builds_the_pdp_request_from_the_security_context() {
    let pdp = FakePdp::allow(tenant_only());
    let chat_id = Uuid::from_u128(0xC1);

    service(&pdp)
        .chat_scope(&ctx(), ChatAction::Read, Some(chat_id))
        .await
        .expect("allowed");

    let request = pdp.only_request();
    assert_eq!(request.subject.id, user());
    assert_eq!(
        request.subject.subject_type.as_deref(),
        Some(SUBJECT_USER_TYPE)
    );
    assert_eq!(
        request.subject.properties.get("tenant_id"),
        Some(&serde_json::json!(tenant().to_string()))
    );
    assert_eq!(request.action.name, "read");
    assert_eq!(request.resource.resource_type, CHAT_RESOURCE_TYPE);
    assert_eq!(request.resource.id, Some(chat_id));
    assert!(request.context.require_constraints);
    assert_eq!(
        request.context.token_scopes,
        vec!["ai:mini_chat".to_owned()]
    );
}

#[tokio::test]
async fn declares_capabilities_and_supported_properties() {
    let pdp = FakePdp::allow(tenant_only());

    service(&pdp)
        .chat_scope(&ctx(), ChatAction::List, None)
        .await
        .expect("allowed");

    let request = pdp.only_request();
    assert!(
        request.context.capabilities.is_empty(),
        "no tenant_hierarchy, group_membership or group_hierarchy: {:?}",
        request.context.capabilities
    );
    let mut supported = request.context.supported_properties;
    supported.sort();
    let mut expected = vec![
        pep_properties::RESOURCE_ID.to_owned(),
        pep_properties::OWNER_ID.to_owned(),
        pep_properties::OWNER_TENANT_ID.to_owned(),
    ];
    expected.sort();
    assert_eq!(supported, expected);
}

#[tokio::test]
async fn sends_every_chat_action_name_with_the_chat_resource_type() {
    for (action, name) in ALL_CHAT_ACTIONS {
        let pdp = FakePdp::allow(tenant_only());
        let chat_id = (action != ChatAction::Create && action != ChatAction::List)
            .then(|| Uuid::from_u128(0xC1));

        service(&pdp)
            .chat_scope(&ctx(), action, chat_id)
            .await
            .unwrap_or_else(|e| panic!("`{name}` must be allowed: {e}"));

        let request = pdp.only_request();
        assert_eq!(request.action.name, name);
        assert_eq!(request.resource.resource_type, CHAT_RESOURCE_TYPE);
        assert_eq!(request.resource.id, chat_id, "resource.id of `{name}`");
    }
}

#[tokio::test]
async fn create_adds_owner_properties_from_the_security_context() {
    let pdp = FakePdp::allow(tenant_only());

    service(&pdp)
        .chat_scope(&ctx(), ChatAction::Create, None)
        .await
        .expect("allowed");

    let request = pdp.only_request();
    assert_eq!(request.resource.id, None);
    assert_eq!(
        request
            .resource
            .properties
            .get(pep_properties::OWNER_TENANT_ID),
        Some(&serde_json::json!(tenant().to_string()))
    );
    assert_eq!(
        request.resource.properties.get(pep_properties::OWNER_ID),
        Some(&serde_json::json!(user().to_string()))
    );
}

#[tokio::test]
async fn other_actions_send_no_resource_properties() {
    let pdp = FakePdp::allow(tenant_only());

    service(&pdp)
        .chat_scope(&ctx(), ChatAction::List, None)
        .await
        .expect("allowed");

    assert!(pdp.only_request().resource.properties.is_empty());
}

#[tokio::test]
async fn model_resource_is_a_decision_only_check() {
    for (action, name) in [(ModelAction::List, "list"), (ModelAction::Read, "read")] {
        let pdp = FakePdp::allow(Vec::new());

        service(&pdp)
            .model_decision(&ctx(), action)
            .await
            .unwrap_or_else(|e| panic!("decision-only `{name}` without constraints: {e}"));

        let request = pdp.only_request();
        assert_eq!(request.action.name, name);
        assert_eq!(request.resource.resource_type, MODEL_RESOURCE_TYPE);
        assert_eq!(request.resource.id, None);
        assert!(!request.context.require_constraints);
    }
}

#[tokio::test]
async fn user_quota_requests_constraints_for_the_read_action() {
    let pdp = FakePdp::allow(tenant_only());

    let scope = service(&pdp)
        .user_quota_scope(&ctx())
        .await
        .expect("allowed");

    let request = pdp.only_request();
    assert_eq!(request.action.name, "read");
    assert_eq!(request.resource.resource_type, USER_QUOTA_RESOURCE_TYPE);
    assert_eq!(request.resource.id, None);
    assert!(request.context.require_constraints);
    assert!(scope.contains_uuid(pep_properties::OWNER_TENANT_ID, tenant()));
}

#[tokio::test]
async fn user_quota_declares_exactly_the_tenant_and_owner_properties() {
    let pdp = FakePdp::allow(tenant_only());

    service(&pdp)
        .user_quota_scope(&ctx())
        .await
        .expect("allowed");

    let mut supported = pdp.only_request().context.supported_properties;
    supported.sort();
    let mut expected = vec![
        pep_properties::OWNER_ID.to_owned(),
        pep_properties::OWNER_TENANT_ID.to_owned(),
    ];
    expected.sort();
    assert_eq!(supported.len(), 2, "{supported:?}");
    assert_eq!(supported, expected, "the quota resource has no `id`");
}

#[tokio::test]
async fn user_quota_scope_adds_the_owner_predicate_to_a_tenant_only_constraint() {
    let pdp = FakePdp::allow(tenant_only());

    let scope = service(&pdp)
        .user_quota_scope(&ctx())
        .await
        .expect("allowed");

    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        vec![tenant()]
    );
    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_ID),
        vec![user()],
        "owner_id Eq on the subject"
    );
}

#[tokio::test]
async fn user_quota_scope_never_admits_another_owner() {
    let other_owner = vec![Constraint {
        predicates: vec![
            Predicate::Eq(EqPredicate::new(pep_properties::OWNER_TENANT_ID, tenant())),
            Predicate::Eq(EqPredicate::new(
                pep_properties::OWNER_ID,
                Uuid::from_u128(0x99),
            )),
        ],
    }];
    let pdp = FakePdp::allow(other_owner);

    let scope = service(&pdp).user_quota_scope(&ctx()).await;

    if let Ok(scope) = scope {
        assert!(
            !scope.contains_uuid(pep_properties::OWNER_ID, Uuid::from_u128(0x99)),
            "the scope must not expose another user's quota"
        );
    }
}

#[tokio::test]
async fn user_quota_scope_for_a_foreign_tenant_is_denied() {
    let foreign = vec![Constraint {
        predicates: vec![Predicate::Eq(EqPredicate::new(
            pep_properties::OWNER_TENANT_ID,
            Uuid::from_u128(0xF0),
        ))],
    }];
    let pdp = FakePdp::allow(foreign);

    let err = service(&pdp)
        .user_quota_scope(&ctx())
        .await
        .expect_err("foreign tenant");

    assert!(matches!(err, DomainError::AuthzDenied), "got {err:?}");
}

// ---- decision-only model check ------------------------------------------------------

#[tokio::test]
async fn model_decision_allows_despite_constraints_the_model_resource_cannot_express() {
    let unknown_property = vec![Constraint {
        predicates: vec![Predicate::Eq(EqPredicate::new("department", tenant()))],
    }];
    for (action, name) in [(ModelAction::List, "list"), (ModelAction::Read, "read")] {
        let pdp = FakePdp::allow(unknown_property.clone());

        service(&pdp)
            .model_decision(&ctx(), action)
            .await
            .unwrap_or_else(|e| panic!("allowed `{name}` must stay allowed: {e}"));
    }
}

// ---- fail-closed mapping -----------------------------------------------------------

#[tokio::test]
async fn a_denied_decision_is_authz_denied() {
    let pdp = FakePdp::deny();

    let err = service(&pdp)
        .chat_scope(&ctx(), ChatAction::Read, Some(Uuid::from_u128(0xC1)))
        .await
        .expect_err("denied");

    assert!(matches!(err, DomainError::AuthzDenied), "got {err:?}");
}

#[tokio::test]
async fn a_denied_model_decision_is_authz_denied() {
    let pdp = FakePdp::deny();

    let err = service(&pdp)
        .model_decision(&ctx(), ModelAction::List)
        .await
        .expect_err("denied");

    assert!(matches!(err, DomainError::AuthzDenied), "got {err:?}");
}

#[tokio::test]
async fn empty_constraints_with_required_constraints_are_denied() {
    let pdp = FakePdp::allow(Vec::new());

    let err = service(&pdp)
        .chat_scope(&ctx(), ChatAction::List, None)
        .await
        .expect_err("no constraints");

    assert!(matches!(err, DomainError::AuthzDenied), "got {err:?}");
}

#[tokio::test]
async fn empty_user_quota_constraints_are_denied() {
    let pdp = FakePdp::allow(Vec::new());

    let err = service(&pdp)
        .user_quota_scope(&ctx())
        .await
        .expect_err("no constraints");

    assert!(matches!(err, DomainError::AuthzDenied), "got {err:?}");
}

#[tokio::test]
async fn a_constraint_on_an_unknown_property_is_false_and_all_false_is_denied() {
    let pdp = FakePdp::allow(vec![Constraint {
        predicates: vec![Predicate::Eq(EqPredicate::new("department", tenant()))],
    }]);

    let err = service(&pdp)
        .chat_scope(&ctx(), ChatAction::List, None)
        .await
        .expect_err("every constraint is false");

    assert!(matches!(err, DomainError::AuthzDenied), "got {err:?}");
}

#[tokio::test]
async fn an_unknown_property_only_voids_its_own_constraint() {
    let mut constraints = vec![Constraint {
        predicates: vec![Predicate::Eq(EqPredicate::new("department", tenant()))],
    }];
    constraints.extend(tenant_only());
    let pdp = FakePdp::allow(constraints);

    let scope = service(&pdp)
        .chat_scope(&ctx(), ChatAction::List, None)
        .await
        .expect("the valid constraint remains");

    assert!(scope.contains_uuid(pep_properties::OWNER_TENANT_ID, tenant()));
}

#[tokio::test]
async fn a_pdp_failure_is_pdp_unavailable() {
    let pdp = FakePdp::new(Reply::Failure);

    let chat = service(&pdp)
        .chat_scope(&ctx(), ChatAction::List, None)
        .await
        .expect_err("pdp failure");
    let model = service(&pdp)
        .model_decision(&ctx(), ModelAction::List)
        .await
        .expect_err("pdp failure");

    assert!(matches!(chat, DomainError::PdpUnavailable), "got {chat:?}");
    assert!(
        matches!(model, DomainError::PdpUnavailable),
        "got {model:?}"
    );
}

#[tokio::test]
async fn a_pdp_that_does_not_answer_in_time_is_pdp_unavailable() {
    let pdp = FakePdp::new(Reply::Hang);
    let service = AuthzService::with_timeout(
        Arc::clone(&pdp) as Arc<dyn AuthZResolverApi>,
        Duration::from_millis(50),
    );

    let err = tokio::time::timeout(
        Duration::from_secs(5),
        service.chat_scope(&ctx(), ChatAction::List, None),
    )
    .await
    .expect("the service enforces its own bound")
    .expect_err("timeout");

    assert!(matches!(err, DomainError::PdpUnavailable), "got {err:?}");
}

// ---- owner predicates ----------------------------------------------------------------

#[tokio::test]
async fn chat_scope_adds_the_owner_predicate_to_a_tenant_only_constraint() {
    let pdp = FakePdp::allow(tenant_only());

    let scope = service(&pdp)
        .chat_scope(&ctx(), ChatAction::Read, Some(Uuid::from_u128(0xC1)))
        .await
        .expect("allowed");

    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        vec![tenant()],
        "tenant_id predicate"
    );
    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_ID),
        vec![user()],
        "user_id predicate (defence in depth)"
    );
}

#[tokio::test]
async fn chat_scope_keeps_owner_predicates_the_pdp_already_returned() {
    let pdp = FakePdp::allow(tenant_and_owner());

    let scope = service(&pdp)
        .chat_scope(&ctx(), ChatAction::Read, Some(Uuid::from_u128(0xC1)))
        .await
        .expect("allowed");

    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        vec![tenant()]
    );
    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_ID),
        vec![user()]
    );
}

#[tokio::test]
async fn chat_scope_never_admits_another_owner() {
    let other_owner = vec![Constraint {
        predicates: vec![
            Predicate::Eq(EqPredicate::new(pep_properties::OWNER_TENANT_ID, tenant())),
            Predicate::Eq(EqPredicate::new(
                pep_properties::OWNER_ID,
                Uuid::from_u128(0x99),
            )),
        ],
    }];
    let pdp = FakePdp::allow(other_owner);

    let outcome = service(&pdp)
        .chat_scope(&ctx(), ChatAction::List, None)
        .await;

    match outcome {
        Err(DomainError::AuthzDenied) => {}
        Ok(scope) => {
            assert!(
                !scope.contains_uuid(pep_properties::OWNER_ID, Uuid::from_u128(0x99)),
                "the scope must not expose another owner's chats"
            );
        }
        Err(other) => panic!("unexpected error {other:?}"),
    }
}
