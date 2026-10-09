use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::json;
use unibus::{
    Message,
    client::{Client, ClientConfig},
    gateway::{
        GATEWAY_INVOKE, GATEWAY_REGISTER, GATEWAY_RESULT, GatewayDispatch, GatewayLease,
        GatewayRegistration, GatewayResult,
    },
};
use unibus_canvas::{CanvasAdapter, INSPECT, SWITCH};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    unibus: ClientConfig,
    adapter: CanvasAdapter,
    lease: GatewayLease,
}
#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: unibus-canvas CONFIG.json"))?;
    let file = std::fs::File::open(&path)?;
    ensure!(
        file.metadata()?.len() <= 65536,
        "Canvas adapter config exceeds bound"
    );
    let mut config: Config = serde_json::from_reader(file)?;
    config
        .unibus
        .resolve_token_file(std::path::Path::new(&path));
    config.adapter.validate()?;
    config
        .lease
        .validate_at(unibus::envelope::now_ms() / 1000)?;
    ensure!(
        config.lease.resource == config.adapter.resource,
        "adapter resource differs from lease"
    );
    ensure!(
        config
            .lease
            .operations
            .iter()
            .all(|op| matches!(op.as_str(), INSPECT | SWITCH)),
        "unsupported advertised Canvas operation"
    );
    let addr: std::net::SocketAddr = config.unibus.hub_endpoint.parse()?;
    ensure!(
        addr.ip().is_loopback() && config.unibus.schema_version == 2,
        "Canvas adapter requires an authenticated v2 owner-local router session"
    );
    config.unibus.read_timeout_ms = 500;
    let client = Client::connect(config.unibus)?;
    let registration = client.envelope(
        None,
        Message::new(
            GATEWAY_REGISTER,
            serde_json::to_value(GatewayRegistration {
                lease: config.lease.clone(),
            })?,
        ),
    )?;
    client.send(&registration, &[])?;
    let mut registered = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        if let Some((reply, payload)) = client.try_read()? {
            ensure!(
                payload.is_empty() && reply.message.kind == "reply",
                "unexpected registration reply"
            );
            ensure!(
                reply.message.body.get("success") == Some(&json!(true)),
                "gateway registration denied"
            );
            registered = true;
            break;
        }
    }
    ensure!(registered, "gateway registration deadline exceeded");
    while unibus::envelope::now_ms() / 1000 < config.lease.expires_at_unix_s {
        let Some((envelope, payload)) = client.try_read()? else {
            continue;
        };
        if envelope.message.kind == "reply" {
            ensure!(
                envelope.message.body.get("success") == Some(&json!(true)),
                "gateway result rejected"
            );
            continue;
        }
        ensure!(
            envelope.message.kind == GATEWAY_INVOKE && payload.is_empty(),
            "unexpected Canvas gateway message"
        );
        envelope.validate(&client.config().installation_id, unibus::envelope::now_ms())?;
        let dispatch: GatewayDispatch =
            serde_json::from_value(serde_json::Value::Object(envelope.message.body))?;
        let invocation = dispatch.invocation;
        config
            .lease
            .validate_at(unibus::envelope::now_ms() / 1000)?;
        ensure!(
            dispatch.caller_producer == envelope.producer_id,
            "caller provenance differs from dispatch"
        );
        ensure!(
            invocation.generation == config.lease.generation
                && config.lease.operations.contains(&invocation.operation),
            "invocation differs from registered lease"
        );
        let output = match config.adapter.execute(&invocation).await {
            Ok(reply) => json!({"success":reply["outcome"]["status"] == "ok","reply":reply}),
            Err(error) => json!({"success":false,"error":error.to_string()}),
        };
        let result = GatewayResult {
            caller_producer: dispatch.caller_producer,
            request_id: invocation.request_id,
            resource: invocation.resource,
            generation: invocation.generation,
            output,
        };
        client.send(
            &client.envelope(
                None,
                Message::new(GATEWAY_RESULT, serde_json::to_value(result)?),
            )?,
            &[],
        )?;
    }
    anyhow::bail!("Canvas adapter lease expired; fresh operator policy and lease required")
}
