// Test that gear names starting with hyphen are rejected

use toolkit::Gear;

#[toolkit::gear(
    name = "-parser",  // Should fail: starts with hyphen
    capabilities = []
)]
pub struct TestGear;

impl Gear for TestGear {
    async fn init(&self, _ctx: &toolkit::GearCtx) -> anyhow::Result<()> {
        Ok(())
    }
}

fn main() {}
