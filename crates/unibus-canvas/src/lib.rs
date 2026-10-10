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
pub mod fleet;
pub mod sync;
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
        run_cli(&self.canvasctl, &[], &request, Duration::from_secs(3)).await
    }
}
pub async fn run_cli(
    program: &std::path::Path,
    args: &[String],
    request: &Value,
    deadline: Duration,
) -> Result<Value> {
    run_cli_contract(
        program,
        args,
        request,
        deadline,
        CliReplyContract::Correlated,
    )
    .await
}

/// Operator-selected owner CLI contract, never chosen by gateway callers.
#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliReplyContract {
    #[default]
    Correlated,
    /// One request/receipt per authenticated presentation connection.
    /// This does not provide wire-level request-ID correlation.
    PresentationTransaction,
}

fn validate_cli_reply(
    mut reply: Value,
    request: &Value,
    contract: CliReplyContract,
) -> Result<Value> {
    match contract {
        CliReplyContract::Correlated => ensure!(
            reply["schema_version"] == 1 && reply["request_id"] == request["request_id"],
            "uncorrelated Canvas reply"
        ),
        CliReplyContract::PresentationTransaction => {
            ensure!(
                reply.is_object() && reply["ok"].is_boolean() && reply["revision"].is_u64(),
                "invalid presentation transaction receipt"
            );
            if let Some(version) = reply.get("schema_version") {
                ensure!(version == 1, "conflicting presentation schema version");
            }
            if let Some(id) = reply.get("request_id") {
                ensure!(
                    id == &request["request_id"],
                    "conflicting presentation request ID"
                );
            }
            // Mark the local normalization explicitly; do not claim wire correlation.
            reply["schema_version"] = json!(1);
            reply["request_id"] = request["request_id"].clone();
            reply["reply_provenance"] = json!("single_authenticated_transaction");
        }
    }
    ensure!(
        matches!(reply["outcome"]["status"].as_str(), Some("ok" | "error"))
            || reply["ok"].is_boolean(),
        "invalid Canvas outcome"
    );
    Ok(reply)
}

pub async fn run_cli_contract(
    program: &std::path::Path,
    args: &[String],
    request: &Value,
    deadline: Duration,
    contract: CliReplyContract,
) -> Result<Value> {
    ensure!(program.is_absolute(), "control executable must be absolute");
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() <= MAX_REPLY as usize,
        "control request exceeds bound"
    );
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let result = tokio::time::timeout(deadline, async {
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
        let status = child.wait().await?;
        let reply = match serde_json::from_slice(&reply) {
            Ok(reply) => validate_cli_reply(reply, request, contract)?,
            Err(_) if !status.success() => anyhow::bail!("canvasctl failed"),
            Err(error) => return Err(error.into()),
        };
        // Owner CLIs may exit nonzero for a correctly framed rejection. Keep
        // that receipt so callers can distinguish conflict from transport failure.
        ensure!(
            status.success() || reply["outcome"]["status"] == "error" || reply["ok"] == false,
            "canvasctl failed despite a successful reply"
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn presentation_normalization_is_explicit_and_rejects_conflicts() {
        let request = json!({"schema_version":1,"request_id":"check-1"});
        let receipt = json!({"ok":true,"revision":7,"data":{}});
        assert!(
            validate_cli_reply(receipt.clone(), &request, CliReplyContract::Correlated).is_err()
        );
        let normalized = validate_cli_reply(
            receipt.clone(),
            &request,
            CliReplyContract::PresentationTransaction,
        )
        .unwrap();
        assert_eq!(normalized["request_id"], "check-1");
        assert_eq!(
            normalized["reply_provenance"],
            "single_authenticated_transaction"
        );
        for bad in [
            json!({"ok":true}),
            json!({"ok":"true","revision":7}),
            json!({"ok":true,"revision":7,"request_id":"other"}),
            json!({"ok":true,"revision":7,"schema_version":2}),
        ] {
            assert!(
                validate_cli_reply(bad, &request, CliReplyContract::PresentationTransaction)
                    .is_err()
            );
        }
        let rejected = validate_cli_reply(
            json!({"ok":false,"revision":7}),
            &request,
            CliReplyContract::PresentationTransaction,
        )
        .unwrap();
        assert_eq!(rejected["ok"], false);
    }
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
    async fn nonzero_owner_rejection_retains_correlated_reason() {
        let reply = run_cli(std::path::Path::new("/bin/sh"),
            &["-c".into(), "printf '%s\\n' '{\"schema_version\":1,\"request_id\":\"rejected\",\"revision\":2,\"outcome\":{\"status\":\"error\",\"message\":\"stale revision\"}}'; exit 1".into()],
            &json!({"schema_version":1,"request_id":"rejected"}), Duration::from_secs(1)).await.unwrap();
        assert_eq!(reply["outcome"]["message"], "stale revision");
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
        adapter.canvasctl = "/bin/cat".into();
        let error = adapter.execute(&invocation).await.unwrap_err().to_string();
        assert!(error.contains("outcome"), "unexpected rejection: {error}");
    }
}
