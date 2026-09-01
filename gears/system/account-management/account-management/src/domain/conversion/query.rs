//! `OData` filter / order surface for the conversion-request listing
//! endpoints.
//!
//! [`ConversionRequestQuery`] declares the public set of filterable /
//! orderable columns the REST handlers expose via
//! `?$filter=` / `?$orderby=` on
//! `GET /tenants/{tenant_id}/conversions` and
//! `GET /tenants/{tenant_id}/child-conversions`. The
//! [`toolkit_odata_macros::ODataFilterable`] derive expands into
//! `ConversionRequestQueryFilterField` (re-exported as
//! [`ConversionRequestFilterField`] for the short ergonomic alias) which
//! the repo-side `paginate_odata` mapper consumes to project field
//! references onto `conversion_requests::Column` and to extract cursor
//! tiebreaker values.
//!
//! Filter-only fields: `id` (`UUIDv4`), `tenant_id`, `parent_id`,
//! `requested_by`. These columns are useful for narrow queries but are
//! not natural `$orderby` keys — callers paginating the listing surface
//! pick `created_at` or `updated_at` for chronological order and rely
//! on the cursor's `id` tiebreaker (appended by `paginate_odata`'s
//! `ensure_tiebreaker`) for total-order stability across page boundaries.
//!
//! Status / target-mode / initiator-side: callers filter against the
//! public enum strings; the impl-side
//! [`ConversionRequestODataMapper::map_value`](../../../infra/storage/repo_impl/conversion.rs)
//! hook translates the string to the storage-side `SMALLINT` before the
//! predicate reaches `SeaORM`, mirroring the
//! [`TenantInfoFilterField::Status`] convention.
//!
//! * `?$filter=status eq 'pending'` (also `'approved'`, `'cancelled'`,
//!   `'rejected'`, `'expired'`).
//! * `?$filter=target_mode eq 'self_managed'` (also `'managed'`).
//! * `?$filter=initiator_side eq 'child'` (also `'parent'`).
//!
//! Only the membership-style operators (`Eq` / `Ne` / `In`) are
//! admissible on these categorical columns; ordered operators
//! (`Gt`/`Ge`/`Lt`/`Le`) are rejected by the mapper because they would
//! silently fall back to the hidden `SMALLINT` ordinal. `$orderby` on
//! these columns is rejected by `is_orderable = false` for the same
//! reason — there is no consistent ordering across the wire
//! (alphabetical) and storage (numeric) shapes.
//!
//! Audit-comment columns (`requested_comment`, `approved_comment`,
//! `cancelled_comment`, `rejected_comment`) are intentionally NOT
//! exposed here: they are unbounded plaintext audit payloads, not
//! filterable / orderable metadata. Surfacing them as `OData` columns
//! would invite caller-side substring scans across audit text that
//! the underlying DB index set does not support.
//!
//! [`TenantInfoFilterField::Status`]: account_management_sdk::TenantInfoFilterField::Status

use time::OffsetDateTime;
use toolkit_macros::domain_model;
use toolkit_odata_macros::ODataFilterable;
use uuid::Uuid;

/// `OData` filter / order column declaration for the conversion-request
/// listing endpoints. The struct is **never** constructed; its only
/// role is to drive the [`ODataFilterable`] derive. The `dead_code`
/// allow keeps clippy quiet on the unused fields — the derive consumes
/// them at compile time.
#[domain_model]
#[derive(ODataFilterable)]
#[allow(dead_code)]
pub struct ConversionRequestQuery {
    /// `conversion_requests.id` (`UUIDv4`, primary key). Exposed as the
    /// cursor tiebreaker so the listing surface can compose
    /// `(created_at DESC, id ASC)` as a total order — preventing silent
    /// row-loss when two rows share a `requested_at` microsecond on
    /// batch INSERT. Exact-id reads go through the dedicated
    /// `GET /conversions/{request_id}` endpoint.
    #[odata(filter(kind = "Uuid"))]
    pub id: Uuid,
    /// `conversion_requests.tenant_id` — the converting tenant. Filter
    /// use is intentional for cross-tenant operator dashboards on the
    /// parent-side listing (`/child-conversions`); the own-side listing
    /// has `tenant_id` pinned by the URL and an explicit caller filter
    /// is redundant there but allowed.
    #[odata(filter(kind = "Uuid"))]
    pub tenant_id: Uuid,
    /// `conversion_requests.parent_id` (nullable in storage; non-null
    /// once root-tenant refusal is in force). Mostly useful on the
    /// parent-side `/child-conversions` listing for operator scoping.
    #[odata(filter(kind = "Uuid"))]
    pub parent_id: Uuid,
    /// `conversion_requests.status` projected as the public lifecycle
    /// string: `"pending"`, `"approved"`, `"cancelled"`, `"rejected"`,
    /// or `"expired"`. The `OData` parser only validates the value is
    /// a `String`; the impl-side
    /// [`ConversionRequestODataMapper::map_value`](../../../infra/storage/repo_impl/conversion.rs)
    /// hook translates the string to the storage-side `SMALLINT`
    /// before the predicate reaches `SeaORM`. Unknown values surface
    /// as a validation error at the boundary, never as a numeric
    /// fallback against the hidden ordinal.
    #[odata(filter(kind = "String"))]
    pub status: String,
    /// `conversion_requests.target_mode` projected as the public
    /// string: `"managed"` or `"self_managed"`. Same string-to-`SMALLINT`
    /// mapping convention as [`Self::status`].
    #[odata(filter(kind = "String"))]
    pub target_mode: String,
    /// `conversion_requests.initiator_side` projected as the public
    /// string: `"child"` or `"parent"`. Same string-to-`SMALLINT`
    /// mapping convention as [`Self::status`].
    #[odata(filter(kind = "String"))]
    pub initiator_side: String,
    /// `conversion_requests.requested_by` — actor UUID stamped at
    /// request initiation. Useful for "show me everything I initiated"
    /// operator dashboards.
    #[odata(filter(kind = "Uuid"))]
    pub requested_by: Uuid,
    /// `conversion_requests.requested_at` — chronological pagination
    /// key. When callers omit `$orderby`, the impl-side
    /// `list_own_for_tenant` / `list_inbound_for_parent` injects
    /// `requested_at DESC` so the default-recent posture is preserved;
    /// `id ASC` is then appended as the unique tiebreaker, yielding
    /// effective order `(requested_at DESC, id ASC)`.
    #[odata(filter(kind = "DateTimeUtc"))]
    pub created_at: OffsetDateTime,
    /// `conversion_requests.expires_at` — TTL boundary stamped at
    /// request initiation. Exposed for operator dashboards probing
    /// near-expiry pending rows; the system-driven reaper consults
    /// this column independently of the public listing.
    #[odata(filter(kind = "DateTimeUtc"))]
    pub expires_at: OffsetDateTime,
    /// `conversion_requests.resolved_at` — wall-clock at which the row
    /// transitioned to a terminal status (`Approved` / `Cancelled` /
    /// `Rejected` / `Expired`). NULL on pending rows. Useful for
    /// "recently resolved" operator queries; the system retention
    /// sweep consults this column independently of the public listing.
    #[odata(filter(kind = "DateTimeUtc"))]
    pub updated_at: OffsetDateTime,
}

/// Short ergonomic alias. The derive produces
/// `ConversionRequestQueryFilterField`; this re-export mirrors the
/// `TenantInfoFilterField` / `MetadataEntryFilterField` convention used
/// across the AM listing surfaces.
pub use ConversionRequestQueryFilterField as ConversionRequestFilterField;
