// Test that gear names ending with hyphen are rejected

use toolkit::Gear;

#[toolkit::gear(
    name = "parser-",  // Should fail: ends with hyphen
    capabilities = []
)]
pub struct TestGear;

impl Gear for TestGear {
    async fn init(&self, _ctx: &toolkit::GearCtx) -> anyhow::Result<()> {
        Ok(())
    }
}

fn main() {}
