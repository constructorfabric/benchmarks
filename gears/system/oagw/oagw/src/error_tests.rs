//! Tests of the OAGW error model and its wire projections.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(coverage_nightly, coverage(off))]

use super::*;

/// One representative failure per documented PRD/DESIGN error code.
fn sample(code: ErrorCode) -> OagwError {
    match code {
        ErrorCode::ValidationError => OagwError::Validation {
            detail: String::from("bad request"),
        },
        ErrorCode::AuthenticationFailed => OagwError::AuthenticationFailed {
            detail: String::from("bad api key"),
        },
        ErrorCode::RouteNotFound => OagwError::UpstreamNotFound {
            alias: String::from("api.example.com"),
        },
        ErrorCode::AliasConflict => OagwError::AliasConflict {
            alias: String::from("api.example.com"),
            existing_upstream_id: uuid::uuid!("00000000-0000-0000-0000-000000000001"),
        },
        ErrorCode::PluginInUse => OagwError::PluginInUse {
            plugin_ref: String::from("cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"),
        },
        ErrorCode::Conflict => OagwError::Conflict {
            detail: String::from("route /v1/chat is already claimed by route 0192"),
        },
        ErrorCode::PayloadTooLarge => OagwError::PayloadTooLarge { limit_bytes: 1000 },
        ErrorCode::RateLimitExceeded => OagwError::RateLimitExceeded {
            retry_after_secs: 7,
            detail: String::from("too many requests"),
        },
        ErrorCode::SecretNotFound => OagwError::SecretNotFound {
            secret_ref: String::from("cred://openai/key"),
        },
        ErrorCode::Internal => OagwError::Internal {
            detail: String::from("boom"),
        },
        ErrorCode::ProtocolError => OagwError::ProtocolError {
            detail: String::from("bad protocol"),
        },
        ErrorCode::DownstreamError => OagwError::DownstreamError {
            detail: String::from("upstream 500"),
        },
        ErrorCode::StreamAborted => OagwError::StreamAborted {
            detail: String::from("stream aborted"),
        },
        ErrorCode::LinkUnavailable => OagwError::LinkUnavailable {
            detail: String::from("link down"),
        },
        ErrorCode::CircuitBreakerOpen => OagwError::CircuitBreakerOpen {
            detail: String::from("open"),
        },
        ErrorCode::PluginNotFound => OagwError::PluginNotFound {
            plugin_ref: String::from("cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"),
        },
        ErrorCode::Timeout => OagwError::RequestTimeout {
            detail: String::from("timeout"),
        },
    }
}

/// Expected HTTP status of each documented code (`docs/DESIGN.md` §3.3).
const fn status_of(code: ErrorCode) -> u16 {
    match code {
        ErrorCode::ValidationError => 400,
        ErrorCode::AuthenticationFailed => 401,
        ErrorCode::RouteNotFound => 404,
        ErrorCode::AliasConflict | ErrorCode::PluginInUse | ErrorCode::Conflict => 409,
        ErrorCode::PayloadTooLarge => 413,
        ErrorCode::RateLimitExceeded => 429,
        ErrorCode::SecretNotFound | ErrorCode::Internal => 500,
        ErrorCode::ProtocolError | ErrorCode::DownstreamError | ErrorCode::StreamAborted => 502,
        ErrorCode::LinkUnavailable | ErrorCode::CircuitBreakerOpen | ErrorCode::PluginNotFound => {
            503
        }
        ErrorCode::Timeout => 504,
    }
}

#[test]
fn every_error_code_maps_to_the_documented_status() {
    for code in ErrorCode::ALL {
        let err = sample(code);
        assert_eq!(err.code(), code, "code of {code}");
        assert_eq!(err.http_status(), status_of(code), "status of {code}");
        assert_eq!(err.metadata().code, code);
    }
}

#[test]
fn every_error_code_carries_a_gts_instance_id() {
    for code in ErrorCode::ALL {
        let err = sample(code);
        let gts_type_id = err.gts_type_id();
        assert!(
            gts_type_id.starts_with(ERROR_GTS_TYPE),
            "{gts_type_id} must be an instance of {ERROR_GTS_TYPE}"
        );
        assert!(
            gts::GtsInstanceId::try_new(gts_type_id).is_ok(),
            "{gts_type_id} must be a valid GTS instance id"
        );
        assert_eq!(err.metadata().gts_type_id, gts_type_id);
    }
}

#[test]
fn timeout_variants_share_the_code_but_differ_in_gts_ids() {
    assert_eq!(
        OagwError::ConnectionTimeout {
            detail: String::new()
        }
        .code(),
        ErrorCode::Timeout
    );
    assert_eq!(
        OagwError::IdleTimeout {
            detail: String::new()
        }
        .code(),
        ErrorCode::Timeout
    );
    let connection = OagwError::ConnectionTimeout {
        detail: String::new(),
    }
    .gts_type_id();
    let request = OagwError::RequestTimeout {
        detail: String::new(),
    }
    .gts_type_id();
    let idle = OagwError::IdleTimeout {
        detail: String::new(),
    }
    .gts_type_id();
    assert_ne!(connection, request);
    assert_ne!(request, idle);
    assert_ne!(connection, idle);
}

#[test]
fn retry_advice_follows_the_documented_table() {
    assert_eq!(
        sample(ErrorCode::RateLimitExceeded).retry_advice(),
        RetryAdvice::Always
    );
    assert_eq!(
        sample(ErrorCode::LinkUnavailable).retry_advice(),
        RetryAdvice::Always
    );
    assert_eq!(
        sample(ErrorCode::CircuitBreakerOpen).retry_advice(),
        RetryAdvice::Always
    );
    assert_eq!(
        sample(ErrorCode::Timeout).retry_advice(),
        RetryAdvice::Always
    );
    assert_eq!(
        sample(ErrorCode::DownstreamError).retry_advice(),
        RetryAdvice::Depends
    );
    assert_eq!(
        sample(ErrorCode::ValidationError).retry_advice(),
        RetryAdvice::Never
    );
    assert!(!RetryAdvice::Never.is_retryable());
    assert!(RetryAdvice::Always.is_retryable());
    assert!(RetryAdvice::Depends.is_retryable());
}

#[test]
fn every_gateway_error_is_tagged_as_gateway_source() {
    for code in ErrorCode::ALL {
        let err = sample(code);
        assert_eq!(err.error_source(), ErrorSource::Gateway);
        assert_eq!(ErrorSource::Gateway.as_str(), "gateway");
        assert_eq!(ErrorSource::Upstream.as_str(), "upstream");
    }
}

#[test]
fn alias_conflict_reports_the_existing_upstream() {
    let existing = uuid::uuid!("00000000-0000-0000-0000-000000000042");
    let err = OagwError::AliasConflict {
        alias: String::from("api.example.com"),
        existing_upstream_id: existing,
    };
    assert_eq!(err.resource_name(), Some("api.example.com"));
    assert!(
        err.to_string()
            .contains("00000000-0000-0000-0000-000000000042")
    );
}

#[test]
fn rate_limit_exceeded_carries_the_retry_hint() {
    let err = OagwError::RateLimitExceeded {
        retry_after_secs: 7,
        detail: String::from("slow down"),
    };
    assert_eq!(err.retry_after_secs(), Some(7));
    assert_eq!(
        OagwError::Internal {
            detail: String::new()
        }
        .retry_after_secs(),
        None
    );
}

#[test]
fn problem_body_uses_the_oagw_gts_type_and_context() {
    let upstream_id = uuid::uuid!("00000000-0000-0000-0000-000000000002");
    let err = OagwError::RateLimitExceeded {
        retry_after_secs: 12,
        detail: String::from("tenant budget exhausted"),
    };
    let problem = err.problem(ProblemExtensions {
        instance: Some(String::from("/oagw/v1/proxy/api.example.com")),
        upstream_id: Some(upstream_id),
        host: Some(String::from("api.example.com")),
        path: Some(String::from("/v1/chat")),
        trace_id: Some(String::from("trace-1")),
    });
    assert_eq!(
        problem.problem_type,
        ErrorCode::RateLimitExceeded.gts_type_id()
    );
    assert_eq!(problem.status, 429);
    assert_eq!(problem.error_code.as_deref(), Some("RateLimitExceeded"));
    assert_eq!(problem.error_domain.as_deref(), Some(ERROR_DOMAIN));
    assert_eq!(
        problem.instance.as_deref(),
        Some("/oagw/v1/proxy/api.example.com")
    );
    assert_eq!(problem.trace_id.as_deref(), Some("trace-1"));
    assert_eq!(problem.context["error_source"], "gateway");
    assert_eq!(problem.context["upstream_id"], upstream_id.to_string());
    assert_eq!(problem.context["host"], "api.example.com");
    assert_eq!(problem.context["path"], "/v1/chat");
    assert_eq!(problem.context["retry_after_seconds"], 12);
    assert_eq!(problem.context["resource_type"], ERROR_GTS_TYPE);
}

#[test]
fn problem_title_and_detail_come_from_the_canonical_projection() {
    let problem = OagwError::UpstreamNotFound {
        alias: String::from("api.example.com"),
    }
    .problem(ProblemExtensions::default());
    assert_eq!(problem.status, 404);
    assert!(!problem.title.is_empty());
    assert!(problem.detail.contains("api.example.com"));
}

#[test]
fn canonical_projection_uses_the_aip_193_categories() {
    let upstream_id = uuid::uuid!("00000000-0000-0000-0000-000000000003");
    let cases = [
        (
            OagwError::Validation {
                detail: String::from("nope"),
            },
            "Invalid Argument",
        ),
        (
            OagwError::AuthenticationFailed {
                detail: String::from("who"),
            },
            "Unauthenticated",
        ),
        (
            OagwError::UpstreamNotFound {
                alias: String::from("api.example.com"),
            },
            "Not Found",
        ),
        (
            OagwError::AliasConflict {
                alias: String::from("api.example.com"),
                existing_upstream_id: upstream_id,
            },
            "Already Exists",
        ),
        (
            OagwError::PayloadTooLarge { limit_bytes: 42 },
            "Invalid Argument",
        ),
        (
            OagwError::RateLimitExceeded {
                retry_after_secs: 1,
                detail: String::from("slow"),
            },
            "Resource Exhausted",
        ),
        (
            OagwError::Internal {
                detail: String::from("x"),
            },
            "Internal",
        ),
        (
            OagwError::DownstreamError {
                detail: String::from("x"),
            },
            "Internal",
        ),
        (
            OagwError::LinkUnavailable {
                detail: String::from("x"),
            },
            "Service Unavailable",
        ),
        (
            OagwError::RequestTimeout {
                detail: String::from("x"),
            },
            "Deadline Exceeded",
        ),
    ];
    for (err, expected_title) in cases {
        let canonical = err.to_canonical();
        assert_eq!(canonical.title(), expected_title, "{err}");
        assert_eq!(canonical.status_code(), err.http_status(), "{err}");
    }
}

#[test]
fn canonical_projection_propagates_through_from() {
    let err = OagwError::Validation {
        detail: String::from("nope"),
    };
    let canonical: CanonicalError = err.into();
    assert_eq!(canonical.status_code(), 400);
}

#[test]
fn plugin_errors_carry_the_plugin_identifier() {
    let err = OagwError::PluginNotFound {
        plugin_ref: String::from("cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"),
    };
    assert_eq!(
        err.resource_name(),
        Some("cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1")
    );
    assert_eq!(err.http_status(), 503);
    let secret = OagwError::SecretNotFound {
        secret_ref: String::from("cred://openai/key"),
    };
    assert_eq!(secret.resource_name(), Some("cred://openai/key"));
}

#[test]
fn error_codes_have_unique_titles_and_tokens() {
    let mut tokens: Vec<&str> = ErrorCode::ALL.iter().map(|code| code.as_str()).collect();
    tokens.sort_unstable();
    let count = tokens.len();
    tokens.dedup();
    assert_eq!(tokens.len(), count, "error code tokens must be unique");
    for code in ErrorCode::ALL {
        assert!(!code.title().is_empty());
    }
}

#[test]
fn retry_advice_is_reported_for_every_code() {
    // `docs/DESIGN.md` §3.3: only server-fault rows that the table marks
    // retryable carry `Yes`, and `DownstreamError` carries `Depends`.
    let always = [
        ErrorCode::RateLimitExceeded,
        ErrorCode::LinkUnavailable,
        ErrorCode::CircuitBreakerOpen,
        ErrorCode::Timeout,
    ];
    for code in always {
        assert_eq!(code.retry_advice(), RetryAdvice::Always, "{code}");
    }
    for code in ErrorCode::ALL {
        if !always.contains(&code) && code != ErrorCode::DownstreamError {
            assert_eq!(code.retry_advice(), RetryAdvice::Never, "{code}");
        }
    }
}

#[test]
fn unknown_alias_is_reported_by_upstream_not_found() {
    let err = OagwError::UpstreamNotFound {
        alias: String::from("missing.example.com"),
    };
    assert_eq!(err.http_status(), 404);
    assert_eq!(err.code(), ErrorCode::RouteNotFound);
    assert_eq!(err.gts_type_id(), ErrorCode::RouteNotFound.gts_type_id());
    assert_eq!(err.retry_after_secs(), None);
}
