//! In-process fake PDP implementing `AuthZResolverApi`.

use std::sync::Mutex;

use async_trait::async_trait;
use authz_resolver_sdk::{
    AuthZResolverApi, Constraint, EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
    InPredicate, Predicate,
};
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::{PlatformSecurityContext, pep_properties};
use uuid::Uuid;

/// How the fake PDP answers.
#[derive(Clone, Copy, Debug)]
pub enum PdpMode {
    /// Allow with `In(owner_tenant_id, [subject tenant])`, like the static-authz plugin (only
    /// when the PEP advertises `owner_tenant_id` as a supported property).
    TenantConstraint,
    /// `decision = false`.
    Deny,
    /// Infrastructure failure (`service_unavailable`).
    Fail,
}

pub struct FakePdp {
    mode: PdpMode,
    requests: Mutex<Vec<EvaluationRequest>>,
}

impl FakePdp {
    pub fn new(mode: PdpMode) -> Self {
        Self {
            mode,
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Every evaluation request received so far.
    pub fn requests(&self) -> Vec<EvaluationRequest> {
        self.requests.lock().expect("lock").clone()
    }
}

#[async_trait]
impl AuthZResolverApi for FakePdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.requests.lock().expect("lock").push(req.clone());
        match self.mode {
            PdpMode::Fail => Err(CanonicalError::service_unavailable()
                .with_detail("fake pdp failure")
                .create()),
            PdpMode::Deny => Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            }),
            PdpMode::TenantConstraint => {
                let tenant = req
                    .context
                    .tenant_context
                    .as_ref()
                    .and_then(|tc| tc.root_id)
                    .or_else(|| {
                        req.subject
                            .properties
                            .get("tenant_id")
                            .and_then(|v| v.as_str())
                            .and_then(|s| Uuid::parse_str(s).ok())
                    });
                let Some(tenant) = tenant.filter(|t| !t.is_nil()) else {
                    return Ok(EvaluationResponse {
                        decision: false,
                        context: EvaluationResponseContext::default(),
                    });
                };
                let constraints = if req
                    .context
                    .supported_properties
                    .iter()
                    .any(|p| p == pep_properties::OWNER_TENANT_ID)
                {
                    vec![Constraint {
                        predicates: vec![Predicate::In(InPredicate::new(
                            pep_properties::OWNER_TENANT_ID,
                            [tenant],
                        ))],
                    }]
                } else {
                    Vec::new()
                };
                Ok(EvaluationResponse {
                    decision: true,
                    context: EvaluationResponseContext {
                        constraints,
                        ..Default::default()
                    },
                })
            }
        }
    }
}
