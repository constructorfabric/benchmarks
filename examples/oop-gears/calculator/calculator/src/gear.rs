// Updated: 2026-04-07 by Constructor Tech
//! Calculator Gear definition
//!
//! A trivial example gRPC service that performs addition.
//! This gear demonstrates the OoP (out-of-process) gear pattern.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use toolkit::context::GearCtx;
use toolkit::contracts::{GrpcServiceCapability, RegisterGrpcServiceFn};

use calculator_sdk::{CalculatorServiceServer, SERVICE_NAME};

use crate::api::grpc::CalculatorServiceImpl;
use crate::domain::Service;

/// Calculator gear.
///
/// Exposes the accumulator service via gRPC through the grpc_hub.
#[toolkit::gear(
    name = "calculator",
    capabilities = [grpc]
)]
pub struct CalculatorGear;

impl Default for CalculatorGear {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl toolkit::Gear for CalculatorGear {
    async fn init(&self, ctx: &GearCtx) -> Result<()> {
        // Create domain service
        let service = Arc::new(Service::new());

        // Register Service in ClientHub for gRPC layer to use
        ctx.client_hub().register::<Service>(service);

        Ok(())
    }
}

/// Export gRPC services to grpc_hub
#[async_trait]
impl GrpcServiceCapability for CalculatorGear {
    async fn get_grpc_services(&self, ctx: &GearCtx) -> Result<Vec<RegisterGrpcServiceFn>> {
        // Get Service from ClientHub
        let service = ctx
            .client_hub()
            .get::<Service>()
            .map_err(|e| anyhow::anyhow!("Service not available: {}", e))?;

        // Build CalculatorService with our domain service
        let svc = CalculatorServiceServer::new(CalculatorServiceImpl::new(service));

        Ok(vec![RegisterGrpcServiceFn {
            service_name: SERVICE_NAME,
            register: Box::new(move |routes| {
                routes.add_service(svc.clone());
            }),
        }])
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "gear_tests.rs"]
mod gear_tests;
