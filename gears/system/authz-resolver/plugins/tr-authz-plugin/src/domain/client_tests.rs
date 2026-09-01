// Created: 2026-04-16 by Constructor Tech
// Updated: 2026-04-29 by Constructor Tech

use std::sync::Arc;

use authz_resolver_sdk::AuthZResolverPluginClient;
use authz_resolver_sdk::models::{
    Action, EvaluationRequest, EvaluationRequestContext, Resource, Subject,
};
use uuid::Uuid;

use crate::domain::service::Service;
use crate::domain::test_support::MockTr;

#[tokio::test]
async fn client_trait_delegates_to_service() {
    let svc = Arc::new(Service::new(Arc::new(MockTr::empty())));
    let client: Arc<dyn AuthZResolverPluginClient> = svc;

    let req = EvaluationRequest {
        subject: Subject {
            id: Uuid::now_v7(),
            subject_type: None,
            properties: std::collections::HashMap::default(),
        },
        action: Action {
            name: "list".to_owned(),
        },
        resource: Resource {
            resource_type: "test".to_owned(),
            id: None,
            properties: std::collections::HashMap::default(),
        },
        context: EvaluationRequestContext {
            tenant_context: None,
            token_scopes: vec![],
            require_constraints: false,
            capabilities: vec![],
            supported_properties: vec![],
            bearer_token: None,
        },
    };

    let resp = client
        .evaluate(req)
        .await
        .expect("evaluate should not error");
    assert!(!resp.decision, "no tenant -> deny");
}
