//! Canvas owns its control protocol and socket checks. This adapter calls its CLI;
//! caller input can never choose an executable, socket, URL or arbitrary command.
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use unibus::gateway::{GatewayInvoke, ResourceId};

pub const INSPECT: &str = "canvas.inspect";
pub const SWITCH: &str = "canvas.workspace.switch";
const MAX_REPLY: u64 = 60 * 1024;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanvasAdapter {
    pub resource: ResourceId,
    /// Operator-owned absolute path; normally /usr/local/bin/canvasctl.
    pub canvasctl: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InspectInput {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SwitchInput {
    name: String,
    expected_revision: u64,
}
impl CanvasAdapter {
    pub fn validate(&self) -> Result<()> {
        self.resource.validate()?;
        ensure!(
            self.canvasctl.is_absolute(),
            "canvasctl path must be absolute"
        );
        Ok(())
    }
    pub fn request(&self, invocation: &GatewayInvoke) -> Result<Value> {
        self.validate()?;
        invocation.validate()?;
        ensure!(
            invocation.resource == self.resource,
            "wrong Canvas resource"
        );
        let (command, revision) = match invocation.operation.as_str() {
            INSPECT => {
                let _: InspectInput = serde_json::from_value(invocation.input.clone())?;
                (json!({"command":"inspect"}), None)
            }
            SWITCH => {
                let input: SwitchInput = serde_json::from_value(invocation.input.clone())?;
                ensure!(
                    !input.name.is_empty()
                        && input.name.len() <= 128
                        && !input.name.chars().any(char::is_control),
                    "invalid workspace name"
                );
                (
                    json!({"command":"workspace_switch","name":input.name}),
                    Some(input.expected_revision),
                )
            }
            _ => anyhow::bail!("unsupported Canvas operation"),
        };
        Ok(
            json!({"schema_version":1,"request_id":invocation.request_id,
            "expected_revision":revision,"command":command}),
        )
    }
    pub async fn execute(&self, invocation: &GatewayInvoke) -> Result<Value> {
        let request = self.request(invocation)?;
        let mut bytes = serde_json::to_vec(&request)?;
        bytes.push(b'\n');
        let mut child = tokio::process::Command::new(&self.canvasctl)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| anyhow::anyhow!("missing CLI stdin"))?;
            stdin.write_all(&bytes).await?;
            drop(stdin);
            let mut stdout = child
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("missing CLI stdout"))?
                .take(MAX_REPLY + 1);
            let mut reply = Vec::new();
            stdout.read_to_end(&mut reply).await?;
            ensure!(
                reply.len() as u64 <= MAX_REPLY,
                "Canvas reply exceeds gateway bound"
            );
            ensure!(child.wait().await?.success(), "canvasctl failed");
            let reply: Value = serde_json::from_slice(&reply)?;
            ensure!(
                reply["schema_version"] == 1 && reply["request_id"] == invocation.request_id,
                "uncorrelated Canvas reply"
            );
            ensure!(
                matches!(reply["outcome"]["status"].as_str(), Some("ok" | "error")),
                "invalid Canvas outcome"
            );
            Ok(reply)
        })
        .await;
        match result {
            Ok(Ok(reply)) => Ok(reply),
            failed => {
                let _ = child.kill().await;
                match failed {
                    Ok(Err(error)) => Err(error),
                    Err(_) => anyhow::bail!("Canvas control deadline exceeded"),
                    _ => unreachable!(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (CanvasAdapter, GatewayInvoke) {
        let resource: ResourceId =
            serde_json::from_value(json!({"scope":{"kind":"peer","id":"home-pi"},"key":"canvas"}))
                .unwrap();
        let adapter = CanvasAdapter {
            resource: resource.clone(),
            canvasctl: "/usr/local/bin/canvasctl".into(),
        };
        let invocation = GatewayInvoke {
            schema_version: 1,
            request_id: "test-1".into(),
            resource,
            generation: 1,
            operation: INSPECT.into(),
            input: json!({}),
        };
        (adapter, invocation)
    }
    #[test]
    fn only_explicit_operations_and_inputs_reach_canvas() {
        let (adapter, mut invocation) = fixture();
        assert_eq!(
            adapter.request(&invocation).unwrap()["command"]["command"],
            "inspect"
        );
        invocation.input = json!({"command":"open_file","path":"/secret"});
        assert!(adapter.request(&invocation).is_err());
        invocation.operation = SWITCH.into();
        invocation.input = json!({"name":"studio"});
        assert!(adapter.request(&invocation).is_err());
        invocation.input = json!({"name":"studio","expected_revision":7});
        assert_eq!(
            adapter.request(&invocation).unwrap()["expected_revision"],
            7
        );
        invocation.resource.key = "another".into();
        assert!(adapter.request(&invocation).is_err());
    }
    #[tokio::test]
    async fn cli_failure_is_not_a_successful_empty_reply() {
        let (mut adapter, invocation) = fixture();
        adapter.canvasctl = "/usr/bin/false".into();
        assert!(adapter.execute(&invocation).await.is_err());
    }
    #[tokio::test]
    async fn oversized_and_non_reply_output_are_rejected() {
        let (mut adapter, invocation) = fixture();
        adapter.canvasctl = "/usr/bin/yes".into();
        assert!(
            adapter
                .execute(&invocation)
                .await
                .unwrap_err()
                .to_string()
                .contains("bound")
        );
        adapter.canvasctl = "/usr/bin/cat".into();
        assert!(
            adapter
                .execute(&invocation)
                .await
                .unwrap_err()
                .to_string()
                .contains("outcome")
        );
    }
}
