#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of `cpt-cf-mini-chat-dod-gear-foundation-license-gate`.

use toolkit::api::operation_builder::LicenseFeature;

use super::{BASE_LICENSE_FEATURE, License};

const EXPECTED: &str = "gts.cf.core.lic.feat.v1~cf.core.global.base.v1";

fn declared_feature<F: LicenseFeature>(feature: &F) -> &str {
    feature.as_ref()
}

#[test]
fn base_license_feature_is_the_global_base_feature() {
    assert_eq!(BASE_LICENSE_FEATURE, EXPECTED);
}

#[test]
fn the_license_marker_declares_the_base_feature() {
    assert_eq!(License.as_ref(), EXPECTED);
    assert_eq!(declared_feature(&License), EXPECTED);
}
