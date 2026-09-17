//! Internal domain DTOs shared between the REST layer and the data plane.
//!
//! These types are the in-process boundary between `api/rest` (transport) and
//! `domain/services` / `infra/proxy` (logic). They never cross the wire
//! directly — the REST DTOs in `api/rest/dto.rs` do.

use http::Method;
use uuid::Uuid;

/// A normalized inbound proxy request.
#[derive(Debug, Clone)]
pub struct ProxyRequest {
    /// HTTP method.
    pub method: Method,
    /// Request path suffix carried after the alias (may be empty).
    pub path_suffix: String,
    /// Raw query parameters in source order.
    pub query: Vec<(String, String)>,
    /// Raw request headers (name, value), pre-hop-by-hop-stripping.
    pub headers: Vec<(String, String)>,
    /// Request body (already size-validated by the handler).
    pub body: Option<bytes::Bytes>,
}

/// The result of a proxy operation.
#[derive(Debug, Clone)]
pub struct ProxyResponse {
    /// HTTP status code.
    pub status: http::StatusCode,
    /// Response headers.
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: bytes::Bytes,
}

impl ProxyResponse {
    /// Create an empty-body response with a single content-type.
    #[must_use]
    pub fn empty(status: http::StatusCode) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: bytes::Bytes::new(),
        }
    }

    /// Create a JSON body response.
    #[must_use]
    pub fn json(status: http::StatusCode, value: &serde_json::Value) -> Self {
        Self {
            status,
            headers: vec![
                ("content-type".to_owned(), "application/json".to_owned()),
                ("x-oagw-error-source".to_owned(), "gateway".to_owned()),
            ],
            body: bytes::Bytes::from(serde_json::to_vec(value).unwrap_or_default()),
        }
    }

    /// True for gateway-originated problem responses carrying the error source header.
    #[must_use]
    pub fn is_gateway_error(&self) -> bool {
        self.headers
            .iter()
            .any(|(n, v)| n.eq_ignore_ascii_case("x-oagw-error-source") && v == "gateway")
    }
}

/// Capability-scoped identity used by the data plane.
#[derive(Debug, Clone, Copy)]
pub struct ProxyIdentity {
    /// Subject (principal) that invoked the proxy.
    pub subject_id: Uuid,
    /// Tenant that owns the request.
    pub tenant_id: Uuid,
}

/// List-query parameters shared by management list endpoints (OData-style).
///
/// The toolkit `ODataParams` extractor deliberately rejects `$skip`, which the
/// OAGW spec explicitly requires, so OAGW owns its own ListParams DTO.
#[derive(Debug, Clone, Default)]
pub struct ListParams {
    /// `$filter` — OData filter expression.
    pub filter: Option<String>,
    /// `$select` — comma-separated fields to return.
    pub select: Option<String>,
    /// `$orderby` — comma-separated sort keys (`field [asc|desc]`).
    pub orderby: Option<String>,
    /// `$top` — max results (default 50, max 100).
    pub top: Option<u32>,
    /// `$skip` — offset for pagination.
    pub skip: Option<u32>,
}
