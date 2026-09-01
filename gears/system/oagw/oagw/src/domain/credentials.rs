// Created: 2026-08-31 by Constructor Tech
//! Credential-shape guard (DESIGN §2.2 `nfr-credential-isolation`, ADR-0002,
//! ADR-0008).
//!
//! The control plane never resolves or stores secret material: bindings
//! reference secrets through `cred://` URIs that the data plane hands to
//! `cred_store` at request time. These checks run **on the write path** so a
//! secret pasted into a config member is rejected before it reaches the store
//! and, later, a response body or a log line.
//!
//! Allowed inline values, per ADR-0008: the `OAuth2` `scopes` list only — scopes
//! are identifiers, not credentials. Every other member that names a
//! credential must be a `cred://` reference.

use serde_json::Value;

use crate::domain::model::{AuthConfig, PluginKind};
use crate::domain::plugin::{PluginRef, lookup_built_in};
use crate::error::{OagwError, OagwResult};

/// Members ADR-0008 allows to carry an inline (non-reference) value.
const INLINE_ALLOWED: &[&str] = &["scopes"];

/// Member names that carry credential material when written inline.
const CREDENTIAL_MEMBERS: &[&str] = &[
    "api_key",
    "apikey",
    "authorization",
    "bearer",
    "client_id",
    "client_secret",
    "credentials",
    "password",
    "passwd",
    "private_key",
    "secret",
    "token",
];

/// Suffixes that mark a member as a credential or a credential reference.
const CREDENTIAL_SUFFIXES: &[&str] = &["_key", "_password", "_ref", "_secret", "_token"];

/// Scheme every credential reference must use.
const CREDENTIAL_SCHEME: &str = "cred://";

/// Validate an `auth` binding (upstream schema `auth`).
///
/// # Errors
/// 400 when the binding names no plugin but is present, when a credential
/// member holds an inline value or a reference does not use the `cred://`
/// scheme, and when a built-in `OAuth2` binding omits one of the members
/// ADR-0008 requires.
pub fn validate_auth_config(auth: &AuthConfig) -> OagwResult<()> {
    validate_members(&auth.raw, &mut |path| is_inline_allowed_auth_member(path))?;
    if let Some(plugin_type) = auth.plugin_type.as_deref() {
        if plugin_type.trim().is_empty() {
            return Err(OagwError::validation(
                "auth binding 'type' must name a plugin; omit the whole 'auth' member to forward                  without credentials",
            )
            .with_extension(|ext| ext.invalid_value = Some(plugin_type.to_owned())));
        }
        validate_oauth2_binding(plugin_type, effective_members(&auth.raw))?;
    }
    Ok(())
}

/// The members an auth plugin actually reads.
///
/// ADR-0008 spells a binding with the plugin members nested under `config`;
/// the flat spelling (`"auth": { "type": …, "key_ref": … }`) is accepted as
/// well, which makes the nested form the one that decides when both are
/// present — exactly what
/// [`auth_plugin_config`](crate::domain::proxy::plugins::auth_plugin_config)
/// reads on the data plane, so what is validated here is what is enforced
/// there.
fn effective_members(raw: &serde_json::Map<String, Value>) -> &serde_json::Map<String, Value> {
    match raw.get("config") {
        Some(Value::Object(nested)) => nested,
        _ => raw,
    }
}

/// Validate the `config` object of a custom plugin.
///
/// # Errors
/// 400 when a credential member holds an inline value or a non-`cred://`
/// reference.
pub fn validate_plugin_config(config: &Value) -> OagwResult<()> {
    match config {
        Value::Object(map) => validate_members(map, &mut |_path| false),
        _ => Ok(()),
    }
}

/// Whether `path` names a member ADR-0008 allows inline.
///
/// `path` is the member chain below the binding root, e.g. `["scopes"]`;
/// nested occurrences are not exempt.
fn is_inline_allowed_auth_member(path: &[String]) -> bool {
    matches!(path, [member]
        if INLINE_ALLOWED
            .iter()
            .any(|allowed| member.eq_ignore_ascii_case(allowed)))
}

/// `OAuth2` client-credentials bindings (ADR-0008 "Plugin Config").
fn validate_oauth2_binding(
    plugin_type: &str,
    raw: &serde_json::Map<String, Value>,
) -> OagwResult<()> {
    if !is_oauth2_client_cred(plugin_type) {
        return Ok(());
    }
    // `raw` is already the effective member set (see `effective_members`), so
    // both the ADR-0008 nested spelling and the flat one validate.
    for member in ["client_id_ref", "client_secret_ref"] {
        if !raw.get(member).is_some_and(Value::is_string) {
            return Err(OagwError::validation(format!(
                "auth binding '{plugin_type}' requires the '{member}' cred:// reference"
            )));
        }
    }
    let has_endpoint = raw.get("token_endpoint").is_some();
    let has_issuer = raw.get("issuer_url").is_some();
    if has_endpoint == has_issuer {
        return Err(OagwError::validation(format!(
            "auth binding '{plugin_type}' requires exactly one of 'token_endpoint' or \
             'issuer_url'"
        )));
    }
    Ok(())
}

/// Whether `plugin_type` names one of the two `OAuth2` client-credentials
/// built-ins, in either the short or the GTS spelling.
fn is_oauth2_client_cred(plugin_type: &str) -> bool {
    let oauth2 = ["oauth2_client_cred", "oauth2_client_cred_basic"];
    match PluginRef::parse(plugin_type) {
        PluginRef::BuiltIn {
            kind: PluginKind::Auth,
            name,
            ..
        } => oauth2.iter().any(|candidate| {
            lookup_built_in(PluginKind::Auth, candidate).is_some_and(|plugin| plugin.name == name)
        }),
        _ => false,
    }
}

/// Walk `map`, applying `rule` to every member.
///
/// `rule` receives the member path below the root and answers whether an
/// inline value is permitted there.
fn validate_members(
    map: &serde_json::Map<String, Value>,
    rule: &mut dyn FnMut(&[String]) -> bool,
) -> OagwResult<()> {
    let mut path = Vec::new();
    for (member, nested) in map {
        path.push(member.clone());
        check_member(member, nested, &path, rule)?;
        walk(nested, &mut path, rule)?;
        path.pop();
    }
    Ok(())
}

fn walk(
    value: &Value,
    path: &mut Vec<String>,
    rule: &mut dyn FnMut(&[String]) -> bool,
) -> OagwResult<()> {
    match value {
        Value::Object(map) => {
            for (member, nested) in map {
                path.push(member.clone());
                check_member(member, nested, path, rule)?;
                walk(nested, path, rule)?;
                path.pop();
            }
            Ok(())
        }
        Value::Array(items) => {
            for item in items {
                walk(item, path, rule)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn check_member(
    member: &str,
    value: &Value,
    path: &[String],
    rule: &mut dyn FnMut(&[String]) -> bool,
) -> OagwResult<()> {
    if !is_credential_member(member) || rule(path) {
        return Ok(());
    }
    if value
        .as_str()
        .is_some_and(|reference| reference.starts_with(CREDENTIAL_SCHEME))
    {
        return Ok(());
    }
    Err(inline_rejected(member))
}

/// Whether `member` names credential material or a credential reference.
fn is_credential_member(member: &str) -> bool {
    let lowered = member.to_ascii_lowercase();
    CREDENTIAL_MEMBERS.contains(&lowered.as_str())
        || CREDENTIAL_SUFFIXES
            .iter()
            .any(|suffix| lowered.ends_with(suffix))
}

fn inline_rejected(member: &str) -> OagwError {
    OagwError::validation(format!(
        "'{member}' must reference a secret as 'cred://…'; inline credential material is not \
         accepted"
    ))
    .with_extension(|ext| ext.invalid_value = Some(member.to_owned()))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::domain::credentials::{validate_auth_config, validate_plugin_config};
    use crate::domain::model::{AuthConfig, SharingMode};
    use crate::error::OagwErrorKind;

    /// Binding with `type` fixed to the API-key built-in, plus `raw`.
    fn auth(raw: &Value) -> AuthConfig {
        let mut map = serde_json::Map::new();
        if let Some(object) = raw.as_object() {
            for (key, member) in object {
                map.insert(key.clone(), member.clone());
            }
        }
        AuthConfig {
            plugin_type: Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1".to_owned()),
            sharing: SharingMode::Private,
            raw: map,
        }
    }

    fn oauth2(raw: &Value) -> AuthConfig {
        let mut binding = auth(raw);
        binding.plugin_type =
            Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1".to_owned());
        binding
    }

    fn kind_of(error: &crate::error::OagwError) -> crate::error::OagwErrorKind {
        *error.kind()
    }

    #[test]
    fn a_cred_reference_is_accepted() -> crate::error::OagwResult<()> {
        let binding = auth(&json!({"secret_ref": "cred://partner-openai-key"}));
        validate_auth_config(&binding)
    }

    #[test]
    fn inline_secrets_are_rejected_with_the_member_name() {
        for value in [
            json!({"secret_ref": "sk-live-abcdef"}),
            json!({"api_key": "sk-live-abcdef"}),
            json!({"password": "hunter2"}),
            json!({"nested": {"client_secret": "abc"}}),
            json!({"client_id_ref": 42}),
        ] {
            let binding = auth(&value);
            let Err(error) = validate_auth_config(&binding) else {
                panic!("expected an inline credential rejection");
            };
            assert_eq!(kind_of(&error), OagwErrorKind::Validation);
        }
    }

    #[test]
    fn oauth2_scopes_may_be_inline() -> crate::error::OagwResult<()> {
        let binding = oauth2(&json!({
            "client_id_ref": "cred://ms-graph-client-id",
            "client_secret_ref": "cred://ms-graph-client-secret",
            "issuer_url": "https://login.microsoftonline.com/",
            "scopes": "https://graph.microsoft.com/.default",
        }));
        validate_auth_config(&binding)
    }

    #[test]
    fn a_credential_member_inside_a_nested_object_is_still_guarded() {
        let binding = auth(&json!({
            "secret_ref": "cred://partner-openai-key",
            "deeper": {"scopes": "openid", "api_key": "inline"},
        }));
        assert!(validate_auth_config(&binding).is_err());
    }

    #[test]
    fn oauth2_bindings_need_both_references_and_one_endpoint() {
        let missing_id = oauth2(&json!({
            "client_secret_ref": "cred://secret",
            "token_endpoint": "https://login.microsoftonline.com/token",
        }));
        assert!(validate_auth_config(&missing_id).is_err());

        let missing_endpoint = oauth2(&json!({
            "client_id_ref": "cred://id",
            "client_secret_ref": "cred://secret",
        }));
        assert!(validate_auth_config(&missing_endpoint).is_err());

        let both_endpoints = oauth2(&json!({
            "client_id_ref": "cred://id",
            "client_secret_ref": "cred://secret",
            "token_endpoint": "https://idp/token",
            "issuer_url": "https://idp/",
        }));
        assert!(validate_auth_config(&both_endpoints).is_err());
    }

    #[test]
    fn non_oauth2_bindings_skip_the_shape_check() -> crate::error::OagwResult<()> {
        let binding = auth(&json!({"secret_ref": "cred://partner-openai-key"}));
        validate_auth_config(&binding)
    }

    #[test]
    fn plugin_configs_are_guarded_too() {
        let config = json!({"fields": ["a"], "api_key": "inline"});
        let Err(error) = validate_plugin_config(&config) else {
            panic!("expected an inline credential rejection");
        };
        assert_eq!(kind_of(&error), OagwErrorKind::Validation);

        let harmless = json!({"fields": ["a"], "max_body_size": 1024});
        assert!(validate_plugin_config(&harmless).is_ok());
    }

    #[test]
    fn non_object_configs_are_ignored() -> crate::error::OagwResult<()> {
        validate_plugin_config(&Value::Null)?;
        validate_plugin_config(&json!(["a", "b"]))
    }

    #[test]
    fn an_empty_binding_stays_legal() -> crate::error::OagwResult<()> {
        validate_auth_config(&AuthConfig {
            plugin_type: Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1".to_owned()),
            sharing: SharingMode::Inherit,
            raw: serde_json::Map::new(),
        })
    }
}
