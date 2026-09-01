// Updated: 2026-04-16 by Constructor Tech
// This file is used to ensure that all gears are linked and registered via inventory
// In future we can simply DX via build.rs which will collect all crates in ./gears and generate this file.
// But for now we will manually maintain this file.
#![allow(unused_imports)]

#[cfg(feature = "oagw")]
use api_egress as _;
use api_gateway as _;
use authn_resolver as _;
use authz_resolver as _;
#[cfg(feature = "credstore")]
use credstore as _;
#[cfg(feature = "grpc-hub")]
use grpc_hub as _;
use tenant_resolver as _;
use types_registry as _;

#[cfg(feature = "single-tenant")]
use single_tenant_tr_plugin as _;

#[cfg(feature = "static-tenants")]
use static_tr_plugin as _;

#[cfg(feature = "tenant-resolver-rg")]
use rg_tr_plugin as _;

#[cfg(feature = "static-authn")]
use static_authn_plugin as _;

#[cfg(feature = "static-authz")]
use static_authz_plugin as _;

#[cfg(feature = "tr-authz")]
use tr_authz_plugin as _;

#[cfg(feature = "static-credstore")]
use static_credstore_plugin as _;

#[cfg(feature = "account-management")]
use account_management as _;
#[cfg(feature = "account-management")]
use static_idp_plugin as _;
