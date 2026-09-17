//! Metrics tests (DESIGN §4.2): the instruments the proxy records, asserted
//! against an **in-memory OpenTelemetry exporter**.
//!
//! Every test builds its own `SdkMeterProvider` + `InMemoryMetricExporter`,
//! hands the meter over with [`OagwMetrics::new`], and drives
//! [`DataPlaneService::proxy`] against a real local HTTP server. An assertion
//! here therefore fails when the recording is deleted — not merely when some
//! code ran.
//!
//! The one thing these tests cannot do is install a bucket-boundary view: the
//! provider is built by the test, exactly as it would be by a host, and §4.2's
//! boundaries are that provider's business (see the `metrics` module doc).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use httpmock::MockServer;
use opentelemetry::metrics::{Meter, MeterProvider};
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use oagw::DataPlaneService;
use oagw::OagwConfig;
use oagw::OagwError;
use oagw::PluginEngine;
use oagw::ProxyContext;
use oagw::ProxyHooks;
use oagw::RateLimitHook;
use oagw::RateLimitLimiter;
use oagw::RateLimitService;
use oagw::domain::metrics::OagwMetrics;
use oagw::domain::services::control_plane::ControlPlaneService;
use oagw::domain::services::data_plane::ProxyRequest;
use oagw::domain::storage::{RouteStore, UpstreamStore};
use oagw::domain::types::{
    Endpoint, HttpMatch, PathSuffixMode, Protocol, RateLimitAlgorithm, RateLimitConfig,
    RateLimitScope, RateLimitStrategy, RateLimitSustained, RateLimitWindow, Route, RouteMatch,
    RouteMethod, RouteSpec, Scheme, ServerConfig, SharingMode, Upstream, UpstreamSpec,
};
use oagw::tenant_context::CallerContext;

/// The instrument under test: `oagw_requests_total`.
const REQUESTS_TOTAL: &str = "oagw_requests_total";
/// The instrument under test: `oagw_request_duration_seconds`.
const REQUEST_DURATION: &str = "oagw_request_duration_seconds";
/// The instrument under test: `oagw_requests_in_flight`.
const REQUESTS_IN_FLIGHT: &str = "oagw_requests_in_flight";
/// The instrument under test: `oagw_errors_total`.
const ERRORS_TOTAL: &str = "oagw_errors_total";
/// The instrument under test: `oagw_rate_limit_exceeded_total`.
const RATE_LIMIT_EXCEEDED: &str = "oagw_rate_limit_exceeded_total";
/// The instrument under test: `oagw_rate_limit_usage_ratio`.
const RATE_LIMIT_USAGE: &str = "oagw_rate_limit_usage_ratio";
/// The instrument under test: `oagw_routing_target_host_used`.
const TARGET_HOST_USED: &str = "oagw_routing_target_host_used";
/// The instrument under test: `oagw_routing_endpoint_selected`.
const ENDPOINT_SELECTED: &str = "oagw_routing_endpoint_selected";

/// The GTS type id of a 404 for an unknown alias.
const UPSTREAM_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1";

/// The GTS type id of a 502 the gateway raised itself.
const DOWNSTREAM_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1";

/// The tenant of this file's calls.
const TENANT: Uuid = Uuid::from_u128(0x6f61_6777_0000_0000_0000_0000_0000_0008);

// ---------------------------------------------------------------------------
// In-memory meter
// ---------------------------------------------------------------------------

/// One data point of one instrument, as the exporter aggregated it.
#[derive(Debug)]
struct Point {
    attrs: Vec<(String, String)>,
    value: PointValue,
}

/// The value of a point: the four kinds this crate records.
#[derive(Debug)]
enum PointValue {
    Sum(u64),
    /// A signed sum — the `i64` up-down counter, whose sign is the whole point
    /// of a gauge.
    ISum(i64),
    Gauge(f64),
    Histogram {
        count: u64,
        sum: f64,
    },
}

/// A histogram point, flattened to the `(attributes, count, sum)` triple the
/// assertions read.
type HistogramPoint = (Vec<(String, String)>, u64, f64);

impl Point {
    /// The value of one attribute of the point.
    fn attr(&self, key: &str) -> &str {
        self.attrs
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
            .unwrap_or_default()
    }

    /// Whether the point carries *exactly* these attributes.
    fn labels(&self, expected: &[(&str, &str)]) -> bool {
        self.attrs.len() == expected.len()
            && expected.iter().all(|(key, value)| self.attr(key) == *value)
    }

    /// The summed value, when the point is a counter.
    fn sum(&self) -> u64 {
        match self.value {
            PointValue::Sum(value) => value,
            _ => 0,
        }
    }

    /// The signed value, when the point is an `i64` sum at all.
    ///
    /// The in-flight gauge is exported as a signed sum, so this is the accessor
    /// that tells "the guard returned the unit exactly" (`Some(0)`) apart from
    /// "the gauge went negative" (`Some(negative)`), and either apart from "the
    /// instrument was never written" (an empty point list). It replaces the
    /// unsigned reader the in-flight tests used before, which folded every
    /// negative into a plausible `0`.
    fn as_i64(&self) -> Option<i64> {
        match self.value {
            PointValue::ISum(value) => Some(value),
            _ => None,
        }
    }
}

/// An in-memory meter provider and the exporter that reads it back.
struct Metrics {
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
}

impl Metrics {
    /// A provider whose reader collects into the exporter.
    fn new() -> Self {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        Self { provider, exporter }
    }

    /// A meter handed to [`OagwMetrics::new`].
    fn meter(&self) -> Meter {
        self.provider.meter("oagw.metrics.test")
    }

    /// Flush, and read back every point recorded for `name`.
    ///
    /// The exporter accumulates collections, so the *last* collection that
    /// carries the instrument is the current state of it; the exporter is then
    /// reset, which is safe because the aggregation lives in the provider.
    fn read(&self, name: &str) -> Vec<Point> {
        self.provider
            .force_flush()
            .expect("test meter provider should flush");
        let metrics = self
            .exporter
            .get_finished_metrics()
            .expect("in-memory exporter should be readable");

        let mut points = Vec::new();
        for resource in &metrics {
            for scope in resource.scope_metrics() {
                for metric in scope.metrics() {
                    if metric.name() == name {
                        points = points_of(metric.data());
                    }
                }
            }
        }
        self.exporter.reset();
        points
    }

    /// The total of every counter point of `name`.
    fn counter(&self, name: &str) -> u64 {
        self.read(name).iter().map(Point::sum).sum()
    }

    /// The counter points of `name`.
    fn counter_points(&self, name: &str) -> Vec<Point> {
        self.read(name)
    }

    /// The gauge value of `name`, when it was recorded at all.
    fn gauge(&self, name: &str) -> Option<f64> {
        self.read(name)
            .into_iter()
            .find_map(|point| match point.value {
                PointValue::Gauge(value) => Some(value),
                _ => None,
            })
    }

    /// The histogram points of `name`.
    fn histogram(&self, name: &str) -> Vec<HistogramPoint> {
        self.read(name)
            .into_iter()
            .filter_map(|point| match point.value {
                PointValue::Histogram { count, sum } => Some((point.attrs, count, sum)),
                _ => None,
            })
            .collect()
    }
}

/// Flatten one instrument's aggregated data into points.
fn points_of(data: &AggregatedMetrics) -> Vec<Point> {
    match data {
        AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
            .data_points()
            .map(|dp| Point {
                attrs: attrs_of(dp.attributes()),
                value: PointValue::Sum(dp.value()),
            })
            .collect(),
        AggregatedMetrics::I64(MetricData::Sum(sum)) => sum
            .data_points()
            .map(|dp| Point {
                attrs: attrs_of(dp.attributes()),
                // The sign is kept: a gauge that ratchets the wrong way must be
                // visible as a negative, not folded into a plausible zero.
                value: PointValue::ISum(dp.value()),
            })
            .collect(),
        AggregatedMetrics::F64(MetricData::Gauge(gauge)) => gauge
            .data_points()
            .map(|dp| Point {
                attrs: attrs_of(dp.attributes()),
                value: PointValue::Gauge(dp.value()),
            })
            .collect(),
        AggregatedMetrics::F64(MetricData::Histogram(histogram)) => histogram
            .data_points()
            .map(|dp| Point {
                attrs: attrs_of(dp.attributes()),
                value: PointValue::Histogram {
                    count: dp.count(),
                    sum: dp.sum(),
                },
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Collect the attributes of one point as sortable pairs.
fn attrs_of<'a>(
    values: impl Iterator<Item = &'a opentelemetry::KeyValue>,
) -> Vec<(String, String)> {
    let mut attrs: Vec<(String, String)> = values
        .map(|kv| (kv.key.as_str().to_owned(), kv.value.as_str().into_owned()))
        .collect();
    attrs.sort();
    attrs
}

// ---------------------------------------------------------------------------
// Proxy harness
// ---------------------------------------------------------------------------

/// A proxy stack whose instruments are bound to one in-memory meter.
struct Harness {
    server: MockServer,
    metrics: Metrics,
    service: DataPlaneService,
    upstreams: Arc<UpstreamStore>,
    routes: Arc<RouteStore>,
    /// The label value of the upstream alias the tests resolve.
    alias: &'static str,
}

impl Harness {
    /// A stack with the built-in rate-limit hook when `rate_limit_hook` is set.
    fn new(rate_limit_hook: bool) -> Self {
        Self::build(rate_limit_hook, None)
    }

    /// The same stack with a plugin engine the test supplies.
    ///
    /// Only the response-phase test needs one: the engine has to reject *after*
    /// the upstream answered, which is the path `DataPlaneService::respond`
    /// turns into a rendered gateway error (ADR-0002, ADR-0007).
    fn with_plugin_engine(plugins: Arc<dyn PluginEngine>) -> Self {
        Self::build(false, Some(plugins))
    }

    fn build(rate_limit_hook: bool, plugins: Option<Arc<dyn PluginEngine>>) -> Self {
        let server = MockServer::start();
        let config = OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        };
        let control_plane = Arc::new(ControlPlaneService::new(config));
        let upstreams = Arc::clone(control_plane.upstream_store());
        let routes = Arc::clone(control_plane.route_store());

        let metrics = Metrics::new();
        let rate_limit = rate_limit_hook.then(|| {
            Arc::new(RateLimitService::new(Arc::new(RateLimitLimiter::new())))
                as Arc<dyn RateLimitHook>
        });
        let hooks = ProxyHooks::new(rate_limit, None, plugins);

        let service = DataPlaneService::new(
            config,
            control_plane,
            Arc::clone(&upstreams),
            Arc::clone(&routes),
        )
        .with_metrics(OagwMetrics::new(&metrics.meter()))
        .with_hooks(hooks);

        Self {
            server,
            metrics,
            service,
            upstreams,
            routes,
            alias: "api.vendor.test",
        }
    }

    /// Seed an enabled upstream over `endpoints`.
    fn seed_upstream(&self, alias: &str, endpoints: Vec<Endpoint>) -> Uuid {
        let mut spec = UpstreamSpec {
            alias: Some(alias.to_owned()),
            server: ServerConfig { endpoints },
            protocol: Protocol::Http,
            ..UpstreamSpec::default()
        };
        spec = spec.validate().expect("the upstream spec normalizes");

        self.upstreams
            .insert(Upstream {
                id: Uuid::new_v4(),
                tenant_id: TENANT,
                alias: alias.to_owned(),
                created_at: 0,
                updated_at: 0,
                spec,
            })
            .expect("the upstream inserts")
            .id
    }

    /// Seed a load-balancing pool of loopback endpoints.
    ///
    /// Inserted without `UpstreamSpec::validate`, because the pool rule (one
    /// scheme and one port per upstream) is a management-API concern, and two
    /// local servers necessarily listen on two ports.
    fn seed_pool(&self, alias: &str, endpoints: Vec<Endpoint>) -> Uuid {
        self.upstreams
            .insert(Upstream {
                id: Uuid::new_v4(),
                tenant_id: TENANT,
                alias: alias.to_owned(),
                created_at: 0,
                updated_at: 0,
                spec: UpstreamSpec {
                    alias: Some(alias.to_owned()),
                    server: ServerConfig { endpoints },
                    protocol: Protocol::Http,
                    ..UpstreamSpec::default()
                },
            })
            .expect("the upstream inserts")
            .id
    }

    /// Seed a route whose `http.path` pattern is `path`.
    fn seed_route(&self, upstream: Uuid, path: &str) -> Uuid {
        self.routes
            .insert(Route {
                id: Uuid::new_v4(),
                tenant_id: TENANT,
                upstream_id: upstream,
                created_at: 0,
                updated_at: 0,
                spec: RouteSpec {
                    upstream_id: upstream,
                    match_rules: RouteMatch {
                        http: Some(HttpMatch {
                            methods: vec![RouteMethod::Get],
                            path: path.to_owned(),
                            query_allowlist: Vec::new(),
                            path_suffix_mode: PathSuffixMode::Append,
                        }),
                        grpc: None,
                    },
                    enabled: true,
                    tags: Vec::new(),
                    plugins: None,
                    rate_limit: None,
                },
            })
            .expect("the route inserts")
            .id
    }

    /// Impose a reject-strategy tenant limit of `rate` per second.
    fn limit(&self, route: Uuid, rate: u32) {
        let route = self.routes.get(TENANT, route).expect("the route exists");
        let mut spec = route.spec.clone();
        spec.rate_limit = Some(RateLimitConfig {
            sharing: SharingMode::Private,
            algorithm: RateLimitAlgorithm::TokenBucket,
            sustained: RateLimitSustained {
                rate,
                window: RateLimitWindow::Second,
            },
            burst: None,
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::Reject,
            cost: 1,
        });
        self.routes
            .replace(TENANT, route.id, spec, 0)
            .expect("the route updates");
    }

    /// Answer every `GET` of `path` with `status`.
    ///
    /// The [`httpmock::Mock`] must stay bound for the request to reach it, so
    /// the tests hold it and (where it matters) read its `hits`.
    fn serve(&self, path: &str, status: u16) -> httpmock::Mock<'_> {
        self.server.mock(|when, then| {
            when.method(httpmock::Method::GET).path(path);
            then.status(status);
        })
    }

    /// Proxy one request and report the outcome.
    async fn call(
        &self,
        method: &str,
        alias: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> Result<u16, oagw::OagwError> {
        let security = SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(TENANT)
            .build()
            .expect("security context builds");

        let request = ProxyRequest {
            alias: alias.to_owned(),
            method: method.parse().expect("a method token"),
            stripped_path: path.to_owned(),
            query: String::new(),
            headers: headers
                .iter()
                .fold(HeaderMap::new(), |mut all, (name, value)| {
                    let name = HeaderName::from_bytes(name.as_bytes()).expect("a header name");
                    let value = HeaderValue::from_str(value).expect("a header value");
                    all.insert(name, value);
                    all
                }),
            body: axum::body::Body::empty(),
            request_path: format!("/oagw/v1/proxy/{alias}{path}"),
            request_id: "01JMETRICS".to_owned(),
        };

        self.service
            .proxy(&security, &CallerContext::new(TENANT), request)
            .await
            .map(|response| response.status().as_u16())
    }
}

/// A plaintext loopback endpoint on `port`.
fn endpoint(port: u16) -> Endpoint {
    Endpoint {
        scheme: Scheme::Http,
        host: "127.0.0.1".to_owned(),
        port,
    }
}

/// A plugin engine whose response phase always fails (ADR-0002).
///
/// The request phase passes, so the request is forwarded and the upstream
/// answers: the rejection happens in `DataPlaneService::respond`, which
/// renders a gateway error and returns it as `Ok` — the one path the wrapper's
/// final `Result` cannot see.
struct ResponseRejectingEngine;

#[async_trait::async_trait]
impl PluginEngine for ResponseRejectingEngine {
    async fn on_request(
        &self,
        _context: &ProxyContext,
        _headers: &mut HeaderMap,
    ) -> Result<(), OagwError> {
        Ok(())
    }

    async fn on_response(
        &self,
        _context: &ProxyContext,
        _headers: &mut HeaderMap,
    ) -> Result<(), OagwError> {
        Err(OagwError::downstream_error(
            "the response transform refused this response",
        ))
    }
}

// ---------------------------------------------------------------------------
// `oagw_requests_total`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_proxied_request_is_counted_with_the_upstream_status_and_the_full_label_set() {
    let harness = Harness::new(false);
    let upstream = harness.seed_upstream(harness.alias, vec![endpoint(harness.server.port())]);
    harness.seed_route(upstream, "/v1/models");

    // The gateway's own status is never substituted for the upstream's: the
    // upstream answers 201, so 201 is what the label carries.
    let _mock = harness.serve("/v1/models", 201);

    let status = harness
        .call("GET", harness.alias, "/v1/models", &[])
        .await
        .expect("the request forwards");

    assert_eq!(status, 201);
    let points = harness.metrics.counter_points(REQUESTS_TOTAL);
    assert_eq!(points.len(), 1, "one request, one point: {points:?}");
    assert!(
        points[0].labels(&[
            ("host", harness.alias),
            ("http.request.method", "GET"),
            ("http.route", "/v1/models"),
            ("http.response.status_code", "201"),
        ]),
        "the label set must be the §4.2 one: {:?}",
        points[0].attrs
    );
    assert_eq!(points[0].sum(), 1);
}

#[tokio::test]
async fn a_non_standard_method_is_normalized_to_other() {
    let harness = Harness::new(false);
    let upstream = harness.seed_upstream(harness.alias, vec![endpoint(harness.server.port())]);
    harness.seed_route(upstream, "/v1/models");

    // `RouteMethod` is a closed set of five verbs, so a caller-invented method
    // cannot match a route: the request is rejected with 404 *and* counted, and
    // the method label is `_OTHER` rather than the invented token.
    let error = harness
        .call("PROPFIND", harness.alias, "/v1/models", &[])
        .await
        .expect_err("no route matches PROPFIND");
    assert_eq!(error.status().as_u16(), 404);

    let points = harness.metrics.counter_points(REQUESTS_TOTAL);
    assert_eq!(points.len(), 1, "the record still lands: {points:?}");
    assert_eq!(points[0].attr("http.request.method"), "_OTHER");
    assert_eq!(points[0].attr("host"), harness.alias);
    assert_eq!(points[0].attr("http.response.status_code"), "404");
}

#[tokio::test]
async fn a_gateway_error_is_counted_under_its_own_status() {
    let harness = Harness::new(false);

    // No upstream is configured at all, so alias resolution rejects the request.
    let error = harness
        .call("GET", harness.alias, "/v1/models", &[])
        .await
        .expect_err("the alias is unknown");
    assert_eq!(error.status().as_u16(), 404);

    let points = harness.metrics.counter_points(REQUESTS_TOTAL);
    assert_eq!(points.len(), 1);
    // Replaced by the sentinel assertion: the alias the caller asked for is
    // request data, so before resolution the `host` label is the fixed
    // sentinel, never that alias (DESIGN §4.2 "Cardinality management").
    assert_eq!(
        points[0].attr("host"),
        OagwMetrics::UNRESOLVED_HOST,
        "the requested alias is a log field, not a label"
    );
    assert_eq!(points[0].attr("http.route"), "", "nothing matched");
    assert_eq!(points[0].attr("http.response.status_code"), "404");
    assert_eq!(points[0].sum(), 1);
}

#[tokio::test]
async fn two_unknown_aliases_aggregate_into_one_time_series() {
    let harness = Harness::new(false);

    for alias in ["nope.vendor.test", "also.nope.vendor.test"] {
        let error = harness
            .call("GET", alias, "/v1/models", &[])
            .await
            .expect_err("neither alias is configured");
        assert_eq!(error.status().as_u16(), 404);
    }

    // Two requests, two different bogus aliases, one bounded series: an
    // authenticated caller may invent an alias per request, and each distinct
    // value would otherwise add a time series to every instrument that carries
    // `host` (DESIGN §4.2). `http.route` is the other pre-resolution label, and
    // it is the same sentinel for the same reason.
    let points = harness.metrics.counter_points(REQUESTS_TOTAL);
    assert_eq!(points.len(), 1, "one series, not one per alias: {points:?}");
    assert_eq!(points[0].sum(), 2);
    assert_eq!(points[0].attr("host"), OagwMetrics::UNRESOLVED_HOST);
    assert_eq!(points[0].attr("http.route"), "");
}

// ---------------------------------------------------------------------------
// `http.route` is the pattern, never the raw path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_route_label_is_the_configured_pattern_not_the_requested_path() {
    let harness = Harness::new(false);
    let upstream = harness.seed_upstream(harness.alias, vec![endpoint(harness.server.port())]);
    let route = harness.seed_route(upstream, "/v1/models");

    let _mock = harness.serve("/v1/models/123", 200);

    let status = harness
        .call("GET", harness.alias, "/v1/models/123", &[])
        .await
        .expect("the request forwards");
    assert_eq!(status, 200);
    assert_eq!(
        harness
            .routes
            .get(TENANT, route)
            .expect("the route exists")
            .spec
            .match_rules
            .http,
        Some(HttpMatch {
            methods: vec![RouteMethod::Get],
            path: "/v1/models".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        }),
        "the fixture must be a pattern route, or the assertion below is vacuous"
    );

    let points = harness.metrics.counter_points(REQUESTS_TOTAL);
    assert_eq!(points.len(), 1);
    assert_eq!(
        points[0].attr("http.route"),
        "/v1/models",
        "the raw path `/v1/models/123` must never become a label value"
    );
}

// ---------------------------------------------------------------------------
// `oagw_request_duration_seconds`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn both_phases_are_recorded_for_one_request() {
    let harness = Harness::new(false);
    let upstream = harness.seed_upstream(harness.alias, vec![endpoint(harness.server.port())]);
    harness.seed_route(upstream, "/v1/models");

    let _mock = harness.serve("/v1/models", 200);

    harness
        .call("GET", harness.alias, "/v1/models", &[])
        .await
        .expect("the request forwards");

    let phases = harness.metrics.histogram(REQUEST_DURATION);
    assert_eq!(phases.len(), 2, "exactly the two phases: {phases:?}");

    let mut by_phase = Vec::new();
    for (attrs, count, sum) in &phases {
        let phase = attrs
            .iter()
            .find(|(key, _)| key == "phase")
            .map(|(_, value)| value.as_str())
            .expect("the phase label is always there");
        by_phase.push((phase.to_owned(), *count, *sum));
        assert!(
            attrs
                .iter()
                .any(|(key, value)| key == "host" && value == harness.alias),
            "{attrs:?}"
        );
    }
    by_phase.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(
        by_phase
            .iter()
            .map(|(phase, _, _)| phase.as_str())
            .collect::<Vec<_>>(),
        vec!["total", "upstream"],
        "the two values the module documents, and no third"
    );
    assert_eq!(by_phase.iter().map(|(_, count, _)| count).sum::<u64>(), 2);

    // `total` wraps the whole request and `upstream` only the outbound leg, so
    // the total can never be shorter than the upstream leg.
    let total = by_phase
        .iter()
        .find(|(phase, _, _)| phase == "total")
        .expect("total");
    let upstream = by_phase
        .iter()
        .find(|(phase, _, _)| phase == "upstream")
        .expect("upstream");
    assert!(
        total.2 >= upstream.2,
        "total {:?} must be at least the upstream leg {:?}",
        total,
        upstream
    );
}

// ---------------------------------------------------------------------------
// `oagw_requests_in_flight`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_in_flight_gauge_returns_to_zero_after_a_forwarded_request() {
    let harness = Harness::new(false);
    let upstream = harness.seed_upstream(harness.alias, vec![endpoint(harness.server.port())]);
    harness.seed_route(upstream, "/v1/models");

    let _mock = harness.serve("/v1/models", 200);

    harness
        .call("GET", harness.alias, "/v1/models", &[])
        .await
        .expect("the request forwards");

    // The point *exists* — which is what proves the guard took the unit — and
    // it is back at zero, which is what proves the drop guard gave it back.
    let points = harness.metrics.counter_points(REQUESTS_IN_FLIGHT);
    assert_eq!(points.len(), 1, "one unit was taken: {points:?}");
    // Signed on purpose: a value folded to `0` could not tell "the guard
    // returned the unit" from "the guard took it twice".
    assert_eq!(
        points[0].as_i64(),
        Some(0),
        "the drop guard must return the unit, neither more nor less"
    );
    // The guard is taken before resolution runs, so its `host` is the sentinel:
    // the alias the caller asked for is request data, not a label value (§4.2).
    assert_eq!(points[0].attr("host"), OagwMetrics::UNRESOLVED_HOST);
}

#[tokio::test]
async fn the_in_flight_gauge_returns_to_zero_after_a_gateway_error() {
    let harness = Harness::new(false);

    // The alias is unknown: `execute` returns early, long before any upstream.
    let error = harness
        .call("GET", harness.alias, "/v1/models", &[])
        .await
        .expect_err("the alias is unknown");
    assert_eq!(error.status().as_u16(), 404);

    let points = harness.metrics.counter_points(REQUESTS_IN_FLIGHT);
    assert_eq!(points.len(), 1, "the unit was taken at entry: {points:?}");
    assert_eq!(
        points[0].as_i64(),
        Some(0),
        "an early return must not leak the unit, and must not over-correct it either"
    );
}

// ---------------------------------------------------------------------------
// `oagw_errors_total`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_gateway_error_is_counted_with_its_error_type() {
    let harness = Harness::new(false);

    let error = harness
        .call("GET", harness.alias, "/v1/models", &[])
        .await
        .expect_err("the alias is unknown");
    assert_eq!(error.kind().gts_type_id(), UPSTREAM_NOT_FOUND);

    let points = harness.metrics.counter_points(ERRORS_TOTAL);
    assert_eq!(points.len(), 1, "{points:?}");
    // `host` is the sentinel, as in every instrument: an unknown alias is
    // request data, and must not become a label value (DESIGN §4.2).
    assert!(
        points[0].labels(&[
            ("host", OagwMetrics::UNRESOLVED_HOST),
            ("http.route", ""),
            ("error_type", UPSTREAM_NOT_FOUND),
        ]),
        "error_type is the GTS type id, a fixed vocabulary: {:?}",
        points[0].attrs
    );
}

#[tokio::test]
async fn a_response_phase_gateway_error_is_counted_with_its_error_type() {
    // A transform plugin that rejects the response: the gateway answers 502 with
    // its own problem document, but `execute` has already returned `Ok`, so the
    // counter can only be fed by what the rendered response reports. The engine
    // is the test's own rather than the real `PluginEngineService`, because what
    // is under test here is the recording, not chain resolution — and a
    // test-local engine rejects the response phase without binding anything into
    // the upstream configuration first.
    let harness = Harness::with_plugin_engine(Arc::new(ResponseRejectingEngine));
    let upstream = harness.seed_upstream(harness.alias, vec![endpoint(harness.server.port())]);
    harness.seed_route(upstream, "/v1/models");

    let _mock = harness.serve("/v1/models", 200);

    let status = harness
        .call("GET", harness.alias, "/v1/models", &[])
        .await
        .expect("the gateway answers");
    assert_eq!(status, 502, "the plugin rejected the response");

    let points = harness.metrics.counter_points(ERRORS_TOTAL);
    assert_eq!(
        points.len(),
        1,
        "a rendered gateway error is still a gateway error: {points:?}"
    );
    assert!(
        points[0].labels(&[
            ("host", harness.alias),
            ("http.route", "/v1/models"),
            ("error_type", DOWNSTREAM_ERROR),
        ]),
        "the same labels a raised gateway error carries: {:?}",
        points[0].attrs
    );
    assert_eq!(points[0].sum(), 1);

    // Counted once: the request itself lands in `oagw_requests_total` under the
    // gateway's status, and the two counters do not double-report it.
    let requests = harness.metrics.counter_points(REQUESTS_TOTAL);
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].sum(), 1);
    assert_eq!(requests[0].attr("http.response.status_code"), "502");
}

#[tokio::test]
async fn a_forwarded_request_is_not_counted_as_a_gateway_error() {
    let harness = Harness::new(false);
    let upstream = harness.seed_upstream(harness.alias, vec![endpoint(harness.server.port())]);
    harness.seed_route(upstream, "/v1/models");

    // An upstream failure is still not a *gateway* failure.
    let _mock = harness.serve("/v1/models", 503);

    let status = harness
        .call("GET", harness.alias, "/v1/models", &[])
        .await
        .expect("the upstream answered");
    assert_eq!(status, 503);

    assert_eq!(harness.metrics.counter(ERRORS_TOTAL), 0);
    let points = harness.metrics.counter_points(REQUESTS_TOTAL);
    assert_eq!(points[0].attr("http.response.status_code"), "503");
}

// ---------------------------------------------------------------------------
// Rate-limit instruments
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_allowed_request_reports_the_share_of_its_bucket_it_consumed() {
    let harness = Harness::new(true);
    let upstream = harness.seed_upstream(harness.alias, vec![endpoint(harness.server.port())]);
    let route = harness.seed_route(upstream, "/v1/models");
    harness.limit(route, 4);

    let _mock = harness.serve("/v1/models", 200);

    let status = harness
        .call("GET", harness.alias, "/v1/models", &[])
        .await
        .expect("the first request is within budget");
    assert_eq!(status, 200);

    assert_eq!(harness.metrics.counter(RATE_LIMIT_EXCEEDED), 0);
    let ratio = harness
        .metrics
        .gauge(RATE_LIMIT_USAGE)
        .expect("a numeric capacity");
    assert!((0.0..=1.0).contains(&ratio), "{ratio}");
    // One of the four tokens of a `4/second` bucket.
    assert!((ratio - 0.25).abs() < 1e-9, "{ratio}");

    let points = harness.metrics.counter_points(RATE_LIMIT_USAGE);
    assert_eq!(
        points[0].attr("path"),
        "/v1/models",
        "`path` is the matched route's pattern, never the raw request path"
    );
    assert_eq!(points[0].attr("host"), harness.alias);
}

#[tokio::test]
async fn an_exceeded_limit_is_counted_and_its_ratio_stays_in_range() {
    let harness = Harness::new(true);
    let upstream = harness.seed_upstream(harness.alias, vec![endpoint(harness.server.port())]);
    let route = harness.seed_route(upstream, "/v1/models");
    harness.limit(route, 4);
    let _mock = harness.serve("/v1/models", 200);

    // Four requests empty the bucket; the fifth is rejected.
    for _ in 0..4 {
        let status = harness
            .call("GET", harness.alias, "/v1/models", &[])
            .await
            .expect("within budget");
        assert_eq!(status, 200);
    }
    let error = harness
        .call("GET", harness.alias, "/v1/models", &[])
        .await
        .expect_err("the bucket is empty");
    assert_eq!(error.status().as_u16(), 429);

    let points = harness.metrics.counter_points(RATE_LIMIT_EXCEEDED);
    assert_eq!(points.len(), 1, "{points:?}");
    assert_eq!(points[0].sum(), 1);
    assert_eq!(points[0].attr("host"), harness.alias);
    assert_eq!(points[0].attr("path"), "/v1/models");

    let ratio = harness
        .metrics
        .gauge(RATE_LIMIT_USAGE)
        .expect("a numeric capacity");
    assert!(
        (0.0..=1.0).contains(&ratio),
        "the gauge must stay in range: {ratio}"
    );
}

#[tokio::test]
async fn a_request_without_a_limit_records_no_rate_limit_instrument() {
    let harness = Harness::new(true);
    let upstream = harness.seed_upstream(harness.alias, vec![endpoint(harness.server.port())]);
    harness.seed_route(upstream, "/v1/models");

    let _mock = harness.serve("/v1/models", 200);

    harness
        .call("GET", harness.alias, "/v1/models", &[])
        .await
        .expect("the request forwards");

    assert!(
        harness
            .metrics
            .counter_points(RATE_LIMIT_EXCEEDED)
            .is_empty()
    );
    assert!(harness.metrics.gauge(RATE_LIMIT_USAGE).is_none());
}

// ---------------------------------------------------------------------------
// Routing instruments
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_single_endpoint_pool_is_reported_as_the_default_selection() {
    let harness = Harness::new(false);
    let upstream = harness.seed_upstream(harness.alias, vec![endpoint(harness.server.port())]);
    harness.seed_route(upstream, "/v1/models");

    let _mock = harness.serve("/v1/models", 200);

    harness
        .call("GET", harness.alias, "/v1/models", &[])
        .await
        .expect("the request forwards");

    let points = harness.metrics.counter_points(ENDPOINT_SELECTED);
    assert_eq!(points.len(), 1, "{points:?}");
    assert!(
        points[0].labels(&[
            ("upstream_id", upstream.to_string().as_str()),
            ("endpoint_host", "127.0.0.1"),
            ("selection_method", "default"),
        ]),
        "{:?}",
        points[0].attrs
    );
    // No header was sent, so the pin counter is silent — and it has no points
    // at all, which is what makes the absence observable.
    assert!(harness.metrics.counter_points(TARGET_HOST_USED).is_empty());
}

#[tokio::test]
async fn a_pinned_endpoint_is_reported_as_an_explicit_selection() {
    let harness = Harness::new(false);
    let upstream = harness.seed_upstream(harness.alias, vec![endpoint(harness.server.port())]);
    harness.seed_route(upstream, "/v1/models");

    let _mock = harness.serve("/v1/models", 200);

    let status = harness
        .call(
            "GET",
            harness.alias,
            "/v1/models",
            &[("x-oagw-target-host", "127.0.0.1")],
        )
        .await
        .expect("the pin names a configured endpoint");
    assert_eq!(status, 200);

    let selected = harness.metrics.counter_points(ENDPOINT_SELECTED);
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].attr("selection_method"), "explicit_header");

    let pinned = harness.metrics.counter_points(TARGET_HOST_USED);
    assert_eq!(pinned.len(), 1, "recorded once, when the header was sent");
    assert!(
        pinned[0].labels(&[
            ("upstream_id", upstream.to_string().as_str()),
            ("endpoint_host", "127.0.0.1"),
        ]),
        "{:?}",
        pinned[0].attrs
    );
}

#[tokio::test]
async fn a_multi_endpoint_pool_is_reported_as_round_robin() {
    // Two loopback servers, so both endpoints of the pool are reachable and the
    // request forwards rather than failing on a socket.
    let first = MockServer::start();
    let second = MockServer::start();
    let harness = Harness::new(false);
    let upstream = harness.seed_pool(
        harness.alias,
        vec![endpoint(first.port()), endpoint(second.port())],
    );
    harness.seed_route(upstream, "/v1/models");

    let mocks: Vec<httpmock::Mock> = [&first, &second]
        .iter()
        .map(|server| {
            server.mock(|when, then| {
                when.method(httpmock::Method::GET).path("/v1/models");
                then.status(200);
            })
        })
        .collect();

    let mut statuses = Vec::new();
    for _ in 0..2 {
        statuses.push(
            harness
                .call("GET", harness.alias, "/v1/models", &[])
                .await
                .expect("the request forwards"),
        );
    }
    assert_eq!(statuses, vec![200, 200]);
    assert_eq!(
        mocks.iter().map(|mock| mock.calls()).sum::<usize>(),
        2,
        "one request reached each endpoint of the pool"
    );

    // Both pool endpoints have the same *host* (they differ only in port, which
    // §4.2's label list does not carry), so the two selections aggregate into
    // one time series of two — which is the bounded-cardinality property the
    // instrument exists for.
    let points = harness.metrics.counter_points(ENDPOINT_SELECTED);
    assert_eq!(points.len(), 1, "{points:?}");
    assert_eq!(points[0].sum(), 2, "{points:?}");
    assert!(
        points[0].labels(&[
            ("upstream_id", upstream.to_string().as_str()),
            ("endpoint_host", "127.0.0.1"),
            ("selection_method", "round_robin"),
        ]),
        "{:?}",
        points[0].attrs
    );
    assert!(
        !points[0]
            .attrs
            .iter()
            .any(|(key, _)| key == "host" || key == "http.route"),
        "the routing instruments carry no alias and no path label"
    );
    assert!(harness.metrics.counter_points(TARGET_HOST_USED).is_empty());
}

#[tokio::test]
async fn an_unusable_pin_is_not_recorded_as_a_used_target_host() {
    let harness = Harness::new(false);
    let upstream = harness.seed_upstream(harness.alias, vec![endpoint(harness.server.port())]);
    harness.seed_route(upstream, "/v1/models");

    let error = harness
        .call(
            "GET",
            harness.alias,
            "/v1/models",
            &[("x-oagw-target-host", "nope.vendor.test")],
        )
        .await
        .expect_err("the header names no configured endpoint");
    assert_eq!(error.status().as_u16(), 400);

    // `endpoint_host` is a configured host or nothing: a caller-controlled
    // string must never become a label value.
    assert!(harness.metrics.counter_points(TARGET_HOST_USED).is_empty());
    assert!(harness.metrics.counter_points(ENDPOINT_SELECTED).is_empty());
    assert_eq!(harness.metrics.counter(ERRORS_TOTAL), 1);
}
