//! Wire DTOs mirroring `docs/schemas/*.json`.
//!
//! The wire shapes are deliberately separate from `domain::dto`: the wire
//! spells `match`, accepts an optional `port` defaulted per scheme, and
//! serializes `protocol` as the GTS identifier the JSON Schemas declare.

use std::collections::BTreeMap;

use uuid::Uuid;


/// One endpoint of an upstream pool.
#[toolkit_macros::api_dto(request, response)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointDto {
    /// `https`, `wss`, `wt`, `grpc` and, in this deployment, `http`.
    #[serde(default = "default_scheme")]
    pub scheme: String,
    /// Hostname or IP address.
    pub host: String,
    /// Port; defaults to the scheme's default.
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_scheme() -> String {
    "https".to_owned()
}

fn default_port() -> u16 {
    443
}

/// `server` block.
#[toolkit_macros::api_dto(request, response)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerDto {
    /// At least one endpoint.
    pub endpoints: Vec<EndpointDto>,
}

/// Auth block.
#[toolkit_macros::api_dto(request, response)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthDto {
    /// Auth plugin GTS identifier.
    #[serde(rename = "type")]
    pub auth_type: String,
    /// Sharing mode.
    #[serde(default)]
    pub sharing: String,
    /// Plugin configuration.
    #[serde(default)]
    pub config: BTreeMap<String, serde_json::Value>,
}

/// Header rules for one direction.
#[toolkit_macros::api_dto(request, response)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestHeaderRulesDto {
    /// Overwrite if present.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Append, allowing duplicates.
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Drop if present.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Which inbound headers to forward.
    #[serde(default)]
    pub passthrough: String,
    /// Headers forwarded when `passthrough` is `allowlist`.
    #[serde(default)]
    pub passthrough_allowlist: Vec<String>,
}

/// Inbound response header rules.
#[toolkit_macros::api_dto(request, response)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResponseHeaderRulesDto {
    /// Overwrite if present.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Append, allowing duplicates.
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Drop if present.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Header transformation configuration.
#[toolkit_macros::api_dto(request, response)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeadersDto {
    /// Rules applied to the outbound request.
    #[serde(default)]
    pub request: Option<RequestHeaderRulesDto>,
    /// Rules applied to the response.
    #[serde(default)]
    pub response: Option<ResponseHeaderRulesDto>,
}

/// Plugin chain.
#[toolkit_macros::api_dto(request, response)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginsDto {
    /// Sharing mode.
    #[serde(default)]
    pub sharing: String,
    /// Built-in plugins by GTS identifier, custom plugins by UUID.
    ///
    /// An entry is either a bare reference or an object carrying that
    /// plugin's configuration document:
    ///
    /// ```json
    /// {"items": ["gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1",
    ///            {"plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
    ///             "config": {"required_request_headers": "x-correlation-id"}}]}
    /// ```
    #[serde(default)]
    pub items: Vec<serde_json::Value>,
}

/// Sustained rate component.
#[toolkit_macros::api_dto(request, response)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SustainedRateDto {
    /// Tokens replenished per window.
    pub rate: u64,
    /// Window length.
    #[serde(default = "default_window")]
    pub window: String,
}

fn default_window() -> String {
    "second".to_owned()
}

/// Burst component.
#[toolkit_macros::api_dto(request, response)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BurstDto {
    /// Bucket capacity.
    pub capacity: u64,
}

/// Rate-limit configuration.
#[toolkit_macros::api_dto(request, response)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateLimitDto {
    /// Sharing mode.
    #[serde(default)]
    pub sharing: String,
    /// Algorithm.
    #[serde(default = "default_algorithm")]
    pub algorithm: String,
    /// Sustained rate; the only required field.
    pub sustained: SustainedRateDto,
    /// Burst capacity; defaults to `sustained.rate`.
    #[serde(default)]
    pub burst: Option<BurstDto>,
    /// Counter scope.
    #[serde(default = "default_scope")]
    pub scope: String,
    /// Overflow behaviour.
    #[serde(default = "default_strategy")]
    pub strategy: String,
    /// Tokens consumed per request.
    #[serde(default = "default_cost")]
    pub cost: u64,
}

fn default_algorithm() -> String {
    "token_bucket".to_owned()
}

fn default_scope() -> String {
    "tenant".to_owned()
}

fn default_strategy() -> String {
    "reject".to_owned()
}

fn default_cost() -> u64 {
    1
}

/// CORS configuration.
#[toolkit_macros::api_dto(request, response)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CorsDto {
    /// Sharing mode.
    #[serde(default)]
    pub sharing: String,
    /// Whether CORS handling is active.
    pub enabled: bool,
    /// Allowed origins; `["*"]` permits any origin.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Allowed methods.
    #[serde(default = "default_allowed_methods")]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// Whether credentials are allowed.
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_allowed_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

/// Upstream creation request.
#[toolkit_macros::api_dto(request)]
#[derive(Clone, Debug, Default)]
pub struct CreateUpstreamRequest {
    /// Optional explicit alias.
    #[serde(default)]
    pub alias: Option<String>,
    /// Whether the upstream accepts traffic.
    #[serde(default = "crate::api::rest::dto::default_true")]
    pub enabled: bool,
    /// Tags, unioned across the hierarchy.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerDto,
    /// Protocol GTS identifier.
    pub protocol: String,
    /// Authentication configuration.
    #[serde(default)]
    pub auth: Option<AuthDto>,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: Option<HeadersDto>,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: Option<PluginsDto>,
    /// Rate limit.
    #[serde(default)]
    pub rate_limit: Option<RateLimitDto>,
    /// CORS policy.
    #[serde(default)]
    pub cors: Option<CorsDto>,
}

/// Upstream replacement request; `alias` is never part of it.
#[toolkit_macros::api_dto(request)]
#[derive(Clone, Debug, Default)]
pub struct UpdateUpstreamRequest {
    /// Whether the upstream accepts traffic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Endpoint pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerDto>,
    /// Protocol GTS identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// Authentication configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthDto>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersDto>,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsDto>,
    /// Rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitDto>,
    /// CORS policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsDto>,
}

/// Upstream response.
#[toolkit_macros::api_dto(response)]
#[derive(Clone, Debug)]
pub struct UpstreamDto {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Whether the upstream accepts traffic.
    pub enabled: bool,
    /// Routing key, unique per tenant.
    pub alias: String,
    /// Tags.
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerDto,
    /// Protocol GTS identifier.
    pub protocol: String,
    /// Authentication configuration.
    pub auth: Option<AuthDto>,
    /// Header transformation rules.
    pub headers: Option<HeadersDto>,
    /// Plugin chain.
    pub plugins: Option<PluginsDto>,
    /// Rate limit.
    pub rate_limit: Option<RateLimitDto>,
    /// CORS policy.
    pub cors: Option<CorsDto>,
    /// Creation timestamp, RFC 3339.
    pub created_at: Option<String>,
    /// Last-modified timestamp, RFC 3339.
    pub updated_at: Option<String>,
}

/// HTTP match rules.
#[toolkit_macros::api_dto(request, response)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HttpMatchDto {
    /// Method allowlist; non-empty.
    #[serde(default)]
    pub methods: Vec<String>,
    /// Path prefix.
    #[serde(default)]
    pub path: String,
    /// Query parameters the caller may send; empty permits none.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// Suffix handling.
    #[serde(default = "default_suffix_mode")]
    pub path_suffix_mode: String,
}

fn default_suffix_mode() -> String {
    "append".to_owned()
}

/// gRPC match rules.
#[toolkit_macros::api_dto(request, response)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GrpcMatchDto {
    /// Fully qualified service name.
    #[serde(default)]
    pub service: String,
    /// RPC method name.
    #[serde(default)]
    pub method: String,
}

/// `match` block: exactly one of `http` or `grpc`.
#[toolkit_macros::api_dto(request, response)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MatchDto {
    /// HTTP match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatchDto>,
    /// gRPC match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatchDto>,
}

/// Route creation request.
#[toolkit_macros::api_dto(request)]
#[derive(Clone, Debug, Default)]
pub struct CreateRouteRequest {
    /// Owning upstream.
    pub upstream_id: Uuid,
    /// Whether the route participates in matching.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Match rules.
    #[serde(rename = "match")]
    pub match_rule: MatchDto,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: Option<PluginsDto>,
    /// Rate limit.
    #[serde(default)]
    pub rate_limit: Option<RateLimitDto>,
    /// CORS configuration.
    #[serde(default)]
    pub cors: Option<CorsDto>,
}

/// Route replacement request; `upstream_id` is immutable and absent.
#[toolkit_macros::api_dto(request)]
#[derive(Clone, Debug, Default)]
pub struct UpdateRouteRequest {
    /// Whether the route participates in matching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Match rules.
    #[serde(rename = "match", default, skip_serializing_if = "Option::is_none")]
    pub match_rule: Option<MatchDto>,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsDto>,
    /// Rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitDto>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsDto>,
}

/// Route response.
#[toolkit_macros::api_dto(response)]
#[derive(Clone, Debug)]
pub struct RouteDto {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Owning upstream.
    pub upstream_id: Uuid,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Tags.
    pub tags: Vec<String>,
    /// Match rules.
    #[serde(rename = "match")]
    pub match_rule: MatchDto,
    /// Plugin chain.
    pub plugins: Option<PluginsDto>,
    /// Rate limit.
    pub rate_limit: Option<RateLimitDto>,
    /// CORS configuration.
    pub cors: Option<CorsDto>,
    /// Creation timestamp, RFC 3339.
    pub created_at: Option<String>,
    /// Last-modified timestamp, RFC 3339.
    pub updated_at: Option<String>,
}

/// Plugin creation request.
#[toolkit_macros::api_dto(request)]
#[derive(Clone, Debug, Default)]
pub struct CreatePluginRequest {
    /// Plugin family (`auth_plugin`, `guard_plugin`, `transform_plugin`).
    pub plugin_type: String,
    /// Human readable name.
    pub name: String,
    /// Starlark source.
    #[serde(default)]
    pub source: String,
    /// Plugin configuration document.
    #[serde(default)]
    pub config: BTreeMap<String, serde_json::Value>,
}

/// Plugin response.
#[toolkit_macros::api_dto(response)]
#[derive(Clone, Debug)]
pub struct PluginDto {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Plugin family.
    pub plugin_type: String,
    /// Human readable name.
    pub name: String,
    /// Starlark source.
    pub source: String,
    /// Plugin configuration document.
    pub config: BTreeMap<String, serde_json::Value>,
    /// Earliest instant at which an unlinked plugin may be collected.
    pub gc_eligible_at: Option<String>,
    /// Creation timestamp, RFC 3339.
    pub created_at: Option<String>,
}

/// Plugin source response.
#[toolkit_macros::api_dto(response)]
#[derive(Clone, Debug)]
pub struct PluginSourceDto {
    /// The plugin's identifier.
    pub id: Uuid,
    /// The plugin's name.
    pub name: String,
    /// Starlark source.
    pub source: String,
}

/// List query parameters.
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct ListParams {
    /// `$filter=field eq 'value'`.
    #[serde(rename = "$filter", default)]
    pub filter: Option<String>,
    /// `$orderby=field [asc|desc]`.
    #[serde(rename = "$orderby", default)]
    pub orderby: Option<String>,
    /// `$select=field,field`.
    #[serde(rename = "$select", default)]
    pub select: Option<String>,
    /// `$top`, default 50 and capped at 100.
    #[serde(rename = "$top", default)]
    pub top: Option<usize>,
    /// `$skip`.
    #[serde(rename = "$skip", default)]
    pub skip: Option<usize>,
}

fn default_true() -> bool {
    true
}

/// Merges a replacement's optional fields over the stored record.
pub(crate) trait MergeUpdate {
    /// Fills every `None` field from `base`.
    fn merged_with(&self, base: &Self) -> Self;
}

impl MergeUpdate for UpdateUpstreamRequest {
    fn merged_with(&self, base: &Self) -> Self {
        Self {
            enabled: pick(self.enabled.as_ref(), base.enabled.as_ref()),
            tags: pick(self.tags.as_ref(), base.tags.as_ref()),
            server: self.server.clone().or_else(|| base.server.clone()),
            protocol: self.protocol.clone().or_else(|| base.protocol.clone()),
            auth: self.auth.clone().or_else(|| base.auth.clone()),
            headers: self.headers.clone().or_else(|| base.headers.clone()),
            plugins: self.plugins.clone().or_else(|| base.plugins.clone()),
            rate_limit: self.rate_limit.clone().or_else(|| base.rate_limit.clone()),
            cors: self.cors.clone().or_else(|| base.cors.clone()),
        }
    }
}

impl MergeUpdate for UpdateRouteRequest {
    fn merged_with(&self, base: &Self) -> Self {
        Self {
            enabled: pick(self.enabled.as_ref(), base.enabled.as_ref()),
            tags: pick(self.tags.as_ref(), base.tags.as_ref()),
            match_rule: self.match_rule.clone().or_else(|| base.match_rule.clone()),
            plugins: self.plugins.clone().or_else(|| base.plugins.clone()),
            rate_limit: self.rate_limit.clone().or_else(|| base.rate_limit.clone()),
            cors: self.cors.clone().or_else(|| base.cors.clone()),
        }
    }
}

fn pick<T: Clone>(chosen: Option<&T>, base: Option<&T>) -> Option<T> {
    chosen.cloned().or_else(|| base.cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_defaults_to_true() {
        let text = r#"{"server":{"endpoints":[{"scheme":"https","host":"a.com","port":443}]},"protocol":"http"}"#;
        let request: CreateUpstreamRequest = serde_json::from_str(text).expect("parses");
        assert!(request.enabled);
        assert_eq!(request.server.endpoints.len(), 1);
        assert_eq!(request.server.endpoints[0].port, 443);
    }

    #[test]
    fn route_match_is_spelled_match() {
        let text = r#"{"upstream_id":"00000000-0000-0000-0000-000000000000","match":{"http":{"methods":["GET"],"path":"/v1"}}}"#;
        let request: CreateRouteRequest = serde_json::from_str(text).expect("parses");
        let http = request.match_rule.http.expect("http variant");
        assert_eq!(http.methods, vec!["GET"]);
        assert_eq!(http.path, "/v1");
        assert_eq!(http.path_suffix_mode, "append");
        assert!(request.match_rule.grpc.is_none());
    }

    #[test]
    fn rate_limit_defaults_fill_in() {
        let text = r#"{"sustained":{"rate":5}}"#;
        let config: RateLimitDto = serde_json::from_str(text).expect("parses");
        assert_eq!(config.sustained.window, "second");
        assert_eq!(config.scope, "tenant");
        assert_eq!(config.cost, 1);
        assert!(config.burst.is_none());
    }
}
