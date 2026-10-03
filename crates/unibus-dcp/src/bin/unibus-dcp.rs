use std::{path::Path, time::Duration};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use unibus::client::{Client, ClientConfig};
use unibus_dcp::{
    BINDING_SCHEMA_VERSION, CatalogAdvertisement, EXECUTE_KIND, Execution, ExecutionReceipt,
    MAX_PROVIDER_LEASE_MS, PROVIDER_CAPABILITY, ProviderRegistry, RECEIPT_KIND, decode_message,
    http::{DcpHttpClient, ProviderConfig},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    unibus: ClientConfig,
    #[serde(default = "default_lease_ms")]
    lease_ms: u64,
    providers: Vec<ProviderConfig>,
}

fn default_lease_ms() -> u64 {
    30_000
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args
        .next()
        .context("usage: unibus-dcp inspect|publish <config.json>")?;
    ensure!(
        matches!(command.as_str(), "inspect" | "publish"),
        "command must be `inspect` or `publish`"
    );
    let path = args
        .next()
        .with_context(|| format!("{command} needs <config.json>"))?;
    ensure!(args.next().is_none(), "unexpected argument");
    let config: Config = serde_json::from_slice(&std::fs::read(Path::new(&path))?)?;
    validate_config(&config)?;
    if command == "inspect" {
        println!("{}", serde_json::to_string_pretty(&inspect(&config, 0)?)?);
        return Ok(());
    }
    publish(config)
}

fn validate_config(config: &Config) -> Result<()> {
    ensure!(!config.providers.is_empty(), "no DCP providers configured");
    ensure!(
        (5_000..=MAX_PROVIDER_LEASE_MS).contains(&config.lease_ms),
        "invalid lease_ms"
    );
    ensure!(
        config
            .unibus
            .capabilities
            .iter()
            .any(|capability| capability == PROVIDER_CAPABILITY),
        "publisher must declare dcp.provider capability"
    );
    Ok(())
}

fn inspect(config: &Config, generation: u64) -> Result<Vec<CatalogAdvertisement>> {
    let now = unibus::envelope::now_ms();
    config
        .providers
        .iter()
        .map(|provider| {
            let client = DcpHttpClient::connect(provider)?;
            let discovery = client.discovery()?;
            let catalog = client.catalog()?;
            ensure!(
                discovery.provider.id == provider.provider_id,
                "configured provider_id differs from discovery"
            );
            let advertisement = CatalogAdvertisement {
                binding_schema_version: BINDING_SCHEMA_VERSION,
                node_id: provider.node_id.clone(),
                producer_id: provider.producer_id.clone(),
                lease_id: format!("{}-{generation}", provider.provider_id),
                issued_at_unix_ms: now,
                expires_at_unix_ms: now + config.lease_ms,
                discovery,
                catalog,
            };
            advertisement.validate(now)?;
            Ok(advertisement)
        })
        .collect()
}

fn publish(config: Config) -> Result<()> {
    let bus = Client::connect(config.unibus.clone())?;
    let refresh = Duration::from_millis(config.lease_ms / 2);
    let mut registry = ProviderRegistry::default();
    let clients = config
        .providers
        .iter()
        .map(|provider| {
            Ok((
                provider.provider_id.clone(),
                DcpHttpClient::connect(provider)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut generation = 0_u64;
    let mut next_refresh = std::time::Instant::now();
    loop {
        if std::time::Instant::now() >= next_refresh {
            let now = unibus::envelope::now_ms();
            for advertisement in inspect(&config, generation)? {
                registry.upsert(advertisement.clone(), now)?;
                bus.send(&bus.envelope(None, advertisement.into_message()?)?, &[])?;
            }
            generation = generation.wrapping_add(1);
            next_refresh = std::time::Instant::now() + refresh;
        }
        if let Some((envelope, _)) = bus.try_read()? {
            if envelope.message.kind == EXECUTE_KIND {
                let execution: Execution = decode_message(&envelope.message, EXECUTE_KIND)?;
                registry.resolve_execution(&execution, unibus::envelope::now_ms())?;
                let (_, client) = clients
                    .iter()
                    .find(|(provider_id, _)| provider_id == &execution.provider_id)
                    .context("DCP provider is not configured")?;
                let receipt = client.execute(&execution.request)?;
                let result = ExecutionReceipt {
                    binding_schema_version: BINDING_SCHEMA_VERSION,
                    provider_id: execution.provider_id,
                    receipt,
                };
                result.validate()?;
                bus.send(
                    &bus.envelope(
                        Some(&envelope.producer_id),
                        unibus::Message::new(RECEIPT_KIND, serde_json::to_value(result)?),
                    )?,
                    &[],
                )?;
            }
        }
        ensure!(bus.is_healthy(), "Unibus connection is unhealthy");
        std::thread::sleep(Duration::from_millis(25));
    }
}
