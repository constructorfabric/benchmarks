// Test that gear names using snake_case are rejected

use toolkit::Gear;

#[toolkit::gear(
    name = "file_parser",  // Should fail: uses snake_case instead of kebab-case
    capabilities = []
)]
pub struct TestGear;

impl Gear for TestGear {
    async fn init(&self, _ctx: &toolkit::GearCtx) -> anyhow::Result<()> {
        Ok(())
    }
}

fn main() {}
