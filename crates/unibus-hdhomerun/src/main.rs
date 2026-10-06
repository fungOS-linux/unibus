use anyhow::{Context, Result};
use unibus_hdhomerun::runtime::{Config, run};

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .context("usage: unibus-hdhomerun CONFIG.json [--check]")?;
    let check = match args.next().as_deref() {
        None => false,
        Some("--check") => true,
        Some(_) => anyhow::bail!("usage: unibus-hdhomerun CONFIG.json [--check]"),
    };
    anyhow::ensure!(args.next().is_none(), "too many arguments");
    let bytes = std::fs::read(&path).context("read adapter config")?;
    anyhow::ensure!(bytes.len() <= 64 * 1024, "configuration too large");
    let config: Config = serde_json::from_slice(&bytes).context("parse adapter config")?;
    config.validate()?;
    if check {
        println!("HDHomeRun adapter configuration valid");
        return Ok(());
    }
    run(config).await
}
