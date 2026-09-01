// Test that gear names with consecutive hyphens are rejected

use toolkit::Gear;

#[toolkit::gear(
    name = "file--parser",  // Should fail: consecutive hyphens
    capabilities = []
)]
pub struct TestGear;

impl Gear for TestGear {
    async fn init(&self, _ctx: &toolkit::GearCtx) -> anyhow::Result<()> {
        Ok(())
    }
}

fn main() {}
