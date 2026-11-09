//! Self-hosted room service: a Unibus hub with the room plugin installed.
//! Usage: `unibus-room <router.json>`

use std::{path::Path, sync::Arc};

use anyhow::Result;
use unibus_room::plugin::RoomPlugin;
use unibus_router::{Hub, config::HubConfig};

fn main() -> Result<()> {
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("--version" | "_self-check")
    ) {
        println!(
            "unibus-room {} schema={}",
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
        .ok_or_else(|| anyhow::anyhow!("usage: unibus-room <router.json>"))?;
    let config = HubConfig::read(Path::new(&path))?;
    let hub = Arc::new(Hub::new(config)?.with_plugin(Arc::new(RoomPlugin::new())));
    hub.serve()
}
