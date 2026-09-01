// Lifecycle with await_ready requires ReadySignal parameter
use toolkit_macros::gear;
use tokio_util::sync::CancellationToken;
use anyhow::Result;

#[derive(Default)]
#[gear(name = "demo-ready", capabilities = [stateful], lifecycle(entry = "serve", await_ready, stop_timeout = "1s"))]
pub struct DemoReady;

impl DemoReady {
    async fn serve(
        &self,
        _cancel: CancellationToken,
        _ready: toolkit::lifecycle::ReadySignal,
    ) -> Result<()> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl toolkit::Gear for DemoReady {
    async fn init(&self, _ctx: &toolkit::GearCtx) -> anyhow::Result<()> { Ok(()) }
}

fn main() {}
