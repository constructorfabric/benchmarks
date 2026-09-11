//! The built-in plugin chain.
//!
//! Plugins run in a fixed order — Auth → Guards → Transform(request) → upstream call →
//! Transform(response/error) — with upstream-bound plugins before route-bound plugins.
//! The chain is a plain vector of boxed stages so the order is observable.

pub mod auth;
pub mod guard;
pub mod token_cache;
pub mod transform;

use bytes::Bytes;
use http::HeaderMap;

use crate::error::{ErrorKind, Extensions, OagwError};

/// Result of the request-side plugin chain: the headers to forward upstream.
#[derive(Debug, Default, Clone)]
pub struct RequestTransform {
    /// Headers set on the outbound request (added after the passthrough filter).
    pub set: Vec<(String, String)>,
    /// Headers removed from the outbound request.
    pub remove: Vec<String>,
}

/// Result of the response-side plugin pass.
#[derive(Debug, Default, Clone)]
pub struct ResponseTransform {
    pub set: Vec<(String, String)>,
    pub remove: Vec<String>,
}

/// Per-request context handed to every plugin.
pub struct PluginContext<'a> {
    /// The caller's security context, used to resolve credential references.
    pub security: &'a crate::security::SecurityContextHolder,
    /// The effective configuration for the plugin instance.
    pub config: &'a serde_json::Map<String, serde_json::Value>,
    /// The upstream the request is being sent to.
    pub upstream_id: &'a str,
    /// The upstream host the request is being sent to.
    pub host: &'a str,
    /// The path being forwarded.
    pub path: &'a str,
    /// The correlation identifier the request was given, so a response-phase transform can
    /// hand it back to the caller. Empty when the caller sent none and no transform
    /// generated one.
    pub request_id: &'a str,
    /// Credential resolver.
    pub credentials: &'a dyn crate::security::CredentialResolver,
    /// Shared `OAuth2` token cache.
    pub token_cache: &'a token_cache::TokenCache,
}

impl PluginContext<'_> {
    /// Extension fields describing this request, for error documents.
    #[must_use]
    pub fn extensions(&self) -> Extensions {
        Extensions {
            upstream_id: Some(self.upstream_id.to_owned()),
            host: Some(self.host.to_owned()),
            path: Some(self.path.to_owned()),
            ..Extensions::default()
        }
    }
}

/// The phases a plugin can run in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Before the upstream call.
    Request,
    /// After the upstream call.
    Response,
}

/// What a request-phase plugin decided.
#[derive(Debug, Default, Clone)]
pub struct RequestOutcome {
    pub transform: RequestTransform,
}

/// What a response-phase plugin decided.
#[derive(Debug, Default, Clone)]
pub struct ResponseOutcome {
    pub transform: ResponseTransform,
}

/// A bound plugin instance.
#[derive(Debug, Clone)]
pub struct BoundPlugin {
    /// The plugin identifier as bound (`apikey`, `required_headers`, a plugin id…).
    pub name: String,
    /// The binding's own configuration.
    pub config: serde_json::Map<String, serde_json::Value>,
}

/// The ordered plugin chain for one request.
#[derive(Debug, Clone, Default)]
pub struct Chain {
    plugins: Vec<BoundPlugin>,
}

impl Chain {
    /// Builds the chain from upstream- and route-bound plugins, upstream first.
    #[must_use]
    pub fn new(upstream: &[crate::domain::plugin::PluginBinding], route: &[crate::domain::plugin::PluginBinding]) -> Self {
        let mut plugins = Vec::new();
        for binding in upstream.iter().chain(route.iter()) {
            plugins.push(BoundPlugin {
                name: binding.name.clone(),
                config: binding.config.clone(),
            });
        }
        Self { plugins }
    }

    /// The bound plugins, in execution order.
    #[must_use]
    pub fn plugins(&self) -> &[BoundPlugin] {
        &self.plugins
    }

    /// Runs every auth plugin, returning the credentials each resolved.
    ///
    /// # Errors
    ///
    /// Returns an error when an auth plugin fails or names an unknown identifier.
    pub async fn run_auth(
        &self,
        ctx: &PluginContext<'_>,
    ) -> Result<Vec<InjectedCredential>, OagwError> {
        let mut resolved = Vec::new();
        for plugin in &self.plugins {
            match crate::domain::plugin::builtin_kind(&plugin.name) {
                Some(crate::domain::plugin::PluginKind::Auth) => {
                    resolved.push(auth::execute(plugin, ctx).await?);
                }
                // Guards and transforms do not contribute credentials; the guard chain
                // and the transform chain run them in their own pass.
                Some(
                    crate::domain::plugin::PluginKind::Transform
                    | crate::domain::plugin::PluginKind::Guard,
                ) => {}
                None => {
                    return Err(OagwError::new(
                        ErrorKind::PluginNotFound,
                        format!("plugin `{}` is not resolvable", plugin.name),
                    )
                    .with_extensions(ctx.extensions()));
                }
            }
        }
        Ok(resolved)
    }

    /// Runs the guard plugins over the outbound request headers.
    ///
    /// # Errors
    ///
    /// Returns a validation error when a guard rejects the request.
    pub fn run_request_guards(
        &self,
        ctx: &PluginContext<'_>,
        headers: &HeaderMap,
    ) -> Result<(), OagwError> {
        for plugin in &self.plugins {
            if crate::domain::plugin::builtin_kind(&plugin.name)
                == Some(crate::domain::plugin::PluginKind::Guard)
            {
                guard::execute_request(plugin, ctx, headers)?;
            }
        }
        Ok(())
    }

    /// Runs the guard plugins over the upstream response.
    ///
    /// # Errors
    ///
    /// Returns a downstream error when a guard rejects the response.
    pub fn run_response_guards(
        &self,
        ctx: &PluginContext<'_>,
        headers: &HeaderMap,
    ) -> Result<(), OagwError> {
        for plugin in &self.plugins {
            if crate::domain::plugin::builtin_kind(&plugin.name)
                == Some(crate::domain::plugin::PluginKind::Guard)
            {
                guard::execute_response(plugin, ctx, headers)?;
            }
        }
        Ok(())
    }

    /// Applies the transform plugins to the outbound request headers.
    ///
    /// # Errors
    ///
    /// Returns an error when a transform plugin names an unknown identifier.
    pub fn run_request_transforms(
        &self,
        ctx: &PluginContext<'_>,
        headers: &mut HeaderMap,
    ) -> Result<(), OagwError> {
        for plugin in &self.plugins {
            if crate::domain::plugin::builtin_kind(&plugin.name)
                == Some(crate::domain::plugin::PluginKind::Transform)
            {
                transform::execute_request(plugin, ctx, headers)?;
            }
        }
        Ok(())
    }

    /// Applies the transform plugins to the relayed response headers.
    ///
    /// # Errors
    ///
    /// Returns an error when a transform plugin names an unknown identifier.
    pub fn run_response_transforms(
        &self,
        ctx: &PluginContext<'_>,
        headers: &mut HeaderMap,
    ) -> Result<(), OagwError> {
        for plugin in &self.plugins {
            if crate::domain::plugin::builtin_kind(&plugin.name)
                == Some(crate::domain::plugin::PluginKind::Transform)
            {
                transform::execute_response(plugin, ctx, headers)?;
            }
        }
        Ok(())
    }
}

/// Applies a set of header operations to a header map.
#[must_use]
pub fn apply_ops(headers: &mut HeaderMap, ops: &[(String, String)], remove: &[String]) {
    for (name, value) in ops {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    for name in remove {
        if let Ok(name) = http::HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(name);
        }
    }
}

/// A credential resolved by an auth plugin, ready to be injected.
#[derive(Debug, Clone)]
pub struct InjectedCredential {
    /// Header to set on the outbound request.
    pub header: String,
    /// Value to set, never logged.
    pub value: Bytes,
}

impl InjectedCredential {
    /// Builds a header injection.
    #[must_use]
    pub fn header(header: impl Into<String>, value: impl Into<Bytes>) -> Self {
        Self {
            header: header.into(),
            value: value.into(),
        }
    }
}

#[cfg(test)]
#[path = "guard_tests.rs"]
mod guard_tests;

#[cfg(test)]
#[path = "auth_tests.rs"]
mod auth_tests;

#[cfg(test)]
#[path = "transform_tests.rs"]
mod transform_tests;
