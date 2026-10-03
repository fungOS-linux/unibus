//! Local Streamable-HTTP-shaped MCP bridge backed by Unibus.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use unibus::client::{Client, ClientConfig};

use crate::{
    AuthorizationGrant, Invocation, InvocationOutcome, MCP_SCHEMA_VERSION, PROVIDER_KIND,
    ProviderAdvertisement, ProviderRegistry, RESULT_KIND, ToolResult, decode_message,
};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeConfig {
    pub listen: String,
    #[serde(default = "default_timeout_ms")]
    pub invocation_timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    60_000
}

struct State {
    bus: Arc<Client>,
    registry: Mutex<ProviderRegistry>,
    pending: Mutex<BTreeMap<String, mpsc::Sender<ToolResult>>>,
    audit: Mutex<VecDeque<AuditEntry>>,
}

const MAX_AUDIT_ENTRIES: usize = 256;

#[derive(Debug, Clone, Serialize)]
struct AuditEntry {
    invocation_id: String,
    provider_id: Option<String>,
    tool_name: String,
    started_at_unix_ms: u64,
    duration_ms: u64,
    outcome: String,
}

pub fn serve(bus_config: ClientConfig, config: BridgeConfig) -> Result<()> {
    ensure!(
        bus_config
            .capabilities
            .iter()
            .any(|v| v == crate::BRIDGE_CAPABILITY),
        "bridge must declare mcp.bridge capability"
    );
    ensure!(
        (1_000..=300_000).contains(&config.invocation_timeout_ms),
        "invocation_timeout_ms must be 1000..=300000"
    );
    let listener = TcpListener::bind(&config.listen)
        .with_context(|| format!("bind MCP bridge at {}", config.listen))?;
    let state = Arc::new(State {
        bus: Arc::new(Client::connect(bus_config)?),
        registry: Mutex::new(ProviderRegistry::default()),
        pending: Mutex::new(BTreeMap::new()),
        audit: Mutex::new(VecDeque::with_capacity(MAX_AUDIT_ENTRIES)),
    });
    {
        let state = state.clone();
        std::thread::Builder::new()
            .name("unibus-mcp-reader".into())
            .spawn(move || {
                if let Err(error) = read_bus(&state) {
                    eprintln!("Unibus MCP reader stopped: {error:#}");
                    // Never leave a stale catalog serving after its
                    // authenticated transport dies. launchd restarts both.
                    std::process::exit(1);
                }
            })?;
    }
    eprintln!("unibus-mcp listening on http://{}/mcp", config.listen);
    for stream in listener.incoming() {
        let stream = stream?;
        let state = state.clone();
        let timeout = Duration::from_millis(config.invocation_timeout_ms);
        std::thread::spawn(move || {
            if let Err(error) = handle_http(stream, &state, timeout) {
                eprintln!("MCP request failed: {error:#}");
            }
        });
    }
    Ok(())
}

fn read_bus(state: &State) -> Result<()> {
    loop {
        if let Some((envelope, _)) = state.bus.try_read()? {
            match envelope.message.kind.as_str() {
                PROVIDER_KIND => {
                    let ad: ProviderAdvertisement =
                        decode_message(&envelope.message, PROVIDER_KIND)?;
                    state
                        .registry
                        .lock()
                        .unwrap()
                        .upsert(ad, unibus::envelope::now_ms())?;
                }
                RESULT_KIND => {
                    let result: ToolResult = decode_message(&envelope.message, RESULT_KIND)?;
                    result.validate()?;
                    if let Some(sender) =
                        state.pending.lock().unwrap().remove(&result.invocation_id)
                    {
                        let _ = sender.send(result);
                    }
                }
                _ => {}
            }
        }
    }
}

fn handle_http(mut stream: TcpStream, state: &State, timeout: Duration) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse()?;
        }
    }
    ensure!(
        length <= crate::MAX_ARGUMENT_BYTES,
        "HTTP request is too large"
    );
    if request_line.starts_with("GET /capabilities ") {
        return respond(&mut stream, 200, &capability_view(state));
    }
    if !request_line.starts_with("POST /mcp ") {
        return respond(&mut stream, 404, &serde_json::json!({"error":"not found"}));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let request: Value = serde_json::from_slice(&body)?;
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return rpc_error(&mut stream, id, -32600, "invalid request");
    };
    if request.get("id").is_none() {
        return respond_empty(&mut stream, 202);
    }
    let result = match method {
        "initialize" => serde_json::json!({
            "protocolVersion":"2025-06-18",
            "capabilities":{"tools":{"listChanged":true}},
            "serverInfo":{"name":"unibus-mcp","version":env!("CARGO_PKG_VERSION")}
        }),
        "tools/list" => list_tools(state),
        "tools/call" => match call_tool(
            state,
            request.get("params").cloned().unwrap_or_default(),
            timeout,
        ) {
            Ok(value) => value,
            Err(error) => return rpc_error(&mut stream, id, -32000, &format!("{error:#}")),
        },
        _ => return rpc_error(&mut stream, id, -32601, "method not found"),
    };
    respond(
        &mut stream,
        200,
        &serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
    )
}

fn list_tools(state: &State) -> Value {
    let now = unibus::envelope::now_ms();
    let registry = state.registry.lock().unwrap();
    let mut tools = Vec::new();
    for provider in registry.active(now) {
        for tool in &provider.tools {
            tools.push(serde_json::json!({
                "name": external_name(&provider.provider_id, &tool.name),
                "description": format!("[{}] {}", provider.provider_id, tool.description),
                "inputSchema": tool.input_schema,
                "annotations":{"readOnlyHint": tool.mutation == crate::MutationClass::ReadOnly}
            }));
        }
    }
    serde_json::json!({"tools":tools})
}

fn external_name(provider: &str, tool: &str) -> String {
    format!("{}__{}", provider.replace('-', "_"), tool)
}

fn call_tool(state: &State, params: Value, timeout: Duration) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("missing tool name")?
        .to_owned();
    let started_at = unibus::envelope::now_ms();
    let result = call_tool_inner(state, &name, params, timeout, started_at);
    let (provider_id, tool_name) = split_external_name(&name);
    let outcome = match &result {
        Ok(_) => "success",
        Err(error) if error.to_string().contains("not classified read-only") => "denied",
        Err(error) if error.to_string().contains("timed out") => "timeout",
        Err(_) => "error",
    };
    record_audit(
        state,
        AuditEntry {
            invocation_id: format!("audit-{started_at}-{}", next_sequence()),
            provider_id,
            tool_name,
            started_at_unix_ms: started_at,
            duration_ms: unibus::envelope::now_ms().saturating_sub(started_at),
            outcome: outcome.into(),
        },
    );
    result
}

fn call_tool_inner(
    state: &State,
    name: &str,
    params: Value,
    timeout: Duration,
    now: u64,
) -> Result<Value> {
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let registry = state.registry.lock().unwrap();
    let (provider, tool) = registry
        .active(now)
        .find_map(|provider| {
            provider
                .tools
                .iter()
                .find(|tool| external_name(&provider.provider_id, &tool.name) == name)
                .map(|tool| (provider, tool))
        })
        .context("unknown or unavailable tool")?;
    ensure!(
        tool.mutation == crate::MutationClass::ReadOnly,
        "tool is not classified read-only"
    );
    let invocation_id = format!("mcp-{}-{}", now, next_sequence());
    let invocation = Invocation {
        schema_version: MCP_SCHEMA_VERSION,
        invocation_id: invocation_id.clone(),
        provider_id: provider.provider_id.clone(),
        tool_name: tool.name.clone(),
        arguments,
        authorization: AuthorizationGrant {
            encoded: "unibus-local-read".into(),
        },
    };
    let destination = provider.producer_id.clone();
    drop(registry);
    let (sender, receiver) = mpsc::channel();
    state
        .pending
        .lock()
        .unwrap()
        .insert(invocation_id.clone(), sender);
    let envelope = state
        .bus
        .envelope(Some(&destination), invocation.into_message()?)?;
    if let Err(error) = state.bus.send(&envelope, &[]) {
        state.pending.lock().unwrap().remove(&invocation_id);
        return Err(error);
    }
    let result = match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(error) => {
            state.pending.lock().unwrap().remove(&invocation_id);
            return Err(error).context("provider invocation timed out");
        }
    };
    match result.outcome {
        InvocationOutcome::Success { result } => Ok(result),
        InvocationOutcome::Error { code, message, .. } => anyhow::bail!("{code}: {message}"),
        InvocationOutcome::Cancelled { reason } => anyhow::bail!("cancelled: {reason}"),
    }
}

fn split_external_name(name: &str) -> (Option<String>, String) {
    match name.split_once("__") {
        Some((provider, tool)) => (Some(provider.replace('_', "-")), tool.to_owned()),
        None => (None, name.to_owned()),
    }
}

fn record_audit(state: &State, entry: AuditEntry) {
    let mut audit = state.audit.lock().unwrap();
    if audit.len() == MAX_AUDIT_ENTRIES {
        audit.pop_front();
    }
    audit.push_back(entry);
}

fn capability_view(state: &State) -> Value {
    let now = unibus::envelope::now_ms();
    let registry = state.registry.lock().unwrap();
    let audit = state.audit.lock().unwrap();
    project_capabilities(&registry, &audit, now)
}

fn project_capabilities(
    registry: &ProviderRegistry,
    audit: &VecDeque<AuditEntry>,
    now: u64,
) -> Value {
    let providers = registry
        .all()
        .map(|provider| {
            let mut read_only = 0;
            let mut mutating = 0;
            let mut unknown = 0;
            for tool in &provider.tools {
                match tool.mutation {
                    crate::MutationClass::ReadOnly => read_only += 1,
                    crate::MutationClass::Mutating => mutating += 1,
                    crate::MutationClass::Unknown => unknown += 1,
                }
            }
            serde_json::json!({
                "protocol":"mcp",
                "provider_id":provider.provider_id,
                "node_id":provider.node_id,
                "producer_id":provider.producer_id,
                "lease_id":provider.lease_id,
                "lease_status":if provider.expires_at_unix_ms > now {"active"} else {"stale"},
                "issued_at_unix_ms":provider.issued_at_unix_ms,
                "expires_at_unix_ms":provider.expires_at_unix_ms,
                "catalog_digest":provider.catalog_digest,
                "counts":{"total":provider.tools.len(),"read_only":read_only,"mutating":mutating,"unknown":unknown}
            })
        })
        .collect::<Vec<_>>();
    let audit = audit.iter().cloned().collect::<Vec<_>>();
    serde_json::json!({
        "schema_version":1,
        "generated_at_unix_ms":now,
        "providers":providers,
        "recent_invocations":audit,
        "redaction":{"arguments":true,"results":true,"authorization":true,"credentials":true}
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MutationClass, ToolDescriptor};

    #[test]
    fn capability_projection_marks_stale_leases_and_redacts_call_data() {
        let mut registry = ProviderRegistry::default();
        registry
            .upsert(
                ProviderAdvertisement::new(
                    "studio-test",
                    "studio",
                    "studio-provider",
                    "lease-1",
                    1_000,
                    6_000,
                    vec![ToolDescriptor {
                        name: "inspect".into(),
                        description: "Inspect safely".into(),
                        input_schema: serde_json::json!({"type":"object"}),
                        output_schema: Some(serde_json::json!({"type":"object"})),
                        mutation: MutationClass::ReadOnly,
                    }],
                )
                .unwrap(),
                1_000,
            )
            .unwrap();
        let audit = VecDeque::from([AuditEntry {
            invocation_id: "audit-1".into(),
            provider_id: Some("studio-test".into()),
            tool_name: "inspect".into(),
            started_at_unix_ms: 2_000,
            duration_ms: 3,
            outcome: "success".into(),
        }]);

        let view = project_capabilities(&registry, &audit, 7_000);
        assert_eq!(view["providers"][0]["lease_status"], "stale");
        assert_eq!(view["providers"][0]["counts"]["read_only"], 1);
        assert_eq!(view["recent_invocations"][0]["outcome"], "success");
        let encoded = serde_json::to_string(&view).unwrap();
        for forbidden in ["arguments", "results", "authorization", "credentials"] {
            assert_eq!(view["redaction"][forbidden], true);
        }
        assert!(!encoded.contains("secret-value"));
    }
}

fn next_sequence() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(1);
    SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

fn rpc_error(stream: &mut TcpStream, id: Value, code: i64, message: &str) -> Result<()> {
    respond(
        stream,
        200,
        &serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}),
    )
}

fn respond(stream: &mut TcpStream, status: u16, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    write!(
        stream,
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nMcp-Session-Id: unibus-local\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    Ok(())
}

fn respond_empty(stream: &mut TcpStream, status: u16) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )?;
    Ok(())
}
