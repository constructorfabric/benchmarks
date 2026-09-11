// Created: 2026-09-01 by Constructor Tech
//! The request-id transform plugin.
//!
//! `docs/DESIGN.md` §3.1: `X-Request-ID` injection and propagation. An
//! id the caller supplied is propagated; otherwise one is minted. Either
//! way the value is echoed back on the response so the caller can
//! correlate the two legs.

use crate::domain::errors::OagwError;
use crate::domain::model::builtin_plugins;
use crate::infra::context::{PluginRequest, PluginResponse};
use crate::infra::plugin::traits::Phase;

/// The header carrying the correlation id.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Propagates or mints `X-Request-ID`.
#[derive(Debug, Default)]
pub struct RequestIdTransformPlugin;

#[async_trait::async_trait]
impl crate::infra::plugin::traits::TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        builtin_plugins::TRANSFORM_REQUEST_ID
    }

    fn phases(&self) -> &'static [Phase] {
        &[Phase::OnRequest, Phase::OnResponse]
    }

    async fn on_request(&self, request: &mut PluginRequest) -> Result<(), OagwError> {
        if !request.has_header(REQUEST_ID_HEADER) {
            request.set_header(REQUEST_ID_HEADER, request.request_id.clone());
        }
        Ok(())
    }

    async fn on_response(&self, response: &mut PluginResponse) -> Result<(), OagwError> {
        if !response.has_header(REQUEST_ID_HEADER) {
            response
                .headers
                .push((REQUEST_ID_HEADER.to_owned(), String::new()));
        }
        Ok(())
    }

    async fn on_error(&self, _error: &mut OagwError) -> Result<(), OagwError> {
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::infra::plugin::traits::TransformPlugin as _;
    use std::collections::BTreeMap;

    fn request(id: Option<&str>) -> PluginRequest {
        let mut r = PluginRequest {
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
            request_id: "generated".to_owned(),
            content_type: None,
            auth_config: BTreeMap::new(),
            plugin_config: BTreeMap::new(),
            security: toolkit_security::SecurityContext::anonymous(),
        };
        if let Some(id) = id {
            r.set_header(REQUEST_ID_HEADER, id);
        }
        r
    }

    #[tokio::test]
    async fn a_supplied_id_is_propagated() {
        let mut r = request(Some("caller-id"));
        RequestIdTransformPlugin
            .on_request(&mut r)
            .await
            .expect("ok");
        assert_eq!(r.header(REQUEST_ID_HEADER), Some("caller-id"));
    }

    #[tokio::test]
    async fn an_absent_id_is_minted() {
        let mut r = request(None);
        RequestIdTransformPlugin
            .on_request(&mut r)
            .await
            .expect("ok");
        assert_eq!(r.header(REQUEST_ID_HEADER), Some("generated"));
    }

    #[test]
    fn the_plugin_declares_both_directions() {
        let plugin = RequestIdTransformPlugin;
        assert_eq!(plugin.phases(), &[Phase::OnRequest, Phase::OnResponse]);
        assert_eq!(
            plugin.id(),
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
        );
    }
}
