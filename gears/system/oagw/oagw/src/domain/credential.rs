//! The credential-reference boundary
//! (`cpt-cf-oagw-algo-gear-foundation-credential-boundary`).
//!
//! The only credential-bearing positions in the documented model are the
//! `auth.config.*` values and the `headers` values. Every one of them must
//! hold a `cred://` reference: the secret itself lives in `credstore` and is
//! resolved by entry 2.6 at request time, never at configuration time.
//!
//! A rejection names the offending field path and **never** echoes the
//! rejected value, so no error message, log line or API response of this gear
//! carries secret material (`inst-gf-cred-4`/`-5`).

use crate::domain::dto::{is_cred_reference, AuthConfig, CredentialRef, HeadersConfig};
use crate::domain::error::DomainError;

/// Walk a configuration payload and reject every credential-bearing field that
/// does not hold a `cred://` reference
/// (`cpt-cf-oagw-algo-gear-foundation-credential-boundary`).
///
/// The only credential-bearing positions in the documented model are
/// `auth.config.*` values and `headers` values; a rejection names the field
/// and never echoes the rejected value.
// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-credential-boundary:p1:inst-gf-cred-2
// `inst-gf-cred-1`/`-2`: every credential-bearing field of a configuration
// sub-structure is inspected before it is stored, and only a `cred://`
// reference is accepted as a credential value.
// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-credential-boundary:p1:inst-gf-cred-5
// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-credential-boundary:p1:inst-gf-cred-6
// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-credential-boundary:p1:inst-gf-cred-1
pub fn reject_non_cred_reference_values(
    path: &str,
    auth: Option<&AuthConfig>,
    headers: Option<&HeadersConfig>,
) -> Result<(), DomainError> {
    if let Some(auth) = auth {
        if let Some(config) = &auth.config {
            reject_payload(path, "auth.config", config)?;
        }
    }
    if let Some(headers) = headers {
        if let Some(request) = &headers.request {
            for (name, value) in request.set.iter().flatten() {
                reject_header_value(path, "headers.request.set", name, value)?;
            }
            for (name, value) in request.add.iter().flatten() {
                reject_header_value(path, "headers.request.add", name, value)?;
            }
        }
        if let Some(response) = &headers.response {
            for (name, value) in response.set.iter().flatten() {
                reject_header_value(path, "headers.response.set", name, value)?;
            }
            for (name, value) in response.add.iter().flatten() {
                reject_header_value(path, "headers.response.add", name, value)?;
            }
        }
    }
    Ok(())
    // @cpt-end:cpt-cf-oagw-algo-gear-foundation-credential-boundary:p1:inst-gf-cred-1
    // @cpt-end:cpt-cf-oagw-algo-gear-foundation-credential-boundary:p1:inst-gf-cred-2
}
//
// @cpt-end:cpt-cf-oagw-algo-gear-foundation-credential-boundary:p1:inst-gf-cred-6
// @cpt-end:cpt-cf-oagw-algo-gear-foundation-credential-boundary:p1:inst-gf-cred-5
//

fn reject_header_value(
    path: &str,
    position: &str,
    name: &str,
    value: &str,
) -> Result<(), DomainError> {
    // The configured header name is carried separately from the field path so
    // a name that itself contains a dot is still matched against the
    // credential-bearing set below.
    let field = format!("{path}.{position}.{name}");
    // A malformed credential reference is never accepted.
    if value.starts_with(CredentialRef::PREFIX) && !is_cred_reference(value) {
        return Err(field_rejection(&field));
    }
    // A credential-bearing header carries its value by reference: a literal
    // token on one of those names is secret material on the wire and in the
    // stored record.
    if CREDENTIAL_BEARING_HEADERS.contains(&name.to_ascii_lowercase().as_str())
        && !is_cred_reference(value)
    {
        return Err(field_rejection(&field));
    }
    Ok(())
}

// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-credential-boundary:p1:inst-gf-cred-4
/// `inst-gf-cred-3`/`-4`: the rejection names the field and is a fixed
/// sentence, so the rejected value is never echoed.
fn field_rejection(field: &str) -> DomainError {
    DomainError::field_rejection(field, "credential-bearing fields accept a `cred://` reference only")
}
// @cpt-end:cpt-cf-oagw-algo-gear-foundation-credential-boundary:p1:inst-gf-cred-4

// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-credential-boundary:p1:inst-gf-cred-3
/// The header names whose value is a credential by definition; a literal value
/// on one of them is secret material, not configuration.
// @cpt-end:cpt-cf-oagw-algo-gear-foundation-credential-boundary:p1:inst-gf-cred-3
const CREDENTIAL_BEARING_HEADERS: [&str; 7] = [
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "x-auth-token",
    "cookie",
    "set-cookie",
    "www-authenticate",
];

fn reject_payload(path: &str, field: &str, value: &serde_json::Value) -> Result<(), DomainError> {
    match value {
        serde_json::Value::String(text) => {
            if !is_cred_reference(text) && looks_like_secret(text) {
                return Err(DomainError::field_rejection(
                    &format!("{path}.{field}"),
                    "credential-bearing fields accept a `cred://` reference only",
                ));
            }
            Ok(())
        }
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                reject_payload(path, &format!("{field}[{index}]"), item)?;
            }
            Ok(())
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                if is_secret_key(key) && !matches!(item, serde_json::Value::Null) {
                    if let serde_json::Value::String(text) = item {
                        if !is_cred_reference(text) {
                            return Err(DomainError::field_rejection(
                                &format!("{path}.{field}.{key}"),
                                "credential-bearing fields accept a `cred://` reference only",
                            ));
                        }
                        continue;
                    }
                    return Err(DomainError::field_rejection(
                        &format!("{path}.{field}.{key}"),
                        "credential-bearing fields accept a `cred://` reference only",
                    ));
                }
                reject_payload(path, &format!("{field}.{key}"), item)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The key names the documented model treats as credential-bearing. The list
/// carries the secret spellings the shipped auth plugins read —
/// `client_secret_ref` included, which is the OAuth2 plugin's own key and the
/// one a pasted literal most plausibly lands in — alongside the free-form
/// spellings a custom `auth.config` may use. `client_id_ref` is deliberately
/// absent: a client identifier is not a secret.
const SECRET_KEYS: [&str; 9] = [
    "api_key",
    "api_key_ref",
    "password",
    "password_ref",
    "secret_ref",
    "client_secret",
    "client_secret_ref",
    "credential_ref",
    "private_key",
];

fn is_secret_key(key: &str) -> bool {
    SECRET_KEYS.contains(&key)
}

/// A raw string that must never be accepted where a `cred://` reference is
/// required. A value shaped like a bearer/secret literal is rejected; a plain
/// non-secret string value (a token URL, a scope, a header name) is
/// configuration and is accepted.
/// `inst-gf-cred-5`: nothing that is rejected is ever written to a record, a
/// log line, an error message or an API response; `inst-gf-cred-6` leaves the
/// resolution of an accepted reference to entry 2.6.
fn looks_like_secret(value: &str) -> bool {
    const PREFIXES: [&str; 10] = [
        "sk_",
        "sk-",
        "Bearer ",
        "bearer ",
        "Basic ",
        "basic ",
        "ghp_",
        "github_pat_",
        "AKIA",
        "xox",
    ];
    if PREFIXES.iter().any(|prefix| value.starts_with(prefix)) {
        return true;
    }
    // A compact JWS/JWT, the shape every OIDC/OAuth2 provider mints, and a long
    // hexadecimal secret are secret material by shape alone.
    value.starts_with("eyJ") && value.matches('.').count() >= 2
        || value.len() >= 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
