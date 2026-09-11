//! `OagwError` — the gear's closed error vocabulary.
//!
//! The variant set and the variant → (HTTP status, GTS `type`, `title`,
//! retriable) mapping are the authoritative 22-row table of
//! `cpt-cf-oagw-algo-error-mapping` (the 20 DESIGN rows plus the two ADR 0004
//! CORS rows). The table is closed: no other variant may be added without
//! extending the mapping table, and the placeholder behaviour of
//! `cpt-cf-oagw-dod-gear-registration` reuses the existing `RouteNotFound` row
//! rather than adding a new one.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One row of the authoritative error mapping table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorMapping {
    /// Rust variant name, for table-driven diagnostics and tests.
    pub variant: &'static str,
    /// HTTP status the variant maps to.
    pub status: u16,
    /// GTS instance identifier emitted as the RFC 9457 `type` field.
    pub gts_type: &'static str,
    /// Human-readable RFC 9457 `title`.
    pub title: &'static str,
    /// Whether a client may retry the request (`Retry-After` semantics).
    pub retriable: bool,
}

/// Occurrence-scoped extension fields carried by an [`OagwError`].
///
/// Fields are serialized onto the problem+json body only when the variant
/// supplies them (`inst-er-04`), so an error raised outside a request context
/// (for example a startup failure) carries no request extension fields at all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorContext {
    /// Identifier of the upstream the request was routed to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
    /// Upstream host the request was addressed to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Request path that produced the error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// RFC 9457 `instance` — URI reference identifying the occurrence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Retry guidance for the retriable rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    /// Distributed-tracing correlation identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// HTTP status the occurrence is rendered with, when the caller elevates
    /// the status its mapping row assigns without changing the row — the 409
    /// conflicts of `cpt-cf-oagw-algo-conflict-status`, which keep the GTS type
    /// the variant already carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_override: Option<u16>,
    /// Identifier of the plugin a `PluginInUse` conflict names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    /// The resources that still reference that plugin, the `referenced_by`
    /// extension member of the conflict body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referenced_by: Option<Value>,
}

impl ErrorContext {
    /// Creates an empty context: no extension field is carried.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attaches the upstream identifier.
    #[must_use]
    pub fn with_upstream_id(mut self, upstream_id: impl Into<String>) -> Self {
        self.upstream_id = Some(upstream_id.into());
        self
    }

    /// Attaches the upstream host.
    #[must_use]
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    /// Attaches the request path.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Attaches the RFC 9457 `instance` URI reference.
    #[must_use]
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// Attaches the retry guidance (seconds).
    #[must_use]
    pub fn with_retry_after_seconds(mut self, seconds: u64) -> Self {
        self.retry_after_seconds = Some(seconds);
        self
    }

    /// Attaches the tracing correlation identifier.
    #[must_use]
    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }

    /// Elevates the status the occurrence is rendered with, leaving the mapping
    /// row — and therefore the GTS type — untouched.
    #[must_use]
    pub fn with_status_override(mut self, status: u16) -> Self {
        self.status_override = Some(status);
        self
    }

    /// Names the plugin a `PluginInUse` conflict is about.
    #[must_use]
    pub fn with_plugin_id(mut self, plugin_id: impl Into<String>) -> Self {
        self.plugin_id = Some(plugin_id.into());
        self
    }

    /// Carries the `referenced_by` extension member of a `PluginInUse` body.
    #[must_use]
    pub fn with_referenced_by(mut self, referenced_by: Value) -> Self {
        self.referenced_by = Some(referenced_by);
        self
    }
}

/// Declares the closed `OagwError` variant set and its mapping table.
///
/// Every row below is one line of the `cpt-cf-oagw-algo-error-mapping` table,
/// written in table order, with the GTS type and title copied verbatim from the
/// FEATURE table (the two CORS rows come from ADR 0004). The macro expands the
/// same input into the enum, the per-variant lookup and the flattened table, so
/// the two views of the table cannot drift apart.
macro_rules! oagw_error_table {
    (
        $(
            $(#[$doc:meta])*
            $variant:ident / $ctor:ident => $status:literal , $gts:literal , $title:literal , $retriable:literal ;
        )*
    ) => {
        /// The oagw error vocabulary (`cpt-cf-oagw-fr-error-codes`).
        ///
        /// Every variant carries the occurrence `detail` plus the optional
        /// [`ErrorContext`] extension fields. `detail` names the offending key
        /// for the configuration-rejected startup case (§3 "startup failure
        /// error surface"), the missing dependency for an unresolvable platform
        /// dependency, and the owning feature for a placeholder response.
        #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
        pub enum OagwError {
            $(
                $(#[$doc])*
                #[error("{detail}")]
                $variant {
                    /// Occurrence-specific explanation.
                    detail: String,
                    /// Occurrence-scoped extension fields, boxed so the
                    /// error stays small enough to cross `Result` boundaries.
                    context: Box<ErrorContext>,
                },
            )*
        }

        impl OagwError {
            $(
                /// Builds the variant from its table row, with no occurrence
                /// extension fields attached yet.
                #[must_use]
                pub fn $ctor(detail: impl Into<String>) -> Self {
                    Self::$variant {
                        detail: detail.into(),
                        context: Box::new(ErrorContext::new()),
                    }
                }
            )*
        }

        impl OagwError {
            /// Constructor used by the mapping-table-driven tests and by
            /// callers that address a variant by its table row.
            #[must_use]
            pub fn from_variant_name(variant: &str, detail: impl Into<String>) -> Option<Self> {
                $(
                    if variant == stringify!($variant) {
                        return Some(Self::$ctor(detail));
                    }
                )*
                None
            }

            /// Resolves the variant's row of the authoritative mapping table.
            #[must_use]
            pub const fn mapping(&self) -> ErrorMapping {
                // @cpt-begin:cpt-cf-oagw-algo-error-mapping:p1:inst-em-01
                // Match the `OagwError` variant against the closed table.
                // @cpt-begin:cpt-cf-oagw-algo-error-mapping:p1:inst-em-02
                // Resolve the HTTP status and GTS `type` of the row; the two
                // ADR 0004 CORS variants resolve through the same table.
                match self {
                    $(
                        Self::$variant { .. } => ErrorMapping {
                            variant: stringify!($variant),
                            status: $status,
                            gts_type: $gts,
                            title: $title,
                            retriable: $retriable,
                        },
                    )*
                }
                // @cpt-end:cpt-cf-oagw-algo-error-mapping:p1:inst-em-02
                // @cpt-end:cpt-cf-oagw-algo-error-mapping:p1:inst-em-01
            }

            /// HTTP status the variant maps to.
            #[must_use]
            pub const fn status(&self) -> u16 {
                self.mapping().status
            }

            /// GTS instance identifier emitted as the RFC 9457 `type` field.
            #[must_use]
            pub const fn gts_type(&self) -> &'static str {
                self.mapping().gts_type
            }

            /// Human-readable RFC 9457 `title`.
            #[must_use]
            pub const fn title(&self) -> &'static str {
                self.mapping().title
            }

            /// Whether a client may retry the request.
            #[must_use]
            pub const fn is_retriable(&self) -> bool {
                self.mapping().retriable
            }

            /// The occurrence-specific explanation.
            #[must_use]
            pub fn detail(&self) -> &str {
                match self {
                    $(Self::$variant { detail, .. } => detail,)*
                }
            }

            /// The occurrence-scoped extension fields the variant carries.
            #[must_use]
            pub const fn context(&self) -> &ErrorContext {
                match self {
                    $(Self::$variant { context, .. } => &**context,)*
                }
            }

            /// Mutable access to the occurrence-scoped extension fields.
            pub fn context_mut(&mut self) -> &mut ErrorContext {
                match self {
                    $(Self::$variant { context, .. } => &mut **context,)*
                }
            }

            /// Replaces the occurrence-scoped extension fields.
            #[must_use]
            pub fn with_context(mut self, context: ErrorContext) -> Self {
                *self.context_mut() = context;
                self
            }
        }

        /// The authoritative mapping table, in FEATURE table order.
        pub const MAPPING_TABLE: &[ErrorMapping] = &[
            $(
                ErrorMapping {
                    variant: stringify!($variant),
                    status: $status,
                    gts_type: $gts,
                    title: $title,
                    retriable: $retriable,
                },
            )*
        ];
    };
}

oagw_error_table! {
    /// General route validation error.
    RouteError / route_error => 400, "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1", "Route error", false;
    /// Request validation failed.
    ValidationError / validation_error => 400, "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1", "Validation error", false;
    /// `X-OAGW-Target-Host` header required for a multi-endpoint upstream with a common-suffix alias.
    MissingTargetHost / missing_target_host => 400, "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1", "Missing target host", false;
    /// `X-OAGW-Target-Host` header format is invalid.
    InvalidTargetHost / invalid_target_host => 400, "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1", "Invalid target host", false;
    /// `X-OAGW-Target-Host` value does not match any configured endpoint.
    UnknownTargetHost / unknown_target_host => 400, "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1", "Unknown target host", false;
    /// Authentication to upstream failed.
    AuthenticationFailed / authentication_failed => 401, "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1", "Authentication failed", false;
    /// No matching route found.
    RouteNotFound / route_not_found => 404, "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1", "Route not found", false;
    /// Plugin in use.
    PluginInUse / plugin_in_use => 409, "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1", "Plugin in use", false;
    /// Request payload exceeds limit.
    PayloadTooLarge / payload_too_large => 413, "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1", "Payload too large", false;
    /// Rate limit exceeded.
    RateLimitExceeded / rate_limit_exceeded => 429, "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1", "Rate limit exceeded", true;
    /// Referenced secret not found.
    SecretNotFound / secret_not_found => 500, "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1", "Secret not found", false;
    /// Protocol-level error.
    ProtocolError / protocol_error => 502, "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1", "Protocol error", false;
    /// Upstream service error.
    DownstreamError / downstream_error => 502, "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1", "Downstream error", true;
    /// Stream connection aborted.
    StreamAborted / stream_aborted => 502, "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1", "Stream aborted", false;
    /// Upstream link unavailable.
    LinkUnavailable / link_unavailable => 503, "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1", "Link unavailable", true;
    /// Circuit breaker open.
    CircuitBreakerOpen / circuit_breaker_open => 503, "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1", "Circuit breaker open", true;
    /// Plugin not found.
    PluginNotFound / plugin_not_found => 503, "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1", "Plugin not found", false;
    /// Connection timeout.
    ConnectionTimeout / connection_timeout => 504, "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1", "Connection timeout", true;
    /// Request timeout.
    RequestTimeout / request_timeout => 504, "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1", "Request timeout", true;
    /// Idle timeout.
    IdleTimeout / idle_timeout => 504, "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1", "Idle timeout", true;
    /// Origin not in the upstream/route allowed origins list (ADR 0004).
    CorsOriginNotAllowed / cors_origin_not_allowed => 403, "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1", "CORS Origin Not Allowed", false;
    /// Method not in the upstream/route allowed methods list (ADR 0004).
    CorsMethodNotAllowed / cors_method_not_allowed => 403, "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1", "CORS Method Not Allowed", false;
}

impl OagwError {
    /// Wraps `detail` into the variant named by `self`, keeping the context.
    ///
    /// Used by callers that resolve a variant from the table at runtime.
    #[must_use]
    pub fn same_variant_with(&self, detail: impl Into<String>) -> Self {
        let context = self.context().clone();
        match Self::from_variant_name(self.mapping().variant, detail) {
            Some(err) => err.with_context(context),
            None => Self::route_error(format!(
                "variant {} has no mapping row",
                self.mapping().variant
            ))
            .with_context(context),
        }
    }
}

impl From<&OagwError> for ErrorMapping {
    fn from(err: &OagwError) -> Self {
        err.mapping()
    }
}

// @cpt-begin:cpt-cf-oagw-dod-domain-error:p1:inst-full
/// The closed set of domain violation kinds (`cpt-cf-oagw-dod-domain-error`).
///
/// The set is closed for this release: one [`DomainError`] variant per kind
/// below, and no kind outside the list. Every kind carries a stable identity
/// through [`ViolationKind::rule_id`], so a caller can test for a kind without
/// matching the variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViolationKind {
    /// The payload carries a field the aggregate does not declare, or a
    /// declared field whose value is not of the declared type.
    UnknownField,
    /// An identity field is absent or is not a UUID.
    MalformedUuid,
    /// An endpoint port is outside `1..=65535`.
    OutOfRangePort,
    /// The alias does not match the alias pattern.
    MalformedAlias,
    /// A tag does not match the tag pattern or repeats within the resource.
    MalformedTag,
    /// An endpoint of the pool violates an endpoint rule.
    EndpointRule,
    /// The endpoint pool mixes schemes or ports.
    EndpointHeterogeneity,
    /// The rate-limit block is not shaped as the schema declares.
    RateLimitShape,
    /// The cors block is not shaped as the schema declares.
    CorsShape,
    /// The headers block is not shaped as the schema declares.
    HeadersShape,
    /// The auth block is not shaped as the schema declares.
    AuthShape,
    /// The plugins block or the `Plugin` aggregate is not shaped as declared.
    PluginShape,
    /// The route `match` block is not shaped as the schema declares.
    RouteMatchShape,
    /// A resource with the same key already exists in the tenant.
    AlreadyExists,
    /// The resource does not exist in the caller's tenant.
    NotFound,
}

impl ViolationKind {
    /// The closed kind set, in the order `cpt-cf-oagw-dod-domain-error`
    /// enumerates it.
    pub const ALL: &'static [ViolationKind] = &[
        Self::UnknownField,
        Self::MalformedUuid,
        Self::OutOfRangePort,
        Self::MalformedAlias,
        Self::MalformedTag,
        Self::EndpointRule,
        Self::EndpointHeterogeneity,
        Self::RateLimitShape,
        Self::CorsShape,
        Self::HeadersShape,
        Self::AuthShape,
        Self::PluginShape,
        Self::RouteMatchShape,
        Self::AlreadyExists,
        Self::NotFound,
    ];

    /// The stable, testable identity of the kind.
    #[must_use]
    pub const fn rule_id(self) -> &'static str {
        match self {
            Self::UnknownField => "oagw.domain.unknown_field",
            Self::MalformedUuid => "oagw.domain.malformed_uuid",
            Self::OutOfRangePort => "oagw.domain.out_of_range_port",
            Self::MalformedAlias => "oagw.domain.malformed_alias",
            Self::MalformedTag => "oagw.domain.malformed_tag",
            Self::EndpointRule => "oagw.domain.endpoint_rule",
            Self::EndpointHeterogeneity => "oagw.domain.endpoint_heterogeneity",
            Self::RateLimitShape => "oagw.domain.rate_limit_shape",
            Self::CorsShape => "oagw.domain.cors_shape",
            Self::HeadersShape => "oagw.domain.headers_shape",
            Self::AuthShape => "oagw.domain.auth_shape",
            Self::PluginShape => "oagw.domain.plugin_shape",
            Self::RouteMatchShape => "oagw.domain.route_match_shape",
            Self::AlreadyExists => "oagw.domain.already_exists",
            Self::NotFound => "oagw.domain.not_found",
        }
    }

    /// The human-readable name of the kind, as the FEATURE enumerates it.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::UnknownField => "unknown field",
            Self::MalformedUuid => "malformed UUID",
            Self::OutOfRangePort => "out-of-range port",
            Self::MalformedAlias => "malformed alias",
            Self::MalformedTag => "malformed tag",
            Self::EndpointRule => "endpoint rule violation",
            Self::EndpointHeterogeneity => "endpoint heterogeneity",
            Self::RateLimitShape => "rate-limit shape",
            Self::CorsShape => "cors shape",
            Self::HeadersShape => "headers shape",
            Self::AuthShape => "auth shape",
            Self::PluginShape => "plugin shape",
            Self::RouteMatchShape => "route match shape",
            Self::AlreadyExists => "already-exists",
            Self::NotFound => "not-found",
        }
    }
}

/// One violated rule: the closed kind, the offending field path and the
/// occurrence message naming it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The closed kind of the violated rule.
    pub kind: ViolationKind,
    /// Field path of the offending field, e.g. `server.endpoints[0].port`.
    pub field: String,
    /// Occurrence-specific explanation.
    pub message: String,
}

impl Violation {
    /// Builds one violated rule from its kind, field path and message.
    #[must_use]
    pub fn new(kind: ViolationKind, field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind,
            field: field.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({}): '{}' {}",
            self.kind.rule_id(),
            self.kind.title(),
            self.field,
            self.message
        )
    }
}

/// Every violated rule of one payload, in field order
/// (`cpt-cf-oagw-flow-resource-validation`).
///
/// The collector is also the shape the violations reach the caller in: a
/// non-empty collector becomes the single [`DomainError::Invalid`] result, so a
/// payload that violates several rules reports all of them at once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Violations(Vec<Violation>);

impl Violations {
    /// An empty collector.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one violated rule.
    pub fn record(
        &mut self,
        kind: ViolationKind,
        field: impl Into<String>,
        message: impl Into<String>,
    ) {
        // @cpt-begin:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-12
        // FOR EACH violated rule encountered in the preceding steps.
        // @cpt-begin:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-13
        // Collect the rule id and a message naming the offending field path.
        self.0.push(Violation::new(kind, field, message));
        // @cpt-end:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-13
        // @cpt-end:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-12
    }

    /// Records an already-built violated rule.
    pub fn push(&mut self, violation: Violation) {
        self.0.push(violation);
    }

    /// Whether no rule was violated.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many rules were violated.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The violated rules, in the order they were recorded.
    #[must_use]
    pub fn as_slice(&self) -> &[Violation] {
        &self.0
    }

    /// Reports whether a violation was already recorded for a field path.
    ///
    /// The cross-field checks consult it, so a field the shape pass already
    /// reported is not reported twice for the same root cause.
    #[must_use]
    pub fn has_field(&self, field: &str) -> bool {
        self.0.iter().any(|violation| violation.field == field)
    }

    /// Orders the recorded violations in field order and reports them all.
    ///
    /// `field_order` is the declared field order of the aggregate under
    /// validation: a violation is placed at the position of the top-level field
    /// its field path starts with, and a field the aggregate does not declare —
    /// an unknown field — follows the declared fields in discovery order.
    #[must_use]
    pub fn finish(self, field_order: &[&str]) -> DomainError {
        let mut violations = self.0;
        violations.sort_by_key(|violation| {
            let root = violation
                .field
                .split('.')
                .next()
                .unwrap_or(&violation.field);
            let root = root.split('[').next().unwrap_or(root);
            let position = field_order
                .iter()
                .position(|field| *field == root)
                .unwrap_or(usize::MAX);
            (position, 0_usize)
        });
        // A payload that violates exactly one rule keeps that rule's own
        // variant, so the closed conversion of a not-found violation still
        // reaches the 404 row of the mapping table; only a payload that
        // violates several rules at once is carried by the aggregate variant.
        if let [violation] = violations.as_slice() {
            return DomainError::from_violation(violation.clone());
        }
        DomainError::Invalid(Self(violations))
    }

    /// Reports the collected violations, or succeeds when none were recorded.
    ///
    /// # Errors
    /// Returns every violated rule in field order as one
    /// [`DomainError::Invalid`].
    pub fn into_result(self, field_order: &[&str]) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-14
        // RETURN all collected violations together — never only the first — or
        // succeed when the collection is empty.
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(self.finish(field_order))
        }
        // @cpt-end:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-14
    }
}

impl std::fmt::Display for Violations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let rendered = self
            .0
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>();
        write!(f, "{}", rendered.join("; "))
    }
}

/// The domain error surface (`cpt-cf-oagw-dod-domain-error`).
///
/// One variant per closed violation kind, each naming the offending field path
/// and carrying the occurrence message, plus [`DomainError::Invalid`], the
/// aggregate shape the violations collector returns for a payload that violates
/// several rules at once. `DomainError` converts into the gear error contract
/// through `From<DomainError> for OagwError`: `not-found` reuses the existing
/// 404 `RouteNotFound` row and every other kind — `already-exists` included —
/// reuses the existing 400 `ValidationError` row, so no row is added to the
/// mapping table above.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    /// The payload carries a field the aggregate does not declare, or a
    /// declared field whose value is not of the declared type.
    #[error("{}: '{field}' {message}", self.head())]
    UnknownField {
        /// Field path of the offending field.
        field: String,
        /// Occurrence-specific explanation.
        message: String,
    },
    /// An identity field is absent or is not a UUID.
    #[error("{}: '{field}' {message}", self.head())]
    MalformedUuid {
        /// Field path of the offending field.
        field: String,
        /// Occurrence-specific explanation.
        message: String,
    },
    /// An endpoint port is outside `1..=65535`.
    #[error("{}: '{field}' {message}", self.head())]
    OutOfRangePort {
        /// Field path of the offending field.
        field: String,
        /// Occurrence-specific explanation.
        message: String,
    },
    /// The alias does not match the alias pattern.
    #[error("{}: '{field}' {message}", self.head())]
    MalformedAlias {
        /// Field path of the offending field.
        field: String,
        /// Occurrence-specific explanation.
        message: String,
    },
    /// A tag does not match the tag pattern or repeats within the resource.
    #[error("{}: '{field}' {message}", self.head())]
    MalformedTag {
        /// Field path of the offending field.
        field: String,
        /// Occurrence-specific explanation.
        message: String,
    },
    /// An endpoint of the pool violates an endpoint rule.
    #[error("{}: '{field}' {message}", self.head())]
    EndpointRule {
        /// Field path of the offending field.
        field: String,
        /// Occurrence-specific explanation.
        message: String,
    },
    /// The endpoint pool mixes schemes or ports.
    #[error("{}: '{field}' {message}", self.head())]
    EndpointHeterogeneity {
        /// Field path of the offending field.
        field: String,
        /// Occurrence-specific explanation.
        message: String,
    },
    /// The rate-limit block is not shaped as the schema declares.
    #[error("{}: '{field}' {message}", self.head())]
    RateLimitShape {
        /// Field path of the offending field.
        field: String,
        /// Occurrence-specific explanation.
        message: String,
    },
    /// The cors block is not shaped as the schema declares.
    #[error("{}: '{field}' {message}", self.head())]
    CorsShape {
        /// Field path of the offending field.
        field: String,
        /// Occurrence-specific explanation.
        message: String,
    },
    /// The headers block is not shaped as the schema declares.
    #[error("{}: '{field}' {message}", self.head())]
    HeadersShape {
        /// Field path of the offending field.
        field: String,
        /// Occurrence-specific explanation.
        message: String,
    },
    /// The auth block is not shaped as the schema declares.
    #[error("{}: '{field}' {message}", self.head())]
    AuthShape {
        /// Field path of the offending field.
        field: String,
        /// Occurrence-specific explanation.
        message: String,
    },
    /// The plugins block or the `Plugin` aggregate is not shaped as declared.
    #[error("{}: '{field}' {message}", self.head())]
    PluginShape {
        /// Field path of the offending field.
        field: String,
        /// Occurrence-specific explanation.
        message: String,
    },
    /// The route `match` block is not shaped as the schema declares.
    #[error("{}: '{field}' {message}", self.head())]
    RouteMatchShape {
        /// Field path of the offending field.
        field: String,
        /// Occurrence-specific explanation.
        message: String,
    },
    /// A resource with the same key already exists in the tenant.
    #[error("{}: '{field}' {message}", self.head())]
    AlreadyExists {
        /// Field path or key the duplicate collides with.
        field: String,
        /// Occurrence-specific explanation.
        message: String,
    },
    /// The resource does not exist in the caller's tenant.
    #[error("{}: '{field}' {message}", self.head())]
    NotFound {
        /// Field path or key that was looked up.
        field: String,
        /// Occurrence-specific explanation.
        message: String,
    },
    /// Every violated rule of one payload, in field order (`inst-rv-16`): the
    /// shape the violations collector returns.
    #[error("{0}")]
    Invalid(Violations),
}

impl DomainError {
    /// The `"<rule-id> (<title>)"` head every variant renders, so the Display
    /// of a single-violation error names the violated rule exactly as the
    /// collector does.
    fn head(&self) -> String {
        let (rule_id, title) = self
            .kind()
            .map_or(("oagw.domain.invalid", "several rules violated"), |kind| {
                (kind.rule_id(), kind.title())
            });
        format!("{rule_id} ({title})")
    }

    /// Builds the typed already-exists error of a duplicate key.
    #[must_use]
    pub fn already_exists(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::AlreadyExists {
            field: field.into(),
            message: message.into(),
        }
    }

    /// Builds the typed not-found error of a tenant-scoped lookup miss.
    #[must_use]
    pub fn not_found(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::NotFound {
            field: field.into(),
            message: message.into(),
        }
    }

    /// Builds the single-violation error of `violation`.
    #[must_use]
    pub fn from_violation(violation: Violation) -> Self {
        let Violation {
            kind,
            field,
            message,
        } = violation;
        match kind {
            ViolationKind::UnknownField => Self::UnknownField { field, message },
            ViolationKind::MalformedUuid => Self::MalformedUuid { field, message },
            ViolationKind::OutOfRangePort => Self::OutOfRangePort { field, message },
            ViolationKind::MalformedAlias => Self::MalformedAlias { field, message },
            ViolationKind::MalformedTag => Self::MalformedTag { field, message },
            ViolationKind::EndpointRule => Self::EndpointRule { field, message },
            ViolationKind::EndpointHeterogeneity => Self::EndpointHeterogeneity { field, message },
            ViolationKind::RateLimitShape => Self::RateLimitShape { field, message },
            ViolationKind::CorsShape => Self::CorsShape { field, message },
            ViolationKind::HeadersShape => Self::HeadersShape { field, message },
            ViolationKind::AuthShape => Self::AuthShape { field, message },
            ViolationKind::PluginShape => Self::PluginShape { field, message },
            ViolationKind::RouteMatchShape => Self::RouteMatchShape { field, message },
            ViolationKind::AlreadyExists => Self::AlreadyExists { field, message },
            ViolationKind::NotFound => Self::NotFound { field, message },
        }
    }

    /// The closed kind of a single-violation error.
    #[must_use]
    pub fn kind(&self) -> Option<ViolationKind> {
        match self {
            Self::UnknownField { .. } => Some(ViolationKind::UnknownField),
            Self::MalformedUuid { .. } => Some(ViolationKind::MalformedUuid),
            Self::OutOfRangePort { .. } => Some(ViolationKind::OutOfRangePort),
            Self::MalformedAlias { .. } => Some(ViolationKind::MalformedAlias),
            Self::MalformedTag { .. } => Some(ViolationKind::MalformedTag),
            Self::EndpointRule { .. } => Some(ViolationKind::EndpointRule),
            Self::EndpointHeterogeneity { .. } => Some(ViolationKind::EndpointHeterogeneity),
            Self::RateLimitShape { .. } => Some(ViolationKind::RateLimitShape),
            Self::CorsShape { .. } => Some(ViolationKind::CorsShape),
            Self::HeadersShape { .. } => Some(ViolationKind::HeadersShape),
            Self::AuthShape { .. } => Some(ViolationKind::AuthShape),
            Self::PluginShape { .. } => Some(ViolationKind::PluginShape),
            Self::RouteMatchShape { .. } => Some(ViolationKind::RouteMatchShape),
            Self::AlreadyExists { .. } => Some(ViolationKind::AlreadyExists),
            Self::NotFound { .. } => Some(ViolationKind::NotFound),
            Self::Invalid(_) => None,
        }
    }

    /// The offending field path of a single-violation error.
    #[must_use]
    pub fn field(&self) -> &str {
        match self {
            Self::UnknownField { field, .. }
            | Self::MalformedUuid { field, .. }
            | Self::OutOfRangePort { field, .. }
            | Self::MalformedAlias { field, .. }
            | Self::MalformedTag { field, .. }
            | Self::EndpointRule { field, .. }
            | Self::EndpointHeterogeneity { field, .. }
            | Self::RateLimitShape { field, .. }
            | Self::CorsShape { field, .. }
            | Self::HeadersShape { field, .. }
            | Self::AuthShape { field, .. }
            | Self::PluginShape { field, .. }
            | Self::RouteMatchShape { field, .. }
            | Self::AlreadyExists { field, .. }
            | Self::NotFound { field, .. } => field,
            Self::Invalid(violations) => violations
                .as_slice()
                .first()
                .map_or("payload", |violation| violation.field.as_str()),
        }
    }

    /// The occurrence message of a single-violation error.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::UnknownField { message, .. }
            | Self::MalformedUuid { message, .. }
            | Self::OutOfRangePort { message, .. }
            | Self::MalformedAlias { message, .. }
            | Self::MalformedTag { message, .. }
            | Self::EndpointRule { message, .. }
            | Self::EndpointHeterogeneity { message, .. }
            | Self::RateLimitShape { message, .. }
            | Self::CorsShape { message, .. }
            | Self::HeadersShape { message, .. }
            | Self::AuthShape { message, .. }
            | Self::PluginShape { message, .. }
            | Self::RouteMatchShape { message, .. }
            | Self::AlreadyExists { message, .. }
            | Self::NotFound { message, .. } => message,
            Self::Invalid(violations) => violations
                .as_slice()
                .first()
                .map_or("no rule violated", |violation| violation.message.as_str()),
        }
    }

    /// Every violated rule the error carries, as owned violations: the
    /// aggregate shape reports all of them, a single-violation error reports
    /// the one rule it was built from.
    #[must_use]
    pub fn to_violations(&self) -> Vec<Violation> {
        match self {
            Self::Invalid(violations) => violations.as_slice().to_vec(),
            single => single
                .kind()
                .map(|kind| vec![Violation::new(kind, single.field(), single.message())])
                .unwrap_or_default(),
        }
    }
}

/// Converts a `DomainError` into the gear error contract
/// (`cpt-cf-oagw-dod-domain-error`).
///
/// The conversion is feature-local: `not-found` maps onto the existing 404
/// `RouteNotFound` row, every other kind — `already-exists` included — onto the
/// existing 400 `ValidationError` row. No GTS error type is introduced and no
/// row is added to [`MAPPING_TABLE`]; the 409 conflict status a route-match or
/// alias conflict carries is decided by the management-API layer.
impl From<DomainError> for OagwError {
    fn from(error: DomainError) -> Self {
        match &error {
            DomainError::NotFound { field, message } => {
                Self::route_not_found(format!("oagw.domain: '{field}' not found: {message}"))
            }
            other => Self::validation_error(format!("oagw.domain: {other}")),
        }
    }
}

impl From<&DomainError> for OagwError {
    fn from(error: &DomainError) -> Self {
        Self::from(error.clone())
    }
}
// @cpt-end:cpt-cf-oagw-dod-domain-error:p1:inst-full

// @cpt-begin:cpt-cf-oagw-dod-error-status-decision:p1:inst-full
/// The status decision of a rendered occurrence.
///
/// The mapping table stays closed at its 22 rows: the management API elevates a
/// conflict to 409 by overriding the status the variant's row assigns, never by
/// adding a row or a variant. The override is carried on the occurrence's
/// context so the rendering layer can apply it to the body's `status` member
/// and to the HTTP status alike, and the GTS `type` of the row is kept —
/// `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` for the alias and
/// route-match conflicts the control plane renders through the existing
/// `ValidationError` row.
impl OagwError {
    /// Overrides the HTTP status the occurrence is rendered with, leaving the
    /// variant, its mapping row and its GTS `type` untouched.
    #[must_use]
    pub fn with_status(mut self, status: u16) -> Self {
        self.context_mut().status_override = Some(status);
        self
    }

    /// The status the occurrence is rendered with: the override when one was
    /// attached and [`Self::is_representable_status`] admits it, otherwise the
    /// status its mapping row assigns.
    ///
    /// An override the wire cannot represent is ignored here rather than
    /// rendered, so the HTTP status and the body's `status` member are the same
    /// value for every occurrence: an unparsable override never renders a 500
    /// that contradicts its own body. The occurrence keeps the override it was
    /// given, the render boundary being the layer that reports the drop.
    #[must_use]
    pub fn effective_status(&self) -> u16 {
        self.context()
            .status_override
            .filter(|status| Self::is_representable_status(*status))
            .unwrap_or_else(|| self.mapping().status)
    }

    /// True when the occurrence carries a status override.
    #[must_use]
    pub fn has_status_override(&self) -> bool {
        self.context().status_override.is_some()
    }

    /// True when the HTTP status the wire can represent covers `status`.
    ///
    /// A status runs from 100 to 599, so an override outside that range has no
    /// `StatusCode` of its own: rendering it would coerce the HTTP status and
    /// leave the body's `status` member carrying the value the response line
    /// cannot name.
    #[must_use]
    pub fn is_representable_status(status: u16) -> bool {
        (100..=599).contains(&status)
    }

    /// Names the plugin a `PluginInUse` conflict is about.
    #[must_use]
    pub fn with_plugin_id(mut self, plugin_id: impl Into<String>) -> Self {
        self.context_mut().plugin_id = Some(plugin_id.into());
        self
    }

    /// Carries the `referenced_by` extension member of a `PluginInUse` body.
    #[must_use]
    pub fn with_referenced_by(mut self, referenced_by: Value) -> Self {
        self.context_mut().referenced_by = Some(referenced_by);
        self
    }
}
// @cpt-end:cpt-cf-oagw-dod-error-status-decision:p1:inst-full

/// Compile-time pin of the canonical error schema prefix every row derives
/// from, kept next to the table so a table row can never silently point at a
/// foreign schema.
#[cfg(test)]
const MAPPING_ERROR_SCHEMA_MARKER: &str = "gts.cf.core.errors.err.v1~";

#[cfg(test)]
mod tests {
    use super::*;

    const DESIGN_ROWS: usize = 20;
    const CORS_ROWS: usize = 2;
    const GTS_ERROR_SCHEMA: &str = "gts.cf.core.errors.err.v1~";

    fn row(variant: &str) -> ErrorMapping {
        MAPPING_TABLE
            .iter()
            .copied()
            .find(|row| row.variant == variant)
            .unwrap_or_else(|| panic!("variant {variant} missing from the mapping table"))
    }

    #[test]
    fn table_is_the_closed_22_row_set() {
        assert_eq!(
            MAPPING_TABLE.len(),
            DESIGN_ROWS + CORS_ROWS,
            "the mapping table must stay the 20 DESIGN rows plus the 2 ADR 0004 CORS rows"
        );
        let mut names: Vec<_> = MAPPING_TABLE.iter().map(|row| row.variant).collect();
        names.sort_unstable();
        let unique = names.len() == MAPPING_TABLE.len();
        assert!(
            unique,
            "mapping table carries duplicate variants: {names:?}"
        );
    }

    #[test]
    fn every_variant_is_addressable_from_the_table() {
        for expected in MAPPING_TABLE {
            let err = OagwError::from_variant_name(expected.variant, "occurrence")
                .unwrap_or_else(|| panic!("variant {} not constructible", expected.variant));
            assert_eq!(err.mapping(), *expected, "row drift for {expected:?}");
            assert_eq!(err.detail(), "occurrence");
        }
    }

    #[test]
    fn unknown_variant_name_is_not_constructible() {
        assert!(OagwError::from_variant_name("NotAnOagwVariant", "x").is_none());
    }

    #[test]
    fn an_override_outside_the_http_status_range_is_ignored() {
        // The statuses the wire can represent run 100 to 599: an override
        // outside that range is ignored by the status decision so that the HTTP
        // status and the body's `status` member are the same value, the row
        // keeping the status it assigns.
        for status in [0, 99, 600, u16::MAX] {
            let overridden = OagwError::validation_error("rejected").with_status(status);
            assert!(!OagwError::is_representable_status(status));
            assert_eq!(
                overridden.effective_status(),
                400,
                "the row's status is rendered for {status}"
            );
            assert!(
                overridden.has_status_override(),
                "the occurrence keeps the override it was given"
            );
        }
        for status in [100u16, 409, 599] {
            let overridden = OagwError::validation_error("rejected").with_status(status);
            assert!(OagwError::is_representable_status(status));
            assert_eq!(overridden.effective_status(), status);
        }
    }

    #[test]
    fn all_rows_are_canonical_error_instances() {
        for row in MAPPING_TABLE {
            assert!(
                row.gts_type
                    .starts_with("gts.cf.core.errors.err.v1~cf.oagw."),
                "GTS type {} must be an oagw error instance id",
                row.gts_type
            );
            assert!(
                row.gts_type.ends_with(".v1"),
                "GTS type {} must be versioned",
                row.gts_type
            );
        }
    }

    #[test]
    fn status_classes_follow_the_table() {
        assert_eq!(row("RouteError").status, 400);
        assert_eq!(row("ValidationError").status, 400);
        assert_eq!(row("MissingTargetHost").status, 400);
        assert_eq!(row("InvalidTargetHost").status, 400);
        assert_eq!(row("UnknownTargetHost").status, 400);
        assert_eq!(row("AuthenticationFailed").status, 401);
        assert_eq!(row("RouteNotFound").status, 404);
        assert_eq!(row("PluginInUse").status, 409);
        assert_eq!(row("PayloadTooLarge").status, 413);
        assert_eq!(row("RateLimitExceeded").status, 429);
        assert_eq!(row("SecretNotFound").status, 500);
        assert_eq!(row("ProtocolError").status, 502);
        assert_eq!(row("DownstreamError").status, 502);
        assert_eq!(row("StreamAborted").status, 502);
        assert_eq!(row("LinkUnavailable").status, 503);
        assert_eq!(row("CircuitBreakerOpen").status, 503);
        assert_eq!(row("PluginNotFound").status, 503);
        assert_eq!(row("ConnectionTimeout").status, 504);
        assert_eq!(row("RequestTimeout").status, 504);
        assert_eq!(row("IdleTimeout").status, 504);
        assert_eq!(row("CorsOriginNotAllowed").status, 403);
        assert_eq!(row("CorsMethodNotAllowed").status, 403);
    }

    #[test]
    fn gts_types_are_verbatim() {
        assert_eq!(
            row("RouteError").gts_type,
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
        assert_eq!(
            row("RouteNotFound").gts_type,
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
        assert_eq!(
            row("CorsOriginNotAllowed").gts_type,
            "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
        );
        assert_eq!(
            row("CorsMethodNotAllowed").gts_type,
            "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
        );
        assert_eq!(
            row("SecretNotFound").gts_type,
            "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"
        );
        assert_eq!(
            row("IdleTimeout").gts_type,
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1"
        );
    }

    #[test]
    fn retriable_rows_match_the_table() {
        let retriable = [
            "RateLimitExceeded",
            "DownstreamError",
            "LinkUnavailable",
            "CircuitBreakerOpen",
            "ConnectionTimeout",
            "RequestTimeout",
            "IdleTimeout",
        ];
        for expected in MAPPING_TABLE {
            assert_eq!(
                expected.retriable,
                retriable.contains(&expected.variant),
                "retriable flag mismatch for {}",
                expected.variant
            );
        }
        assert_eq!(retriable.len(), 7);
        assert_eq!(MAPPING_ERROR_SCHEMA_MARKER, GTS_ERROR_SCHEMA);
    }
    #[test]
    fn context_extensions_are_carried_per_occurrence() {
        let err = OagwError::rate_limit_exceeded("tenant quota exhausted")
            .with_context(ErrorContext::new().with_retry_after_seconds(7));
        assert_eq!(err.context().retry_after_seconds, Some(7));
        assert!(err.is_retriable());

        let err = OagwError::downstream_error("upstream 502").with_context(
            ErrorContext::new()
                .with_upstream_id("payments")
                .with_host("api.example.com"),
        );
        assert_eq!(err.context().upstream_id.as_deref(), Some("payments"));
        assert_eq!(err.context().host.as_deref(), Some("api.example.com"));
        assert_eq!(err.context().retry_after_seconds, None);
    }

    #[test]
    fn with_context_keeps_the_variant_identity() {
        let err = OagwError::route_not_found("no matching route")
            .with_context(ErrorContext::new().with_path("/oagw/v1/proxy/payments"));
        let moved = err.with_context(ErrorContext::new());
        assert_eq!(moved.mapping().variant, "RouteNotFound");
        assert_eq!(moved.context(), &ErrorContext::new());
    }
}

/// Tests of `cpt-cf-oagw-dod-domain-error`: the closed kind set, the violations
/// collector and the conversion into the gear error contract.
#[cfg(test)]
mod domain_tests {
    use super::*;
    use crate::domain::model::Upstream;

    /// The 15 closed violation kinds `cpt-cf-oagw-dod-domain-error` enumerates.
    const CLOSED_KINDS: usize = 15;

    #[test]
    fn the_kind_set_is_the_closed_fifteen_kinds() {
        assert_eq!(ViolationKind::ALL.len(), CLOSED_KINDS);
        let expected = [
            "unknown field",
            "malformed UUID",
            "out-of-range port",
            "malformed alias",
            "malformed tag",
            "endpoint rule violation",
            "endpoint heterogeneity",
            "rate-limit shape",
            "cors shape",
            "headers shape",
            "auth shape",
            "plugin shape",
            "route match shape",
            "already-exists",
            "not-found",
        ];
        for (kind, title) in ViolationKind::ALL.iter().zip(expected) {
            assert_eq!(
                kind.title(),
                title,
                "kind {kind:?} drifted from the FEATURE"
            );
        }
        let mut ids: Vec<_> = ViolationKind::ALL
            .iter()
            .map(|kind| kind.rule_id())
            .collect();
        ids.sort_unstable();
        let unique = ids.len() == CLOSED_KINDS;
        assert!(unique, "rule ids must be unique: {ids:?}");
        for id in ids {
            assert!(id.starts_with("oagw.domain."), "{id} must be namespaced");
        }
    }

    #[test]
    fn every_kind_has_its_own_variant() {
        for kind in ViolationKind::ALL {
            let error = DomainError::from_violation(Violation::new(*kind, "field.path", "message"));
            assert_eq!(error.kind(), Some(*kind), "variant missing for {kind:?}");
            assert_eq!(error.field(), "field.path", "field path lost for {kind:?}");
            assert_eq!(error.message(), "message", "message lost for {kind:?}");
        }
    }

    #[test]
    fn a_single_violation_error_reports_its_one_rule() {
        let error = DomainError::not_found("upstream_id", "no such upstream in the tenant");
        assert_eq!(error.kind(), Some(ViolationKind::NotFound));
        let violations = error.to_violations();
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].kind, ViolationKind::NotFound);
        assert_eq!(violations[0].field, "upstream_id");
    }

    #[test]
    fn an_empty_collector_succeeds() {
        assert!(Violations::new().into_result(&["id"]).is_ok());
    }

    #[test]
    fn the_collector_reports_every_violated_rule_in_field_order() {
        let mut violations = Violations::new();
        violations.record(
            ViolationKind::CorsShape,
            "cors.allowed_origins[0]",
            "allow_credentials with a wildcard origin",
        );
        violations.record(
            ViolationKind::MalformedTag,
            "tags[0]",
            "tag 'LLM' is not lowercase",
        );
        violations.record(ViolationKind::UnknownField, "nope", "undeclared field");
        violations.record(
            ViolationKind::OutOfRangePort,
            "server.endpoints[0].port",
            "0",
        );

        let error = violations.finish(Upstream::FIELD_ORDER);
        let fields: Vec<_> = error
            .to_violations()
            .into_iter()
            .map(|violation| violation.field)
            .collect();
        assert_eq!(
            fields,
            [
                "server.endpoints[0].port",
                "tags[0]",
                "cors.allowed_origins[0]",
                "nope"
            ],
            "declared fields come in schema order, unknown fields follow them"
        );
        assert!(
            error.kind().is_none(),
            "the aggregate carries no single kind"
        );
        assert_eq!(error.to_violations().len(), 4);
        assert_eq!(error.field(), "server.endpoints[0].port");
    }

    #[test]
    fn the_collector_renders_every_rule_in_its_display() {
        let mut violations = Violations::new();
        violations.record(ViolationKind::MalformedAlias, "alias", "must be lowercase");
        let error = violations.finish(Upstream::FIELD_ORDER);
        let rendered = error.to_string();
        assert!(
            rendered.contains("oagw.domain.malformed_alias"),
            "{rendered}"
        );
        assert!(rendered.contains("'alias'"), "{rendered}");
    }

    #[test]
    fn not_found_converts_to_the_existing_404_row() {
        let error = OagwError::from(DomainError::not_found("upstream_id", "absent"));
        assert_eq!(error.mapping().variant, "RouteNotFound");
        assert_eq!(error.status(), 404);
        assert!(
            error.detail().contains("'upstream_id'"),
            "{}",
            error.detail()
        );
    }

    #[test]
    fn already_exists_converts_to_the_existing_400_row() {
        let error = OagwError::from(DomainError::already_exists("alias", "payments"));
        assert_eq!(error.mapping().variant, "ValidationError");
        assert_eq!(error.status(), 400);
        assert!(!error.is_retriable());
    }

    #[test]
    fn every_other_kind_converts_to_the_existing_400_row() {
        for kind in ViolationKind::ALL {
            if matches!(kind, ViolationKind::NotFound | ViolationKind::AlreadyExists) {
                continue;
            }
            let error = OagwError::from(DomainError::from_violation(Violation::new(
                *kind,
                "field",
                "occurrence",
            )));
            assert_eq!(
                error.mapping().variant,
                "ValidationError",
                "{kind:?} must map onto the existing 400 row"
            );
            assert_eq!(error.status(), 400);
        }
    }

    #[test]
    fn the_conversion_adds_no_row_to_the_mapping_table() {
        assert_eq!(MAPPING_TABLE.len(), 22, "the mapping table stays closed");
        let error = OagwError::from(DomainError::not_found("id", "absent"));
        assert!(
            MAPPING_TABLE
                .iter()
                .any(|row| row.gts_type == error.gts_type()),
            "not-found reuses an existing row"
        );
    }

    #[test]
    fn a_violation_by_reference_converts_to_oagw_error() {
        let error = DomainError::not_found("route_id", "absent");
        let converted = OagwError::from(&error);
        assert_eq!(converted.status(), 404);
    }
}
