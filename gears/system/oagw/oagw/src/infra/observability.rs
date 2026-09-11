//! The post-write invalidation and audit hook of entry 2.9
//! (`cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation`,
//! `cpt-cf-oagw-flow-observability-and-state-dp-cache-flush`,
//! `cpt-cf-oagw-flow-observability-and-state-config-change-audit`).
//!
//! The hook is the single seam 2.2/2.3 left open: the management services call
//! it **after** the store write and **before** they return, and it performs, in
//! order (`inst-os-cpinv-1` .. `-9`):
//!
//! 1. the CP L1 invalidation of the affected documented keys;
//! 2. the DP L1 flush of the affected keys **and** of every entry whose
//!    dependency set intersects them, or the whole cache when the affected
//!    key set cannot be derived;
//! 3. the `config_change` structured audit event of DESIGN §4.3.
//!
//! A hook failure fails the operation, because the data plane must not serve a
//! stale record (`cpt-cf-oagw-dod-observability-and-state-cp-cache`).

use std::sync::Arc;

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers as gts;
use crate::domain::proxy::ProxyObservation;
use crate::domain::services::management::ConfigWriteHook;
use crate::infra::audit::{AuditRecord, AuditSink};
use crate::infra::cp_cache::{CacheKey, CPState, WriteNotification};
use crate::infra::dp_cache::DpHotConfig;
use crate::infra::metrics::MetricsRegistry;
use crate::infra::proxy::TransportObserver;

/// The management resource path a `config_change` record carries as its
/// resource identifier (`inst-os-algo-audit-2b`).
#[must_use]
pub fn resource_path(notification: &WriteNotification) -> String {
    let instance = notification.resource_id.split('~').nth(1).unwrap_or(&notification.resource_id);
    match notification.event {
        "upstream.create" | "upstream.replace" | "upstream.delete" => {
            format!("/oagw/v1/upstreams/{instance}")
        }
        "route.create" | "route.replace" | "route.delete" => {
            format!("/oagw/v1/routes/{instance}")
        }
        "plugin.created" | "plugin.deleted" => format!("/oagw/v1/plugins/{instance}"),
        _ => notification.resource_id.clone(),
    }
}

/// The observability surface the transport layer records a proxied exchange
/// through: the metric registry and the audit emitter, handed to the proxy
/// handler as one extension
/// (`cpt-cf-oagw-dod-observability-and-state-metric-surface`,
/// `cpt-cf-oagw-dod-observability-and-state-audit-log`).
#[derive(Clone)]
pub struct Observability {
    /// The registry `/metrics` renders and the proxy path records through.
    pub metrics: Arc<MetricsRegistry>,
    /// The structured JSON emitter on stdout.
    pub audit: Arc<AuditSink>,
}

impl std::fmt::Debug for Observability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Observability")
    }
}

impl Observability {
    /// The surface over one registry and one emitter.
    #[must_use]
    pub fn new(metrics: Arc<MetricsRegistry>, audit: Arc<AuditSink>) -> Self {
        Self { metrics, audit }
    }

    /// Record the metric families one completed exchange produced, from the
    /// pipeline-boundary observation
    /// (`inst-os-req-1` .. `-8`, `inst-os-algo-label-1`, `-3`, `-5`, `-9`).
    ///
    /// `host` is the addressed alias — never a tenant identifier — and
    /// `http.route` the normalized match pattern the pipeline resolved, omitted
    /// when the request matched none.
    pub fn record_observation(&self, host: &str, method: &str, observation: &ProxyObservation) {
        let route = observation.route.as_deref().filter(|route| !route.is_empty());
        self.metrics.record_request(host, method, route, observation.status);
        self.observe_phases(host, route, &observation.phases);
        if let Some(routing) = &observation.routing {
            self.metrics.record_routing(
                &routing.upstream_id,
                &routing.endpoint_host,
                routing.target_host_used,
                routing.selection_method,
            );
            self.metrics.set_upstream_available(
                host,
                &routing.endpoint_host,
                observation.error_type.is_none_or(|error| !is_transport_error(error)),
            );
        }
        if let Some(error_type) = observation.error_type {
            self.metrics.record_error(host, route, error_type);
        }
        if let Some(rate_limit) = &observation.rate_limit {
            self.metrics.record_rate_limit(
                &rate_limit.host,
                &rate_limit.path,
                rate_limit.refused,
                rate_limit.usage_ratio_parts_per_million,
            );
        }
    }

    /// The five phase histograms of one exchange
    /// (`inst-os-req-2`).
    fn observe_phases(
        &self,
        host: &str,
        route: Option<&str>,
        phases: &crate::domain::proxy::PhaseObservation,
    ) {
        for (phase, milliseconds) in [
            ("route_match", phases.route_match_ms),
            ("plugin_chain_request", phases.plugin_chain_request_ms),
            ("upstream_call", phases.upstream_call_ms),
            ("plugin_chain_response", phases.plugin_chain_response_ms),
            ("response", phases.response_ms),
        ] {
            if let Some(milliseconds) = milliseconds {
                self.metrics.observe_phase(
                    host,
                    route,
                    phase,
                    f64::from(u32::try_from(milliseconds).unwrap_or(u32::MAX)) / 1_000.0,
                );
            }
        }
    }

    /// The audit record of one completed proxy request
    /// (`cpt-cf-oagw-flow-observability-and-state-audit-record`,
    /// `inst-os-audit-1` .. `-4`).
    ///
    /// The record is emitted unconditionally: the sampling policy never applies
    /// to the `proxy_request` class (`inst-os-algo-audit-4`).
    pub fn audit_proxy_request(
        &self,
        request_id: &str,
        tenant_id: Uuid,
        principal_id: Uuid,
        host: Option<&str>,
        path: &str,
        method: &str,
        status: u16,
        duration_ms: u64,
        request_size: u64,
        response_size: u64,
        error_type: Option<&str>,
        refused_by_rate_limit: bool,
    ) {
        self.audit.emit(&AuditRecord::proxy_request(
            request_id,
            &tenant_id.to_string(),
            &principal_id.to_string(),
            host,
            path,
            method,
            status,
            duration_ms,
            request_size,
            response_size,
            error_type,
            refused_by_rate_limit,
        ));
    }
}

/// The transport observer the data plane reports to, so the availability gauge
/// follows the connection outcome the shared client observed and not the
/// upstream's response status
/// (`cpt-cf-oagw-flow-observability-and-state-request-metrics`).
///
/// `host` is the addressed alias and `endpoint` the `host:port` the connection
/// was opened to — the two labels the request families carry, so one series per
/// endpoint reports the recovery.
impl TransportObserver for Observability {
    fn observed(&self, host: &str, _upstream_id: &str, endpoint: &str, available: bool) {
        self.metrics.set_upstream_available(host, endpoint, available);
    }

    fn connections(&self, host: &str, idle: u64, active: u64, max: u64) {
        self.metrics.set_upstream_connections(host, idle, active, max);
    }
}

/// The transport failure types an upstream availability gauge falls on.
fn is_transport_error(error_type: &str) -> bool {
    matches!(
        error_type,
        gts::ERR_LINK_UNAVAILABLE
            | gts::ERR_CONNECTION_TIMEOUT
            | gts::ERR_REQUEST_TIMEOUT
            | gts::ERR_IDLE_TIMEOUT
            | gts::ERR_DOWNSTREAM
            | gts::ERR_STREAM_ABORTED
    )
}


/// The Control Plane invalidation, the Data Plane flush and the audit emitter
/// of one accepted configuration write
/// (`cpt-cf-oagw-dod-observability-and-state-cp-cache`).
pub struct ObservabilityHook {
    cp: CPState,
    dp: Arc<DpHotConfig>,
    audit: Arc<AuditSink>,
}

impl std::fmt::Debug for ObservabilityHook {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ObservabilityHook")
    }
}

impl ObservabilityHook {
    /// The hook over one CP state, one DP cache and one audit emitter.
    #[must_use]
    pub fn new(cp: CPState, dp: Arc<DpHotConfig>, audit: Arc<AuditSink>) -> Self {
        Self { cp, dp, audit }
    }

    /// The invalidation, the flush and the audit event of one notification
    /// (`inst-os-cpinv-4`, `inst-os-cpinv-5`, `inst-os-algo-inval-8`).
    // @cpt-begin:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-3
    // `inst-os-algo-inval-3` .. `-9`: the store write has already been applied
    // behind the repository boundary, the Control Plane L1 flush runs before
    // the Data Plane one so no entry is repopulated from a layer still holding
    // the pre-write value, and the write is reported complete only after both
    // have run.
    fn observe_write(&self, notification: &WriteNotification) {
        let affected = CPState::affected_keys(notification);
        if affected.is_empty() {
            // `inst-os-algo-inval-8`: the affected key set cannot be derived,
            // so both caches are cleared rather than partially invalidated.
            self.cp.flush_all();
            self.dp.flush_all();
        } else {
            self.cp.invalidate(&affected);
            self.dp.flush(&affected);
        }
        if notification.outcome == "accepted" {
            self.audit.emit(&AuditRecord::config_change(
                None,
                &notification.tenant_id.to_string(),
                &notification.principal_id.to_string(),
                &resource_path(notification),
                notification.status,
            ));
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-3
}

// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-1
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-10
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-2
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-3b
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-4
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-5
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-6
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-7
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-8
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-9
#[async_trait::async_trait]
impl ConfigWriteHook for ObservabilityHook {
    async fn on_config_written(&self, tenant_id: Uuid, upstream_id: Uuid) -> Result<(), DomainError> {
        self.on_upstream_written(WriteNotification {
            event: "upstream.replace",
            tenant_id,
            principal_id: Uuid::nil(),
            resource_id: crate::domain::gts_helpers::upstream_resource_id(upstream_id),
            upstream_id: Some(upstream_id),
            upstream_alias: None,
            route: None,
            plugin_id: None,
            status: 200,
            outcome: "accepted",
        })
        .await
    }

    async fn on_upstream_written(
        &self,
        notification: WriteNotification,
    ) -> Result<(), DomainError> {
        self.observe_write(&notification);
        Ok(())
    }

    async fn on_route_written(&self, notification: WriteNotification) -> Result<(), DomainError> {
        self.observe_write(&notification);
        Ok(())
    }

    async fn on_plugin_written(&self, notification: WriteNotification) -> Result<(), DomainError> {
        self.observe_write(&notification);
        Ok(())
    }
}
//
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-9
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-8
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-7
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-6
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-5
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-4
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-3b
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-2
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-10
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-write-invalidation-and-flush:p1:inst-os-algo-inval-1
//

/// The key set one written record affects, for a test to assert the derivation
/// against the cache families.
#[must_use]
pub fn affected_key_strings(notification: &WriteNotification) -> Vec<String> {
    CPState::affected_keys(notification).iter().map(CacheKey::as_string).collect()
}

#[cfg(test)]
#[path = "observability_tests.rs"]
mod observability_tests;
