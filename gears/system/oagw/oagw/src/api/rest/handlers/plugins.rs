//! Management handlers for plugins.

use crate::api::extract::{CallerContext, parse_json};
use crate::api::rest::dto::{BuiltinPlugin, CollectionEnvelope, PluginCatalogue, PluginView};
use crate::api::rest::error::gateway_error_response;
use crate::api::rest::handlers::upstreams::{MAX_BODY, Shared, json_response};
use crate::domain::error::OagwError;
use crate::domain::model::Plugin;
use crate::gts_helpers;
use axum::body::Bytes;
use axum::extract::{Extension, Path};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};

/// `POST /plugins`.
///
/// # Errors
///
/// Returns the problem document for any domain failure.
pub async fn create(
    Extension(service): Extension<Shared>,
    uri: Uri,
    caller: CallerContext,
    body: Bytes,
) -> Response {
    let mut plugin: Plugin = match parse_json(&body, MAX_BODY) {
        Ok(value) => value,
        Err(error) => return failure(&error, uri.path()),
    };
    plugin.tenant_id = caller.tenant_id;
    match service.create_plugin(caller.tenant_id, plugin).await {
        Ok(created) => {
            let mut response =
                json_response(StatusCode::CREATED, &PluginView::from(created.clone()));
            if let Ok(value) = axum::http::HeaderValue::from_str(&format!(
                "/oagw/v1/plugins/{}/source",
                created.id
            )) {
                response
                    .headers_mut()
                    .insert(axum::http::header::LOCATION, value);
            }
            response
        }
        Err(error) => failure(&error, uri.path()),
    }
}

/// `GET /plugins`.
///
/// # Errors
///
/// Returns the problem document for any domain failure.
pub async fn list(
    Extension(service): Extension<Shared>,
    uri: Uri,
    caller: CallerContext,
) -> Response {
    match service.list_plugins(caller.tenant_id).await {
        Ok(all) => {
            let custom: Vec<PluginView> = all.into_iter().map(PluginView::from).collect();
            let catalogue = PluginCatalogue {
                builtins: builtin_catalogue(),
                custom,
            };
            json_response(
                StatusCode::OK,
                &CollectionEnvelope::paginate(vec![catalogue], Some(1), 0),
            )
        }
        Err(error) => failure(&error, uri.path()),
    }
}

/// `GET /plugins/{id}`.
///
/// # Errors
///
/// Returns the problem document for any domain failure.
pub async fn get(
    Extension(service): Extension<Shared>,
    Path(id): Path<String>,
    uri: Uri,
    caller: CallerContext,
) -> Response {
    match service.get_plugin(caller.tenant_id, &id).await {
        Ok(found) => json_response(StatusCode::OK, &PluginView::from(found)),
        Err(error) => failure(&error, uri.path()),
    }
}

/// `GET /plugins/{id}/source`.
///
/// Returns the stored source text as `text/plain`; `404` for built-ins.
///
/// # Errors
///
/// Returns the problem document for any domain failure.
pub async fn source(
    Extension(service): Extension<Shared>,
    Path(id): Path<String>,
    uri: Uri,
    caller: CallerContext,
) -> Response {
    match service.get_plugin_source(caller.tenant_id, &id).await {
        Ok(source) => Response::builder()
            .status(StatusCode::OK)
            .header(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )
            .body(axum::body::Body::from(source))
            .unwrap_or_else(|error| {
                (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
            }),
        Err(error) => failure(&error, uri.path()),
    }
}

/// `DELETE /plugins/{id}`.
///
/// # Errors
///
/// Returns the problem document for any domain failure.
pub async fn delete(
    Extension(service): Extension<Shared>,
    Path(id): Path<String>,
    uri: Uri,
    caller: CallerContext,
) -> Response {
    match service.delete_plugin(caller.tenant_id, &id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => failure(&error, uri.path()),
    }
}

/// The plugins the gear implements itself; they have no stored source.
#[must_use]
pub fn builtin_catalogue() -> Vec<BuiltinPlugin> {
    vec![
        BuiltinPlugin {
            plugin_type: gts_helpers::AUTH_NOOP.to_owned(),
            name: "noop".to_owned(),
            kind: crate::domain::model::PluginKind::Auth,
            configurable: false,
        },
        BuiltinPlugin {
            plugin_type: gts_helpers::AUTH_APIKEY.to_owned(),
            name: "apikey".to_owned(),
            kind: crate::domain::model::PluginKind::Auth,
            configurable: true,
        },
        BuiltinPlugin {
            plugin_type: gts_helpers::AUTH_OAUTH2_CLIENT_CRED.to_owned(),
            name: "oauth2_client_cred".to_owned(),
            kind: crate::domain::model::PluginKind::Auth,
            configurable: true,
        },
        BuiltinPlugin {
            plugin_type: gts_helpers::AUTH_OAUTH2_CLIENT_CRED_BASIC.to_owned(),
            name: "oauth2_client_cred_basic".to_owned(),
            kind: crate::domain::model::PluginKind::Auth,
            configurable: true,
        },
        BuiltinPlugin {
            plugin_type: gts_helpers::GUARD_REQUIRED_HEADERS.to_owned(),
            name: "required_headers".to_owned(),
            kind: crate::domain::model::PluginKind::Guard,
            configurable: true,
        },
        BuiltinPlugin {
            plugin_type: gts_helpers::TRANSFORM_REQUEST_ID.to_owned(),
            name: "request_id".to_owned(),
            kind: crate::domain::model::PluginKind::Transform,
            configurable: false,
        },
    ]
}

fn failure(error: &OagwError, instance: &str) -> Response {
    gateway_error_response(error, Some(instance))
}
