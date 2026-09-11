//! The plugin-execution contexts the three plugin contracts take.
//!
//! `DECOMPOSITION` §2.4 lists `AuthContext`, `RequestContext`,
//! `ResponseContext`, and `ErrorContext` as entities the plugin-system feature
//! consumes *from* `cpt-cf-oagw-feature-gear-foundation`, and that feature's
//! own §1.5 records that its entity list names only `ErrorContext` among the
//! four. This module is the vocabulary that closes the recorded gap: it holds
//! the three contexts the foundation never named, at the same layer and with
//! the same layering rules, so the plugin contracts declared beside them and
//! the proxy path that executes them cannot disagree about what a context
//! carries. `ErrorContext` is not redeclared: the foundation's own
//! [`crate::domain::error::ErrorContext`] is the fourth context.
//!
//! No `http`/`axum` type appears here: a header is a lowercase name and a
//! string value, a status is a `u16`, and a body is never carried at all — the
//! contexts carry what a plugin reads and writes, and nothing more.

use std::collections::BTreeMap;

use toolkit_macros::domain_model;
use uuid::Uuid;

/// The context an auth plugin runs in and writes its credential into.
///
/// The plugin's whole output is a header: the credential material it resolved
/// is injected as the value of one header and never stored anywhere else. The
/// identity the context carries is the projection of the request's
/// `SecurityContext` — the subject tenant and the subject identifier — which
/// is the identity both the credential store's sharing policy and the token
/// cache's isolation key are asked about.
#[domain_model]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthContext {
    /// Tenant of the subject the request was authenticated as.
    pub tenant_id: Uuid,
    /// Subject identifier, when the request was authenticated as one.
    pub subject_id: Option<Uuid>,
    /// The outbound headers the auth plugin writes.
    pub headers: BTreeMap<String, String>,
}

impl AuthContext {
    /// Builds the context of one authenticated subject.
    #[must_use]
    pub fn new(tenant_id: Uuid, subject_id: Option<Uuid>) -> Self {
        Self {
            tenant_id,
            subject_id,
            headers: BTreeMap::new(),
        }
    }

    /// The subject identifier, when the request was authenticated as one.
    #[must_use]
    pub fn subject_id(&self) -> Option<Uuid> {
        self.subject_id
    }

    /// Writes one outbound header. Names are stored lowercased, so a plugin
    /// that writes `Authorization` and a proxy that reads `authorization`
    /// cannot disagree.
    pub fn set_header(&mut self, name: &str, value: impl Into<String>) {
        self.headers.insert(header_name(name), value.into());
    }

    /// Reads one outbound header.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&header_name(name)).map(String::as_str)
    }
}

/// The context a guard or transform plugin reads the inbound request from and
/// a transform plugin mutates.
#[domain_model]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestContext {
    /// Request method.
    pub method: String,
    /// Request path, as the request carried it.
    pub path: String,
    /// Query string, without the leading `?`.
    pub query: Option<String>,
    /// Request headers, names lowercased.
    pub headers: BTreeMap<String, String>,
}

impl RequestContext {
    /// Builds the context of one request.
    #[must_use]
    pub fn new(method: String, path: String, query: Option<String>) -> Self {
        Self {
            method,
            path,
            query,
            headers: BTreeMap::new(),
        }
    }

    /// Reads one request header.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&header_name(name)).map(String::as_str)
    }

    /// Sets one request header.
    pub fn set_header(&mut self, name: &str, value: impl Into<String>) {
        self.headers.insert(header_name(name), value.into());
    }

    /// Removes one request header, leaving the rest untouched.
    pub fn remove_header(&mut self, name: &str) {
        self.headers.remove(&header_name(name));
    }
}

/// The context a guard or transform plugin reads the upstream response from
/// and a transform plugin mutates.
#[domain_model]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponseContext {
    /// Upstream response status.
    pub status: u16,
    /// Response headers, names lowercased.
    pub headers: BTreeMap<String, String>,
}

impl ResponseContext {
    /// Builds the context of one response.
    #[must_use]
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: BTreeMap::new(),
        }
    }

    /// Reads one response header.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&header_name(name)).map(String::as_str)
    }

    /// Sets one response header.
    pub fn set_header(&mut self, name: &str, value: impl Into<String>) {
        self.headers.insert(header_name(name), value.into());
    }
}

/// Lowercases a header name, so every context agrees on one spelling.
fn header_name(name: &str) -> String {
    name.to_ascii_lowercase()
}
