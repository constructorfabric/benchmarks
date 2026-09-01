// Minimal stateful gear with lifecycle (no ready)
use toolkit_macros::gear;
use tokio_util::sync::CancellationToken;
use anyhow::Result;

#[derive(Default)]
#[gear(name = "demo", capabilities = [stateful], lifecycle(entry = "serve", stop_timeout = "1s"))]
pub struct Demo;

impl Demo {
    async fn serve(&self, _cancel: CancellationToken) -> Result<()> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl toolkit::Gear for Demo {
    async fn init(&self, _ctx: &toolkit::GearCtx) -> anyhow::Result<()> {
        Ok(())
    }
}

fn main() {}
