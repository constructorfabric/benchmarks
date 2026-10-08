//! Mock PDP with the static authz plugin's behaviour.

use std::sync::Mutex;

use async_trait::async_trait;
use authz_resolver_sdk::models::{
    EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use authz_resolver_sdk::{AuthZResolverApi, Constraint, InPredicate, Predicate};
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::{PlatformSecurityContext, pep_properties};
use uuid::Uuid;

/// Behaviour of [`MockPdp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PdpMode {
    /// Permit with one constraint `In(owner_tenant_id, [tenant])` (static plugin).
    #[default]
    Allow,
    /// `decision: false`.
    Deny,
    /// Evaluation failure (PDP unreachable).
    Fail,
}

/// [`AuthZResolverApi`] fake. Records every evaluation request.
#[derive(Debug, Default)]
pub struct MockPdp {
    mode: Mutex<PdpMode>,
    requests: Mutex<Vec<EvaluationRequest>>,
}

impl MockPdp {
    #[must_use]
    pub fn new(mode: PdpMode) -> Self {
        Self {
            mode: Mutex::new(mode),
            requests: Mutex::default(),
        }
    }

    /// Change the behaviour of later evaluations.
    pub fn set_mode(&self, mode: PdpMode) {
        *self
            .mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = mode;
    }

    /// Every request evaluated so far, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<EvaluationRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn mode(&self) -> PdpMode {
        *self
            .mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn deny() -> EvaluationResponse {
    EvaluationResponse {
        decision: false,
        context: EvaluationResponseContext::default(),
    }
}

/// Static plugin rules: tenant from the tenant context root, else the subject's
/// `tenant_id`; nil or missing tenant denies; the tenant constraint is emitted
/// only for PEPs supporting `owner_tenant_id`.
fn static_plugin(req: &EvaluationRequest) -> EvaluationResponse {
    let tenant = req
        .context
        .tenant_context
        .as_ref()
        .and_then(|t| t.root_id)
        .or_else(|| {
            req.subject
                .properties
                .get("tenant_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|s| Uuid::parse_str(s).ok())
        });
    let Some(tenant) = tenant.filter(|t| !t.is_nil()) else {
        return deny();
    };
    let mut constraints = Vec::new();
    if req
        .context
        .supported_properties
        .iter()
        .any(|p| p == pep_properties::OWNER_TENANT_ID)
    {
        constraints.push(Constraint {
            predicates: vec![Predicate::In(InPredicate::new(
                pep_properties::OWNER_TENANT_ID,
                [tenant],
            ))],
        });
    }
    EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints,
            ..Default::default()
        },
    }
}

#[async_trait]
impl AuthZResolverApi for MockPdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        match self.mode() {
            PdpMode::Allow => Ok(static_plugin(&request)),
            PdpMode::Deny => Ok(deny()),
            PdpMode::Fail => Err(CanonicalError::service_unavailable()
                .with_detail("mock PDP unavailable")
                .create()),
        }
    }
}
