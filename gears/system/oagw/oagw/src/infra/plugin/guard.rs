//! The `required_headers` guard plugin
//! (`cpt-cf-oagw-dod-builtin-plugin-behaviors`,
//! `cpt-cf-oagw-algo-required-headers-guard`, ADR 0009).
//!
//! A stateless guard that checks the presence of the configured header names on
//! the request and on the upstream response, failing open when unconfigured and
//! rejecting with the phase status of ADR 0009 — 400 in the request phase, 502
//! in the response phase — with `error_code` `REQUIRED_HEADER_MISSING` and the
//! first missing header name. A configured value that is present but not a
//! string is not unconfigured: it is the 400 validation error naming the key.
// @cpt-begin:cpt-cf-oagw-dod-builtin-plugin-behaviors:p1:inst-guard-full

use std::collections::BTreeMap;

use http::HeaderMap;
use http::header::HeaderName;

use crate::domain::error::OagwError;
use crate::domain::plugin::{
    GUARD_PLUGIN_TYPE, GUARD_REQUEST_PHASE_STATUS, GUARD_RESPONSE_PHASE_STATUS, GuardDecision,
    GuardRejection, REQUIRED_HEADER_MISSING, RequestContext, ResponseContext,
};
use crate::infra::plugin::plan::PlanEntry;
use crate::infra::plugin::registry::REQUIRED_HEADERS_GUARD_PLUGIN_ID;

/// The config key of the request-phase required headers (ADR 0009).
const REQUIRED_REQUEST_HEADERS_KEY: &str = "required_request_headers";
/// The config key of the response-phase required headers (ADR 0009).
const REQUIRED_RESPONSE_HEADERS_KEY: &str = "required_response_headers";

/// The outcome of one guard tier: the phase either continues or stops with the
/// first rejection.
///
/// A rejection is a plugin decision, deliberately not an [`OagwError`]: an
/// `Err` return of a guard invocation is a plugin failure and is mapped per the
/// failure-mapping interpretation of §1.5 instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardOutcome {
    /// Every guard of the tier allowed the phase.
    Allowed,
    /// The first rejecting guard stopped the phase.
    Rejected(GuardRejection),
}

/// The guard plugin enforcing the presence of configured header names
/// (ADR 0009).
///
/// Stateless: the plugin holds no cache, no security-sensitive material and no
/// per-request state, and its only configuration is the binding `config`
/// object the caller hands to the phase.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequiredHeadersGuardPlugin;

impl RequiredHeadersGuardPlugin {
    /// Decides one phase: the headers of the phase and the config key of that
    /// phase are the only inputs, so configuring one key never affects the
    /// other phase.
    ///
    /// # Errors
    /// Returns the 400 validation error of a configured value that is neither
    /// absent nor a string, which is a plugin failure rather than a decision.
    fn decide(
        config: &BTreeMap<String, serde_json::Value>,
        key: &str,
        headers: &HeaderMap,
        phase_status: u16,
    ) -> Result<GuardDecision, OagwError> {
        // @cpt-begin:cpt-cf-oagw-algo-required-headers-guard:p1:inst-rh-01
        // Read the phase's config key from the binding config: the two keys are
        // independent and configuring one never affects the other phase.
        let required = required_headers(config, key)?;
        // @cpt-begin:cpt-cf-oagw-algo-required-headers-guard:p1:inst-rh-02
        // An absent or blank key is the fail-open behaviour of ADR 0009.
        // @cpt-begin:cpt-cf-oagw-algo-required-headers-guard:p1:inst-rh-03
        if required.is_empty() {
            return Ok(GuardDecision::Allow);
        }
        // @cpt-end:cpt-cf-oagw-algo-required-headers-guard:p1:inst-rh-03
        // @cpt-end:cpt-cf-oagw-algo-required-headers-guard:p1:inst-rh-02

        // @cpt-begin:cpt-cf-oagw-algo-required-headers-guard:p1:inst-rh-04
        // The value is parsed by splitting on ',', trimming each entry,
        // lowercasing it and dropping the empty entries.
        for name in required {
            // @cpt-begin:cpt-cf-oagw-algo-required-headers-guard:p1:inst-rh-05
            // Presence is checked case-insensitively, in order, and only
            // presence is checked — never a header value.
            if !header_present(headers, &name) {
                // @cpt-begin:cpt-cf-oagw-algo-required-headers-guard:p1:inst-rh-08
                // The first missing name stops the scan.
                // @cpt-begin:cpt-cf-oagw-algo-required-headers-guard:p1:inst-rh-09
                // The rejection carries the ADR 0009 phase status, the
                // `REQUIRED_HEADER_MISSING` error code and that one name in
                // `detail`, and none of the remaining names.
                return Ok(GuardDecision::Reject(GuardRejection {
                    status: phase_status,
                    error_code: REQUIRED_HEADER_MISSING,
                    detail: name,
                }));
                // @cpt-end:cpt-cf-oagw-algo-required-headers-guard:p1:inst-rh-09
                // @cpt-end:cpt-cf-oagw-algo-required-headers-guard:p1:inst-rh-08
            }
            // @cpt-end:cpt-cf-oagw-algo-required-headers-guard:p1:inst-rh-05
        }
        // @cpt-end:cpt-cf-oagw-algo-required-headers-guard:p1:inst-rh-04
        // @cpt-begin:cpt-cf-oagw-algo-required-headers-guard:p1:inst-rh-06
        // Every required name is present.
        // @cpt-begin:cpt-cf-oagw-algo-required-headers-guard:p1:inst-rh-07
        Ok(GuardDecision::Allow)
        // @cpt-end:cpt-cf-oagw-algo-required-headers-guard:p1:inst-rh-07
        // @cpt-end:cpt-cf-oagw-algo-required-headers-guard:p1:inst-rh-06
        // @cpt-begin:cpt-cf-oagw-algo-required-headers-guard:p1:inst-rh-10
        // The decision is a stateless result: the plugin holds no cache, no
        // security-sensitive material and no per-request state.
        // @cpt-end:cpt-cf-oagw-algo-required-headers-guard:p1:inst-rh-10
        // @cpt-end:cpt-cf-oagw-algo-required-headers-guard:p1:inst-rh-01
    }
}

#[async_trait::async_trait]
impl crate::domain::plugin::GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        REQUIRED_HEADERS_GUARD_PLUGIN_ID
    }

    fn plugin_type(&self) -> &str {
        GUARD_PLUGIN_TYPE
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, OagwError> {
        Self::decide(
            &ctx.config,
            REQUIRED_REQUEST_HEADERS_KEY,
            &ctx.headers,
            GUARD_REQUEST_PHASE_STATUS,
        )
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, OagwError> {
        Self::decide(
            &ctx.config,
            REQUIRED_RESPONSE_HEADERS_KEY,
            &ctx.headers,
            GUARD_RESPONSE_PHASE_STATUS,
        )
    }
}

/// Reads one phase's required-header config key as the parsed name list.
///
/// A key that is absent, `null`, blank after trimming, or empty after parsing
/// yields an empty list, which is the fail-open behaviour of ADR 0009: an
/// unconfigured binding has no behaviour change in either phase, and a malformed
/// comma list silently no-ops rather than erroring.
///
/// A value of any other type is **not** unconfigured — it is a configured key
/// the guard cannot read, and silently treating it as unconfigured would widen
/// the guard past ADR 0009's absent/blank rule and fail every request open on a
/// misconfigured binding. Such a value is the 400 validation error naming the
/// key instead.
///
/// # Errors
/// Returns the 400 validation error naming the key for a value that is present
/// and not a string.
fn required_headers(
    config: &BTreeMap<String, serde_json::Value>,
    key: &str,
) -> Result<Vec<String>, OagwError> {
    match config.get(key) {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::String(value)) => Ok(value
            .split(',')
            .map(str::trim)
            .map(str::to_lowercase)
            .filter(|entry| !entry.is_empty())
            .collect()),
        Some(_) => Err(invalid_config(
            key,
            "must be a string of comma-separated header names",
        )),
    }
}

/// Builds the 400 validation error naming the offending key.
fn invalid_config(key: &str, reason: &str) -> OagwError {
    OagwError::validation_error(format!("oagw.plugin.required_headers: '{key}' {reason}"))
}

/// Whether the phase's headers carry the name, case-insensitively.
fn header_present(headers: &HeaderMap, name: &str) -> bool {
    HeaderName::from_bytes(name.as_bytes())
        .map(|parsed| headers.contains_key(&parsed))
        .unwrap_or(false)
}

/// Drives the request phase of the guard tier, in plan order.
///
/// Each guard is invoked with the request context whose `config` is that
/// guard's own binding config; the first rejection stops the tier, so no guard
/// after it runs.
///
/// # Errors
/// Returns the typed failure of a guard invocation, which is a plugin failure
/// and not a rejection decision.
pub async fn guard_request_phase(
    guards: &[PlanEntry<dyn crate::domain::plugin::GuardPlugin>],
    ctx: &RequestContext,
) -> Result<GuardOutcome, crate::domain::error::OagwError> {
    // @cpt-begin:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-gr-02
    // FOR EACH guard in the guard tier, in plan order, invoke
    // guard_request(&RequestContext) and take its GuardDecision; an Err return
    // of the guard invocation is a plugin failure rather than a rejection
    // decision and is returned to the caller to be mapped per §1.5.
    for entry in guards {
        let mut scoped = RequestContext::clone(ctx);
        scoped.config = entry.binding.config.clone().unwrap_or_default();
        // @cpt-begin:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-gr-03
        // A guard decision that rejects is decided by the required-headers
        // guard algorithm above.
        match entry.plugin.guard_request(&scoped).await? {
            crate::domain::plugin::GuardDecision::Allow => {}
            // @cpt-begin:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-gr-04
            // The rejection is returned to the caller with the request-phase
            // status 400, the `error_code` `REQUIRED_HEADER_MISSING` and the
            // first missing header name in `detail`; the caller renders it
            // through the problem+json contract with
            // `X-OAGW-Error-Source: gateway`. No transform runs and no upstream
            // call is made.
            crate::domain::plugin::GuardDecision::Reject(rejection) => {
                return Ok(GuardOutcome::Rejected(rejection));
            } // @cpt-end:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-gr-04
        }
        // @cpt-end:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-gr-03
    }
    Ok(GuardOutcome::Allowed)
    // @cpt-end:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-gr-02
}

/// Drives the response phase of the guard tier, in plan order.
///
/// The symmetric counterpart of [`guard_request_phase`] over the upstream
/// response, whose rejections carry the response-phase status 502.
///
/// # Errors
/// Returns the typed failure of a guard invocation, which is a plugin failure
/// and not a rejection decision.
pub async fn guard_response_phase(
    guards: &[PlanEntry<dyn crate::domain::plugin::GuardPlugin>],
    ctx: &ResponseContext,
) -> Result<GuardOutcome, crate::domain::error::OagwError> {
    // @cpt-begin:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-gr-07
    // FOR EACH guard in the guard tier, in plan order, invoke
    // guard_response(&ResponseContext) and take its GuardDecision; an Err
    // return of the guard invocation is a plugin failure rather than a
    // rejection decision and is returned to the caller to be mapped per §1.5.
    for entry in guards {
        let mut scoped = ResponseContext::clone(ctx);
        scoped.config = entry.binding.config.clone().unwrap_or_default();
        // @cpt-begin:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-gr-08
        // A guard decision that rejects in the response phase carries the
        // response-phase status of ADR 0009.
        match entry.plugin.guard_response(&scoped).await? {
            crate::domain::plugin::GuardDecision::Allow => {}
            // @cpt-begin:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-gr-09
            // The rejection is returned with the response-phase status 502, the
            // same `error_code` `REQUIRED_HEADER_MISSING`, the first missing
            // header name in `detail` and `X-OAGW-Error-Source: gateway`.
            crate::domain::plugin::GuardDecision::Reject(rejection) => {
                return Ok(GuardOutcome::Rejected(rejection));
            } // @cpt-end:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-gr-09
        }
        // @cpt-end:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-gr-08
    }
    Ok(GuardOutcome::Allowed)
    // @cpt-end:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-gr-07
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::PLUGIN_TYPE_IDS;
    use crate::domain::plugin::GuardDecision;
    use crate::domain::plugin::GuardPlugin;
    use serde_json::Value;

    fn guard() -> RequiredHeadersGuardPlugin {
        RequiredHeadersGuardPlugin
    }

    fn request(config: BTreeMap<String, Value>) -> RequestContext {
        let security = toolkit_security::SecurityContext::builder()
            .subject_id(uuid::Uuid::nil())
            .subject_tenant_id(uuid::Uuid::nil())
            .build()
            .expect("a test context carries a subject and a tenant");
        let mut ctx = RequestContext::new(security);
        ctx.config = config;
        ctx
    }

    fn response(config: BTreeMap<String, Value>) -> ResponseContext {
        ResponseContext {
            config,
            ..ResponseContext::default()
        }
    }

    fn config(value: &str) -> BTreeMap<String, Value> {
        BTreeMap::from([(
            REQUIRED_REQUEST_HEADERS_KEY.to_owned(),
            Value::String(value.to_owned()),
        )])
    }

    fn response_config(value: &str) -> BTreeMap<String, Value> {
        BTreeMap::from([(
            REQUIRED_RESPONSE_HEADERS_KEY.to_owned(),
            Value::String(value.to_owned()),
        )])
    }

    async fn request_decision(config: BTreeMap<String, Value>) -> GuardDecision {
        guard()
            .guard_request(&request(config))
            .await
            .expect("a decision")
    }

    async fn response_decision(config: BTreeMap<String, Value>) -> GuardDecision {
        guard()
            .guard_response(&response(config))
            .await
            .expect("a decision")
    }

    fn rejection_of(decision: GuardDecision) -> GuardRejection {
        match decision {
            GuardDecision::Reject(rejection) => rejection,
            GuardDecision::Allow => panic!("expected a rejection, got Allow"),
        }
    }

    #[tokio::test]
    async fn an_absent_key_fails_open_in_both_phases() {
        assert_eq!(
            request_decision(BTreeMap::new()).await,
            GuardDecision::Allow
        );
        assert_eq!(
            response_decision(BTreeMap::new()).await,
            GuardDecision::Allow
        );
    }

    #[tokio::test]
    async fn a_blank_key_fails_open_in_both_phases() {
        for value in ["", "   ", ",", " , , ", ",,"] {
            assert_eq!(
                request_decision(config(value)).await,
                GuardDecision::Allow,
                "'{value}' must fail open in the request phase"
            );
            assert_eq!(
                response_decision(response_config(value)).await,
                GuardDecision::Allow,
                "'{value}' must fail open in the response phase"
            );
        }
    }

    #[tokio::test]
    async fn a_present_value_that_is_not_a_string_is_a_validation_error() {
        // A value that is present but not a string is a configured key the guard
        // cannot read, so it is a 400 validation error naming the key instead of
        // the fail-open of an unconfigured binding.
        for value in [
            Value::from(7),
            Value::from(vec!["x-auth"]),
            Value::Bool(true),
        ] {
            for key in [REQUIRED_REQUEST_HEADERS_KEY, REQUIRED_RESPONSE_HEADERS_KEY] {
                let config = BTreeMap::from([(key.to_owned(), value.clone())]);
                let error = if key == REQUIRED_REQUEST_HEADERS_KEY {
                    guard().guard_request(&request(config)).await.unwrap_err()
                } else {
                    guard().guard_response(&response(config)).await.unwrap_err()
                };

                assert_eq!(error.mapping().variant, "ValidationError", "{key}");
                assert_eq!(error.status(), 400, "{key}");
                assert_eq!(
                    error.gts_type(),
                    "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
                    "{key}"
                );
                assert!(
                    error.detail().contains(key),
                    "the failure names the offending key '{key}': {}",
                    error.detail()
                );
            }
        }
    }

    #[tokio::test]
    async fn a_null_value_fails_open_like_an_absent_key() {
        for key in [REQUIRED_REQUEST_HEADERS_KEY, REQUIRED_RESPONSE_HEADERS_KEY] {
            let config = BTreeMap::from([(key.to_owned(), Value::Null)]);
            let decision = if key == REQUIRED_REQUEST_HEADERS_KEY {
                guard().guard_request(&request(config)).await.unwrap()
            } else {
                guard().guard_response(&response(config)).await.unwrap()
            };
            assert_eq!(decision, GuardDecision::Allow, "{key}");
        }
    }

    #[tokio::test]
    async fn the_value_is_split_trimmed_lowercased_and_emptied() {
        // The parse is observed through the rejection detail, which carries the
        // first surviving parsed name.
        let rejection =
            rejection_of(request_decision(config("  X-Correlation-ID , accept ,, ")).await);
        assert_eq!(rejection.detail, "x-correlation-id");

        let mut ctx = request(config("X-Api-Key"));
        ctx.headers.insert("x-API-key", "value".parse().unwrap());
        assert_eq!(
            guard().guard_request(&ctx).await.unwrap(),
            GuardDecision::Allow,
            "a name is lowercased before it is matched"
        );
    }

    #[tokio::test]
    async fn the_check_is_presence_only_and_case_insensitive() {
        let mut ctx = request(config("X-Correlation-Id"));
        ctx.headers.insert("x-CORRELATION-id", "".parse().unwrap());

        assert_eq!(
            guard().guard_request(&ctx).await.unwrap(),
            GuardDecision::Allow,
            "an empty header value still counts as present, and the match is case-insensitive"
        );
    }

    #[tokio::test]
    async fn the_first_missing_header_is_the_single_detail() {
        let rejection = rejection_of(request_decision(config("x-first,x-second,x-third")).await);

        assert_eq!(rejection.detail, "x-first", "only the first missing name");
        assert_eq!(rejection.error_code, REQUIRED_HEADER_MISSING);
        assert_eq!(rejection.status, GUARD_REQUEST_PHASE_STATUS);

        let mut ctx = request(config("x-first,x-second,x-third"));
        ctx.headers.insert("x-first", "1".parse().unwrap());
        let rejection = rejection_of(guard().guard_request(&ctx).await.unwrap());
        assert_eq!(
            rejection.detail, "x-second",
            "the next missing name in order"
        );
    }

    #[tokio::test]
    async fn a_request_phase_rejection_carries_the_400_status() {
        let rejection = rejection_of(request_decision(config("x-missing")).await);

        assert_eq!(rejection.status, 400);
        assert_eq!(rejection.error_code, "REQUIRED_HEADER_MISSING");
    }

    #[tokio::test]
    async fn a_response_phase_rejection_carries_the_502_status() {
        let rejection = rejection_of(response_decision(response_config("content-type")).await);

        assert_eq!(rejection.status, 502);
        assert_eq!(rejection.error_code, "REQUIRED_HEADER_MISSING");
        assert_eq!(rejection.detail, "content-type");
    }

    #[tokio::test]
    async fn each_key_affects_only_its_own_phase() {
        let request_only = request(config("x-request-only"));
        assert_eq!(
            guard()
                .guard_response(&response_from_request(&request_only))
                .await
                .unwrap(),
            GuardDecision::Allow,
            "required_request_headers never affects the response phase"
        );

        let response_only = response(response_config("x-response-only"));
        assert_eq!(
            guard()
                .guard_request(&request_from_response(&response_only))
                .await
                .unwrap(),
            GuardDecision::Allow,
            "required_response_headers never affects the request phase"
        );
    }

    fn response_from_request(ctx: &RequestContext) -> ResponseContext {
        ResponseContext {
            headers: ctx.headers.clone(),
            request_id: None,
            config: BTreeMap::new(),
        }
    }

    fn request_from_response(ctx: &ResponseContext) -> RequestContext {
        let security = toolkit_security::SecurityContext::builder()
            .subject_id(uuid::Uuid::nil())
            .subject_tenant_id(uuid::Uuid::nil())
            .build()
            .expect("a test context carries a subject and a tenant");
        let mut request = RequestContext::new(security);
        request.headers = ctx.headers.clone();
        request.config = BTreeMap::new();
        request
    }

    #[tokio::test]
    async fn a_satisfied_phase_is_allowed() {
        let mut ctx = request(config("x-correlation-id, accept"));
        ctx.headers
            .insert("x-correlation-id", "c-1".parse().unwrap());
        ctx.headers
            .insert("accept", "application/json".parse().unwrap());

        assert_eq!(
            guard().guard_request(&ctx).await.unwrap(),
            GuardDecision::Allow
        );

        let mut upstream = response(response_config("content-type"));
        upstream
            .headers
            .insert("content-type", "application/json".parse().unwrap());
        assert_eq!(
            guard().guard_response(&upstream).await.unwrap(),
            GuardDecision::Allow
        );
    }

    #[tokio::test]
    async fn a_rejection_is_an_ok_decision_and_never_an_error() {
        let decision = guard().guard_request(&request(config("x-missing"))).await;

        assert!(
            decision.is_ok(),
            "a rejection is a GuardDecision::Reject and not an Err"
        );
        assert_eq!(
            decision.unwrap(),
            GuardDecision::Reject(rejection_of(request_decision(config("x-missing")).await)),
        );
        assert_eq!(PLUGIN_TYPE_IDS.len(), 3);
    }
}

// @cpt-end:cpt-cf-oagw-dod-builtin-plugin-behaviors:p1:inst-guard-full
