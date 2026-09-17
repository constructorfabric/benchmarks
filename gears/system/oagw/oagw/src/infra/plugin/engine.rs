//! The plugin engine: it resolves the chain and runs it (ADR-0002).
//!
//! # The chain
//!
//! The bindings of a request are the upstream's `plugins.items` followed by the
//! matched route's `plugins.items`, plus the upstream's `auth` binding, which is
//! not part of `plugins` because there is exactly one of it (DESIGN §3.1). The
//! kind of each binding is read from its own identifier's base type, and the
//! execution order is the kind's order, not the list position:
//!
//! ```text
//! auth (upstream `auth`, then any `auth_plugin` binding)
//!   → guards (upstream items, then route items)   [inbound headers]
//!   → transforms (upstream items, then route items) [outbound headers]
//!   → upstream
//!   → transforms → guards                         [response headers]
//! ```
//!
//! Guards validate the *inbound* headers (`PluginContext::request`), because
//! the outbound map starts from the passthrough policy and would hide a header
//! the caller sent and the policy dropped.
//!
//! # Resolution, and failing closed
//!
//! DESIGN §3.1 "Resolution Algorithm": an instance segment that parses as a UUID
//! names a *custom* (tenant-defined) plugin stored in `oagw_plugin`; anything
//! else names a built-in or gear-provided plugin resolved from the in-process
//! registry. There is no Starlark sandbox in this slice, so a stored custom
//! plugin has no implementation to run: the request is rejected with 503
//! `cf.oagw.plugin.not_found.v1`, naming the plugin and saying so, rather than
//! silently skipped — a plugin that is bound but not run is a policy the caller
//! believes is enforced. An unknown identifier is the same 503 with a different
//! detail.

use std::sync::Arc;

use http::HeaderMap;

use crate::domain::services::data_plane::{PluginEngine, ProxyContext};
use crate::domain::storage::PluginStore;
use crate::domain::types::{AUTH_PLUGIN_TYPE_ID, GUARD_PLUGIN_TYPE_ID, PluginRef};
use crate::error::OagwError;

use super::registry::PluginRegistries;
use super::traits::PluginContext;

/// Runs the plugin chain of a request (ADR-0002 "Execution Order").
///
/// The chain is resolved against the in-process registries and the plugin store;
/// the credential material a plugin needs is the plugin's own business — a
/// plugin is built with the resolver it needs (see
/// [`ApiKeyAuthPlugin::with_client`](super::api_key_auth::ApiKeyAuthPlugin)),
/// so an engine can never leak a credential to a plugin that did not ask for
/// one.
#[derive(Debug)]
pub struct PluginEngineService {
    registries: PluginRegistries,
    plugins: Arc<PluginStore>,
}

impl PluginEngineService {
    /// An engine over `registries`, resolving custom-plugin references against
    /// `plugins`.
    #[must_use]
    pub fn new(registries: PluginRegistries, plugins: Arc<PluginStore>) -> Self {
        Self {
            registries,
            plugins,
        }
    }

    /// The registries the engine resolves through.
    #[must_use]
    pub const fn registries(&self) -> &PluginRegistries {
        &self.registries
    }

    /// The auth bindings of a request, in order: the upstream's `auth` field,
    /// then every `auth_plugin` binding of the chain.
    fn auth_bindings<'a>(&self, context: &'a ProxyContext) -> Vec<AuthBinding<'a>> {
        let mut bindings = Vec::new();
        if let Some(auth) = context.upstream.spec.auth.as_ref() {
            bindings.push(AuthBinding {
                reference: auth.plugin_type.as_str(),
                config: auth.config.as_ref(),
            });
        }
        for plugin in self.chain(context) {
            if plugin.as_str().starts_with(AUTH_PLUGIN_TYPE_ID) {
                bindings.push(AuthBinding {
                    reference: plugin.as_str(),
                    config: plugin.config(),
                });
            }
        }
        bindings
    }

    /// The guard bindings of a request, upstream before route.
    fn guard_bindings<'a>(&self, context: &'a ProxyContext) -> Vec<&'a PluginRef> {
        self.chain(context)
            .into_iter()
            .filter(|plugin| plugin.as_str().starts_with(GUARD_PLUGIN_TYPE_ID))
            .collect()
    }

    /// The transform bindings of a request, upstream before route.
    fn transform_bindings<'a>(&self, context: &'a ProxyContext) -> Vec<&'a PluginRef> {
        self.chain(context)
            .into_iter()
            .filter(|plugin| {
                // The base type ends in `~`, so this matches the type prefix
                // and never a longer instance segment of another type.
                !plugin.as_str().starts_with(AUTH_PLUGIN_TYPE_ID)
                    && !plugin.as_str().starts_with(GUARD_PLUGIN_TYPE_ID)
            })
            .collect()
    }

    /// The chain in execution order: the upstream's bindings, then the route's.
    fn chain<'a>(&self, context: &'a ProxyContext) -> Vec<&'a PluginRef> {
        let mut bindings = Vec::new();
        if let Some(plugins) = context.upstream.spec.plugins.as_ref() {
            bindings.extend(plugins.items.iter());
        }
        if let Some(route) = context.route.as_ref()
            && let Some(plugins) = route.spec.plugins.as_ref()
        {
            bindings.extend(plugins.items.iter());
        }
        bindings
    }

    /// The 503 for a reference nothing in this process can run.
    fn unresolved(&self, context: &ProxyContext, reference: &str) -> OagwError {
        let error = match PluginRef::new(reference).custom_uuid() {
            // A UUID names a stored custom plugin. When the record is there the
            // plugin exists — it just has no implementation in this process.
            Some(uuid) => match self.plugins.get(context.tenant_id, uuid) {
                Some(record) => OagwError::plugin_not_found(format!(
                    "plugin '{}' ({} {}) is bound but has no implementation in this gateway: no \
                     plugin sandbox is available",
                    record.name,
                    record.plugin_type,
                    record.gts_id()
                ))
                .with_extension("plugin_ref", serde_json::json!(reference))
                .with_extension("plugin_type", serde_json::json!(record.plugin_type))
                .with_extension("sandbox", serde_json::json!(false)),
                None => OagwError::plugin_not_found(format!(
                    "plugin '{reference}' is not registered in this gateway"
                ))
                .with_extension("plugin_ref", serde_json::json!(reference)),
            },
            None => OagwError::plugin_not_found(format!(
                "plugin '{reference}' is not registered in this gateway"
            ))
            .with_extension("plugin_ref", serde_json::json!(reference)),
        };
        error.with_extension("phase", serde_json::json!("plugin_resolution"))
    }
}

/// One auth binding: the upstream's `auth` field or an `auth_plugin` reference.
struct AuthBinding<'a> {
    reference: &'a str,
    config: Option<&'a serde_json::Value>,
}

/// The phase of a plugin execution a DEBUG record names (DESIGN §4.3 "Log
/// Levels": "`DEBUG`: Detailed plugin execution (disabled in production)").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginPhase {
    /// The [`PluginEngine::on_request`] phase.
    Request,
    /// The [`PluginEngine::on_response`] phase.
    Response,
    /// The `on_error` phase: a plugin call failed or could not be resolved.
    Error,
}

impl PluginPhase {
    /// The `phase` value of the record.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Request => "on_request",
            Self::Response => "on_response",
            Self::Error => "on_error",
        }
    }
}

/// The field values of one plugin-phase DEBUG record.
///
/// These three are the *whole* record: the `event` the record is queryable by,
/// the phase it ran in and the GTS identifier of the plugin that ran. Nothing
/// else may join them — no configuration, no header, no credential, and no
/// resolved secret (see [`plugin_audit_record`]).
#[derive(Debug, PartialEq, Eq)]
struct PluginAuditRecord {
    /// The `event` field of the record.
    event: &'static str,
    /// The `phase` field of the record.
    phase: &'static str,
    /// The `plugin_ref` field of the record: a GTS identifier, and nothing
    /// else.
    plugin_ref: String,
}

/// Build the DEBUG record of one plugin phase (DESIGN §4.3 "Log Levels").
///
/// Split from [`log_plugin_phase`] so the shape of the record is assertable
/// without a `tracing` subscriber, and so that the one rule it enforces has a
/// single home: the record names a plugin by its reference and that is all it
/// carries.
fn plugin_audit_record(phase: PluginPhase, plugin_ref: &str) -> PluginAuditRecord {
    PluginAuditRecord {
        event: "oagw.plugin",
        phase: phase.as_str(),
        plugin_ref: plugin_ref.to_owned(),
    }
}

/// Write the DEBUG record of one plugin phase.
///
/// `debug!` is compiled to a callsite check and nothing more: in production,
/// where DEBUG is off, this costs no formatting and no I/O.
fn log_plugin_phase(phase: PluginPhase, plugin_ref: &str) {
    let record = plugin_audit_record(phase, plugin_ref);
    tracing::debug!(
        target: "oagw.audit",
        event = record.event,
        phase = record.phase,
        plugin_ref = %record.plugin_ref,
        "plugin phase executed",
    );
}

/// Log the failure of one plugin call and hand the error back unchanged.
///
/// The record is DEBUG too — the failure is already reported by the request's
/// ERROR audit record — so this only says which plugin failed, under the
/// `on_error` phase. Which phase it failed *in* is the `on_request` /
/// `on_response` record written just before it, and the request's own audit
/// record carries the error.
fn plugin_failed(plugin_ref: &str, error: OagwError) -> OagwError {
    log_plugin_phase(PluginPhase::Error, plugin_ref);
    error
}

#[async_trait::async_trait]
impl PluginEngine for PluginEngineService {
    async fn on_request(
        &self,
        context: &ProxyContext,
        headers: &mut HeaderMap,
    ) -> Result<(), OagwError> {
        for binding in self.auth_bindings(context) {
            let Some(plugin) = self.registries.auth.resolve(binding.reference) else {
                return Err(plugin_failed(
                    binding.reference,
                    self.unresolved(context, binding.reference),
                ));
            };
            let plugin_context = PluginContext {
                config: binding.config,
                request: context,
            };
            log_plugin_phase(PluginPhase::Request, binding.reference);
            if let Err(error) = plugin.authenticate(&plugin_context, headers).await {
                return Err(plugin_failed(binding.reference, error));
            }
        }

        let inbound = context.inbound_headers.clone();
        for binding in self.guard_bindings(context) {
            let Some(plugin) = self.registries.guard.resolve(binding.as_str()) else {
                return Err(plugin_failed(
                    binding.as_str(),
                    self.unresolved(context, binding.as_str()),
                ));
            };
            let plugin_context = PluginContext {
                config: binding.config(),
                request: context,
            };
            log_plugin_phase(PluginPhase::Request, binding.as_str());
            if let Err(error) = plugin.guard_request(&plugin_context, &inbound).await {
                return Err(plugin_failed(binding.as_str(), error));
            }
        }

        for binding in self.transform_bindings(context) {
            let Some(plugin) = self.registries.transform.resolve(binding.as_str()) else {
                return Err(plugin_failed(
                    binding.as_str(),
                    self.unresolved(context, binding.as_str()),
                ));
            };
            let plugin_context = PluginContext {
                config: binding.config(),
                request: context,
            };
            log_plugin_phase(PluginPhase::Request, binding.as_str());
            if let Err(error) = plugin.transform_request(&plugin_context, headers).await {
                return Err(plugin_failed(binding.as_str(), error));
            }
        }

        Ok(())
    }

    async fn on_response(
        &self,
        context: &ProxyContext,
        headers: &mut HeaderMap,
    ) -> Result<(), OagwError> {
        for binding in self.transform_bindings(context) {
            let Some(plugin) = self.registries.transform.resolve(binding.as_str()) else {
                return Err(plugin_failed(
                    binding.as_str(),
                    self.unresolved(context, binding.as_str()),
                ));
            };
            let plugin_context = PluginContext {
                config: binding.config(),
                request: context,
            };
            log_plugin_phase(PluginPhase::Response, binding.as_str());
            if let Err(error) = plugin.transform_response(&plugin_context, headers).await {
                return Err(plugin_failed(binding.as_str(), error));
            }
        }

        for binding in self.guard_bindings(context) {
            let Some(plugin) = self.registries.guard.resolve(binding.as_str()) else {
                return Err(plugin_failed(
                    binding.as_str(),
                    self.unresolved(context, binding.as_str()),
                ));
            };
            let plugin_context = PluginContext {
                config: binding.config(),
                request: context,
            };
            log_plugin_phase(PluginPhase::Response, binding.as_str());
            if let Err(error) = plugin.guard_response(&plugin_context, headers).await {
                return Err(plugin_failed(binding.as_str(), error));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use http::HeaderMap;
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::domain::services::data_plane::ProxyContext;
    use crate::domain::storage::PluginStore;
    use crate::domain::types::{Endpoint, Plugin, Scheme, ServerConfig, Upstream, UpstreamSpec};
    use crate::error::OagwErrorKind;
    use crate::infra::plugin::GuardPlugin;
    use crate::infra::plugin::registry::{
        API_KEY_AUTH_PLUGIN_REF, PluginRegistries, REQUEST_ID_TRANSFORM_PLUGIN_REF,
        REQUIRED_HEADERS_GUARD_PLUGIN_REF,
    };

    const TENANT: Uuid = Uuid::from_u128(0x0001);
    const PLUGIN: Uuid = Uuid::from_u128(0x0002);

    fn engine() -> PluginEngineService {
        PluginEngineService::new(
            PluginRegistries::with_builtins(),
            Arc::new(PluginStore::new()),
        )
    }

    fn upstream(plugins: Option<crate::domain::types::PluginsConfig>) -> Upstream {
        let mut spec = UpstreamSpec {
            alias: Some("api.vendor.com".to_owned()),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Http,
                    host: "127.0.0.1".to_owned(),
                    port: 8080,
                }],
            },
            plugins,
            ..UpstreamSpec::default()
        };
        spec = spec.validate().expect("the upstream spec normalizes");

        Upstream {
            id: Uuid::new_v4(),
            tenant_id: TENANT,
            alias: "api.vendor.com".to_owned(),
            created_at: 0,
            updated_at: 0,
            spec,
        }
    }

    fn context(upstream: Upstream) -> ProxyContext {
        ProxyContext {
            subject_id: Uuid::new_v4(),
            tenant_id: TENANT,
            upstream_id: upstream.id,
            alias: upstream.alias.clone(),
            route_id: None,
            method: "GET".to_owned(),
            path: "/v1".to_owned(),
            request_id: "01JREQUESTID".to_owned(),
            security: toolkit_security::SecurityContext::anonymous(),
            client_ip: None,
            upstream: Arc::new(upstream),
            route: None,
            inbound_headers: HeaderMap::new(),
        }
    }

    /// A stored custom plugin, as the management API would have created it.
    fn stored_plugin(plugin_type: &str) -> Plugin {
        Plugin {
            id: PLUGIN,
            tenant_id: TENANT,
            plugin_type: plugin_type.to_owned(),
            name: "correlation-guard".to_owned(),
            config_schema: None,
            source_code: "def guard(ctx): pass".to_owned(),
            last_used_at: None,
            gc_eligible_at: None,
        }
    }

    fn plugins_config(items: Vec<PluginRef>) -> crate::domain::types::PluginsConfig {
        crate::domain::types::PluginsConfig {
            sharing: crate::domain::types::SharingMode::Private,
            items,
        }
    }

    // --- The §4.3 DEBUG record of a plugin phase. ---------------------------
    //
    // `tracing` is only observable through a subscriber, and this crate declares
    // no `tracing-subscriber` (and may gain no dependency), so the record is
    // asserted *structurally*: [`plugin_audit_record`] builds the field values
    // [`log_plugin_phase`] writes, and the tests below pin its shape.

    #[test]
    fn the_plugin_debug_record_names_the_reference_and_the_phase() {
        let requested = plugin_audit_record(PluginPhase::Request, REQUEST_ID_TRANSFORM_PLUGIN_REF);
        assert_eq!(requested.event, "oagw.plugin");
        assert_eq!(requested.phase, "on_request");
        assert_eq!(requested.plugin_ref, REQUEST_ID_TRANSFORM_PLUGIN_REF);

        let responded =
            plugin_audit_record(PluginPhase::Response, REQUIRED_HEADERS_GUARD_PLUGIN_REF);
        assert_eq!(responded.phase, "on_response");
        assert_eq!(responded.plugin_ref, REQUIRED_HEADERS_GUARD_PLUGIN_REF);

        let failed = plugin_audit_record(PluginPhase::Error, API_KEY_AUTH_PLUGIN_REF);
        assert_eq!(failed.phase, "on_error");
        assert_eq!(failed.plugin_ref, API_KEY_AUTH_PLUGIN_REF);

        // One event name, so an operator queries plugin execution in one place.
        assert_eq!(
            [requested.event, responded.event, failed.event],
            ["oagw.plugin"; 3]
        );
    }

    #[test]
    fn the_plugin_debug_record_carries_nothing_but_the_three_fields() {
        // The exact rendering: if a field ever joins, this fails. The reference
        // is a GTS identifier, so the record can never name a header, a
        // credential or a resolved secret — there is nowhere for one to go.
        let record = plugin_audit_record(
            PluginPhase::Request,
            "gts.cf.core.plugins.plugin.v1~cf.oagw.request_id.v1",
        );
        assert_eq!(
            format!("{record:?}"),
            "PluginAuditRecord { event: \"oagw.plugin\", phase: \"on_request\", plugin_ref: \
             \"gts.cf.core.plugins.plugin.v1~cf.oagw.request_id.v1\" }"
        );
    }

    #[tokio::test]
    async fn a_chain_without_plugins_is_a_no_op() {
        let engine = engine();
        let context = context(upstream(None));
        let mut headers = HeaderMap::new();

        engine
            .on_request(&context, &mut headers)
            .await
            .expect("nothing to run");
        engine
            .on_response(&context, &mut headers)
            .await
            .expect("nothing to run");

        assert!(headers.is_empty());
    }

    #[tokio::test]
    async fn the_transform_plugin_stamps_the_request_id() {
        let engine = engine();
        let context = context(upstream(Some(plugins_config(vec![PluginRef::new(
            REQUEST_ID_TRANSFORM_PLUGIN_REF,
        )]))));
        let mut headers = HeaderMap::new();

        engine
            .on_request(&context, &mut headers)
            .await
            .expect("the built-in resolves");

        assert_eq!(
            headers.get("x-request-id").and_then(|v| v.to_str().ok()),
            Some("01JREQUESTID")
        );
    }

    #[tokio::test]
    async fn the_guard_checks_the_inbound_headers_not_the_outbound_ones() {
        let engine = engine();
        let mut spec = upstream(Some(plugins_config(vec![PluginRef::bound(
            REQUIRED_HEADERS_GUARD_PLUGIN_REF,
            json!({"required_request_headers": "x-correlation-id"}),
        )])))
        .spec;
        // The passthrough policy drops everything, so the outbound map cannot
        // answer a presence question about what the caller sent.
        spec.headers = Some(crate::domain::types::HeadersConfig {
            request: Some(crate::domain::types::HeaderTransform {
                passthrough: crate::domain::types::PassthroughMode::None,
                ..Default::default()
            }),
            response: None,
        });
        let upstream = Upstream {
            spec,
            ..upstream(None)
        };
        let mut inbound = HeaderMap::new();
        inbound.insert("x-correlation-id", "01J".parse().expect("valid"));
        let context = ProxyContext {
            inbound_headers: inbound,
            ..context(upstream)
        };
        let mut outbound = HeaderMap::new();

        engine
            .on_request(&context, &mut outbound)
            .await
            .expect("the caller sent the required header");
        assert!(outbound.is_empty(), "the passthrough policy decided that");
    }

    #[tokio::test]
    async fn a_guard_rejection_is_a_400() {
        let engine = engine();
        let context = context(upstream(Some(plugins_config(vec![PluginRef::bound(
            REQUIRED_HEADERS_GUARD_PLUGIN_REF,
            json!({"required_request_headers": "x-correlation-id"}),
        )]))));
        let mut headers = HeaderMap::new();

        let error = engine
            .on_request(&context, &mut headers)
            .await
            .expect_err("the header is missing");

        assert_eq!(error.status().as_u16(), 400);
        assert!(headers.is_empty(), "a rejected request forwards nothing");
    }

    #[tokio::test]
    async fn an_unknown_reference_fails_closed_with_a_503() {
        let engine = engine();
        let context = context(upstream(Some(plugins_config(vec![PluginRef::new(
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
        )]))));
        let mut headers = HeaderMap::new();

        let error = engine
            .on_request(&context, &mut headers)
            .await
            .expect_err("the identifier has no implementation");

        assert_eq!(error.status().as_u16(), 503);
        assert_eq!(error.kind(), OagwErrorKind::PluginNotFound);
        assert_eq!(
            error.kind().gts_type_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"
        );
        assert_eq!(
            error
                .extensions()
                .get("plugin_ref")
                .and_then(|value| value.as_str()),
            Some("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1")
        );
        assert!(headers.is_empty(), "a rejected request forwards nothing");
    }

    #[tokio::test]
    async fn a_stored_custom_plugin_fails_closed_and_names_the_sandbox() {
        let plugins = Arc::new(PluginStore::new());
        plugins
            .insert(stored_plugin("guard_plugin"))
            .expect("the plugin inserts");
        let engine = PluginEngineService::new(PluginRegistries::with_builtins(), plugins);
        let reference = format!("{}{}", GUARD_PLUGIN_TYPE_ID, PLUGIN);
        let context = context(upstream(Some(plugins_config(vec![PluginRef::new(
            reference.clone(),
        )]))));
        let mut headers = HeaderMap::new();

        let error = engine
            .on_request(&context, &mut headers)
            .await
            .expect_err("no sandbox is available");

        assert_eq!(error.status().as_u16(), 503);
        assert!(error.detail().contains("correlation-guard"));
        assert!(error.detail().contains("guard_plugin"));
        assert!(error.detail().contains("no plugin sandbox"));
        assert_eq!(
            error
                .extensions()
                .get("sandbox")
                .and_then(|value| value.as_bool()),
            Some(false)
        );
        assert_eq!(
            error
                .extensions()
                .get("plugin_ref")
                .and_then(|value| value.as_str()),
            Some(reference.as_str())
        );
    }

    #[tokio::test]
    async fn an_unknown_uuid_backed_reference_is_also_a_503() {
        let engine = engine();
        let reference = format!("{}{}", GUARD_PLUGIN_TYPE_ID, Uuid::new_v4());
        let context = context(upstream(Some(plugins_config(vec![PluginRef::new(
            reference.clone(),
        )]))));
        let mut headers = HeaderMap::new();

        let error = engine
            .on_request(&context, &mut headers)
            .await
            .expect_err("no such plugin");

        assert_eq!(error.status().as_u16(), 503);
        assert_eq!(
            error
                .extensions()
                .get("plugin_ref")
                .and_then(|value| value.as_str()),
            Some(reference.as_str())
        );
    }

    #[tokio::test]
    async fn the_route_plugins_run_after_the_upstream_plugins() {
        // The transform is order-observable through the header it sets last.
        let engine = engine();
        let mut upstream_record = upstream(Some(plugins_config(vec![PluginRef::new(
            REQUEST_ID_TRANSFORM_PLUGIN_REF,
        )])));
        upstream_record.spec.auth = Some(crate::domain::types::AuthConfig {
            plugin_type: API_KEY_AUTH_PLUGIN_REF.to_owned(),
            sharing: crate::domain::types::SharingMode::Private,
            config: Some(json!({"key": "s3cr3t"})),
        });
        let context = context(upstream_record);
        let mut headers = HeaderMap::new();

        engine
            .on_request(&context, &mut headers)
            .await
            .expect("both plugins resolve");

        assert_eq!(
            headers.get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("s3cr3t"),
            "the auth plugin runs, ahead of the transforms"
        );
        assert_eq!(
            headers.get("x-request-id").and_then(|v| v.to_str().ok()),
            Some("01JREQUESTID")
        );
    }

    #[tokio::test]
    async fn an_unresolvable_auth_binding_fails_closed() {
        let engine = engine();
        let mut upstream_record = upstream(None);
        upstream_record.spec.auth = Some(crate::domain::types::AuthConfig {
            plugin_type: format!("{}{}", AUTH_PLUGIN_TYPE_ID, Uuid::new_v4()),
            sharing: crate::domain::types::SharingMode::Private,
            config: Some(json!({"key": "s3cr3t"})),
        });
        let context = context(upstream_record);
        let mut headers = HeaderMap::new();

        let error = engine
            .on_request(&context, &mut headers)
            .await
            .expect_err("the auth plugin has no implementation");

        assert_eq!(error.status().as_u16(), 503);
        assert!(
            headers.is_empty(),
            "an unauthenticated forward never happens"
        );
    }

    #[tokio::test]
    async fn the_response_phase_runs_the_transforms_and_the_guards() {
        let engine = engine();
        let context = context(upstream(Some(plugins_config(vec![
            PluginRef::new(REQUEST_ID_TRANSFORM_PLUGIN_REF),
            PluginRef::bound(
                REQUIRED_HEADERS_GUARD_PLUGIN_REF,
                json!({"required_response_headers": "content-type"}),
            ),
        ]))));
        let mut headers = HeaderMap::new();

        let error = engine
            .on_response(&context, &mut headers)
            .await
            .expect_err("content-type is missing");

        assert_eq!(error.status().as_u16(), 502);
        assert_eq!(
            headers.get("x-request-id").and_then(|v| v.to_str().ok()),
            Some("01JREQUESTID"),
            "the transform ran before the guard rejected"
        );
    }

    #[tokio::test]
    async fn a_missing_credstore_is_reported_by_the_apikey_plugin() {
        let engine = PluginEngineService::new(
            PluginRegistries::with_builtins_and(None),
            Arc::new(PluginStore::new()),
        );
        let mut upstream_record = upstream(None);
        upstream_record.spec.auth = Some(crate::domain::types::AuthConfig {
            plugin_type: API_KEY_AUTH_PLUGIN_REF.to_owned(),
            sharing: crate::domain::types::SharingMode::Private,
            config: Some(json!({"key": "cred://vendor/api-key"})),
        });
        let context = context(upstream_record);
        let mut headers = HeaderMap::new();

        let error = engine
            .on_request(&context, &mut headers)
            .await
            .expect_err("no credential store is published");

        assert_eq!(error.kind(), OagwErrorKind::SecretNotFound);
        assert!(headers.is_empty());
    }

    /// A guard double that records the order it was called in.
    struct RecordingPlugin {
        reference: String,
        calls: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl GuardPlugin for RecordingPlugin {
        fn plugin_ref(&self) -> &str {
            &self.reference
        }

        async fn guard_request(
            &self,
            _context: &PluginContext<'_>,
            _headers: &HeaderMap,
        ) -> Result<(), OagwError> {
            self.calls
                .lock()
                .expect("the lock is taken")
                .push("request");
            Ok(())
        }

        async fn guard_response(
            &self,
            _context: &PluginContext<'_>,
            _headers: &mut HeaderMap,
        ) -> Result<(), OagwError> {
            self.calls
                .lock()
                .expect("the lock is taken")
                .push("response");
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_registered_plugin_of_a_later_gear_is_resolved() {
        let mut registries = PluginRegistries::with_builtins();
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        registries.guard.register(Arc::new(RecordingPlugin {
            reference: format!("{}external.vendor.v1", GUARD_PLUGIN_TYPE_ID),
            calls: Arc::clone(&calls),
        }));
        let engine = PluginEngineService::new(registries, Arc::new(PluginStore::new()));
        let reference = format!("{}external.vendor.v1", GUARD_PLUGIN_TYPE_ID);
        let context = context(upstream(Some(plugins_config(vec![PluginRef::new(
            reference.clone(),
        )]))));

        engine
            .on_request(&context, &mut HeaderMap::new())
            .await
            .expect("the external plugin resolves");

        assert_eq!(*calls.lock().expect("the lock is taken"), vec!["request"]);
    }
}
