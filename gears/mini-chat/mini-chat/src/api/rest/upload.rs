//! Multipart upload handler (DESIGN §3.3 "Upload Attachment").

use axum::Extension;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::BytesMut;
use tokio::time::Instant;
use toolkit::api::canonical_prelude::*;
use toolkit::api::rest::extract;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::handlers::{Svc, attachment_detail_dto};
use crate::domain::error::DomainError;
use crate::domain::upload::normalize_filename;

fn multipart_error(
    field: &'static str,
    reason: &'static str,
    detail: impl Into<String>,
) -> DomainError {
    DomainError::Multipart {
        field,
        reason,
        detail: detail.into(),
    }
}

pub async fn upload_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    extract::Path(id): extract::Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let started = Instant::now();
    let target = svc.prepare_upload(&ctx, id).await?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let boundary = multer::parse_boundary(content_type).map_err(|_| {
        multipart_error(
            "content_type",
            "BOUNDARY_REQUIRED",
            "multipart boundary is required",
        )
    })?;
    let _permit = std::sync::Arc::clone(&svc.upload_permits)
        .try_acquire_owned()
        .map_err(|_| DomainError::UploadConcurrency)?;

    let mut multipart = multer::Multipart::new(body.into_data_stream(), boundary);
    let mut field = loop {
        match multipart.next_field().await {
            Ok(Some(f)) if f.name() == Some("file") => break f,
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err(multipart_error(
                    "file",
                    "MISSING_FILE",
                    "the 'file' field is required",
                )
                .into());
            }
            Err(e) => {
                return Err(multipart_error(
                    "multipart",
                    "MULTIPART_ERROR",
                    format!("invalid multipart body: {e}"),
                )
                .into());
            }
        }
    };
    let Some(part_ct) = field.content_type().map(ToString::to_string) else {
        return Err(multipart_error(
            "content_type",
            "MISSING_CONTENT_TYPE",
            "the file part has no content type",
        )
        .into());
    };
    let filename = normalize_filename(field.file_name());
    let class = svc.classify_upload(&part_ct, &filename, &target)?;

    let mut buf = BytesMut::new();
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                if (buf.len() + chunk.len()) as u64 > class.limit_bytes {
                    return Err(DomainError::FileTooLarge {
                        limit_bytes: class.limit_bytes,
                    }
                    .into());
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => {
                return Err(multipart_error(
                    "multipart",
                    "MULTIPART_ERROR",
                    format!("invalid multipart body: {e}"),
                )
                .into());
            }
        }
    }
    drop(field);
    // Drain the rest of the body so the connection stays usable.
    while let Ok(Some(mut f)) = multipart.next_field().await {
        while let Ok(Some(_)) = f.chunk().await {}
    }

    let row = svc
        .store_upload(&ctx, target, class, filename, buf.freeze(), started)
        .await?;
    Ok((StatusCode::CREATED, Json(attachment_detail_dto(&row))).into_response())
}
