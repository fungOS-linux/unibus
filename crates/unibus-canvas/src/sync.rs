//! Explicit, bounded owner-document replication. Canvas owns document semantics.
use crate::fleet::{Action, Config, Receipt};
use anyhow::{Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Serialize)]
pub struct SyncReceipt {
    pub target: String,
    pub ok: bool,
    pub state: String,
    pub error: Option<String>,
}

fn document(reply: &Value) -> Option<&Value> {
    reply
        .pointer("/outcome/receipt/document")
        .or_else(|| reply.get("data"))
        .filter(|d| !d.is_null())
}
fn validate(document: &Value, name: &str) -> Result<u64> {
    ensure!(
        serde_json::to_vec(document)?.len() <= 48 * 1024,
        "workspace document exceeds 48 KiB sync bound"
    );
    ensure!(
        document["project_id"] == name,
        "source document project does not match requested workspace"
    );
    ensure!(
        document["schema_version"] == 1,
        "unsupported workspace document schema"
    );
    ensure!(
        document["id"].as_str().is_some_and(|id| !id.is_empty()),
        "workspace document has no identity"
    );
    document["revision"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("workspace document has no revision"))
}
fn compare(source: &Value, target: &Value) -> Result<bool> {
    ensure!(
        source["id"] == target["id"],
        "workspace document identity differs; sync refused"
    );
    if source == target {
        return Ok(true);
    }
    let source_revision = source["revision"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("source document has no revision"))?;
    let target_revision = target["revision"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("target document has no revision"))?;
    ensure!(
        source_revision > target_revision,
        "target document is newer or conflicts at the same revision; sync refused"
    );
    Ok(false)
}
async fn request(
    config: &Config,
    target: &str,
    command: Value,
    revision: Option<u64>,
) -> Result<Receipt> {
    config
        .run(
            &[target.to_owned()],
            Action::Workspace { command, revision },
        )
        .await
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing endpoint receipt"))
}
fn success(receipt: Receipt) -> Result<Value> {
    let detail = receipt
        .reply
        .as_ref()
        .and_then(|reply| {
            reply
                .pointer("/outcome/message")
                .or_else(|| reply.get("error"))
        })
        .and_then(Value::as_str)
        .map(str::to_owned);
    ensure!(
        receipt.ok,
        "{}: {}",
        receipt.target,
        detail
            .or(receipt.error)
            .unwrap_or_else(|| "command rejected".into())
    );
    receipt
        .reply
        .ok_or_else(|| anyhow::anyhow!("missing Canvas reply"))
}
fn missing(receipt: &Receipt, name: &str) -> bool {
    let Some(reply) = &receipt.reply else {
        return false;
    };
    (receipt.ok && reply.get("data").is_some_and(Value::is_null))
        || (reply.pointer("/outcome/code").and_then(Value::as_str) == Some("invalid_request")
            && reply.pointer("/outcome/message").and_then(Value::as_str)
                == Some(format!("unknown project workspace: {name}").as_str()))
}

pub async fn sync(
    config: &Config,
    source: &str,
    name: &str,
    targets: &[String],
    activate: bool,
    dry_run: bool,
) -> Result<Vec<SyncReceipt>> {
    ensure!(
        config.targets.contains_key(source),
        "unknown source endpoint"
    );
    ensure!(
        !name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control),
        "invalid workspace name"
    );
    let reply = success(
        request(
            config,
            source,
            json!({"command":"workspace_read","name":name}),
            None,
        )
        .await?,
    )?;
    let source_document = document(&reply)
        .ok_or_else(|| anyhow::anyhow!("source returned no workspace document"))?
        .clone();
    validate(&source_document, name)?;
    let mut tasks = tokio::task::JoinSet::new();
    ensure!(targets.len() <= 32, "oversized target selection");
    let mut results = Vec::new();
    for target in targets.iter().filter(|t| t.as_str() != source) {
        if !config.targets.contains_key(target) {
            results.push(SyncReceipt {
                target: target.clone(),
                ok: false,
                state: "failed".into(),
                error: Some("unknown target endpoint".into()),
            });
            continue;
        }
        let config = config.clone();
        let target = target.clone();
        let name = name.to_owned();
        let source_document = source_document.clone();
        tasks.spawn(async move {
            let result =
                sync_target(&config, &target, &name, &source_document, activate, dry_run).await;
            match result {
                Ok(state) => SyncReceipt {
                    target,
                    ok: true,
                    state: state.into(),
                    error: None,
                },
                Err(error) => SyncReceipt {
                    target,
                    ok: false,
                    state: "failed".into(),
                    error: Some(error.to_string()),
                },
            }
        });
    }
    while let Some(result) = tasks.join_next().await {
        results.push(result?);
    }
    results.sort_by(|a, b| a.target.cmp(&b.target));
    Ok(results)
}
async fn sync_target(
    config: &Config,
    target: &str,
    name: &str,
    source: &Value,
    activate: bool,
    dry_run: bool,
) -> Result<&'static str> {
    let inspection = config
        .run(&[target.to_owned()], Action::Inspect)
        .await
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing inspection"))?;
    let inspection = success(inspection)?;
    let revision = inspection["revision"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("inspection omitted endpoint revision"))?;
    let current = selected_project(&inspection)?;
    if inspection.get("data").is_some() {
        ensure!(
            inspection
                .pointer("/data/capabilities")
                .and_then(Value::as_array)
                .is_some_and(|caps| caps.iter().any(|cap| cap == "workspace_import")),
            "endpoint does not advertise incremental workspace_import; upgrade required"
        );
    }
    let target_read = request(
        config,
        target,
        json!({"command":"workspace_read","name":name}),
        None,
    )
    .await?;
    if !missing(&target_read, name) {
        let reply = success(target_read)?;
        let existing =
            document(&reply).ok_or_else(|| anyhow::anyhow!("target returned no document"))?;
        validate(existing, name)?;
        if compare(source, existing)? && (!activate || current == name) {
            return Ok("unchanged");
        }
    }
    if dry_run {
        return Ok("would_import");
    }
    success(request(config, target, json!({"command":"workspace_import","document":source,"activate":activate || current == name}), Some(revision)).await?)?;
    let verification = success(
        request(
            config,
            target,
            json!({"command":"workspace_read","name":name}),
            None,
        )
        .await?,
    )?;
    ensure!(
        document(&verification) == Some(source),
        "read-back document differs; import not verified"
    );
    let selected = success(
        config
            .run(&[target.to_owned()], Action::Inspect)
            .await
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing post-import inspection"))?,
    )?;
    ensure!(
        selected_project(&selected)? == if activate { name } else { current },
        "post-import selected project differs; selection preservation not verified"
    );
    Ok("synced")
}
fn selected_project(inspection: &Value) -> Result<&str> {
    inspection
        .pointer("/outcome/receipt/snapshot/project_id")
        .or_else(|| inspection.pointer("/data/document/project_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!("inspection omitted selected project; refusing to guess selection")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn never_rebases_or_overwrites_conflicting_revisions() {
        assert!(
            compare(
                &json!({"revision":4,"id":"a"}),
                &json!({"revision":3,"id":"b"})
            )
            .is_err()
        );
        assert!(
            compare(
                &json!({"revision":3,"id":"a"}),
                &json!({"revision":3,"id":"b"})
            )
            .is_err()
        );
        assert!(compare(&json!({"revision":2}), &json!({"revision":3})).is_err());
        assert!(!compare(&json!({"revision":4}), &json!({"revision":3})).unwrap());
        assert!(compare(&json!({"revision":3}), &json!({"revision":3})).unwrap());
    }
    #[test]
    fn document_projection_accepts_only_owner_document_shapes() {
        assert!(document(&json!({"data":null})).is_none());
        assert_eq!(
            document(&json!({"outcome":{"receipt":{"document":{"revision":2}}}})).unwrap()["revision"],
            2
        );
        assert!(
            validate(
                &json!({"schema_version":1,"project_id":"other","revision":2}),
                "home"
            )
            .is_err()
        );
    }
    #[test]
    fn selection_is_project_identity_not_display_document_id() {
        assert_eq!(
            selected_project(
                &json!({"data":{"document":{"id":"screen-copy","project_id":"home"}}})
            )
            .unwrap(),
            "home"
        );
        assert_eq!(
            selected_project(&json!({"outcome":{"receipt":{"snapshot":{"project_id":"studio"}}}}))
                .unwrap(),
            "studio"
        );
        assert!(selected_project(&json!({"data":{"document":{"id":"not-a-project"}}})).is_err());
    }
    #[test]
    fn missing_requires_exact_owner_error_not_transport_failure() {
        let absent = Receipt {
            target: "neo".into(),
            ok: false,
            error: Some("rejected".into()),
            reply: Some(
                json!({"outcome":{"status":"error","code":"invalid_request","message":"unknown project workspace: home"}}),
            ),
        };
        assert!(missing(&absent, "home"));
        assert!(!missing(&absent, "other"));
        let unavailable = Receipt {
            target: "tv".into(),
            ok: false,
            reply: None,
            error: Some("timeout".into()),
        };
        assert!(!missing(&unavailable, "home"));
    }
    fn fixture(existing: Value, project: Option<&str>, capabilities: bool) -> Config {
        let doc = json!({"schema_version":1,"id":"home-document","project_id":"home","revision":4});
        let source = format!(
            "read -r request; printf '%s\\n' '{}'",
            json!({"ok":true,"revision":1,"data":doc})
        );
        let inspect = json!({"ok":true,"revision":9,"data":{"document":{"project_id":project},"capabilities":if capabilities {json!(["workspace_import"])} else {json!([])}}});
        let read = json!({"ok":true,"revision":9,"data":existing});
        let target = format!(
            "read -r request; case \"$request\" in *workspace_read*) printf '%s\\n' '{read}' ;; *workspace_import*) exit 97 ;; *inspect*) printf '%s\\n' '{inspect}' ;; *) exit 98 ;; esac"
        );
        serde_json::from_value(json!({"targets":{
            "source":{"driver":"command","program":"/bin/sh","args":["-c",source],"reply_contract":"presentation_transaction"},
            "target":{"driver":"command","program":"/bin/sh","args":["-c",target],"reply_contract":"presentation_transaction"}
        }})).unwrap()
    }
    #[tokio::test]
    async fn subprocess_dry_run_missing_workspace_never_writes_and_excludes_source() {
        let config = fixture(Value::Null, Some("other"), true);
        let results = sync(
            &config,
            "source",
            "home",
            &["source".into(), "target".into()],
            false,
            true,
        )
        .await
        .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].ok, "{:?}", results[0].error);
        assert_eq!(results[0].state, "would_import");
    }
    #[tokio::test]
    async fn subprocess_idempotence_and_failures_never_write() {
        let same =
            json!({"schema_version":1,"id":"home-document","project_id":"home","revision":4});
        let config = fixture(same, Some("other"), true);
        let results = sync(
            &config,
            "source",
            "home",
            &["target".into(), "unknown".into()],
            false,
            false,
        )
        .await
        .unwrap();
        assert!(results[0].ok);
        assert_eq!(results[0].state, "unchanged");
        assert!(!results[1].ok);
        for config in [
            fixture(Value::Null, None, true),
            fixture(Value::Null, Some("other"), false),
            fixture(
                json!({"schema_version":1,"id":"home-document","project_id":"home","revision":5}),
                Some("other"),
                true,
            ),
        ] {
            let results = sync(&config, "source", "home", &["target".into()], false, true)
                .await
                .unwrap();
            assert!(!results[0].ok);
            assert!(!results[0].error.as_ref().unwrap().contains("97"));
        }
    }
}
