//! Fake PDP.

use async_trait::async_trait;
use authz_resolver_sdk::constraints::{Constraint, EqPredicate, Predicate};
use authz_resolver_sdk::models::EvaluationResponseContext;
use authz_resolver_sdk::{AuthZResolverApi, EvaluationRequest, EvaluationResponse};
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::{PlatformSecurityContext, pep_properties};
use uuid::Uuid;

/// How the fake PDP answers every evaluation request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AuthzMode {
    /// `decision = true` with `eq(owner_tenant_id)` and `eq(owner_id)` of the subject.
    #[default]
    OwnerScoped,
    /// `decision = true` with `eq(owner_tenant_id)` only (the gear must add the owner predicate).
    TenantOnly,
    /// `decision = false`.
    Deny,
    /// The PDP evaluation fails.
    Unavailable,
    /// `decision = true` with no constraints.
    NoConstraints,
}

/// The fake PDP. Like the real one it returns constraints only when the request asks for them.
pub struct FakeAuthz {
    mode: AuthzMode,
}

impl FakeAuthz {
    pub fn new(mode: AuthzMode) -> Self {
        Self { mode }
    }
}

fn allow(constraints: Vec<Constraint>) -> EvaluationResponse {
    EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints,
            deny_reason: None,
        },
    }
}

fn subject_tenant(req: &EvaluationRequest) -> Option<Uuid> {
    req.subject
        .properties
        .get("tenant_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|text| Uuid::parse_str(text).ok())
}

#[async_trait]
impl AuthZResolverApi for FakeAuthz {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        let Some(tenant) = subject_tenant(&req) else {
            return Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            });
        };
        let tenant_predicate =
            Predicate::Eq(EqPredicate::new(pep_properties::OWNER_TENANT_ID, tenant));
        let owner_predicate =
            Predicate::Eq(EqPredicate::new(pep_properties::OWNER_ID, req.subject.id));
        match self.mode {
            AuthzMode::Deny => Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            }),
            AuthzMode::Unavailable => {
                Err(CanonicalError::internal("fake PDP evaluation failure").create())
            }
            _ if !req.context.require_constraints => Ok(allow(Vec::new())),
            AuthzMode::NoConstraints => Ok(allow(Vec::new())),
            AuthzMode::TenantOnly => Ok(allow(vec![Constraint {
                predicates: vec![tenant_predicate],
            }])),
            AuthzMode::OwnerScoped => Ok(allow(vec![Constraint {
                predicates: vec![tenant_predicate, owner_predicate],
            }])),
        }
    }
}
