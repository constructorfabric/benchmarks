// Created: 2026-09-04 by Constructor Tech
//! Gear wiring of the OAGW gear (`docs/DESIGN.md` §1.4 "High-Level
//! Architecture Diagram").
//!
//! The `#[toolkit::gear]` declaration registers the gear in the host inventory
//! under the name `oagw`, so the host reads `gears.oagw.config`, calls
//! [`Gear::init`] during the init phase and then asks
//! [`RestApiCapability::register_rest`] for the gear's slice of the composed
//! router (`libs/toolkit/src/runtime/host_runtime.rs`).
//!
//! `init` builds the whole wiring exactly once: the validated gear
//! configuration, the in-memory control plane, the data plane resolving
//! against it, and the shutdown probe derived from the host cancellation token
//! so a shutdown also stops an in-flight stream
//! (`docs/DESIGN.md` §3.2 "Streaming").
//!
//! `register_rest` is a pure projection of that state: it mounts the
//! management surface and the proxy surface at their *gear-relative* paths —
//! `/oagw/v1/...` with no leading `/api` segment, because the api-gateway
//! nests gear paths under its own prefix, which is empty in the graded
//! configuration (`config/e2e-local.yaml`).

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use axum::Router;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;

use crate::api::routes::register_routes;
use crate::config::OagwConfig;
use crate::controlplane::service::ControlPlaneService;
use crate::controlplane::store::ControlPlaneStore;
use crate::dataplane::{CancelProbe, DataPlane, register_proxy_routes};

/// The state `register_rest` serves, built once by [`Gear::init`].
struct Wiring {
    /// Gear configuration in force (`gears.oagw.config`).
    config: Arc<OagwConfig>,
    /// Source of truth the management surface mutates and the data plane
    /// resolves.
    control_plane: Arc<ControlPlaneService>,
    /// Proxy engine over the control plane.
    data_plane: Arc<DataPlane>,
}

/// The OAGW gear: the centralized outbound API gateway of Constructor Fabric.
#[toolkit::gear(name = "oagw", capabilities = [rest])]
pub struct OagwGear {
    /// The wiring of the gear, set by `init` and read by `register_rest`.
    wiring: OnceLock<Arc<Wiring>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            wiring: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// The configuration `init` installed, when it already ran.
    #[must_use]
    pub fn config(&self) -> Option<Arc<OagwConfig>> {
        self.wiring.get().map(|wiring| Arc::clone(&wiring.config))
    }

    /// The data plane `init` installed, when it already ran.
    #[must_use]
    pub fn data_plane(&self) -> Option<Arc<DataPlane>> {
        self.wiring
            .get()
            .map(|wiring| Arc::clone(&wiring.data_plane))
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // The host initializes every gear once; a duplicate call neither
        // panics nor rebuilds the wiring, it keeps the installed one.
        if self.wiring.get().is_some() {
            tracing::debug!(
                gear = Self::MODULE_NAME,
                "duplicate init ignored: the gear is already wired"
            );
            return Ok(());
        }

        let config = Arc::new(load_config(ctx)?);
        let control_plane = Arc::new(ControlPlaneService::new(Arc::new(ControlPlaneStore::new())));
        let data_plane = Arc::new(DataPlane::new(
            Arc::clone(&control_plane),
            Arc::clone(&config),
            cancellation_probe(ctx),
        ));

        // A concurrent duplicate init loses the race: `OnceLock::set` keeps
        // the first wiring instead of panicking or replacing it.
        let wiring = Arc::new(Wiring {
            config: Arc::clone(&config),
            control_plane,
            data_plane,
        });
        if self.wiring.set(wiring).is_err() {
            tracing::debug!(
                gear = Self::MODULE_NAME,
                "duplicate init ignored: another init installed the wiring first"
            );
        }

        info!(
            gear = Self::MODULE_NAME,
            proxy_timeout_secs = config.proxy_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            ssrf_enabled = config.ssrf_enabled(),
            max_body_bytes = config.max_body_bytes,
            "OAGW gear initialized (control plane, data plane and plugin chain)"
        );
        Ok(())
    }
}

impl RestApiCapability for OagwGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<Router> {
        let wiring = self.wiring.get().ok_or_else(|| {
            anyhow::anyhow!(
                "the oagw gear is not initialized: Gear::init must run before register_rest"
            )
        })?;

        // Both surfaces are mounted at their gear-relative paths and publish
        // their operations on the registry the host passes in.
        let router = register_routes(router, openapi, Arc::clone(&wiring.control_plane));
        let router = register_proxy_routes(router, Arc::clone(&wiring.data_plane));

        info!(gear = Self::MODULE_NAME, "OAGW REST routes registered");
        Ok(router)
    }
}

/// Loads `gears.oagw.config`, falling back to the documented defaults for
/// every missing key (`crate::config::OagwConfig::default`).
///
/// # Errors
///
/// Returns the host configuration error — a section that does not deserialize
/// into [`OagwConfig`] — wrapped with the gear name, or the validation error of
/// a value the gear cannot honour.
fn load_config(ctx: &GearCtx) -> anyhow::Result<OagwConfig> {
    let config: OagwConfig = ctx.config_or_default().map_err(|error| {
        anyhow::anyhow!("the oagw gear configuration (gears.oagw.config) is invalid: {error}")
    })?;
    validate_config(&config)?;
    Ok(config)
}

/// Rejects the configuration values the gear cannot honour.
///
/// # Errors
///
/// `proxy_timeout_secs` and `max_body_bytes` must be positive: `0` would abort
/// every proxied request before the upstream is dialed.
fn validate_config(config: &OagwConfig) -> anyhow::Result<()> {
    if config.proxy_timeout_secs == 0 {
        return Err(anyhow::anyhow!(
            "the oagw gear configuration is invalid: proxy_timeout_secs must be at least 1 second"
        ));
    }
    if config.max_body_bytes == 0 {
        return Err(anyhow::anyhow!(
            "the oagw gear configuration is invalid: max_body_bytes must be at least 1 byte"
        ));
    }
    Ok(())
}

/// Builds the data-plane shutdown probe from the host cancellation token, so a
/// shutdown also stops an in-flight stream.
fn cancellation_probe(ctx: &GearCtx) -> CancelProbe {
    let token = ctx.cancellation_token().clone();
    Arc::new(move || token.is_cancelled())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "gear_tests.rs"]
mod tests;
