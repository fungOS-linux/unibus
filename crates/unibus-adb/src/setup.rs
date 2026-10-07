use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use std::path::Path;
use unibus_adb::{KIND, inventory::INVENTORY_KIND};
pub struct Options {
    pub server_id: String,
    pub client_id: String,
    pub installation_id: String,
    pub adb_binary: Option<std::path::PathBuf>,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            server_id: "studio-adb".into(),
            client_id: "neo-adb".into(),
            installation_id: "adb-studio".into(),
            adb_binary: None,
        }
    }
}
impl Options {
    pub fn parse(args: impl Iterator<Item = std::ffi::OsString>) -> Result<Self> {
        let mut result = Self::default();
        let mut args = args;
        while let Some(key) = args.next() {
            let value = args.next().context("init option requires a value")?;
            match key.to_str() {
                Some("--server-id") => {
                    result.server_id = value
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("invalid server ID"))?
                }
                Some("--client-id") => {
                    result.client_id = value
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("invalid client ID"))?
                }
                Some("--installation-id") => {
                    result.installation_id = value
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("invalid installation ID"))?
                }
                Some("--adb-binary") => result.adb_binary = Some(value.into()),
                _ => bail!("unknown init option: {}", key.to_string_lossy()),
            }
        }
        result.validate()?;
        Ok(result)
    }
    fn validate(&self) -> Result<()> {
        for id in [&self.server_id, &self.client_id, &self.installation_id] {
            ensure!(
                !id.is_empty()
                    && id.len() <= 64
                    && id
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
                "IDs must be 1..64 ASCII letters, digits, hyphens or underscores"
            );
        }
        ensure!(
            self.server_id != self.client_id
                && self.server_id != "adb-observer"
                && self.client_id != "adb-observer",
            "server, client and observer identities must be distinct"
        );
        if let Some(binary) = &self.adb_binary {
            ensure!(
                binary.is_absolute() && binary.is_file(),
                "--adb-binary must name an existing absolute executable path"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                ensure!(
                    std::fs::metadata(binary)?.permissions().mode() & 0o111 != 0,
                    "--adb-binary is not executable"
                );
            }
        }
        Ok(())
    }
}
#[cfg(test)]
pub fn init(path: &Path) -> Result<()> {
    init_with_options(path, &Options::default())
}
pub fn init_with_options(path: &Path, options: &Options) -> Result<()> {
    options.validate()?;
    let server = options.server_id.as_str();
    let client = options.client_id.as_str();
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
    for producer in [server, client, "adb-observer"] {
        unibus_router::auth::issue_token(
            &path.join(format!("{producer}.token")),
            &unibus_router::auth::generate_token()?,
        )?;
    }
    let mut nodes: Vec<_> = [server, client].map(|id| {
        json!({"producer_id":id,"token_file":format!("{id}.token"),
        "zones":["default"],"capabilities":["adb.socket"],"publishes":if id == server {vec![KIND,INVENTORY_KIND,
            stream_descriptors::service::SERVICE_CATALOG_KIND, stream_descriptors::service::SERVICE_HEALTH_KIND]} else {vec![KIND]},
        "publish_producers":[if id == server {client} else {server}]})
    }).into_iter().collect();
    nodes.push(
        json!({"producer_id":"adb-observer", "token_file":"adb-observer.token",
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
    let router = json!({"installation_id":options.installation_id,"listen":"127.0.0.1:18793", "nodes":nodes,"routes":routes});
    let bus = |id| {
        json!({"installation_id":options.installation_id,"producer_id":id,"hub_endpoint":if id == client {"127.0.0.1:19893"} else {"127.0.0.1:18793"},
        "token_file":format!("{id}.token"),"zones":["default"],"capabilities":["adb.socket"],"schema_version":2})
    };
    let serve = json!({"bus":bus(server),"mode":{"mode":"serve","adb_endpoint":"127.0.0.1:5037",
        "adb_binary":options.adb_binary,"allowed_clients":[client],"publish_services":true}});
    let attach = json!({"bus":bus(client),"mode":{"mode":"attach","listen":"127.0.0.1:5038","destination":server}});
    let watch = json!({"bus":{"installation_id":options.installation_id,"producer_id":"adb-observer",
        "hub_endpoint":"127.0.0.1:19893","token_file":"adb-observer.token",
        "zones":["default"],"capabilities":["services.observe"],"schema_version":2},
        "sources":[{"producer_id":server,"service_id":format!("adb.{server}"),
            "owner_reference":format!("producer/{server}"),"presenter_reference":format!("producer/{server}")}]});
    for (name, value) in [
        ("router.json", router),
        ("serve.json", serve),
        ("attach.json", attach),
        ("watch.json", watch),
    ] {
        std::fs::write(path.join(name), serde_json::to_vec_pretty(&value)?)?;
    }
    println!("Created private configuration in {}", path.display());
    if options.adb_binary.is_none() {
        eprintln!(
            "ADB auto-start disabled: run adb start-server on the USB host before serve, or use init --adb-binary ABSOLUTE_PATH."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn custom_identity_and_relocated_credentials_work() {
        let temporary = tempfile::tempdir().unwrap();
        let origin = temporary.path().join("host");
        let destination = temporary.path().join("james");
        let options = Options {
            server_id: "james-host".into(),
            client_id: "james-laptop".into(),
            installation_id: "james-adb".into(),
            adb_binary: None,
        };
        init_with_options(&origin, &options).unwrap();
        std::fs::create_dir(&destination).unwrap();
        for file in ["attach.json", "james-laptop.token"] {
            std::fs::copy(origin.join(file), destination.join(file)).unwrap();
        }
        let file = destination.join("attach.json");
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(config["mode"]["destination"], "james-host");
        let mut bus: unibus::client::ClientConfig =
            serde_json::from_value(config["bus"].clone()).unwrap();
        assert_eq!(
            bus.token_file.as_ref().unwrap(),
            Path::new("james-laptop.token")
        );
        bus.resolve_token_file(&file);
        assert_eq!(bus.token().unwrap().len(), 64);
        let server: serde_json::Value =
            serde_json::from_slice(&std::fs::read(origin.join("serve.json")).unwrap()).unwrap();
        assert!(server["mode"]["adb_binary"].is_null());
        assert_eq!(server["mode"]["allowed_clients"], json!(["james-laptop"]));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&origin).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(origin.join("james-laptop.token"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    #[test]
    fn invalid_options_fail_before_creating_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("no-side-effects");
        let options = Options {
            server_id: "../escape".into(),
            ..Options::default()
        };
        assert!(init_with_options(&path, &options).is_err());
        assert!(!path.exists());
        for args in [
            vec!["--client-id"],
            vec!["--unknown", "value"],
            vec!["--client-id", "studio-adb"],
            vec!["--adb-binary", "adb"],
        ] {
            assert!(Options::parse(args.into_iter().map(std::ffi::OsString::from)).is_err());
        }
    }
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
