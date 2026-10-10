//! Local orchestration; no leader, global state, renderer or discovery scanner.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::{Duration, Instant},
};
use unibus::{
    Message,
    client::{Client, ClientConfig},
    gateway::{GATEWAY_INVOKE, GATEWAY_RESULT, GatewayInvoke, GatewayLease, GatewayResult},
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub targets: BTreeMap<String, Driver>,
    #[serde(default)]
    pub groups: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub unibus: Option<ClientConfig>,
}
#[derive(Clone, Deserialize)]
#[serde(tag = "driver", rename_all = "snake_case", deny_unknown_fields)]
pub enum Driver {
    Command {
        program: PathBuf,
        #[serde(default)]
        args: Vec<String>,
    },
    Gateway {
        lease: Box<GatewayLease>,
        result_producer: String,
    },
}
#[derive(Clone)]
pub enum Action {
    Inspect,
    Switch {
        name: String,
        revisions: BTreeMap<String, u64>,
    },
}
impl Action {
    fn command(&self, _target: &str) -> Result<Value> {
        Ok(match self {
            Self::Inspect => json!({"command":"inspect"}),
            Self::Switch { name, .. } => {
                ensure!(
                    !name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control),
                    "invalid workspace name"
                );
                json!({"command":"workspace_switch","name":name})
            }
        })
    }
    fn revision(&self, target: &str) -> Result<Option<u64>> {
        match self {
            Self::Inspect => Ok(None),
            Self::Switch { revisions, .. } => {
                Ok(Some(*revisions.get(target).ok_or_else(|| {
                    anyhow::anyhow!("missing target revision")
                })?))
            }
        }
    }
}
#[derive(Serialize)]
pub struct Receipt {
    pub target: String,
    pub ok: bool,
    pub reply: Option<Value>,
    pub error: Option<String>,
}

/// Read-only projection of an owner inspection. Missing fields remain unknown.
#[derive(Debug, Serialize)]
pub struct EndpointStatus {
    pub endpoint: String,
    pub ok: bool,
    pub workspace: Option<String>,
    pub revision: Option<u64>,
    pub windows: Option<Vec<Value>>,
    pub error: Option<String>,
}

pub fn endpoint_status(receipt: &Receipt) -> EndpointStatus {
    let snapshot = receipt.reply.as_ref().and_then(|reply| {
        reply
            .pointer("/outcome/receipt/snapshot")
            .or_else(|| reply.get("data"))
    });
    EndpointStatus {
        endpoint: receipt.target.clone(),
        ok: receipt.ok,
        workspace: snapshot
            .and_then(|s| s.get("workspace_id"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        revision: receipt
            .reply
            .as_ref()
            .and_then(|r| r.get("revision"))
            .and_then(Value::as_u64),
        windows: snapshot
            .and_then(|s| s.get("surfaces"))
            .and_then(Value::as_array)
            .cloned(),
        error: receipt.error.clone(),
    }
}

pub fn format_status(status: &EndpointStatus) -> String {
    if !status.ok {
        return format!(
            "{}: ERROR — {}",
            status.endpoint,
            status.error.as_deref().unwrap_or("inspection failed")
        );
    }
    let mut output = format!(
        "{}: workspace={} revision={}",
        status.endpoint,
        status.workspace.as_deref().unwrap_or("unknown"),
        status
            .revision
            .map(|r| r.to_string())
            .unwrap_or_else(|| "unknown".into())
    );
    match &status.windows {
        Some(windows) if windows.is_empty() => output.push_str("\n  (no Canvas windows)"),
        Some(windows) => {
            for window in windows {
                output.push_str(&format!(
                    "\n  {} — {} [{}]",
                    window["id"].as_str().unwrap_or("unknown"),
                    window["title"].as_str().unwrap_or("untitled"),
                    window["content"].as_str().unwrap_or("unknown")
                ));
            }
        }
        None => output.push_str("\n  (window information unavailable)"),
    }
    output
}
fn receipt(target: String, result: Result<Value>) -> Receipt {
    match result {
        Ok(reply) => {
            let ok = reply["outcome"]["status"] == "ok" || reply["ok"] == true;
            Receipt {
                target,
                ok,
                error: if ok {
                    None
                } else {
                    Some("Canvas rejected command; see reply".into())
                },
                reply: Some(reply),
            }
        }
        Err(error) => Receipt {
            target,
            ok: false,
            reply: None,
            error: Some(error.to_string()),
        },
    }
}
impl Config {
    pub fn select(&self, selector: &str) -> Result<Vec<String>> {
        ensure!(
            !self.targets.is_empty() && self.targets.len() <= 32,
            "fleet needs 1..=32 targets"
        );
        let targets = self
            .groups
            .get(selector)
            .cloned()
            .unwrap_or_else(|| selector.split(',').map(str::to_owned).collect());
        ensure!(
            !targets.is_empty() && targets.len() <= 32,
            "empty/oversized selection"
        );
        let mut unique = BTreeSet::new();
        for name in targets {
            ensure!(
                self.targets.contains_key(&name),
                "unknown Canvas target {name}"
            );
            unique.insert(name);
        }
        Ok(unique.into_iter().collect())
    }
    pub async fn run(&self, targets: &[String], action: Action) -> Vec<Receipt> {
        let mut tasks = tokio::task::JoinSet::new();
        let mut gateways = Vec::new();
        for name in targets {
            let Some(driver) = self.targets.get(name).cloned() else {
                continue;
            };
            let name = name.clone();
            match driver {
                Driver::Command { program, args } => {
                    let action = action.clone();
                    tasks.spawn(async move {
                        let result = async {
                            ensure!(args.len() <= 32, "driver argument count exceeds bound");
                            let request = json!({"schema_version":1,"request_id":unibus::envelope::next_request_id(),"expected_revision":action.revision(&name)?,"command":action.command(&name)?});
                            super::run_cli(&program,&args,&request,Duration::from_secs(12)).await
                        }.await;
                        vec![receipt(name,result)]
                    });
                }
                Driver::Gateway {
                    lease,
                    result_producer,
                } => gateways.push((name, *lease, result_producer)),
            }
        }
        if !gateways.is_empty() {
            let config = self.unibus.clone();
            tasks.spawn_blocking(move || gateway_batch(config, gateways, action));
        }
        let mut receipts = Vec::new();
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(result) => receipts.extend(result),
                Err(error) => receipts.push(receipt("controller".into(), Err(error.into()))),
            }
        }
        receipts.sort_by(|a, b| a.target.cmp(&b.target));
        receipts
    }
}
fn gateway_batch(
    config: Option<ClientConfig>,
    targets: Vec<(String, GatewayLease, String)>,
    action: Action,
) -> Vec<Receipt> {
    let connect = || -> Result<Client> {
        let mut config = config
            .ok_or_else(|| anyhow::anyhow!("gateway targets require local Unibus client config"))?;
        let endpoint: std::net::SocketAddr = config.hub_endpoint.parse()?;
        ensure!(
            endpoint.ip().is_loopback() && config.schema_version == 2,
            "controller requires authenticated v2 local Unibus; remote transport belongs to Unibus"
        );
        config.read_timeout_ms = 100;
        Client::connect(config)
    };
    let client = match connect() {
        Ok(client) => client,
        Err(error) => {
            return targets
                .into_iter()
                .map(|(name, _, _)| receipt(name, Err(anyhow::anyhow!(error.to_string()))))
                .collect();
        }
    };
    let mut pending = BTreeMap::new();
    let mut results = Vec::new();
    for (name, lease, producer) in targets {
        let id = unibus::envelope::next_request_id();
        let send = || -> Result<()> {
            lease.validate_at(unibus::envelope::now_ms() / 1000)?;
            let operation = match action {
                Action::Inspect => super::INSPECT,
                Action::Switch { .. } => super::SWITCH,
            };
            ensure!(
                lease.operations.contains(operation),
                "target does not advertise requested capability"
            );
            let input = match &action {
                Action::Inspect => json!({}),
                Action::Switch {
                    name: workspace, ..
                } => json!({"name":workspace,"expected_revision":action.revision(&name)?}),
            };
            let invocation = GatewayInvoke {
                schema_version: 1,
                request_id: id.clone(),
                resource: lease.resource.clone(),
                generation: lease.generation,
                operation: operation.into(),
                input,
            };
            let mut envelope = client.envelope(
                Some(&lease.endpoint.peer_id),
                Message::new(GATEWAY_INVOKE, serde_json::to_value(invocation)?),
            )?;
            envelope.request_id = id.clone();
            client.send(&envelope, &[])
        };
        match send() {
            Ok(()) => {
                pending.insert(id, (name, lease, producer));
            }
            Err(error) => results.push(receipt(name, Err(error))),
        }
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    while !pending.is_empty() && Instant::now() < deadline {
        let frame = match client.try_read() {
            Ok(Some(frame)) => frame,
            Ok(None) => continue,
            Err(_) => break,
        };
        let (envelope, payload) = frame;
        if !payload.is_empty()
            || envelope
                .validate(&client.config().installation_id, unibus::envelope::now_ms())
                .is_err()
        {
            continue;
        }
        if envelope.message.kind == GATEWAY_RESULT {
            let result: GatewayResult =
                match serde_json::from_value(Value::Object(envelope.message.body)) {
                    Ok(result) => result,
                    Err(_) => continue,
                };
            let Some((name, lease, producer)) = pending.get(&result.request_id) else {
                continue;
            };
            if envelope.producer_id != *producer
                || result.caller_producer != client.config().producer_id
                || result.resource != lease.resource
                || result.generation != lease.generation
            {
                continue;
            }
            let name = name.clone();
            pending.remove(&result.request_id);
            let output = if result.output["success"] == false {
                Err(anyhow::anyhow!(
                    "adapter rejected command: {}",
                    result.output["error"]
                ))
            } else {
                Ok(result.output["reply"].clone())
            };
            results.push(receipt(name, output));
        } else if envelope.message.kind == "reply"
            && envelope.message.body["success"] == false
            && let Some(id) = envelope.message.body["request_id"].as_str()
            && let Some((name, _, _)) = pending.remove(id)
        {
            results.push(receipt(
                name,
                Err(anyhow::anyhow!("Unibus denied gateway request")),
            ));
        }
    }
    results.extend(pending.into_values().map(|(name, _, _)| {
        receipt(
            name,
            Err(anyhow::anyhow!(
                "gateway timeout or disconnected; delivery may be uncertain"
            )),
        )
    }));
    results
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_is_explicit_and_deduplicated() {
        let cfg:Config=serde_json::from_value(json!({"targets":{"neo":{"driver":"command","program":"/usr/bin/false"},"studio":{"driver":"command","program":"/usr/bin/false"}},"groups":{"all":["neo","studio","neo"]}})).unwrap();
        assert_eq!(cfg.select("all").unwrap(), vec!["neo", "studio"]);
        assert!(cfg.select("missing").is_err());
    }
    #[test]
    fn status_projects_desktop_snapshot_and_preserves_errors() {
        let receipt = Receipt {
            target: "neo".into(),
            ok: true,
            error: None,
            reply: Some(
                json!({"revision":22,"outcome":{"receipt":{"snapshot":{"workspace_id":"home","surfaces":[{"id":"clock","title":"Local time","content":"widget"}]}}}}),
            ),
        };
        let status = endpoint_status(&receipt);
        assert_eq!(status.workspace.as_deref(), Some("home"));
        assert_eq!(status.windows.as_ref().unwrap().len(), 1);
        assert!(format_status(&status).contains("clock — Local time [widget]"));
        let failed = endpoint_status(&Receipt {
            target: "tv".into(),
            ok: false,
            reply: None,
            error: Some("uncorrelated Canvas reply".into()),
        });
        assert!(failed.windows.is_none());
        assert!(format_status(&failed).contains("ERROR — uncorrelated Canvas reply"));
    }
    #[test]
    fn status_does_not_invent_missing_windows() {
        let status = endpoint_status(&Receipt {
            target: "tv".into(),
            ok: true,
            error: None,
            reply: Some(json!({"revision":3,"data":{"workspace_id":"studio"}})),
        });
        assert_eq!(status.workspace.as_deref(), Some("studio"));
        assert!(status.windows.is_none());
        assert!(format_status(&status).contains("unavailable"));
    }
    #[tokio::test]
    async fn failed_screen_does_not_hide_other_receipts() {
        let cfg:Config=serde_json::from_value(json!({"targets":{"neo":{"driver":"command","program":"/usr/bin/false"},"studio":{"driver":"command","program":"/bin/cat"}}})).unwrap();
        let targets = cfg.select("neo,studio").unwrap();
        let results = cfg.run(&targets, Action::Inspect).await;
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| !r.ok));
    }
}
