#[cfg(feature = "iroh")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use anyhow::Context;
    use std::io::Write;
    let mut args = std::env::args().skip(1);
    let action = args
        .next()
        .context("usage: holodeck-iroh keygen KEY_FILE | check|run CONFIG.json")?;
    let path = args.next().context("missing config path")?;
    anyhow::ensure!(args.next().is_none(), "unexpected arguments");
    if action == "keygen" {
        use std::os::unix::fs::OpenOptionsExt;
        let key = iroh::SecretKey::generate();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        for byte in key.to_bytes() {
            write!(file, "{byte:02x}")?;
        }
        writeln!(file)?;
        file.sync_all()?;
        println!("{}", key.public());
        return Ok(());
    }
    let config: unibus_holodeck::iroh_runtime::Config =
        serde_json::from_slice(&std::fs::read(path)?)?;
    unibus_holodeck::iroh_runtime::validate(&config)?;
    match action.as_str() {
        "check" => {
            println!("valid Holodeck Iroh configuration");
            Ok(())
        }
        "run" => unibus_holodeck::iroh_runtime::run(config).await,
        _ => anyhow::bail!("usage: holodeck-iroh keygen KEY_FILE | check|run CONFIG.json"),
    }
}

#[cfg(not(feature = "iroh"))]
fn main() {
    eprintln!("holodeck-iroh requires --features iroh");
    std::process::exit(2);
}
