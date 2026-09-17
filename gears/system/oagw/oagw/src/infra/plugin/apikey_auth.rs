//! `ApiKeyAuthPlugin` — API key injection into a header or a query parameter.
//!
//! Config keys (read from the upstream `auth.config` map):
//!
//! | Key | Required | Meaning |
//! |---|---|---|
//! | `api_key_ref` | yes | `cred://` reference holding the key value |
//! | `api_key_in` | no | `header` (default) or `query` |
//! | `api_key_name` | no | target header/query name (default `X-API-Key`) |
//! | `api_key_prefix` | no | literal prefix rendered before the key |

use async_trait::async_trait;

use crate::domain::gts_helpers::APIKEY_AUTH_PLUGIN_ID;
use crate::domain::plugin::{AuthPlugin, PluginError, RequestContext, SecretResolver};

/// Default target header name.
pub const DEFAULT_HEADER_NAME: &str = "X-API-Key";
/// Default target query parameter name.
pub const DEFAULT_QUERY_NAME: &str = "api_key";

/// Injects an API key resolved from the CredStore.
#[derive(Debug, Default, Clone, Copy)]
pub struct ApiKeyAuthPlugin;

impl ApiKeyAuthPlugin {
    fn target(ctx: &RequestContext) -> (String, bool) {
        let into_query = matches!(
            ctx.config.get("api_key_in").map(String::as_str),
            Some("query") | Some("QUERY") | Some("Query")
        );
        let name = ctx
            .config
            .get("api_key_name")
            .map(String::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(if into_query {
                DEFAULT_QUERY_NAME
            } else {
                DEFAULT_HEADER_NAME
            })
            .to_owned();
        (name, into_query)
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        APIKEY_AUTH_PLUGIN_ID
    }

    async fn authenticate(
        &self,
        ctx: &mut RequestContext,
        secrets: &dyn SecretResolver,
    ) -> Result<(), PluginError> {
        let reference = ctx
            .config
            .get("api_key_ref")
            .map(String::as_str)
            .ok_or_else(|| {
                PluginError::new("APIKEY_CONFIG_MISSING", "api_key_ref is not configured")
            })?;
        let security_context = ctx.security_context.clone().ok_or_else(|| {
            PluginError::new(
                "APIKEY_NO_SUBJECT",
                "no security context for secret resolution",
            )
        })?;
        let resolved = secrets
            .resolve(&security_context, reference)
            .await?
            .ok_or_else(|| PluginError::new("SECRET_NOT_FOUND", "api key reference unresolved"))?;

        let (name, into_query) = Self::target(ctx);
        let prefix = ctx
            .config
            .get("api_key_prefix")
            .map(String::as_str)
            .unwrap_or("");
        let value = resolved.render_prefixed(prefix);

        if into_query {
            ctx.query.retain(|(k, _)| !k.eq_ignore_ascii_case(&name));
            ctx.query.push((name, value));
        } else {
            ctx.set_header(&name, value);
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct Fixed(Vec<(&'static str, String)>);

    #[async_trait::async_trait]
    impl SecretResolver for Fixed {
        async fn resolve(
            &self,
            _ctx: &toolkit_security::SecurityContext,
            reference: &str,
        ) -> Result<Option<crate::domain::plugin::ResolvedSecret>, PluginError> {
            let bare = reference.strip_prefix("cred://").unwrap_or(reference);
            for (key, value) in &self.0 {
                if *key == bare {
                    return Ok(Some(crate::domain::plugin::ResolvedSecret::new(
                        value.clone(),
                    )));
                }
            }
            Ok(None)
        }
    }

    fn ctx(config: &[(&str, &str)]) -> RequestContext {
        RequestContext {
            security_context: Some(toolkit_security::SecurityContext::anonymous()),
            config: config
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect::<BTreeMap<_, _>>(),
            ..RequestContext::default()
        }
    }

    #[tokio::test]
    async fn injects_into_header_by_default() {
        let resolver = Fixed(vec![("partner-key", "s3cr3t".to_owned())]);
        let mut request = ctx(&[("api_key_ref", "cred://partner-key")]);
        ApiKeyAuthPlugin
            .authenticate(&mut request, &resolver)
            .await
            .expect("ok");
        assert_eq!(request.header("x-api-key"), Some("s3cr3t"));
        assert!(request.query.is_empty());
    }

    #[tokio::test]
    async fn injects_into_query_with_prefix_and_name() {
        let resolver = Fixed(vec![("partner-key", "s3cr3t".to_owned())]);
        let mut request = ctx(&[
            ("api_key_ref", "cred://partner-key"),
            ("api_key_in", "query"),
            ("api_key_name", "key"),
            ("api_key_prefix", "Bearer "),
        ]);
        ApiKeyAuthPlugin
            .authenticate(&mut request, &resolver)
            .await
            .expect("ok");
        assert_eq!(
            request.query,
            vec![("key".to_owned(), "Bearer s3cr3t".to_owned())]
        );
        assert!(request.header("x-api-key").is_none());
    }

    #[tokio::test]
    async fn missing_config_and_unresolved_reference_fail() {
        let resolver = Fixed(Vec::new());
        let mut request = ctx(&[]);
        let err = ApiKeyAuthPlugin.authenticate(&mut request, &resolver).await;
        assert_eq!(
            err.expect_err("missing config").code,
            "APIKEY_CONFIG_MISSING"
        );

        let mut request = ctx(&[("api_key_ref", "cred://absent")]);
        let err = ApiKeyAuthPlugin.authenticate(&mut request, &resolver).await;
        assert_eq!(err.expect_err("unresolved").code, "SECRET_NOT_FOUND");
    }
}
