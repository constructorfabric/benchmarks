//! Attachment upload/get/delete handlers.

use super::prelude::*;

fn multipart_err(field: &str, reason: &str, desc: &str) -> CanonicalError {
    err(DomainError::invalid(Res::Attachment, field, reason, desc))
}

/// `POST /v1/chats/{id}/attachments` — multipart file upload.
///
/// # Errors
/// Returns the canonical error mapped from the service `DomainError`.
pub async fn upload_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path(id): extract::Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, CanonicalError> {
    let content_type = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
    // Chat and model are checked before the body is read.
    let up = svc.upload_prepare(&ctx, id).await.map_err(|e| match e {
        DomainError::NotFound(_) => err(DomainError::NotFound(Res::Chat)),
        other => err(other),
    })?;
    let boundary = multer::parse_boundary(&content_type)
        .map_err(|_| multipart_err("content_type", "BOUNDARY_REQUIRED", "multipart boundary is required"))?;
    let mut multipart = multer::Multipart::new(body.into_data_stream(), boundary);
    let mut field = loop {
        match multipart.next_field().await {
            Ok(Some(f)) if f.name() == Some("file") => break f,
            Ok(Some(_)) => {}
            Ok(None) => return Err(multipart_err("file", "MISSING_FILE", "multipart field 'file' is required")),
            Err(e) => return Err(multipart_err("multipart", "MULTIPART_ERROR", &format!("invalid multipart body: {e}"))),
        }
    };
    let filename = normalize_filename(field.file_name());
    let Some(part_type) = field.content_type().map(ToString::to_string) else {
        return Err(multipart_err("content_type", "MISSING_CONTENT_TYPE", "the file part has no content type"));
    };
    let classified = svc.classify_upload(&up, &part_type, &filename).map_err(err)?;
    let limit = MiniChatService::size_limit(&up, &classified);
    let mut buf = BytesMut::new();
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                if (buf.len() + chunk.len()) as u64 > limit {
                    return Err(err(DomainError::out_of_range(
                        Res::Attachment,
                        "content_length",
                        "FILE_TOO_LARGE",
                        "file exceeds the upload size limit",
                    )));
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => return Err(multipart_err("multipart", "MULTIPART_ERROR", &format!("invalid multipart body: {e}"))),
        }
    }
    let data: Bytes = buf.freeze();
    let row = svc.upload_commit(&ctx, up, classified, filename, data).await.map_err(err)?;
    Ok((StatusCode::CREATED, Json(AttachmentDetailDto::from(row))).into_response())
}

/// `GET /v1/chats/{id}/attachments/{attachment_id}` — attachment status.
///
/// # Errors
/// Returns the canonical error mapped from the service `DomainError`.
pub async fn get_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path((id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<AttachmentDetailDto>> {
    Ok(Json(svc.get_attachment(&ctx, id, attachment_id).await.map_err(err)?.into()))
}

/// `DELETE /v1/chats/{id}/attachments/{attachment_id}` — delete an attachment.
///
/// # Errors
/// Returns the canonical error mapped from the service `DomainError`.
pub async fn delete_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path((id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    svc.delete_attachment(&ctx, id, attachment_id).await.map_err(err)?;
    Ok(StatusCode::NO_CONTENT)
}
