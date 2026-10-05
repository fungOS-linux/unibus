//! Prepare a private, paired input channel without printing or committing keys.
#[cfg(not(unix))]
fn main() {
    eprintln!("desktop setup requires Unix");
    std::process::exit(1);
}
#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    use std::{
        io::{Read, Write},
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
        path::PathBuf,
    };
    use unibus::client::ClientConfig;
    use unibus_input::desktop::{ACK_CONTRACT, CaptureConfig, EdgeConfig};
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .context("usage: unibus-desktop-setup PRIVATE_DIRECTORY")?,
    );
    ensure!(
        root.is_absolute(),
        "configuration directory must be absolute"
    );
    ensure!(
        !root.exists(),
        "refusing to modify an existing directory; choose a fresh private configuration directory"
    );
    std::fs::create_dir_all(&root)?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    let write = |name: &str, bytes: &[u8]| -> anyhow::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(root.join(name))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    };
    for name in ["controller.token", "edge.token"] {
        let mut random = [0u8; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
        let token = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        write(name, token.as_bytes())?;
    }
    let source = "neo-desktop-control";
    let destination = "home-pi-desktop";
    let bus = |producer: &str, endpoint: &str, token: PathBuf, cap: &str| ClientConfig {
        installation_id: "home".into(),
        producer_id: producer.into(),
        hub_endpoint: endpoint.into(),
        token_file: Some(token),
        token_env: None,
        zones: vec!["desktop-control".into()],
        capabilities: vec![cap.into()],
        screen_anchors: vec![],
        schema_version: 2,
        read_timeout_ms: 10,
    };
    let capture = CaptureConfig {
        bus: bus(
            source,
            "127.0.0.1:18797",
            root.join("controller.token"),
            "input.desktop.source",
        ),
        destination: destination.into(),
        session_id: "neo-tv".into(),
        controller_id: "neo".into(),
        target_id: "home-pi".into(),
        width: 1920,
        height: 1080,
        hotkey_keycode: 49,
        scroll_enabled: false,
    };
    let edge = EdgeConfig {
        bus: bus(
            destination,
            "127.0.0.1:18796",
            PathBuf::from("edge.token"),
            "input.desktop.sink",
        ),
        session_id: capture.session_id.clone(),
        controller_id: capture.controller_id.clone(),
        target_id: capture.target_id.clone(),
        authorized_producer: source.into(),
        compositor_socket: Some("/home/ajm/.local/state/dock/compositor-control.sock".into()),
    };
    let kind = unibus_input::DESKTOP_STATE_CONTRACT;
    let router = serde_json::json!({"installation_id":"home","listen":"127.0.0.1:18796","nodes":[
        {"producer_id":source,"token_file":"controller.token","zones":["desktop-control"],"capabilities":["input.desktop.source"],"publishes":[kind],"publish_producers":[destination]},
        {"producer_id":destination,"token_file":"edge.token","zones":["desktop-control"],"capabilities":["input.desktop.sink"],"publishes":[ACK_CONTRACT],"publish_producers":[source]}
    ],"routes":[{"kind":kind,"target":{"mode":"producer"}},{"kind":ACK_CONTRACT,"target":{"mode":"producer"}}]});
    write("capture.json", &serde_json::to_vec_pretty(&capture)?)?;
    // Omit absent optional client fields for older deployed Unibus clients.
    let mut edge_json = serde_json::to_value(&edge)?;
    if let Some(bus) = edge_json["bus"].as_object_mut() {
        bus.retain(|_, value| !value.is_null());
    }
    write("edge.json", &serde_json::to_vec_pretty(&edge_json)?)?;
    write("router.json", &serde_json::to_vec_pretty(&router)?)?;
    println!(
        "Prepared private input configuration in {} (credentials not printed)",
        root.display()
    );
    Ok(())
}
