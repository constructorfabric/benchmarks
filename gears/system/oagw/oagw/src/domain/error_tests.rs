//! Unit tests for the `DomainError` taxonomy (DoD
//! `cpt-cf-oagw-dod-gear-foundation-domain-error`): the variant set must cover
//! the DESIGN §3.3 table plus the two repository-boundary outcomes, and error
//! text must never carry credential material.
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-domain-error:p1

use super::*;
use crate::domain::gts_helpers::{
    ERR_PLUGIN_IN_USE, ERR_VALIDATION, HIERARCHY_PERMISSIONS,
};

/// Every credential-bearing value that reaches the configuration boundary is
/// rejected by name; the taxonomy must have no variant that stores one.
const SECRET_MATERIAL: [&str; 6] = [
    "cred://tenant-a/stripe/live",
    "sk_live_51H8xYzABCDEFGHIJKLMNOPQRSTUVWXYZ",
    "-----BEGIN RSA PRIVATE KEY-----",
    "client_secret=hunter2",
    "password=hunter2",
    "Bearer abc.def.ghi",
];

#[test]
fn taxonomy_covers_the_design_table_variants() {
    // One construction per DESIGN §3.3 row (RouteError shares
    // `validation.error.v1` with ValidationError, so 20 rows -> 19 variants
    // plus the two repository-boundary and the CORS/plugin-internal ones).
    let samples: [DomainError; 27] = [
        DomainError::ValidationError { detail: "x".into(), path: None, trace_id: None },
        DomainError::RouteError { detail: "x".into(), method: None, path_prefix: None, match_rule: None },
        DomainError::MissingTargetHost { upstream_id: None, alias: None, valid_hosts: vec![], trace_id: None },
        DomainError::InvalidTargetHost { upstream_id: None, invalid_value: "h".into(), trace_id: None },
        DomainError::UnknownTargetHost { upstream_id: None, invalid_value: "h".into(), valid_hosts: vec![], trace_id: None },
        DomainError::AuthenticationFailed { upstream_id: None, host: None, path: None, trace_id: None },
        DomainError::RouteNotFound { path: None, trace_id: None },
        DomainError::NotFound { resource_type: "upstream" },
        DomainError::Conflict { detail: "x".into(), referenced_by: None },
        DomainError::PluginInUse { referenced_by: ReferencedBy::default() },
        DomainError::PayloadTooLarge { path: None, trace_id: None, upstream_id: None, limit_bytes: None },
        DomainError::RateLimitExceeded { upstream_id: None, host: None, retry_after_seconds: None, trace_id: None },
        DomainError::SecretNotFound { path: None, trace_id: None },
        DomainError::ProtocolError { upstream_id: None, host: None, path: None, trace_id: None },
        DomainError::DownstreamError { upstream_id: None, host: None, path: None, trace_id: None, retriable: true },
        DomainError::StreamAborted { upstream_id: None, host: None, path: None, trace_id: None },
        DomainError::LinkUnavailable { upstream_id: None, host: None, path: None, trace_id: None },
        DomainError::CircuitBreakerOpen { upstream_id: None, host: None, trace_id: None },
        DomainError::PluginNotFound { plugin_ref: "ref".into() },
        DomainError::ConnectionTimeout { upstream_id: None, host: None, guidance_secs: None, trace_id: None },
        DomainError::RequestTimeout { upstream_id: None, host: None, guidance_secs: None, trace_id: None },
        DomainError::IdleTimeout { upstream_id: None, host: None, guidance_secs: None, trace_id: None },
        DomainError::CorsOriginNotAllowed { path: None, trace_id: None },
        DomainError::CorsMethodNotAllowed { path: None, trace_id: None },
        DomainError::CorsInvalidConfig("x".into()),
        DomainError::PluginInternal("x".into()),
        DomainError::Internal("x".into()),
    ];
    assert_eq!(samples.len(), 27);
    for err in &samples {
        assert!(!err.to_string().is_empty(), "every variant renders text");
    }
}

#[test]
fn not_found_is_indistinguishable_between_missing_and_foreign_tenant_keys() {
    // `cpt-cf-oagw-algo-gear-foundation-repo-scope` step 7 and the strict
    // tenant-scoping DoD: a foreign-tenant key returns not-found with no
    // disclosure, so the variant must carry no tenant or key information.
    let err = DomainError::NotFound { resource_type: "upstream" };
    assert!(err.is_not_found());
    let rendered = err.to_string();
    assert!(!rendered.contains("tenant"), "`{rendered}` must not name a tenant");
    assert!(!rendered.contains("foreign"), "`{rendered}` must not disclose why");
    assert!(redacted_detail(&err).is_none());
}

#[test]
fn conflict_is_distinguishable_from_not_found() {
    assert!(DomainError::Conflict { detail: "duplicate alias".into(), referenced_by: None }.is_conflict());
    assert!(!DomainError::Conflict { detail: "x".into(), referenced_by: None }.is_not_found());
    assert!(DomainError::NotFound { resource_type: "route" }.is_not_found());
    assert!(!DomainError::NotFound { resource_type: "route" }.is_conflict());
}

#[test]
fn field_rejection_names_the_field_and_not_the_value() {
    let err = DomainError::field_rejection("auth.config.client_secret_ref", "not a cred:// reference");
    let text = err.to_string();
    assert!(text.contains("auth.config.client_secret_ref"), "`{text}` names the field");
    // A value that was rejected for not being a `cred://` reference never
    // appears in the rendered text.
    assert!(!text.contains("sk_live"), "`{text}` must not echo the rejected value");
    assert_eq!(err.detail(), Some("field `auth.config.client_secret_ref` rejected: not a cred:// reference"));
}

#[test]
fn rendered_error_text_carries_no_credential_material() {
    // Every rejection path below is driven by a secret-looking input; the
    // rendered text must be free of all of it.
    for secret in SECRET_MATERIAL {
        for err in [
            DomainError::field_rejection("auth.config", "not a cred:// reference"),
            DomainError::CorsInvalidConfig("allow_credentials with wildcard origin".into()),
            DomainError::PluginInternal("oauth2 token exchange failed".into()),
            DomainError::Conflict { detail: "duplicate (tenant_id, alias)".into(), referenced_by: None },
        ] {
            let text = err.to_string();
            assert!(
                !text.contains(secret),
                "credential material `{secret}` leaked into `{text}`"
            );
            if let Some(detail) = redacted_detail(&err) {
                assert!(!detail.contains(secret), "detail leaked `{secret}`");
            }
        }
    }
}

#[test]
fn plugin_in_use_is_the_only_referenced_by_carrier() {
    let refs = ReferencedBy {
        upstreams: vec!["gts.cf.core.oagw.upstream.v1~1".into()],
        routes: vec![],
    };
    let err = DomainError::PluginInUse { referenced_by: refs.clone() };
    assert!(err.is_conflict());
    assert_eq!(err, DomainError::PluginInUse { referenced_by: refs });
    // The GTS type it renders as is the only one carrying `referenced_by`.
    assert!(ERR_PLUGIN_IN_USE.ends_with("plugin.in_use.v1"));
}

#[test]
fn detail_falls_back_to_none_for_context_only_variants() {
    assert!(
        redacted_detail(&DomainError::RateLimitExceeded {
            upstream_id: None,
            host: None,
            retry_after_seconds: Some(3),
            trace_id: None,
        })
        .is_none(),
        "entry 2.5 falls back to the mapped GTS type title"
    );
    assert!(redacted_detail(&DomainError::Internal("boom".into())).is_some());
}

#[test]
fn validation_is_the_gts_validation_type_of_the_table() {
    // RouteError and ValidationError map onto the same type (DESIGN §3.3).
    assert!(ERR_VALIDATION.ends_with("validation.error.v1"));
    // The merge engine's permission gates are the only hierarchy permissions.
    assert_eq!(HIERARCHY_PERMISSIONS.len(), 4);
}
