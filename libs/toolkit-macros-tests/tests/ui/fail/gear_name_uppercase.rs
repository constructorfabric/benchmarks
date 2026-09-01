// Test that gear names with uppercase letters are rejected

use toolkit::Gear;

#[toolkit::gear(
    name = "FileParser",  // Should fail: contains uppercase letters
    capabilities = []
)]
pub struct TestGear;

impl Gear for TestGear {
    async fn init(&self, _ctx: &toolkit::GearCtx) -> anyhow::Result<()> {
        Ok(())
    }
}

fn main() {}
