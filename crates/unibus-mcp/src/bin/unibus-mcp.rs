use std::path::Path;
use std::time::Duration;

mod provider_worker;
use provider_worker::{Call, Event, Worker};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use unibus::client::{Client, ClientConfig};
use unibus_mcp::http::{McpClient, ProviderConfig};
use unibus_mcp::server::{BridgeConfig, serve};
use unibus_mcp::{
    AuthorizationGrant, AuthorizationVerifier, Claim, INVOKE_KIND, Invocation, InvocationLedger,
    InvocationOutcome, MCP_SCHEMA_VERSION, MutationClass, ProviderAdvertisement, ProviderRegistry,
    RESULT_KIND, ToolDescriptor, ToolResult, catalog_digest, decode_message,
    require_tool_authorization,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    unibus: Option<ClientConfig>,
    #[serde(default = "default_lease_ms")]
    lease_ms: u64,
    #[serde(default)]
    providers: Vec<ProviderConfig>,
    #[serde(default)]
    bridge: Option<BridgeConfig>,
}

fn default_lease_ms() -> u64 {
    30_000
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args
        .next()
        .context("usage: unibus-mcp inspect|publish|serve <config.json>")?;
    ensure!(
        matches!(command.as_str(), "inspect" | "publish" | "serve"),
        "command must be `inspect`, `publish`, or `serve`"
    );
    let path = args
        .next()
        .with_context(|| format!("{command} needs <config.json>"))?;
    ensure!(args.next().is_none(), "unexpected argument");
    let config: Config = serde_json::from_slice(
        &std::fs::read(Path::new(&path)).with_context(|| format!("read {path}"))?,
    )?;
    ensure!(
        (5_000..=unibus_mcp::MAX_PROVIDER_LEASE_MS).contains(&config.lease_ms),
        "lease_ms must be 5000..={} ms",
        unibus_mcp::MAX_PROVIDER_LEASE_MS
    );
    match command.as_str() {
        "publish" => {
            ensure!(!config.providers.is_empty(), "no MCP providers configured");
            publish(config)
        }
        "serve" => serve(
            config
                .unibus
                .context("serve requires `unibus` client configuration")?,
            config
                .bridge
                .context("serve requires `bridge` configuration")?,
        ),
        _ => {
            ensure!(!config.providers.is_empty(), "no MCP providers configured");
            println!("{}", serde_json::to_string_pretty(&inspect(&config)?)?);
            Ok(())
        }
    }
}

fn inspect(config: &Config) -> Result<Vec<ProviderAdvertisement>> {
    let now = unibus::envelope::now_ms();
    let mut output = Vec::new();
    for provider in &config.providers {
        let mut client = McpClient::connect(provider)
            .with_context(|| format!("initialize {}", provider.provider_id))?;
        let tools = client
            .list_tools(&provider.mutation_overrides)
            .with_context(|| format!("list tools for {}", provider.provider_id))?;
        let digest = catalog_digest(&tools)?;
        let advertisement = ProviderAdvertisement::new(
            provider.provider_id.clone(),
            provider.node_id.clone(),
            provider.producer_id.clone(),
            format!("inspect-{now}"),
            now,
            now + config.lease_ms,
            tools,
        )?;
        debug_assert_eq!(digest, advertisement.catalog_digest);
        output.push(advertisement);
    }
    Ok(output)
}

fn publish(config: Config) -> Result<()> {
    let unibus = config
        .unibus
        .clone()
        .context("publish requires `unibus` client configuration")?;
    ensure!(
        unibus
            .capabilities
            .iter()
            .any(|value| value == unibus_mcp::PROVIDER_CAPABILITY),
        "publisher must declare mcp.provider capability"
    );
    let bus = Client::connect(unibus)?;
    let verifier = std::sync::Arc::new(LocalReadOnlyVerifier);
    let refresh = Duration::from_millis(config.lease_ms / 2);
    let (events, received) = std::sync::mpsc::sync_channel(128);
    let mut providers = config
        .providers
        .iter()
        .enumerate()
        .map(|(index, provider)| {
            let verifier = verifier.clone();
            let authorize: provider_worker::Authorizer = std::sync::Arc::new(move |call, tool| {
                let now = unibus::envelope::now_ms();

                verifier
                    .verify(
                        &call.invocation.authorization,
                        &call.invocation.provider_id,
                        tool,
                        now,
                    )
                    .map(|_| ())
            });
            Worker::start(index, provider.clone(), refresh, events.clone(), authorize)
        })
        .collect::<Result<Vec<_>>>()?;
    drop(events);
    let mut registry = ProviderRegistry::default();
    let mut ledger = InvocationLedger::new(1024)?;
    let mut generation = 0_u64;
    loop {
        for _ in 0..64 {
            let event = match received.try_recv() {
                Ok(event) => event,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    anyhow::bail!("all MCP provider workers stopped")
                }
            };
            match event {
                Event::Catalog {
                    index,
                    tools,
                    observed_at,
                } => {
                    let worker = &mut providers[index];
                    let provider = &worker.config;
                    let now = unibus::envelope::now_ms();
                    // Never renew a delayed observation as if it were fresh.
                    if observed_at.saturating_add(config.lease_ms) <= now {
                        continue;
                    }
                    let advertisement = ProviderAdvertisement::new(
                        provider.provider_id.clone(),
                        provider.node_id.clone(),
                        provider.producer_id.clone(),
                        format!("{}-{generation}", provider.provider_id),
                        observed_at,
                        observed_at.saturating_add(config.lease_ms),
                        tools,
                    )?;
                    registry.upsert(advertisement.clone(), now)?;
                    let envelope = bus.envelope(None, advertisement.into_message()?)?;
                    bus.send(&envelope, &[])?;
                    if !worker.ready {
                        eprintln!("MCP provider {} ready", provider.provider_id);
                    }
                    worker.ready = true;
                    generation = generation.wrapping_add(1);
                }
                Event::Failed {
                    index,
                    error,
                    retry,
                } => {
                    providers[index].ready = false;
                    eprintln!(
                        "MCP provider {} unavailable: {}; retry in {}s",
                        providers[index].config.provider_id,
                        error,
                        retry.as_secs()
                    );
                }
                Event::Completed {
                    invocation,
                    producer,
                    result,
                } => {
                    ledger.complete(&invocation.invocation_id);
                    send_result(&bus, &invocation, &producer, result)?;
                }
            }
        }
        if let Some((envelope, _)) = bus.try_read()? {
            if envelope.message.kind == INVOKE_KIND {
                let invocation: Invocation = decode_message(&envelope.message, INVOKE_KIND)?;
                let result = execute_invocation(
                    &mut providers,
                    &registry,
                    &*verifier,
                    &mut ledger,
                    &invocation,
                    &envelope.producer_id,
                );
                if let Err(error) = result {
                    send_result(&bus, &invocation, &envelope.producer_id, Err(error))?;
                }
            }
        }
        ensure!(bus.is_healthy(), "Unibus connection is unhealthy");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn send_result(
    bus: &Client,
    invocation: &Invocation,
    producer: &str,
    result: Result<serde_json::Value>,
) -> Result<()> {
    let reply = ToolResult {
        schema_version: MCP_SCHEMA_VERSION,
        invocation_id: invocation.invocation_id.clone(),
        outcome: match result {
            Ok(result) => InvocationOutcome::Success { result },
            Err(error) => InvocationOutcome::Error {
                code: "invocation_failed".into(),
                message: format!("{error:#}"),
                data: None,
            },
        },
    };
    reply.validate()?;
    let message = unibus::Message::new(RESULT_KIND, serde_json::to_value(reply)?);
    bus.send(&bus.envelope(Some(producer), message)?, &[])?;
    Ok(())
}

struct LocalReadOnlyVerifier;

impl AuthorizationVerifier for LocalReadOnlyVerifier {
    type Claims = ();
    fn verify(
        &self,
        grant: &AuthorizationGrant,
        _provider: &str,
        tool: &ToolDescriptor,
        _now: u64,
    ) -> Result<()> {
        ensure!(
            grant.encoded == "unibus-local-read",
            "untrusted authorization grant"
        );
        ensure!(
            tool.mutation == MutationClass::ReadOnly,
            "tool is not classified read-only"
        );
        Ok(())
    }
}

fn execute_invocation(
    providers: &mut [Worker],
    registry: &ProviderRegistry,
    verifier: &LocalReadOnlyVerifier,
    ledger: &mut InvocationLedger,
    invocation: &Invocation,
    authenticated_producer: &str,
) -> Result<()> {
    require_tool_authorization(verifier, registry, invocation, unibus::envelope::now_ms())?;
    let worker = providers
        .iter()
        .find(|worker| worker.config.provider_id == invocation.provider_id)
        .context("provider is not configured")?;
    ensure!(worker.ready, "provider is unavailable");
    ensure!(
        ledger.claim(invocation)? == Claim::New,
        "duplicate invocation"
    );
    let result = worker.submit(Call {
        invocation: invocation.clone(),
        producer: authenticated_producer.to_owned(),
    });
    if result.is_err() {
        ledger.complete(&invocation.invocation_id);
    }
    result
}
