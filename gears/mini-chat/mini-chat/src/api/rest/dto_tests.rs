#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;
use time::macros::datetime;

use super::{QuotaDecisionKindDto, QuotaStatusResponse, QuotaWarning};
use crate::domain::services::quota_service::{
    QuotaDecisionKind, QuotaPeriodKind, QuotaPeriodStatusView, QuotaStatusView, QuotaTierKind,
    QuotaTierStatusView, QuotaWarningView,
};

#[test]
fn quota_status_response_serializes_contract_shape() {
    let reset = datetime!(2026-10-05 00:00:00 UTC);
    let view = QuotaStatusView {
        tiers: vec![
            QuotaTierStatusView {
                tier: QuotaTierKind::Premium,
                periods: vec![QuotaPeriodStatusView {
                    period: QuotaPeriodKind::Daily,
                    limit_credits_micro: 100,
                    used_credits_micro: 80,
                    remaining_credits_micro: 20,
                    remaining_percentage: 20,
                    next_reset: reset,
                    warning: true,
                    exhausted: false,
                }],
            },
            QuotaTierStatusView {
                tier: QuotaTierKind::Total,
                periods: vec![],
            },
        ],
        warning_threshold_pct: 80,
    };
    let body = serde_json::to_value(QuotaStatusResponse::from(view)).unwrap();
    assert_eq!(
        body,
        json!({
            "tiers": [
                {"tier": "premium", "periods": [{
                    "period": "daily",
                    "limit_credits_micro": 100,
                    "used_credits_micro": 80,
                    "remaining_credits_micro": 20,
                    "remaining_percentage": 20,
                    "next_reset": "2026-10-05T00:00:00Z",
                    "warning": true,
                    "exhausted": false
                }]},
                {"tier": "total", "periods": []}
            ],
            "warning_threshold_pct": 80
        })
    );
}

#[test]
fn quota_warning_omits_absent_next_reset() {
    let mut view = QuotaWarningView {
        tier: QuotaTierKind::Total,
        period: QuotaPeriodKind::Monthly,
        remaining_percentage: 55,
        warning: false,
        exhausted: false,
        next_reset: None,
    };
    assert_eq!(
        serde_json::to_value(QuotaWarning::from(view)).unwrap(),
        json!({"tier": "total", "period": "monthly", "remaining_percentage": 55,
               "warning": false, "exhausted": false})
    );
    view.next_reset = Some(datetime!(2026-11-01 00:00:00 UTC));
    view.warning = true;
    let body = serde_json::to_value(QuotaWarning::from(view)).unwrap();
    assert_eq!(body["next_reset"], "2026-11-01T00:00:00Z");
}

#[test]
fn quota_decision_maps_to_lowercase_values() {
    assert_eq!(
        serde_json::to_value(QuotaDecisionKindDto::from(QuotaDecisionKind::Allow)).unwrap(),
        "allow"
    );
    assert_eq!(
        serde_json::to_value(QuotaDecisionKindDto::from(QuotaDecisionKind::Downgrade)).unwrap(),
        "downgrade"
    );
}

#[test]
fn attachment_detail_omits_absent_fields() {
    use super::AttachmentDetailDto;
    use crate::domain::enums::{AttachmentKind, AttachmentStatus};
    use crate::domain::services::attachment_service::AttachmentView;
    use crate::domain::services::message_service::ThumbnailView;

    let id = uuid::Uuid::nil();
    let doc = AttachmentView {
        id,
        filename: "a.pdf".to_owned(),
        content_type: "application/pdf".to_owned(),
        size_bytes: 42,
        status: AttachmentStatus::Uploaded,
        kind: AttachmentKind::Document,
        error_code: None,
        img_thumbnail: None,
        created_at: datetime!(2026-10-04 12:00:00 UTC),
    };
    assert_eq!(
        serde_json::to_value(AttachmentDetailDto::from(doc.clone())).unwrap(),
        json!({
            "id": "00000000-0000-0000-0000-000000000000",
            "filename": "a.pdf",
            "content_type": "application/pdf",
            "size_bytes": 42,
            "status": "uploaded",
            "kind": "document",
            "created_at": "2026-10-04T12:00:00Z"
        })
    );

    let failed = AttachmentView {
        status: AttachmentStatus::Failed,
        error_code: Some("indexing_failed".to_owned()),
        ..doc.clone()
    };
    let body = serde_json::to_value(AttachmentDetailDto::from(failed)).unwrap();
    assert_eq!(body["error_code"], "indexing_failed");
    assert_eq!(body["status"], "failed");

    let image = AttachmentView {
        status: AttachmentStatus::Ready,
        kind: AttachmentKind::Image,
        img_thumbnail: Some(ThumbnailView {
            content_type: "image/webp",
            width: 2,
            height: 1,
            data: vec![1, 2, 3],
        }),
        ..doc
    };
    let body = serde_json::to_value(AttachmentDetailDto::from(image)).unwrap();
    assert_eq!(
        body["img_thumbnail"],
        json!({"content_type": "image/webp", "width": 2, "height": 1, "data_base64": "AQID"})
    );
    assert_eq!(body["kind"], "image");
    assert!(body.get("doc_summary").is_none());
    assert!(body.get("summary_updated_at").is_none());
}
