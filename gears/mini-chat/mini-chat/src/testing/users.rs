//! Fixed test identities.

use toolkit_security::SecurityContext;
use uuid::Uuid;

/// Subject type of end users (DESIGN §3.8 evaluation examples).
pub const USER_SUBJECT_TYPE: &str = "gts.cf.core.security.subject_user.v1~";

/// A caller identity: tenant + user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TestUser {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
}

const TENANT_A: Uuid = Uuid::from_u128(0x0000_0000_df51_5b42_9538_d2b5_6b7e_e953);
const TENANT_B: Uuid = Uuid::from_u128(0xbbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb);

impl TestUser {
    /// Tenant A, first user.
    pub const A1: Self = Self {
        tenant_id: TENANT_A,
        user_id: Uuid::from_u128(0x1111_1111_6a88_4768_9dfc_6bcd_5187_d9ed),
    };
    /// Tenant A, second user.
    pub const A2: Self = Self {
        tenant_id: TENANT_A,
        user_id: Uuid::from_u128(0x4444_4444_6a88_4768_9dfc_6bcd_5187_d9ed),
    };
    /// The gear's S2S identity in tests (the harness pre-sets the S2S context
    /// with it; same tenant and subject as [`Self::A1`]).
    pub const S2S: Self = Self {
        tenant_id: TENANT_A,
        user_id: Uuid::from_u128(0x1111_1111_6a88_4768_9dfc_6bcd_5187_d9ed),
    };
    /// Tenant B, first user.
    pub const B1: Self = Self {
        tenant_id: TENANT_B,
        user_id: Uuid::from_u128(0x2222_2222_6a88_4768_9dfc_6bcd_5187_d9ed),
    };

    /// First-party user context (`token_scopes = ["*"]`).
    ///
    /// # Panics
    /// Never: every builder field is set.
    #[must_use]
    #[allow(clippy::expect_used)]
    pub fn security_context(self) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(self.user_id)
            .subject_tenant_id(self.tenant_id)
            .subject_type(USER_SUBJECT_TYPE)
            .token_scopes(vec!["*".to_owned()])
            .build()
            .expect("test security context")
    }
}
