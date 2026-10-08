use anyhow::{Result, ensure};
use serde::Deserialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use unibus::{
    ServiceDirectory, ServiceObservation, StaticServiceDirectory,
    client::{Client, ClientConfig},
};
use unibus_iroh::gateways;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    observations: Vec<ServiceObservation>,
    #[serde(default)]
    client: Option<ClientConfig>,
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.len() == 2 && matches!(args[0].as_str(), "list" | "publish"),
        "usage: unibus-iroh list CONFIG.json | publish CONFIG.json"
    );
    let path = std::path::Path::new(&args[1]);
    let mut config: Config = serde_json::from_slice(&std::fs::read(path)?)?;
    let directory: Box<dyn ServiceDirectory> =
        Box::new(StaticServiceDirectory::new(config.observations)?);
    if args[0] == "list" {
        println!(
            "{}",
            serde_json::to_string_pretty(&gateways(directory.as_ref(), now()?)?)?
        );
        return Ok(());
    }
    let mut client_config = config
        .client
        .take()
        .ok_or_else(|| anyhow::anyhow!("publisher requires client credentials"))?;
    client_config.resolve_token_file(path);
    let endpoint: std::net::SocketAddr = client_config.hub_endpoint.parse()?;
    ensure!(
        endpoint.ip().is_loopback() && client_config.schema_version == 2,
        "gateway publication requires schema v2 over a local authenticated bus or private tunnel"
    );
    client_config.read_timeout_ms = 100;
    let client = Client::connect(client_config)?;
    loop {
        let entries = gateways(directory.as_ref(), now()?)?;
        unibus_iroh::bus::publish(&client, &entries)?;
        std::thread::sleep(Duration::from_secs(15));
    }
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}
