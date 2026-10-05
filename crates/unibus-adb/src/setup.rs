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
    for producer in ["studio-adb", "neo-adb"] {
        unibus_router::auth::issue_token(
            &path.join(format!("{producer}.token")),
            &unibus_router::auth::generate_token()?,
        )?;
    }
    let nodes = ["studio-adb", "neo-adb"].map(|id| {
        json!({"producer_id":id,"token_file":path.join(format!("{id}.token")),
        "zones":["default"],"capabilities":["adb.socket"],"publishes":if id == "studio-adb" {vec![KIND,INVENTORY_KIND]} else {vec![KIND]},
        "publish_producers":[if id == "studio-adb" {"neo-adb"} else {"studio-adb"}]})
    });
    let routes =
        [KIND, INVENTORY_KIND].map(|kind| json!({"kind":kind,"target":{"mode":"producer"}}));
    let router = json!({"installation_id":"adb-studio","listen":"127.0.0.1:18793", "nodes":nodes,"routes":routes});
    let bus = |id| {
        json!({"installation_id":"adb-studio","producer_id":id,"hub_endpoint":if id == "neo-adb" {"127.0.0.1:19893"} else {"127.0.0.1:18793"},
        "token_file":path.join(format!("{id}.token")),"zones":["default"],"capabilities":["adb.socket"],"schema_version":2})
    };
    let serve = json!({"bus":bus("studio-adb"),"mode":{"mode":"serve","adb_endpoint":"127.0.0.1:5037",
        "adb_binary":"/opt/homebrew/bin/adb","allowed_clients":["neo-adb"]}});
    let attach = json!({"bus":bus("neo-adb"),"mode":{"mode":"attach","listen":"127.0.0.1:5038","destination":"studio-adb"}});
    for (name, value) in [
        ("router.json", router),
        ("serve.json", serve),
        ("attach.json", attach),
    ] {
        std::fs::write(path.join(name), serde_json::to_vec_pretty(&value)?)?;
    }
    println!("Created private configuration in {}", path.display());
    Ok(())
}
