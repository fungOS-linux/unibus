use anyhow::{Context, Result};
use unibus_holodeck::runtime::{Config, run};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let operation = args
        .next()
        .context("usage: unibus-holodeck check|run CONFIG.json")?;
    let path = args.next().context("missing config path")?;
    anyhow::ensure!(args.next().is_none(), "unexpected arguments");
    let config: Config = serde_json::from_slice(&std::fs::read(path)?)?;
    config.validate()?;
    match operation.as_str() {
        "check" => {
            println!("valid Holodeck bridge configuration");
            Ok(())
        }
        "run" => run(config).await,
        _ => anyhow::bail!("usage: unibus-holodeck check|run CONFIG.json"),
    }
}
