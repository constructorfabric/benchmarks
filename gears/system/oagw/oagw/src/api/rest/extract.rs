//! A JSON body extractor whose rejection is a documented `400`.
//!
//! axum's [`Json`] extractor answers a malformed or schema-violating body with
//! `422`; `contracts/management-api.md` promises `400` for every validation
//! failure, so the gear carries its own extractor that translates the
//! rejection into the gateway's problem document.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Request};
use serde::de::DeserializeOwned;

use crate::domain::error::DomainError;

/// A JSON request body.
///
/// Extraction failure — unparsable JSON, wrong type, unknown field — is a
/// [`DomainError::Validation`], which renders as `400`.
#[derive(Debug)]
pub struct Body<T>(pub T);

impl<S, T> FromRequest<S> for Body<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = crate::api::rest::error::OagwError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(Self::rejection(rejection)),
        }
    }
}

impl<T> Body<T> {
    fn rejection(rejection: JsonRejection) -> crate::api::rest::error::OagwError {
        crate::api::rest::error::OagwError::gateway(DomainError::Validation(rejection.body_text()))
    }
}
