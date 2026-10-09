//! unibus hub binary. Usage: `unibus-hub <config.json>`

use std::sync::Arc;

use anyhow::Result;
use unibus_router::{Hub, StreamRouter, config::HubConfig};

fn main() -> Result<()> {
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("--version" | "_self-check")
    ) {
        println!(
            "unibus-router {} schema={}",
            env!("CARGO_PKG_VERSION"),
            unibus::SCHEMA_VERSION_V2
        );
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();
    let path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: unibus-hub <config.json>"))?;
    let config = HubConfig::read(std::path::Path::new(&path))?;
    let mut hub = Hub::new(config)?;
    if let Some(path) = std::env::var_os("UNIBUS_GATEWAY_CONFIG") {
        let file = std::fs::File::open(path)?;
        anyhow::ensure!(file.metadata()?.len() <= 1024 * 1024, "gateway config exceeds bound");
        let gateway = serde_json::from_reader(file)?;
        hub = hub.with_plugin(Arc::new(unibus_router::gateway::GatewayRouter::new(
            gateway, unibus::envelope::now_ms() / 1000,
        )?));
    }
    Arc::new(hub.with_plugin(Arc::new(StreamRouter::default()))).serve()
}
