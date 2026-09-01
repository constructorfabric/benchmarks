// Test that valid kebab-case gear names are accepted

#[toolkit::gear(
    name = "file-parser",  // Valid kebab-case
    capabilities = []
)]
#[derive(Default)]
pub struct FileParserGear;

#[async_trait::async_trait]
impl toolkit::Gear for FileParserGear {
    async fn init(&self, _ctx: &toolkit::GearCtx) -> anyhow::Result<()> {
        Ok(())
    }
}

#[toolkit::gear(
    name = "simple-user-settings",  // Valid kebab-case with multiple hyphens
    capabilities = []
)]
#[derive(Default)]
pub struct SettingsGear;

#[async_trait::async_trait]
impl toolkit::Gear for SettingsGear {
    async fn init(&self, _ctx: &toolkit::GearCtx) -> anyhow::Result<()> {
        Ok(())
    }
}

#[toolkit::gear(
    name = "api-gateway",  // Valid kebab-case
    capabilities = []
)]
#[derive(Default)]
pub struct ApiGatewayGear;

#[async_trait::async_trait]
impl toolkit::Gear for ApiGatewayGear {
    async fn init(&self, _ctx: &toolkit::GearCtx) -> anyhow::Result<()> {
        Ok(())
    }
}

#[toolkit::gear(
    name = "gear-v2",  // Valid kebab-case with digit
    capabilities = []
)]
#[derive(Default)]
pub struct GearV2;

#[async_trait::async_trait]
impl toolkit::Gear for GearV2 {
    async fn init(&self, _ctx: &toolkit::GearCtx) -> anyhow::Result<()> {
        Ok(())
    }
}

#[toolkit::gear(
    name = "system",  // Valid single word (no hyphens needed)
    capabilities = []
)]
#[derive(Default)]
pub struct SystemGear;

#[async_trait::async_trait]
impl toolkit::Gear for SystemGear {
    async fn init(&self, _ctx: &toolkit::GearCtx) -> anyhow::Result<()> {
        Ok(())
    }
}

fn main() {}
