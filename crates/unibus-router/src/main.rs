//! unibus hub binary. Usage: `unibus-hub <config.json>`

use std::sync::Arc;

use anyhow::Result;
use unibus_router::{Hub, config::HubConfig};

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
    Arc::new(Hub::new(config)?).serve()
}
