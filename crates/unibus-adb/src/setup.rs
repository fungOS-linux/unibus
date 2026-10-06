use anyhow::{Context, Result};
use serde_json::json;
use std::path::Path;
use unibus_adb::{KIND, inventory::INVENTORY_KIND};
pub fn init(path: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .context("create private directory (must not exist)")?;
    let path = path.canonicalize()?;
    for producer in ["studio-adb", "neo-adb", "adb-observer"] {
        unibus_router::auth::issue_token(
            &path.join(format!("{producer}.token")),
            &unibus_router::auth::generate_token()?,
        )?;
    }
    let mut nodes: Vec<_> = ["studio-adb", "neo-adb"].map(|id| {
        json!({"producer_id":id,"token_file":path.join(format!("{id}.token")),
        "zones":["default"],"capabilities":["adb.socket"],"publishes":if id == "studio-adb" {vec![KIND,INVENTORY_KIND,
            stream_descriptors::service::SERVICE_CATALOG_KIND, stream_descriptors::service::SERVICE_HEALTH_KIND]} else {vec![KIND]},
        "publish_producers":[if id == "studio-adb" {"neo-adb"} else {"studio-adb"}]})
    }).into_iter().collect();
    nodes.push(
        json!({"producer_id":"adb-observer", "token_file":path.join("adb-observer.token"),
        "zones":["default"],"capabilities":["services.observe"],"publishes":[]}),
    );
    let mut routes: Vec<_> = [KIND, INVENTORY_KIND]
        .map(|kind| json!({"kind":kind,"target":{"mode":"producer"}}))
        .into_iter()
        .collect();
    for kind in [
        stream_descriptors::service::SERVICE_CATALOG_KIND,
        stream_descriptors::service::SERVICE_HEALTH_KIND,
    ] {
        routes.push(json!({"kind":kind,"target":{"mode":"any_holder","name":"services.observe"}}));
    }
    let router = json!({"installation_id":"adb-studio","listen":"127.0.0.1:18793", "nodes":nodes,"routes":routes});
    let bus = |id| {
        json!({"installation_id":"adb-studio","producer_id":id,"hub_endpoint":if id == "neo-adb" {"127.0.0.1:19893"} else {"127.0.0.1:18793"},
        "token_file":path.join(format!("{id}.token")),"zones":["default"],"capabilities":["adb.socket"],"schema_version":2})
    };
    let serve = json!({"bus":bus("studio-adb"),"mode":{"mode":"serve","adb_endpoint":"127.0.0.1:5037",
        "adb_binary":"/opt/homebrew/bin/adb","allowed_clients":["neo-adb"],"publish_services":true}});
    let attach = json!({"bus":bus("neo-adb"),"mode":{"mode":"attach","listen":"127.0.0.1:5038","destination":"studio-adb"}});
    let watch = json!({"bus":{"installation_id":"adb-studio","producer_id":"adb-observer",
        "hub_endpoint":"127.0.0.1:19893","token_file":path.join("adb-observer.token"),
        "zones":["default"],"capabilities":["services.observe"],"schema_version":2},
        "sources":[{"producer_id":"studio-adb","service_id":"adb.studio-adb",
            "owner_reference":"producer/studio-adb","presenter_reference":"producer/studio-adb"}]});
    for (name, value) in [
        ("router.json", router),
        ("serve.json", serve),
        ("attach.json", attach),
        ("watch.json", watch),
    ] {
        std::fs::write(path.join(name), serde_json::to_vec_pretty(&value)?)?;
    }
    println!("Created private configuration in {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_observer_can_read_catalogs_but_has_no_adb_control_grant() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("private-adb");
        init(&path).unwrap();
        let router = unibus_router::HubConfig::read(&path.join("router.json")).unwrap();
        let observer = router
            .nodes
            .iter()
            .find(|node| node.producer_id == "adb-observer")
            .unwrap();
        assert_eq!(observer.capabilities, ["services.observe"]);
        assert!(observer.publishes.is_empty());
        assert!(observer.publish_producers.is_empty());
        assert!(
            router
                .nodes
                .iter()
                .find(|node| node.producer_id == "studio-adb")
                .unwrap()
                .publishes
                .iter()
                .any(|kind| kind == stream_descriptors::service::SERVICE_CATALOG_KIND)
        );
        assert!(path.join("watch.json").is_file());
        assert!(init(&path).is_err());
    }
}
