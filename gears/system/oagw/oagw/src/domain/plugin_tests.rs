#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::BTreeMap;

use super::{
    AUTH_BUILTINS, BuiltinPlugin, ChainKind, GUARD_BUILTINS, GuardDecision, TRANSFORM_BUILTINS,
    chain_builtins, has_uuid_tail, is_bindable, is_bindable_builtin, uuid_tail,
};
use crate::domain::dto::ProxyContext;
use crate::domain::model::{AUTH_PLUGIN_TYPE, GUARD_PLUGIN_TYPE, TRANSFORM_PLUGIN_TYPE};

const ROW_ID: &str = "3f2c1b2a-1b1c-2d3e-4f50-61728394a5b6";

const REQUIRED_HEADERS: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
const REQUEST_ID: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

#[test]
fn builtin_catalogs_are_gts_ids_of_the_right_base_type() {
    for entry in AUTH_BUILTINS {
        assert!(entry.gts_id.starts_with(AUTH_PLUGIN_TYPE), "{}", entry.name);
        assert!(!entry.name.is_empty());
    }
    for entry in GUARD_BUILTINS {
        assert!(
            entry.gts_id.starts_with(GUARD_PLUGIN_TYPE),
            "{}",
            entry.name
        );
    }
    for entry in TRANSFORM_BUILTINS {
        assert!(
            entry.gts_id.starts_with(TRANSFORM_PLUGIN_TYPE),
            "{}",
            entry.name
        );
    }
}

/// The catalog-only entries of a family, by name.
fn catalog_only(catalog: &[BuiltinPlugin]) -> Vec<&str> {
    catalog
        .iter()
        .filter(|entry| !entry.bindable)
        .map(|entry| entry.name)
        .collect()
}

/// `F5` — every catalog-only identifier of `DESIGN` §3.2 is present, carries
/// the base type of its family and is marked not bindable.
#[test]
fn catalog_only_entries_are_present_and_not_bindable() {
    assert_eq!(catalog_only(AUTH_BUILTINS), ["basic", "bearer"]);
    assert_eq!(catalog_only(GUARD_BUILTINS), ["timeout", "cors"]);
    assert_eq!(catalog_only(TRANSFORM_BUILTINS), ["logging", "metrics"]);
    for id in [
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1",
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1",
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1",
    ] {
        let entry = chain_builtins()
            .chain(AUTH_BUILTINS.iter())
            .find(|entry| entry.gts_id == id)
            .unwrap_or_else(|| panic!("{id} is not cataloged"));
        assert!(!entry.bindable, "{}", entry.name);
    }
}

#[test]
fn unbindable_builtins_are_catalog_only() {
    let names: Vec<&str> = AUTH_BUILTINS
        .iter()
        .filter(|entry| !entry.bindable)
        .map(|entry| entry.name)
        .collect();
    assert_eq!(names, ["basic", "bearer"]);
}

#[test]
fn uuid_tail_accepts_only_the_dash_layout() {
    let reference = format!("{AUTH_PLUGIN_TYPE}{ROW_ID}");
    assert_eq!(uuid_tail(&reference), Some(ROW_ID.parse().unwrap()));
    assert!(has_uuid_tail(&reference));
}

#[test]
fn uuid_tail_rejects_catalog_only_tails_and_garbage() {
    assert_eq!(
        uuid_tail("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1"),
        None
    );
    assert!(!has_uuid_tail(
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1"
    ));
    // 36 characters but the wrong dash layout.
    assert_eq!(uuid_tail("x12345678x1234x1234x1234x1234567890ab"), None);
    // Not 36 characters.
    assert_eq!(uuid_tail("3f2c1b2a-1b1c-2d3e-4f50-61728394a5b"), None);
    // Non-hex UUID body.
    assert_eq!(uuid_tail("zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz"), None);
}

/// `F1` — both chains bind guards and transforms; the chain carries a set of
/// base types, not one of them.
#[test]
fn chain_kinds_share_the_guard_and_transform_base_types() {
    assert_eq!(ChainKind::Upstream.to_string(), "upstream");
    assert_eq!(ChainKind::Route.to_string(), "route");
    for kind in [ChainKind::Upstream, ChainKind::Route] {
        assert_eq!(
            super::CHAIN_BASE_TYPES,
            [GUARD_PLUGIN_TYPE, TRANSFORM_PLUGIN_TYPE]
        );
        // A bindable built-in of either family resolves on both chains.
        assert!(is_bindable_builtin(REQUIRED_HEADERS));
        assert!(is_bindable_builtin(REQUEST_ID));
        assert!(is_bindable(kind, REQUIRED_HEADERS));
        assert!(is_bindable(kind, REQUEST_ID));
        // An auth id is never a chain entry: `auth` is an upstream field.
        let noop = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
        assert!(!is_bindable_builtin(noop));
        assert!(!is_bindable(kind, noop));
        // A custom row of either family binds.
        let custom_guard = format!("{GUARD_PLUGIN_TYPE}{ROW_ID}");
        let custom_transform = format!("{TRANSFORM_PLUGIN_TYPE}{ROW_ID}");
        assert!(is_bindable(kind, &custom_guard));
        assert!(is_bindable(kind, &custom_transform));
        // A custom auth row does not.
        let custom_auth = format!("{AUTH_PLUGIN_TYPE}{ROW_ID}");
        assert!(!is_bindable(kind, &custom_auth));
    }
}

#[test]
fn bindability_is_narrowed_to_the_catalog_and_the_chain() {
    let basic = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
    let timeout = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";

    // A catalog-only entry never binds, on either chain.
    for reference in [
        basic,
        timeout,
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1",
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1",
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1",
    ] {
        assert!(!is_bindable_builtin(reference), "{reference}");
        assert!(!is_bindable(ChainKind::Upstream, reference), "{reference}");
        assert!(!is_bindable(ChainKind::Route, reference), "{reference}");
    }
    // A name that is not cataloged at all is not bindable either.
    let unknown = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.no_such_guard.v1";
    assert!(!is_bindable_builtin(unknown));
    assert!(!is_bindable(ChainKind::Upstream, unknown));
}

/// `F7` — a bare UUID tail is accepted on an upstream chain only.
#[test]
fn a_bare_uuid_is_bindable_on_the_upstream_chain_alone() {
    assert!(is_bindable(ChainKind::Upstream, ROW_ID));
    assert!(!is_bindable(ChainKind::Route, ROW_ID));
    // A bare token that is not a UUID is not bindable anywhere.
    assert!(!is_bindable(ChainKind::Upstream, "not-a-uuid"));
}

#[test]
fn transform_builtins_are_catalog_only_except_request_id() {
    let bindable: Vec<&str> = TRANSFORM_BUILTINS
        .iter()
        .filter(|entry| entry.bindable)
        .map(|entry| entry.name)
        .collect();
    assert_eq!(bindable, ["request_id"]);
}

/// Build the request context the guards below are evaluated against.
fn request() -> ProxyContext {
    ProxyContext {
        alias: "api.openai.com".to_owned(),
        method: "GET".to_owned(),
        path: "/v1/models".to_owned(),
        query: Vec::new(),
        headers: BTreeMap::new(),
        trace_id: None,
        tenant: uuid::Uuid::from_u128(0xC001),
        subject: uuid::Uuid::from_u128(0x51),
    }
}

#[tokio::test]
async fn guard_plugin_contract_deny_is_propagated() {
    struct Deny;
    #[async_trait::async_trait]
    impl crate::domain::plugin::GuardPlugin for Deny {
        fn gts_id(&self) -> String {
            REQUIRED_HEADERS.to_owned()
        }
        fn plugin_type(&self) -> &str {
            GUARD_PLUGIN_TYPE
        }
        async fn guard_request(
            &self,
            _request: &ProxyContext,
        ) -> Result<GuardDecision, crate::domain::error::DomainError> {
            Ok(GuardDecision::Deny("missing x-tenant".to_owned()))
        }
    }

    assert_eq!(
        crate::domain::plugin::GuardPlugin::plugin_type(&Deny),
        GUARD_PLUGIN_TYPE
    );
    let decision = crate::domain::plugin::GuardPlugin::guard_request(&Deny, &request()).await;
    match decision {
        Ok(GuardDecision::Deny(detail)) => assert_eq!(detail, "missing x-tenant"),
        other => panic!("expected Deny, got {other:?}"),
    }
}

/// `F4` — the response side of a guard defaults to allowing, so a request-side
/// guard needs no second body.
#[tokio::test]
async fn guard_response_defaults_to_allowing() {
    struct RequestSide;
    #[async_trait::async_trait]
    impl crate::domain::plugin::GuardPlugin for RequestSide {
        fn gts_id(&self) -> String {
            REQUIRED_HEADERS.to_owned()
        }
        async fn guard_request(
            &self,
            _request: &ProxyContext,
        ) -> Result<GuardDecision, crate::domain::error::DomainError> {
            Ok(GuardDecision::allow())
        }
    }

    let response = super::ResponseContext {
        status: 200,
        headers: BTreeMap::from([("content-type".to_owned(), "application/json".to_owned())]),
        trace_id: Some("trace".to_owned()),
    };
    let decision = crate::domain::plugin::GuardPlugin::guard_response(&RequestSide, &response)
        .await
        .expect("default guard_response allows");
    assert_eq!(decision, GuardDecision::Allow);
}

#[tokio::test]
async fn guard_decision_allow_is_the_permissive_default() {
    assert_eq!(GuardDecision::allow(), GuardDecision::Allow);
}

#[tokio::test]
async fn auth_plugin_contract_carries_an_outcome() {
    use crate::domain::plugin::AuthOutcome;

    struct NoopAuth;
    #[async_trait::async_trait]
    impl crate::domain::plugin::AuthPlugin for NoopAuth {
        fn gts_id(&self) -> String {
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1".to_owned()
        }
        async fn authenticate(
            &self,
            _request: &mut ProxyContext,
        ) -> Result<AuthOutcome, crate::domain::error::DomainError> {
            Ok(AuthOutcome {
                subject: Some("svc.example".to_owned()),
                forwarded_headers: BTreeMap::from([("x-api-key".to_owned(), "k".to_owned())]),
            })
        }
    }

    let mut context = request();
    let outcome = crate::domain::plugin::AuthPlugin::authenticate(&NoopAuth, &mut context)
        .await
        .unwrap();
    assert_eq!(outcome.subject.as_deref(), Some("svc.example"));
    assert_eq!(
        outcome
            .forwarded_headers
            .get("x-api-key")
            .map(String::as_str),
        Some("k")
    );
}

#[tokio::test]
async fn transform_plugin_contract_can_rewrite_the_request() {
    struct AddRequestId;
    #[async_trait::async_trait]
    impl crate::domain::plugin::TransformPlugin for AddRequestId {
        fn gts_id(&self) -> String {
            REQUEST_ID.to_owned()
        }
        async fn transform_request(
            &self,
            request: &mut ProxyContext,
        ) -> Result<(), crate::domain::error::DomainError> {
            request
                .headers
                .insert("x-request-id".to_owned(), "42".to_owned());
            Ok(())
        }
    }

    let mut context = request();
    crate::domain::plugin::TransformPlugin::transform_request(&AddRequestId, &mut context)
        .await
        .unwrap();
    assert_eq!(context.header("X-Request-Id"), Some("42"));
}

/// `F4` — the response side of a transform defaults to a no-op.
#[tokio::test]
async fn transform_plugin_response_and_error_default_to_no_ops() {
    struct RequestSide;
    #[async_trait::async_trait]
    impl crate::domain::plugin::TransformPlugin for RequestSide {
        fn gts_id(&self) -> String {
            REQUEST_ID.to_owned()
        }
        async fn transform_request(
            &self,
            _request: &mut ProxyContext,
        ) -> Result<(), crate::domain::error::DomainError> {
            Ok(())
        }
    }

    let mut response = super::ResponseContext::default();
    crate::domain::plugin::TransformPlugin::transform_response(&RequestSide, &mut response)
        .await
        .expect("default transform_response is a no-op");

    let mut error = super::ErrorContext {
        error: crate::domain::error::DomainError::validation("boom"),
        trace_id: Some("trace".to_owned()),
    };
    crate::domain::plugin::TransformPlugin::transform_error(&RequestSide, &mut error)
        .await
        .expect("default transform_error is a no-op");
    assert_eq!(error.error.to_string(), "boom");
}

/// `F4` — `SharedPlugin` is the dispatch seam of the data-plane slice.
#[test]
fn shared_plugin_dispatches_over_the_three_contracts() {
    use crate::domain::plugin::SharedPlugin;

    struct AnyPlugin;
    #[async_trait::async_trait]
    impl crate::domain::plugin::AuthPlugin for AnyPlugin {
        fn gts_id(&self) -> String {
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1".to_owned()
        }
        async fn authenticate(
            &self,
            _request: &mut ProxyContext,
        ) -> Result<crate::domain::plugin::AuthOutcome, crate::domain::error::DomainError> {
            Ok(crate::domain::plugin::AuthOutcome::default())
        }
    }

    let shared = SharedPlugin::Auth(std::sync::Arc::new(AnyPlugin));
    let plugin = match shared {
        SharedPlugin::Auth(plugin) => plugin,
        SharedPlugin::Guard(_) | SharedPlugin::Transform(_) => {
            panic!("expected the Auth arm")
        }
    };
    assert_eq!(
        plugin.plugin_type(),
        "gts.cf.core.oagw.auth_plugin.v1~",
        "the default plugin_type is the auth base type"
    );
}
