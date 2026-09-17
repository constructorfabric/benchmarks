use super::super::model::{Endpoint, Scheme};
use super::*;

fn ep(scheme: Scheme, host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme,
        host: host.to_owned(),
        port,
    }
}

#[test]
fn single_hostname_standard_port_derives_the_hostname() {
    // `api.openai.com:443` -> `api.openai.com`
    let derived = compute_derived_alias(&[ep(Scheme::Https, "api.openai.com", 443)]);
    assert_eq!(derived.as_deref(), Some("api.openai.com"));
}

#[test]
fn single_hostname_non_standard_port_derives_host_port() {
    // `api.openai.com:8443` -> `api.openai.com:8443`
    let derived = compute_derived_alias(&[ep(Scheme::Https, "api.openai.com", 8443)]);
    assert_eq!(derived.as_deref(), Some("api.openai.com:8443"));
}

#[test]
fn single_http_hostname_derives_host_without_port_80() {
    let derived = compute_derived_alias(&[ep(Scheme::Http, "internal.svc", 80)]);
    assert_eq!(derived.as_deref(), Some("internal.svc"));
}

#[test]
fn single_hostname_derives_without_psl_validation() {
    // `localhost` is not a registrable domain, but a single hostname is always
    // derivable per the contract table.
    let derived = compute_derived_alias(&[ep(Scheme::Https, "localhost", 443)]);
    assert_eq!(derived.as_deref(), Some("localhost"));
}

#[test]
fn multi_hostname_registrable_suffix_is_derived() {
    // `us.vendor.com` + `eu.vendor.com` -> `vendor.com`
    let derived = compute_derived_alias(&[
        ep(Scheme::Https, "us.vendor.com", 443),
        ep(Scheme::Https, "eu.vendor.com", 443),
    ]);
    assert_eq!(derived.as_deref(), Some("vendor.com"));
}

#[test]
fn multi_hostname_non_standard_port_keeps_the_port() {
    // `us.vendor.com:8443` + `eu.vendor.com:8443` -> `vendor.com:8443`
    let derived = compute_derived_alias(&[
        ep(Scheme::Https, "us.vendor.com", 8443),
        ep(Scheme::Https, "eu.vendor.com", 8443),
    ]);
    assert_eq!(derived.as_deref(), Some("vendor.com:8443"));
}

#[test]
fn bare_public_suffix_is_not_derivable() {
    // `foo.co.uk` + `bar.co.uk` -> not derivable (`co.uk` is a public suffix)
    let derived = compute_derived_alias(&[
        ep(Scheme::Https, "foo.co.uk", 443),
        ep(Scheme::Https, "bar.co.uk", 443),
    ]);
    assert_eq!(derived, None);
}

#[test]
fn unrelated_hostnames_are_not_derivable() {
    // `us.foo.com` + `eu.bar.com` -> explicit alias required
    let derived = compute_derived_alias(&[
        ep(Scheme::Https, "us.foo.com", 443),
        ep(Scheme::Https, "eu.bar.com", 443),
    ]);
    assert_eq!(derived, None);
}

#[test]
fn ip_endpoints_are_never_derivable() {
    // `10.0.1.1` + `10.0.1.2` -> explicit alias required
    let derived = compute_derived_alias(&[
        ep(Scheme::Http, "10.0.1.1", 80),
        ep(Scheme::Http, "10.0.1.2", 80),
    ]);
    assert_eq!(derived, None);
    let single = compute_derived_alias(&[ep(Scheme::Http, "127.0.0.1", 8080)]);
    assert_eq!(single, None);
}

#[test]
fn mixed_ip_and_hostname_is_not_derivable() {
    let derived = compute_derived_alias(&[
        ep(Scheme::Https, "a.vendor.com", 443),
        ep(Scheme::Https, "10.0.1.2", 443),
    ]);
    assert_eq!(derived, None);
}

#[test]
fn derive_alias_accepts_the_exact_derived_value() {
    let outcome = derive_alias(
        &[ep(Scheme::Https, "api.openai.com", 443)],
        Some("Api.OpenAI.COM."),
    )
    .expect("tolerated");
    assert_eq!(outcome, AliasOutcome::Derived("api.openai.com".to_owned()));
}

#[test]
fn derive_alias_rejects_a_mismatched_explicit_alias() {
    let error = derive_alias(
        &[ep(Scheme::Https, "api.openai.com", 443)],
        Some("my-upstream"),
    )
    .expect_err("rejected");
    assert!(error.detail.contains("auto-derive"));
}

#[test]
fn derive_alias_requires_an_explicit_alias_for_ips() {
    let error = derive_alias(&[ep(Scheme::Http, "127.0.0.1", 8080)], None).expect_err("rejected");
    assert!(error.detail.contains("explicit alias required"));
}

#[test]
fn derive_alias_normalizes_and_validates_explicit_values() {
    let outcome =
        derive_alias(&[ep(Scheme::Http, "127.0.0.1", 8080)], Some("My-Service")).expect("valid");
    assert_eq!(outcome, AliasOutcome::Explicit("my-service".to_owned()));

    let malformed = derive_alias(&[ep(Scheme::Http, "127.0.0.1", 8080)], Some("-bad-"));
    assert!(malformed.is_err());
}

#[test]
fn alias_update_matrix_is_enforced() {
    // Derivable -> derivable, same alias: allowed.
    assert!(
        enforce_alias_update(
            "api.openai.com",
            true,
            &[ep(Scheme::Https, "api.openai.com", 443)],
            None
        )
        .is_ok()
    );

    // Derivable -> derivable, different alias: rejected.
    let changed = enforce_alias_update(
        "api.openai.com",
        true,
        &[ep(Scheme::Https, "other.openai.com", 443)],
        None,
    );
    assert!(changed.is_err());

    // Derivable -> non-derivable: rejected even with an explicit alias.
    let to_ip = enforce_alias_update(
        "api.openai.com",
        true,
        &[ep(Scheme::Http, "127.0.0.1", 80)],
        Some("api.openai.com"),
    );
    assert!(to_ip.is_err());

    // Non-derivable -> non-derivable with the same alias: retained.
    assert!(
        enforce_alias_update(
            "my-service",
            false,
            &[ep(Scheme::Http, "10.0.0.1", 80)],
            Some("my-service")
        )
        .is_ok()
    );

    // Non-derivable -> non-derivable with a different alias: rejected.
    let renamed = enforce_alias_update(
        "my-service",
        false,
        &[ep(Scheme::Http, "10.0.0.2", 80)],
        Some("other"),
    );
    assert!(renamed.is_err());

    // Non-derivable -> derivable, derived != existing: rejected.
    let to_host = enforce_alias_update(
        "my-service",
        false,
        &[ep(Scheme::Https, "api.openai.com", 443)],
        None,
    );
    assert!(to_host.is_err());

    // No endpoint change, exact-match alias tolerated.
    assert!(
        enforce_alias_update(
            "api.openai.com",
            true,
            &[ep(Scheme::Https, "api.openai.com", 443)],
            Some("api.openai.com")
        )
        .is_ok()
    );

    // No endpoint change, alias override rejected.
    let override_attempt = enforce_alias_update(
        "api.openai.com",
        true,
        &[ep(Scheme::Https, "api.openai.com", 443)],
        Some("other"),
    );
    assert!(override_attempt.is_err());
}

#[test]
fn is_derivable_matches_compute() {
    assert!(is_derivable(&[ep(Scheme::Https, "api.openai.com", 443)]));
    assert!(!is_derivable(&[ep(Scheme::Http, "10.0.0.1", 80)]));
}
