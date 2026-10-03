//! Minimal MCP Streamable HTTP client used by provider edges.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{MutationClass, ToolDescriptor};

const PROTOCOL_VERSION: &str = "2025-06-18";
const SESSION_HEADER: &str = "mcp-session-id";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub provider_id: String,
    pub node_id: String,
    pub producer_id: String,
    #[serde(default = "default_transport")]
    pub transport: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub origin_chain: Vec<String>,
    #[serde(default)]
    pub aggregate: bool,
    #[serde(default = "default_true")]
    pub reexport_to_unibus: bool,
    #[serde(default)]
    pub mutation_overrides: BTreeMap<String, MutationClass>,
}

fn default_transport() -> String {
    "streamable_http".into()
}
fn default_true() -> bool {
    true
}

pub enum McpClient {
    Http(McpHttpClient),
    Stdio(McpStdioClient),
}

impl McpClient {
    pub fn connect(config: &ProviderConfig) -> Result<Self> {
        ensure!(
            !config.aggregate,
            "aggregate MCP connectors cannot be exported to Unibus"
        );
        ensure!(
            config.reexport_to_unibus,
            "connector policy forbids export to Unibus"
        );
        ensure!(
            !config.origin_chain.iter().any(|origin| origin == "unibus"),
            "connector already originated from Unibus"
        );
        match config.transport.as_str() {
            "streamable_http" => Ok(Self::Http(McpHttpClient::connect(&config.endpoint)?)),
            "stdio" => Ok(Self::Stdio(McpStdioClient::connect(config)?)),
            other => anyhow::bail!("unsupported MCP transport {other}"),
        }
    }

    pub fn list_tools(
        &mut self,
        overrides: &BTreeMap<String, MutationClass>,
    ) -> Result<Vec<ToolDescriptor>> {
        match self {
            Self::Http(client) => client.list_tools(overrides),
            Self::Stdio(client) => client.list_tools(overrides),
        }
    }

    pub fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        match self {
            Self::Http(client) => client.call_tool(name, arguments),
            Self::Stdio(client) => client.call_tool(name, arguments),
        }
    }
}

pub struct McpStdioClient {
    child: Child,
    input: ChildStdin,
    output: mpsc::Receiver<Result<Value>>,
    next_id: u64,
}

impl McpStdioClient {
    fn connect(config: &ProviderConfig) -> Result<Self> {
        let command = PathBuf::from(&config.command);
        ensure!(command.is_absolute(), "stdio MCP command must be absolute");
        ensure!(config.args.len() <= 128, "too many stdio MCP arguments");
        let mut process = Command::new(command);
        process
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(directory) = &config.working_dir {
            process.current_dir(directory);
        }
        let mut child = process.spawn().context("spawn stdio MCP provider")?;
        let input = child.stdin.take().context("stdio MCP stdin unavailable")?;
        let output = spawn_reader(
            child
                .stdout
                .take()
                .context("stdio MCP stdout unavailable")?,
        )?;
        let mut client = Self {
            child,
            input,
            output,
            next_id: 1,
        };
        client.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name":"unibus-mcp", "version":env!("CARGO_PKG_VERSION")}
            }),
        )?;
        client.notify("notifications/initialized", serde_json::json!({}))?;
        Ok(client)
    }

    fn list_tools(
        &mut self,
        overrides: &BTreeMap<String, MutationClass>,
    ) -> Result<Vec<ToolDescriptor>> {
        let response = self.request("tools/list", serde_json::json!({}))?;
        let tools = response
            .pointer("/result/tools")
            .and_then(Value::as_array)
            .context("MCP tools/list returned no tools")?;
        let mut descriptors = tools
            .iter()
            .map(|tool| descriptor(tool, overrides))
            .collect::<Result<Vec<_>>>()?;
        descriptors.sort_by(|left, right| left.name.cmp(&right.name));
        for tool in &descriptors {
            tool.validate()?;
        }
        Ok(descriptors)
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        ensure!(
            arguments.is_object(),
            "MCP tool arguments must be an object"
        );
        self.request(
            "tools/call",
            serde_json::json!({"name":name,"arguments":arguments}),
        )?
        .get("result")
        .cloned()
        .context("MCP tools/call returned no result")
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        loop {
            let timeout = if method == "tools/list" {
                Duration::from_secs(10)
            } else {
                Duration::from_secs(60)
            };
            let response = self.receive(timeout)?;
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = response.get("error") {
                anyhow::bail!("MCP {method} failed: {error}");
            }
            return Ok(response);
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(&serde_json::json!({"jsonrpc":"2.0","method":method,"params":params}))
    }

    fn send(&mut self, value: &Value) -> Result<()> {
        serde_json::to_writer(&mut self.input, value)?;
        self.input.write_all(b"\n")?;
        self.input.flush()?;
        Ok(())
    }

    fn receive(&mut self, timeout: Duration) -> Result<Value> {
        self.output
            .recv_timeout(timeout)
            .context("stdio MCP response timed out")?
    }
}

fn spawn_reader(stdout: ChildStdout) -> Result<mpsc::Receiver<Result<Value>>> {
    let (sender, receiver) = mpsc::sync_channel(32);
    std::thread::Builder::new()
        .name("unibus-mcp-stdio-reader".into())
        .spawn(move || {
            let mut output = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                let result = (|| -> Result<Value> {
                    let read = output
                        .by_ref()
                        .take((crate::MAX_RESULT_BYTES + 1) as u64)
                        .read_line(&mut line)?;
                    ensure!(read > 0, "stdio MCP provider closed output");
                    ensure!(
                        read <= crate::MAX_RESULT_BYTES,
                        "stdio MCP response is too large"
                    );
                    Ok(serde_json::from_str(&line)?)
                })();
                let stop = result.is_err();
                if sender.send(result).is_err() || stop {
                    break;
                }
            }
        })?;
    Ok(receiver)
}

impl Drop for McpStdioClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct McpHttpClient {
    endpoint: String,
    client: Client,
    session_id: Option<HeaderValue>,
    next_id: u64,
}

impl McpHttpClient {
    pub fn connect(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        ensure!(
            endpoint.starts_with("http://127.0.0.1:") || endpoint.starts_with("http://[::1]:"),
            "provider endpoint must be loopback HTTP"
        );
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(30))
            .build()?;
        let mut mcp = Self {
            endpoint,
            client,
            session_id: None,
            next_id: 1,
        };
        let response = mcp.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name":"unibus-mcp", "version":env!("CARGO_PKG_VERSION")}
            }),
        )?;
        ensure!(
            response.get("result").is_some(),
            "MCP initialize returned no result"
        );
        mcp.notify("notifications/initialized", serde_json::json!({}))?;
        Ok(mcp)
    }

    pub fn list_tools(
        &mut self,
        overrides: &BTreeMap<String, MutationClass>,
    ) -> Result<Vec<ToolDescriptor>> {
        let response = self.request("tools/list", serde_json::json!({}))?;
        let tools = response
            .pointer("/result/tools")
            .and_then(Value::as_array)
            .context("MCP tools/list returned no tools")?;
        let mut descriptors = tools
            .iter()
            .map(|tool| descriptor(tool, overrides))
            .collect::<Result<Vec<_>>>()?;
        descriptors.sort_by(|left, right| left.name.cmp(&right.name));
        for tool in &descriptors {
            tool.validate()?;
        }
        Ok(descriptors)
    }

    pub fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        ensure!(
            arguments.is_object(),
            "MCP tool arguments must be an object"
        );
        let response = self.request(
            "tools/call",
            serde_json::json!({"name": name, "arguments": arguments}),
        )?;
        response
            .get("result")
            .cloned()
            .context("MCP tools/call returned no result")
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let response = self.post(serde_json::json!({
            "jsonrpc":"2.0", "id":id, "method":method, "params":params
        }))?;
        let value = decode_response(response)?;
        ensure!(
            value.get("id").and_then(Value::as_u64) == Some(id),
            "MCP response id mismatch"
        );
        if let Some(error) = value.get("error") {
            anyhow::bail!("MCP {method} failed: {error}");
        }
        Ok(value)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let response = self.post(serde_json::json!({
            "jsonrpc":"2.0", "method":method, "params":params
        }))?;
        ensure!(
            response.status().is_success(),
            "MCP notification failed: {}",
            response.status()
        );
        Ok(())
    }

    fn post(&mut self, body: Value) -> Result<Response> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        if let Some(session) = &self.session_id {
            headers.insert(SESSION_HEADER, session.clone());
        }
        let response = self
            .client
            .post(&self.endpoint)
            .headers(headers)
            .json(&body)
            .send()?;
        if self.session_id.is_none() {
            self.session_id = response.headers().get(SESSION_HEADER).cloned();
        }
        ensure!(
            response.status().is_success(),
            "MCP HTTP status {}",
            response.status()
        );
        Ok(response)
    }
}

fn decode_response(response: Response) -> Result<Value> {
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let body = response.text()?;
    if content_type.starts_with("text/event-stream") {
        let data = body
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .context("SSE response has no data event")?;
        Ok(serde_json::from_str(data)?)
    } else {
        Ok(serde_json::from_str(&body)?)
    }
}

fn descriptor(tool: &Value, overrides: &BTreeMap<String, MutationClass>) -> Result<ToolDescriptor> {
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .context("MCP tool has no name")?
        .to_owned();
    let annotations = tool.get("annotations").and_then(Value::as_object);
    let inferred = match annotations {
        Some(values) if values.get("readOnlyHint").and_then(Value::as_bool) == Some(true) => {
            MutationClass::ReadOnly
        }
        Some(values) if values.get("destructiveHint").and_then(Value::as_bool) == Some(true) => {
            MutationClass::Mutating
        }
        _ => MutationClass::Unknown,
    };
    Ok(ToolDescriptor {
        description: tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        input_schema: tool
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type":"object"})),
        output_schema: tool.get("outputSchema").cloned(),
        mutation: overrides.get(&name).copied().unwrap_or(inferred),
        name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotations_infer_policy_and_unknown_fails_closed() {
        let read = descriptor(&serde_json::json!({"name":"read","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}}), &BTreeMap::new()).unwrap();
        let unknown = descriptor(
            &serde_json::json!({"name":"ambiguous","inputSchema":{"type":"object"}}),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(read.mutation, MutationClass::ReadOnly);
        assert_eq!(unknown.mutation, MutationClass::Unknown);
        assert!(unknown.mutation.requires_write_grant());
    }

    #[test]
    fn explicit_override_only_resolves_owner_ambiguity() {
        let mut overrides = BTreeMap::new();
        overrides.insert("save".into(), MutationClass::Mutating);
        let tool = descriptor(
            &serde_json::json!({"name":"save","inputSchema":{"type":"object"}}),
            &overrides,
        )
        .unwrap();
        assert_eq!(tool.mutation, MutationClass::Mutating);
    }

    fn stdio_config() -> ProviderConfig {
        ProviderConfig {
            provider_id: "desktop-dock".into(),
            node_id: "neo".into(),
            producer_id: "neo-mcp-provider".into(),
            transport: "stdio".into(),
            endpoint: String::new(),
            command: "/does/not/matter".into(),
            args: vec![],
            working_dir: None,
            origin_chain: vec!["fpl-desktop-bridge".into()],
            aggregate: false,
            reexport_to_unibus: true,
            mutation_overrides: BTreeMap::new(),
        }
    }

    #[test]
    fn aggregate_and_unibus_origin_connectors_cannot_loop_back() {
        let error = |config: &ProviderConfig| match McpClient::connect(config) {
            Ok(_) => panic!("connector unexpectedly accepted"),
            Err(error) => error.to_string(),
        };
        let mut aggregate = stdio_config();
        aggregate.aggregate = true;
        assert!(error(&aggregate).contains("aggregate"));

        let mut looped = stdio_config();
        looped.origin_chain.push("unibus".into());
        assert!(error(&looped).contains("originated"));

        let mut denied = stdio_config();
        denied.reexport_to_unibus = false;
        assert!(error(&denied).contains("forbids"));
    }
}
