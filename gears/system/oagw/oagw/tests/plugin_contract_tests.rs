//! Plugin contract and registry tests.
//!
//! Covers `cpt-cf-oagw-dod-plugin-contracts-registries` and the steps of
//! `cpt-cf-oagw-algo-plugin-contract-registry` on the contract surface alone:
//! the three contracts with one registry each, the separation that keeps an
//! auth identifier out of the guard and transform registries, the
//! reserved-versus-unknown distinction a catalog-only identifier answers with,
//! the phases each family declares, and the sandbox limits the surface exposes
//! and enforces none of.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

// @cpt-dod:cpt-cf-oagw-dod-plugin-contracts-registries:p1
// @cpt-dod:cpt-cf-oagw-dod-plugin-tests:p1

use std::sync::Arc;

use oagw::domain::context::{AuthContext, RequestContext, ResponseContext};
use oagw::domain::error::ErrorContext;
use oagw::domain::plugin_contract::{
    AuthPlugin, AuthPluginRegistry, GuardDecision, GuardPlugin, GuardPluginRegistry, PluginFamily,
    PluginFailure, PluginPhase, PluginResolveError, SandboxLimits, TransformPlugin,
    TransformPluginRegistry, SANDBOX_LIMITS,
};

/// The four auth identifiers the built-in catalogue backs.
const AUTH_IDS: [&str; 4] = [
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1",
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1",
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1",
];

/// The two catalog-only auth identifiers.
const RESERVED_AUTH_IDS: [&str; 2] = [
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
];

/// The one catalog-only guard identifier and the two transform ones.
const RESERVED_OTHER_IDS: [&str; 3] = [
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1",
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1",
];

/// A do-nothing auth plugin, the stand-in the registry tests resolve.
struct StubAuth;

#[async_trait::async_trait]
impl AuthPlugin for StubAuth {
    fn declares(&self, phase: PluginPhase) -> bool {
        matches!(phase, PluginPhase::Auth)
    }

    async fn authenticate(
        &self,
        _ctx: &mut AuthContext,
        _config: &serde_json::Value,
    ) -> Result<(), PluginFailure> {
        Ok(())
    }
}

/// A do-nothing guard plugin.
struct StubGuard;

impl GuardPlugin for StubGuard {
    fn declares(&self, phase: PluginPhase) -> bool {
        matches!(phase, PluginPhase::GuardRequest)
    }

    fn guard_request(&self, _ctx: &RequestContext, _config: &serde_json::Value) -> GuardDecision {
        GuardDecision::Allow
    }

    fn guard_response(
        &self,
        _ctx: &ResponseContext,
        _config: &serde_json::Value,
    ) -> GuardDecision {
        GuardDecision::Allow
    }
}

/// A do-nothing transform plugin.
struct StubTransform;

impl TransformPlugin for StubTransform {
    fn declares(&self, phase: PluginPhase) -> bool {
        matches!(phase, PluginPhase::TransformRequest)
    }

    fn transform_request(&self, _ctx: &mut RequestContext, _config: &serde_json::Value) {}

    fn transform_response(&self, _ctx: &mut ResponseContext, _config: &serde_json::Value) {}

    fn transform_error(&self, _ctx: &mut ErrorContext, _config: &serde_json::Value) {}
}

/// One registry of each family with the stub registered under its own
/// identifier.
fn registries() -> (
    AuthPluginRegistry,
    GuardPluginRegistry,
    TransformPluginRegistry,
) {
    let mut auth = AuthPluginRegistry::new();
    auth.register(AUTH_IDS[0], Arc::new(StubAuth));
    let mut guard = GuardPluginRegistry::new();
    guard.register(
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
        Arc::new(StubGuard),
    );
    let mut transform = TransformPluginRegistry::new();
    transform.register(
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1",
        Arc::new(StubTransform),
    );
    (auth, guard, transform)
}

#[test]
fn an_identifier_parses_into_its_base_type_and_instance_part() {
    // The instance part is the substring after the `~` separator.
    let (family, instance) = PluginFamily::parse_identifier(AUTH_IDS[0])
        .expect("a full anonymous GTS identifier parses");
    assert_eq!(family, Some(PluginFamily::Auth));
    assert_eq!(instance, "cf.core.oagw.noop.v1");

    // A bare UUID names a custom plugin row and parses with no family.
    let (family, instance) =
        PluginFamily::parse_identifier("0f0e0d0c-0b0a-4918-8267-5a4b3c2d1e0f")
            .expect("a bare UUID parses");
    assert_eq!(instance, "0f0e0d0c-0b0a-4918-8267-5a4b3c2d1e0f");
    assert!(family.is_none(), "a bare UUID names no plugin family");
}

#[test]
fn an_identifier_that_is_not_a_plugin_gts_id_parses_to_nothing() {
    assert!(PluginFamily::parse_identifier("").is_none());
    assert!(PluginFamily::parse_identifier("not-an-identifier").is_none());
    assert!(PluginFamily::parse_identifier("gts.cf.core.oagw.upstream.v1~x.v1").is_none());
}

#[test]
fn the_type_literal_names_one_of_three_families() {
    assert_eq!(PluginFamily::from_type_literal("auth"), Some(PluginFamily::Auth));
    assert_eq!(
        PluginFamily::from_type_literal("guard"),
        Some(PluginFamily::Guard)
    );
    assert_eq!(
        PluginFamily::from_type_literal("transform"),
        Some(PluginFamily::Transform)
    );
    // No fourth literal exists, and none of the three is spelled differently.
    assert_eq!(PluginFamily::from_type_literal("auth_plugin"), None);
    assert_eq!(PluginFamily::from_type_literal(""), None);
    assert_eq!(PluginFamily::from_type_literal("Auth"), None);
}

#[test]
fn each_family_declares_the_phases_its_contract_exposes() {
    assert_eq!(
        PluginFamily::Auth.supported_phases(),
        &[PluginPhase::Auth][..]
    );
    assert_eq!(
        PluginFamily::Guard.supported_phases(),
        &[PluginPhase::GuardRequest, PluginPhase::GuardResponse][..]
    );
    assert_eq!(
        PluginFamily::Transform.supported_phases(),
        &[
            PluginPhase::TransformRequest,
            PluginPhase::TransformResponse,
            PluginPhase::TransformError,
        ][..]
    );
}

#[test]
fn a_registered_entry_resolves_from_its_own_registry() {
    let (auth, guard, transform) = registries();
    assert!(auth.resolve(AUTH_IDS[0]).is_ok());
    assert!(guard
        .resolve("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1")
        .is_ok());
    assert!(transform
        .resolve("gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1")
        .is_ok());
}

#[test]
fn the_three_registries_stay_separate() {
    // An auth identifier is never looked up in the guard or transform registry.
    let (auth, guard, transform) = registries();
    assert!(matches!(
        guard.resolve(AUTH_IDS[0]),
        Err(PluginResolveError::Unknown { .. })
    ));
    assert!(matches!(
        transform.resolve(AUTH_IDS[0]),
        Err(PluginResolveError::Unknown { .. })
    ));
    assert!(auth.resolve(AUTH_IDS[0]).is_ok());
}

#[test]
fn a_reserved_identifier_is_not_resolvable_in_any_registry() {
    let (auth, guard, transform) = registries();
    for identifier in RESERVED_AUTH_IDS {
        assert!(
            matches!(
                auth.resolve(identifier),
                Err(PluginResolveError::Reserved { .. })
            ),
            "{identifier} is catalog-only"
        );
        assert!(matches!(
            guard.resolve(identifier),
            Err(PluginResolveError::Reserved { .. })
        ));
        assert!(matches!(
            transform.resolve(identifier),
            Err(PluginResolveError::Reserved { .. })
        ));
    }
    for identifier in RESERVED_OTHER_IDS {
        assert!(matches!(
            auth.resolve(identifier),
            Err(PluginResolveError::Reserved { .. })
        ));
        assert!(matches!(
            guard.resolve(identifier),
            Err(PluginResolveError::Reserved { .. })
        ));
        assert!(matches!(
            transform.resolve(identifier),
            Err(PluginResolveError::Reserved { .. })
        ));
    }
}

#[test]
fn a_reserved_identifier_is_distinguished_from_an_unknown_one() {
    let (auth, _guard, _transform) = registries();
    // `basic` is in the catalogue table: reserved.
    assert!(matches!(
        auth.resolve(RESERVED_AUTH_IDS[0]),
        Err(PluginResolveError::Reserved { identifier })
            if identifier == RESERVED_AUTH_IDS[0]
    ));
    // A typo names nothing the catalogue reserves and no registry holds.
    let typo = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.opauqe.v1";
    assert!(matches!(
        auth.resolve(typo),
        Err(PluginResolveError::Unknown { identifier }) if identifier == typo
    ));
}

#[test]
fn the_phases_a_registered_entry_declares_are_reported() {
    let (auth, guard, transform) = registries();
    assert_eq!(
        auth.declared_phases(AUTH_IDS[0]),
        Some(vec![PluginPhase::Auth])
    );
    assert_eq!(
        guard.declared_phases("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"),
        Some(vec![PluginPhase::GuardRequest])
    );
    assert_eq!(
        transform.declared_phases("gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"),
        Some(vec![PluginPhase::TransformRequest])
    );
    // An unresolvable identifier declares no phase at all.
    assert_eq!(auth.declared_phases(RESERVED_AUTH_IDS[0]), None);
}

#[test]
fn a_registered_entry_does_not_resolve_in_a_phase_it_does_not_declare() {
    let (_auth, guard, _transform) = registries();
    // The stub declares the request phase only, so the response phase is not
    // resolvable even though the identifier is.
    assert!(matches!(
        guard.resolve_for_phase(
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
            PluginPhase::GuardResponse,
        ),
        Err(PluginResolveError::PhaseNotDeclared { .. })
    ));
    assert!(guard
        .resolve_for_phase(
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
            PluginPhase::GuardRequest,
        )
        .is_ok());
}

#[test]
fn the_sandbox_limits_are_exposed_and_none_is_enforced_here() {
    // The limits are part of the contract surface, stated as values a caller
    // can read; the enforcement is execution-time work of the data plane.
    assert_eq!(
        SANDBOX_LIMITS,
        SandboxLimits {
            network_io: false,
            file_io: false,
            imports: false,
            max_invocation_millis: 100,
            max_invocation_memory_bytes: 10 * 1024 * 1024,
        }
    );
}

#[test]
fn an_auth_plugin_injects_into_the_context_it_is_given() {
    // The contract consumes the foundation's context types rather than
    // declaring its own transport shape.
    let mut ctx = AuthContext::new(uuid::Uuid::from_u128(0x10), Some(uuid::Uuid::from_u128(0x20)));
    ctx.set_header("authorization", "Bearer token");
    assert_eq!(ctx.header("authorization"), Some("Bearer token"));
    assert_eq!(ctx.tenant_id, uuid::Uuid::from_u128(0x10));
    assert_eq!(ctx.subject_id(), Some(uuid::Uuid::from_u128(0x20)));
}

#[test]
fn a_guard_decision_carries_the_code_and_message_of_a_rejection() {
    let allow = GuardDecision::Allow;
    assert!(allow.is_allowed());
    let reject = GuardDecision::reject("REQUIRED_HEADER_MISSING", "the x-request-id header is absent");
    assert!(!reject.is_allowed());
    match reject {
        GuardDecision::Reject { code, message } => {
            assert_eq!(code, "REQUIRED_HEADER_MISSING");
            assert_eq!(message, "the x-request-id header is absent");
        }
        GuardDecision::Allow => unreachable!("the decision above rejected"),
    }
}

#[test]
fn a_request_context_carries_the_selector_a_guard_reads() {
    let ctx = RequestContext::new(
        String::from("GET"),
        String::from("/v1/chat"),
        Some(String::from("model=gpt")),
    );
    assert_eq!(ctx.method, "GET");
    assert_eq!(ctx.path, "/v1/chat");
    assert_eq!(ctx.query.as_deref(), Some("model=gpt"));
}

#[test]
fn a_response_context_carries_the_status_a_response_guard_reads() {
    let mut ctx = ResponseContext::new(200);
    ctx.set_header("x-upstream", "a");
    assert_eq!(ctx.status, 200);
    assert_eq!(ctx.header("x-upstream"), Some("a"));
}
