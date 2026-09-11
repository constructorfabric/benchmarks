//! The `noop` and `apikey` auth plugins
//! (`cpt-cf-oagw-dod-builtin-plugin-behaviors`,
//! `cpt-cf-oagw-algo-apikey-injection`).
//!
//! [`NoopAuthPlugin`] leaves the request context untouched, so a `noop` binding
//! is observable as an unchanged request. [`ApiKeyAuthPlugin`] resolves its
//! `secret_ref` through the cred-store client at request time and injects the
//! value verbatim into the one configured target — `key_header` or
//! `key_query` (§1.5) — never transforming, prefixing or truncating it.
// @cpt-begin:cpt-cf-oagw-dod-builtin-plugin-behaviors:p1:inst-full

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use credstore_sdk::CredStoreClientV1;
use http::header::HeaderName;
use serde_json::Value;

use crate::domain::error::OagwError;
use crate::domain::model::CRED_REF_SCHEME;
use crate::domain::plugin::{AUTH_PLUGIN_TYPE, AuthPlugin, RequestContext};
use crate::infra::plugin::registry::{APIKEY_AUTH_PLUGIN_ID, NOOP_AUTH_PLUGIN_ID};
use crate::infra::plugin::token_cache::resolve_secret;

/// The auth plugin that injects nothing: a `noop` binding is an unchanged
/// request.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopAuthPlugin;

#[async_trait::async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        NOOP_AUTH_PLUGIN_ID
    }

    fn plugin_type(&self) -> &str {
        AUTH_PLUGIN_TYPE
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), OagwError> {
        // @cpt-begin:cpt-cf-oagw-flow-auth-phase:p1:inst-au-03
        // The resolved plugin is the noop auth plugin.
        // @cpt-begin:cpt-cf-oagw-flow-auth-phase:p1:inst-au-04
        // The request context is left untouched and success is returned: no
        // credential, no header and no query parameter is injected.
        Ok(())
        // @cpt-end:cpt-cf-oagw-flow-auth-phase:p1:inst-au-04
        // @cpt-end:cpt-cf-oagw-flow-auth-phase:p1:inst-au-03
    }
}

/// The auth plugin that injects one API key, resolved from the credential
/// store at request time (ADR 0002, §1.5).
pub struct ApiKeyAuthPlugin {
    cred_store: Arc<dyn CredStoreClientV1>,
}

/// The config key carrying the `cred://` reference of the API key.
const SECRET_REF_KEY: &str = "secret_ref";
/// The config key of the header injection target.
const KEY_HEADER_KEY: &str = "key_header";
/// The config key of the query-parameter injection target, whose URI-borne
/// credential exposure is the residual risk recorded on
/// [`InjectionTarget::Query`].
const KEY_QUERY_KEY: &str = "key_query";

/// The injection target of one `apikey` binding: exactly one of the two keys.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InjectionTarget {
    /// Inject into the header this name names.
    Header(String),
    /// Inject into the query parameter this name names — the `key_query`
    /// target of plugin-chain.md §1.5.
    ///
    /// # Accepted residual risk (R-055 / SEC-001)
    ///
    /// This target is URI-borne: the resolved credential is injected into the
    /// request URI, so it travels in the URL of every request the gateway sends
    /// and is therefore recorded by upstream and intermediary access logs —
    /// unlike the [`InjectionTarget::Header`] target, which keeps the value out
    /// of the URI. plugin-chain.md §1.5 declares `key_query` a supported
    /// injection target (`secret_ref` plus exactly one of `key_header` or
    /// `key_query`, the resolved secret injected verbatim), so the target is
    /// contractual and is not removed here. A binding that needs its credential
    /// kept out of access logs configures `key_header` instead.
    Query(String),
}

impl fmt::Debug for ApiKeyAuthPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKeyAuthPlugin").finish_non_exhaustive()
    }
}

impl ApiKeyAuthPlugin {
    /// Builds the plugin over the cred-store client the gear resolved at
    /// startup.
    #[must_use]
    pub fn new(cred_store: Arc<dyn CredStoreClientV1>) -> Self {
        Self { cred_store }
    }

    /// Reads the single injection target from the binding config.
    fn target(config: &BTreeMap<String, Value>) -> Result<InjectionTarget, OagwError> {
        let header = optional_string(config, KEY_HEADER_KEY)?;
        let query = optional_string(config, KEY_QUERY_KEY)?;
        match (header, query) {
            (Some(_), Some(_)) => Err(invalid_config(
                KEY_HEADER_KEY,
                "and 'key_query' are mutually exclusive: an apikey binding carries exactly one injection target",
            )),
            (Some(name), None) => Ok(InjectionTarget::Header(name)),
            (None, Some(name)) => Ok(InjectionTarget::Query(name)),
            (None, None) => Err(invalid_config(
                KEY_QUERY_KEY,
                "or 'key_header' is required: an apikey binding carries exactly one injection target",
            )),
        }
    }

    /// Reads the `cred://` reference of the API key from the binding config.
    fn secret_ref(config: &BTreeMap<String, Value>) -> Result<String, OagwError> {
        match config.get(SECRET_REF_KEY) {
            Some(Value::String(reference)) => {
                if !reference.starts_with(CRED_REF_SCHEME) {
                    return Err(invalid_config(
                        SECRET_REF_KEY,
                        "must be a 'cred://' reference that the credential store resolves at request time",
                    ));
                }
                Ok(reference.clone())
            }
            Some(_) => Err(invalid_config(
                SECRET_REF_KEY,
                "must be a string carrying a 'cred://' reference",
            )),
            None => Err(invalid_config(
                SECRET_REF_KEY,
                "is required: an apikey binding resolves its key from the credential store",
            )),
        }
    }
}

#[async_trait::async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        APIKEY_AUTH_PLUGIN_ID
    }

    fn plugin_type(&self) -> &str {
        AUTH_PLUGIN_TYPE
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        // @cpt-begin:cpt-cf-oagw-flow-auth-phase:p1:inst-au-05
        // The resolved plugin is the apikey auth plugin.
        // @cpt-begin:cpt-cf-oagw-flow-auth-phase:p1:inst-au-06
        // The apikey-injection algorithm runs: it resolves the `secret_ref`
        // through the cred-store client and injects the value into the one
        // configured target.
        let config = ctx.config.clone();
        // @cpt-begin:cpt-cf-oagw-algo-apikey-injection:p1:inst-ak-01
        // Parse `secret_ref` and the injection target from the binding config,
        // requiring exactly one of `key_header` or `key_query`.
        let target = Self::target(&config)?;
        let reference = Self::secret_ref(&config)?;
        // @cpt-begin:cpt-cf-oagw-algo-apikey-injection:p1:inst-ak-02
        // A binding that carries both or neither target, a missing
        // `secret_ref`, or a `secret_ref` that is not a `cred://` reference is
        // a 400 validation error naming the offending key.
        // @cpt-end:cpt-cf-oagw-algo-apikey-injection:p1:inst-ak-02
        // @cpt-end:cpt-cf-oagw-algo-apikey-injection:p1:inst-ak-01

        // @cpt-begin:cpt-cf-oagw-algo-apikey-injection:p1:inst-ak-03
        // Resolve `secret_ref` through the cred-store client into a
        // `SecretString` at request time.
        let secret = resolve_secret(&ctx.security_context, &self.cred_store, &reference).await?;
        // @cpt-begin:cpt-cf-oagw-algo-apikey-injection:p1:inst-ak-04
        // A reference that does not resolve is the 500 `SecretNotFound`, an
        // unreachable credential store the 503 `LinkUnavailable`; both are
        // mapped by the shared resolution of the credential isolation DoD and
        // carry a message with no credential material.
        // @cpt-end:cpt-cf-oagw-algo-apikey-injection:p1:inst-ak-04
        // @cpt-end:cpt-cf-oagw-algo-apikey-injection:p1:inst-ak-03

        // @cpt-begin:cpt-cf-oagw-algo-apikey-injection:p1:inst-ak-05
        // The configured target decides where the resolved value goes.
        match target {
            // @cpt-begin:cpt-cf-oagw-algo-apikey-injection:p1:inst-ak-06
            // The named request header is set to the resolved value verbatim,
            // replacing any value the header already carried.
            InjectionTarget::Header(name) => {
                let name = header_name(&name)?;
                ctx.headers.insert(name, header_value(secret.expose())?);
            }
            // @cpt-end:cpt-cf-oagw-algo-apikey-injection:p1:inst-ak-06
            // @cpt-begin:cpt-cf-oagw-algo-apikey-injection:p1:inst-ak-07
            // The named query parameter is set to the resolved value verbatim,
            // replacing any value that parameter already carried.
            // The value is URI-borne here: the credential travels in the request
            // URL and is recorded by upstream and intermediary access logs. The
            // `key_query` target is contractual (plugin-chain.md §1.5), so the
            // exposure is accepted and recorded on [`InjectionTarget::Query`].
            InjectionTarget::Query(name) => {
                ctx.query.retain(|(key, _)| key != &name);
                ctx.query.push((name, secret.expose().to_owned()));
            } // @cpt-end:cpt-cf-oagw-algo-apikey-injection:p1:inst-ak-07
        }
        // @cpt-end:cpt-cf-oagw-algo-apikey-injection:p1:inst-ak-05
        Ok(())
        // @cpt-begin:cpt-cf-oagw-algo-apikey-injection:p1:inst-ak-08
        // Success: the resolved secret is dropped at the end of the call and is
        // never logged, serialized or stored.
        // @cpt-end:cpt-cf-oagw-algo-apikey-injection:p1:inst-ak-08
        // @cpt-end:cpt-cf-oagw-flow-auth-phase:p1:inst-au-06
        // @cpt-end:cpt-cf-oagw-flow-auth-phase:p1:inst-au-05
    }
}

/// Reads one optional string config value, rejecting a value of another type.
fn optional_string(
    config: &BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<String>, OagwError> {
    match config.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(invalid_config(key, "must be a string")),
    }
}

/// Builds the 400 validation error naming the offending key.
fn invalid_config(key: &str, reason: &str) -> OagwError {
    OagwError::validation_error(format!("oagw.plugin.apikey: '{key}' {reason}"))
}

/// Parses a configured header name, rejecting a name the HTTP layer cannot
/// carry.
fn header_name(name: &str) -> Result<HeaderName, OagwError> {
    HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| invalid_config(KEY_HEADER_KEY, "must name a header the request can carry"))
}

/// Builds a header value from a resolved secret, rejecting a value the HTTP
/// layer cannot carry.
fn header_value(value: &str) -> Result<http::HeaderValue, OagwError> {
    http::HeaderValue::from_str(value)
        .map_err(|_| invalid_config(KEY_HEADER_KEY, "carries a value the header cannot hold"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use credstore_sdk::test_util::MockCredStoreClient;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    const TENANT: Uuid = Uuid::from_u128(0xa11ce);
    const SUBJECT: Uuid = Uuid::from_u128(0xbeef);

    fn security_context() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(SUBJECT)
            .subject_tenant_id(TENANT)
            .build()
            .expect("a test context carries a subject and a tenant")
    }

    fn context(config: BTreeMap<String, Value>) -> RequestContext {
        let mut ctx = RequestContext::new(security_context());
        ctx.config = config;
        ctx
    }

    fn plugin(store: MockCredStoreClient) -> ApiKeyAuthPlugin {
        ApiKeyAuthPlugin::new(Arc::new(store))
    }

    fn config_with(entries: &[(&str, Value)]) -> BTreeMap<String, Value> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect()
    }

    async fn authenticate(
        plugin: &ApiKeyAuthPlugin,
        config: BTreeMap<String, Value>,
    ) -> Result<RequestContext, OagwError> {
        let mut ctx = context(config);
        plugin.authenticate(&mut ctx).await?;
        Ok(ctx)
    }

    #[tokio::test]
    async fn the_noop_plugin_leaves_the_request_context_untouched() {
        let plugin = NoopAuthPlugin;
        let mut ctx = context(config_with(&[("unused", Value::Bool(true))]));
        ctx.headers.insert("x-existing", "value".parse().unwrap());
        ctx.query.push(("existing".to_owned(), "value".to_owned()));

        plugin.authenticate(&mut ctx).await.expect("noop succeeds");

        assert_eq!(ctx.headers.len(), 1, "no header is added");
        assert_eq!(ctx.headers.get("x-existing").unwrap(), "value");
        assert_eq!(ctx.query, vec![("existing".to_owned(), "value".to_owned())]);
        assert_eq!(ctx.config.len(), 1, "no credential is added to the context");
        assert_eq!(plugin.id(), NOOP_AUTH_PLUGIN_ID);
        assert_eq!(plugin.plugin_type(), AUTH_PLUGIN_TYPE);
    }

    #[tokio::test]
    async fn the_apikey_plugin_injects_the_resolved_key_into_the_header() {
        let config = config_with(&[
            ("secret_ref", Value::String("cred://api-key".to_owned())),
            ("key_header", Value::String("x-api-key".to_owned())),
        ]);
        let ctx = authenticate(
            &plugin(MockCredStoreClient::with_secrets(vec![(
                "api-key".to_owned(),
                "s3cr3t-value".to_owned(),
            )])),
            config,
        )
        .await
        .expect("the binding resolves");

        assert_eq!(ctx.headers.get("x-api-key").unwrap(), "s3cr3t-value");
        assert!(
            ctx.query.is_empty(),
            "the key goes to the one configured target only"
        );
    }

    #[tokio::test]
    async fn the_header_injection_replaces_an_existing_value() {
        let config = config_with(&[
            ("secret_ref", Value::String("cred://api-key".to_owned())),
            ("key_header", Value::String("X-Api-Key".to_owned())),
        ]);
        let mut ctx = context(config);
        ctx.headers.append("x-api-key", "stale".parse().unwrap());
        let plugin = plugin(MockCredStoreClient::with_secrets(vec![(
            "api-key".to_owned(),
            "fresh".to_owned(),
        )]));

        plugin
            .authenticate(&mut ctx)
            .await
            .expect("the binding resolves");

        assert_eq!(ctx.headers.get("x-api-key").unwrap(), "fresh");
        assert_eq!(
            ctx.headers.get_all("x-api-key").iter().count(),
            1,
            "the previous value is replaced, not appended to"
        );
    }

    #[tokio::test]
    async fn the_apikey_plugin_injects_the_resolved_key_into_the_query() {
        let config = config_with(&[
            ("secret_ref", Value::String("cred://api-key".to_owned())),
            ("key_query", Value::String("api_key".to_owned())),
        ]);
        let ctx = authenticate(
            &plugin(MockCredStoreClient::with_secrets(vec![(
                "api-key".to_owned(),
                "s3cr3t-value".to_owned(),
            )])),
            config,
        )
        .await
        .expect("the binding resolves");

        assert_eq!(
            ctx.query,
            vec![("api_key".to_owned(), "s3cr3t-value".to_owned())]
        );
        assert!(
            ctx.headers.get("api_key").is_none(),
            "the key goes to the one configured target only"
        );
    }

    #[tokio::test]
    async fn the_query_injection_replaces_an_existing_value() {
        let config = config_with(&[
            ("secret_ref", Value::String("cred://api-key".to_owned())),
            ("key_query", Value::String("api_key".to_owned())),
        ]);
        let mut ctx = context(config);
        ctx.query.push(("api_key".to_owned(), "stale".to_owned()));
        let plugin = plugin(MockCredStoreClient::with_secrets(vec![(
            "api-key".to_owned(),
            "fresh".to_owned(),
        )]));

        plugin
            .authenticate(&mut ctx)
            .await
            .expect("the binding resolves");

        assert_eq!(
            ctx.query,
            vec![("api_key".to_owned(), "fresh".to_owned())],
            "the previous value is replaced, not appended to"
        );
    }

    #[tokio::test]
    async fn both_targets_are_a_validation_error_naming_the_keys() {
        let config = config_with(&[
            ("secret_ref", Value::String("cred://api-key".to_owned())),
            ("key_header", Value::String("x-api-key".to_owned())),
            ("key_query", Value::String("api_key".to_owned())),
        ]);
        let error = authenticate(&plugin(MockCredStoreClient::empty()), config)
            .await
            .unwrap_err();

        assert_eq!(error.mapping().variant, "ValidationError");
        assert_eq!(error.status(), 400);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
        assert!(
            error.detail().contains("'key_header'") && error.detail().contains("'key_query'"),
            "{}",
            error.detail()
        );
    }

    #[tokio::test]
    async fn neither_target_is_a_validation_error_naming_the_keys() {
        let config = config_with(&[("secret_ref", Value::String("cred://api-key".to_owned()))]);
        let error = authenticate(&plugin(MockCredStoreClient::empty()), config)
            .await
            .unwrap_err();

        assert_eq!(error.mapping().variant, "ValidationError");
        assert!(
            error.detail().contains("'key_header'") && error.detail().contains("'key_query'"),
            "{}",
            error.detail()
        );
    }

    #[tokio::test]
    async fn a_missing_secret_ref_is_a_validation_error_naming_the_key() {
        let config = config_with(&[("key_header", Value::String("x-api-key".to_owned()))]);
        let error = authenticate(&plugin(MockCredStoreClient::empty()), config)
            .await
            .unwrap_err();

        assert_eq!(error.mapping().variant, "ValidationError");
        assert!(
            error.detail().contains("'secret_ref'"),
            "{}",
            error.detail()
        );
    }

    #[tokio::test]
    async fn a_non_cred_reference_is_a_validation_error_naming_the_key() {
        let config = config_with(&[
            ("secret_ref", Value::String("s3cr3t-inline".to_owned())),
            ("key_query", Value::String("api_key".to_owned())),
        ]);
        let error = authenticate(&plugin(MockCredStoreClient::empty()), config)
            .await
            .unwrap_err();

        assert_eq!(error.mapping().variant, "ValidationError");
        assert_eq!(error.status(), 400);
        assert!(
            error.detail().contains("'secret_ref'"),
            "{}",
            error.detail()
        );
    }

    #[tokio::test]
    async fn a_non_string_target_is_a_validation_error_naming_the_key() {
        for config in [
            config_with(&[
                ("secret_ref", Value::String("cred://api-key".to_owned())),
                ("key_header", Value::Bool(true)),
            ]),
            config_with(&[
                ("secret_ref", Value::String("cred://api-key".to_owned())),
                ("key_query", Value::from(7)),
            ]),
        ] {
            let error = authenticate(&plugin(MockCredStoreClient::empty()), config)
                .await
                .unwrap_err();
            assert_eq!(error.mapping().variant, "ValidationError");
            assert!(
                error.detail().contains("'key_header'") || error.detail().contains("'key_query'"),
                "{}",
                error.detail()
            );
        }
    }

    #[tokio::test]
    async fn an_unresolvable_secret_ref_is_a_secret_not_found() {
        let config = config_with(&[
            ("secret_ref", Value::String("cred://missing".to_owned())),
            ("key_header", Value::String("x-api-key".to_owned())),
        ]);
        let error = authenticate(&plugin(MockCredStoreClient::empty()), config)
            .await
            .unwrap_err();

        assert_eq!(error.mapping().variant, "SecretNotFound");
        assert_eq!(error.status(), 500);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"
        );
    }

    #[tokio::test]
    async fn a_failing_store_is_a_link_unavailable() {
        let config = config_with(&[
            ("secret_ref", Value::String("cred://api-key".to_owned())),
            ("key_header", Value::String("x-api-key".to_owned())),
        ]);
        let error = authenticate(&plugin(MockCredStoreClient::always_failing()), config)
            .await
            .unwrap_err();

        assert_eq!(error.mapping().variant, "LinkUnavailable");
        assert_eq!(error.status(), 503);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
        );
    }

    #[tokio::test]
    async fn no_failure_message_carries_any_secret_material() {
        let secret = "s3cr3t-never-echoed";
        let config = config_with(&[
            ("secret_ref", Value::String("cred://api-key".to_owned())),
            ("key_header", Value::String("x-api-key".to_owned())),
        ]);
        let store =
            MockCredStoreClient::with_secrets(vec![("api-key".to_owned(), secret.to_owned())]);
        let plugin = ApiKeyAuthPlugin::new(Arc::new(store));

        // The resolved secret reaches the header and nothing else.
        let ctx = authenticate(&plugin, config.clone())
            .await
            .expect("resolves");
        assert_eq!(ctx.headers.get("x-api-key").unwrap(), secret);

        let failing = ApiKeyAuthPlugin::new(Arc::new(MockCredStoreClient::always_failing()));
        let error = authenticate(&failing, config).await.unwrap_err();
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains(secret), "{rendered}");
        assert!(!rendered.contains("api-key"), "{rendered}");
    }

    #[test]
    fn the_auth_plugins_declare_their_identifier_and_family() {
        assert_eq!(
            ApiKeyAuthPlugin::new(Arc::new(MockCredStoreClient::empty())).id(),
            APIKEY_AUTH_PLUGIN_ID
        );
        assert_eq!(NoopAuthPlugin.plugin_type(), AUTH_PLUGIN_TYPE);
    }
}

// @cpt-end:cpt-cf-oagw-dod-builtin-plugin-behaviors:p1:inst-full
