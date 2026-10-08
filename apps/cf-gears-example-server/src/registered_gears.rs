// This file is used to ensure that all gears are linked and registered via inventory
#![allow(unused_imports)]

#[cfg(feature = "oagw")]
use api_egress as _;
use api_gateway as _;
use authn_resolver as _;
use authz_resolver as _;
#[cfg(feature = "credstore")]
use credstore as _;
use gear_orchestrator as _;
use grpc_hub as _;
use license_resolver as _;
use tenant_resolver as _;
use types_registry as _;

#[cfg(feature = "single-tenant")]
use single_tenant_tr_plugin as _;

#[cfg(feature = "static-authn")]
use static_authn_plugin as _;

#[cfg(feature = "static-authz")]
use static_authz_plugin as _;

#[cfg(feature = "static-credstore")]
use static_credstore_plugin as _;

// === Optional Gears ===

#[cfg(feature = "mini-chat")]
use mini_chat as _;

#[cfg(feature = "mini-chat")]
use mini_chat::infra::plugins::static_audit as _;

#[cfg(feature = "mini-chat")]
use mini_chat::infra::plugins::static_model_policy as _;
