//! License feature required by every mini-chat route.

use toolkit::api::operation_builder::{CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature};

/// The platform base license feature (`gts.cf.core.lic.feat.v1~cf.core.global.base.v1`).
///
/// Routes declare it with `.require_license_features([&License])`; the api-gateway rejects
/// tenants without it with 403 `LICENSE_FEATURE_REQUIRED`.
pub struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}
