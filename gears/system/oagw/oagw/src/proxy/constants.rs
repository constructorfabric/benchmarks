//! Shared literal constants for the proxy data plane.

/// The HTTP protocol identifier an `Upstream.protocol` must equal for HTTP
/// match keys to apply (`cpt-cf-oagw-algo-proxy-match-route`
/// `inst-proxy-route-if-not-http`).
pub(crate) const HTTP_PROTOCOL_ID: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// Inbound routing header consumed by endpoint selection and never
/// forwarded (`cpt-cf-oagw-algo-proxy-transform-headers`
/// `inst-proxy-hdr-routing-strip`).
pub(crate) const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Permission allowing a descendant to override an `inherit`-shared `auth`
/// declaration (`DESIGN.md`'s Permissions and Access Control table).
pub(crate) const OVERRIDE_AUTH_PERMISSION: &str = "oagw:upstream:override_auth";
/// Permission allowing a descendant to specify its own rate limit.
pub(crate) const OVERRIDE_RATE_PERMISSION: &str = "oagw:upstream:override_rate";
/// Permission allowing a descendant to append its own plugins.
pub(crate) const ADD_PLUGINS_PERMISSION: &str = "oagw:upstream:add_plugins";

/// The 100 MB hard body-size limit (`cpt-cf-oagw-constraint-body-limit`),
/// fixed and never raised by any upstream/route/tenant configuration.
pub(crate) const MAX_BODY_BYTES: usize = 100 * 1024 * 1024;

/// The base path this feature's proxy endpoint is mounted at (gear-relative,
/// no `/api` prefix -- `DECOMPOSITION.md` §1 correction 2).
pub(crate) const PROXY_PATH_PREFIX: &str = "/oagw/v1/proxy/";

/// Value of `crate::error::ERROR_SOURCE_HEADER_NAME` for a response that
/// originated at the upstream (`cpt-cf-oagw-principle-error-source`). The
/// gateway counterpart, `crate::error::ERROR_SOURCE_GATEWAY`, lives in
/// gear foundation's error renderer; this passthrough half is this
/// feature's own concern per that module's own doc comment.
pub(crate) const ERROR_SOURCE_UPSTREAM: &str = "upstream";
