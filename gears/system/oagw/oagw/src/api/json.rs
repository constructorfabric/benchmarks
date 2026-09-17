// Created: 2026-09-03 by Constructor Tech
//! A JSON body extractor that reports failures in the gateway catalogue.
//!
//! `axum::Json` answers a payload the schema cannot parse with a bare `422`,
//! which is not part of the error table of `DESIGN.md` §4. Wrapping the
//! extractor keeps the handler bodies unchanged while every malformed or
//! schema-violating document becomes a `400` `validation.error` problem.

use serde::de::DeserializeOwned;

use crate::error::{ErrorKind, OagwError};

/// A JSON request body decoded into `T`.
#[derive(Debug, Clone)]
pub struct JsonBody<T>(pub T);

impl<S, T> axum::extract::FromRequest<S> for JsonBody<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = OagwError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(OagwError::new(
                ErrorKind::Validation,
                format!("invalid request body: {}", rejection.body_text()),
            )),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use axum::extract::FromRequest;
    use serde_json::json;

    #[derive(Debug, serde::Deserialize)]
    #[allow(dead_code)]
    struct Payload {
        plugin_type: crate::model::PluginType,
    }

    async fn decode(raw: &str) -> Result<JsonBody<Payload>, OagwError> {
        let request = axum::extract::Request::builder()
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(raw.to_owned()))
            .expect("request");
        JsonBody::<Payload>::from_request(request, &()).await
    }

    #[tokio::test]
    async fn a_schema_violation_is_a_validation_error() {
        let body = json!({ "plugin_type": "webhook" }).to_string();
        let error = decode(&body).await.expect_err("rejected");
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.kind().status(), 400);
    }

    #[tokio::test]
    async fn a_broken_document_is_a_validation_error() {
        let error = decode("{ not json").await.expect_err("rejected");
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.kind().status(), 400);
    }

    #[tokio::test]
    async fn a_valid_document_decodes() {
        let payload = decode(r#"{"plugin_type":"guard"}"#).await.expect("decoded");
        assert_eq!(payload.0.plugin_type, crate::model::PluginType::Guard);
    }
}
