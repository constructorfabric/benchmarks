// Created: 2026-09-01 by Constructor Tech
//! Running a plugin chain in phase order.
//!
//! `docs/DESIGN.md` §3.2: auth first, then guards, then transforms, in the
//! order the configuration lists them. A plugin rejects the request by
//! returning an error, and the first rejection wins.

use crate::domain::errors::OagwError;
use crate::infra::context::{Chain, PluginRequest, PluginResponse};
use crate::infra::plugin::Registries;
use crate::infra::plugin::traits::Phase;

/// Run the request-side phases.
///
/// The binding whose phase is running is published into
/// `request.plugin_config` first, so a plugin reads its own configuration
/// rather than the whole merged set.
///
/// # Errors
/// The first rejection propagates.
pub async fn run_request(
    registries: &Registries,
    request: &mut PluginRequest,
    chain: &Chain,
) -> Result<(), OagwError> {
    if let Some(binding) = &chain.auth {
        let plugin = registries.auth.resolve(binding.id())?;
        request.plugin_config = binding.config().cloned().unwrap_or_default();
        plugin.authenticate(request).await?;
    }
    for binding in &chain.guards {
        let plugin = registries.guards.resolve(binding.id())?;
        request.plugin_config = binding.config().cloned().unwrap_or_default();
        plugin.guard_request(request).await?;
    }
    for binding in &chain.transforms {
        let plugin = registries.transforms.resolve(binding.id())?;
        request.plugin_config = binding.config().cloned().unwrap_or_default();
        if plugin.phases().contains(&Phase::OnRequest) {
            plugin.on_request(request).await?;
        }
    }
    Ok(())
}

/// Run the response-side phases in the same order the request ran.
///
/// # Errors
/// The first rejection propagates.
pub async fn run_response(
    registries: &Registries,
    response: &mut PluginResponse,
    chain: &Chain,
) -> Result<(), OagwError> {
    for binding in &chain.guards {
        let plugin = registries.guards.resolve(binding.id())?;
        response.plugin_config = binding.config().cloned().unwrap_or_default();
        plugin.guard_response(response).await?;
    }
    for binding in &chain.transforms {
        let plugin = registries.transforms.resolve(binding.id())?;
        response.plugin_config = binding.config().cloned().unwrap_or_default();
        if plugin.phases().contains(&Phase::OnResponse) {
            plugin.on_response(response).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::model::PluginBinding;
    use crate::domain::model::builtin_plugins as bp;
    use crate::infra::context::PluginRequest;
    use crate::infra::credstore::SecretResolver;
    use crate::infra::plugin::oauth2::TokenCacheConfig;
    use std::collections::BTreeMap;

    fn registries() -> Registries {
        Registries::builtins(
            SecretResolver::unlinked(),
            None,
            TokenCacheConfig::default(),
        )
    }

    fn request() -> PluginRequest {
        PluginRequest {
            method: "GET".to_owned(),
            path: "/".to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
            target: crate::domain::model::Target {
                host: "h".to_owned(),
                port: 443,
                secure: true,
            },
            alias: "h".to_owned(),
            upstream_id: "u".to_owned(),
            route_id: None,
            tenant_id: "t".to_owned(),
            subject: None,
            request_id: "r".to_owned(),
            content_type: None,
            auth_config: BTreeMap::new(),
            plugin_config: BTreeMap::new(),
            security: toolkit_security::SecurityContext::anonymous(),
        }
    }

    fn bound(id: &str) -> PluginBinding {
        PluginBinding::ref_only(id)
    }

    #[tokio::test]
    async fn an_unknown_plugin_is_reported_by_its_own_id() {
        let mut request = request();
        let chain = Chain {
            auth: Some(bound(
                "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.nope.v1",
            )),
            guards: Vec::new(),
            transforms: Vec::new(),
        };
        let err = run_request(&registries(), &mut request, &chain)
            .await
            .unwrap_err();
        assert_eq!(err.status_value(), 503, "{err}");
        assert!(err.detail().contains("nope"), "{err}");
    }

    #[tokio::test]
    async fn a_catalog_only_guard_cannot_be_resolved() {
        let mut request = request();
        let chain = Chain {
            auth: None,
            guards: vec![bound(bp::GUARD_TIMEOUT)],
            transforms: Vec::new(),
        };
        let err = run_request(&registries(), &mut request, &chain)
            .await
            .unwrap_err();
        assert_eq!(err.status_value(), 503, "{err}");
    }

    #[tokio::test]
    async fn the_request_id_transform_runs() {
        let mut request = request();
        let chain = Chain {
            auth: None,
            guards: Vec::new(),
            transforms: vec![bound(bp::TRANSFORM_REQUEST_ID)],
        };
        run_request(&registries(), &mut request, &chain)
            .await
            .expect("ok");
        assert_eq!(request.header("x-request-id"), Some("r"));
    }

    #[tokio::test]
    async fn an_empty_chain_is_a_no_op() {
        let mut request = request();
        let chain = Chain::empty();
        run_request(&registries(), &mut request, &chain)
            .await
            .expect("ok");
        let mut response = PluginResponse {
            status: 200,
            headers: Vec::new(),
            plugin_config: BTreeMap::new(),
        };
        run_response(&registries(), &mut response, &chain)
            .await
            .expect("ok");
    }

    #[tokio::test]
    async fn a_response_guard_rejects_a_missing_header() {
        let mut config = BTreeMap::new();
        config.insert(
            crate::infra::plugin::required_headers::keys::REQUIRED_RESPONSE_HEADERS.to_owned(),
            serde_json::json!("x-mandatory"),
        );
        let chain = Chain {
            auth: None,
            guards: vec![PluginBinding::with_config(
                bp::GUARD_REQUIRED_HEADERS,
                config,
            )],
            transforms: Vec::new(),
        };
        let mut response = PluginResponse {
            status: 200,
            headers: Vec::new(),
            plugin_config: BTreeMap::new(),
        };
        let err = run_response(&registries(), &mut response, &chain)
            .await
            .unwrap_err();
        assert_eq!(err.status_value(), 502, "{err}");
    }
}
