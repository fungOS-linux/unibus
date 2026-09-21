use anyhow::{Context, Result, ensure};
use unibus::client::{Client, ClientConfig};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let config_path = args
        .next()
        .context("usage: unibus-watch <client.json> [message-kind]")?;
    let wanted = args.next();
    ensure!(args.next().is_none(), "too many arguments");
    let config: ClientConfig = serde_json::from_slice(
        &std::fs::read(&config_path).with_context(|| format!("read {config_path}"))?,
    )
    .with_context(|| format!("parse {config_path}"))?;
    let client = Client::connect(config)?;
    loop {
        if let Some((envelope, _payload)) = client.try_read()?
            && wanted
                .as_deref()
                .is_none_or(|kind| envelope.message.kind == kind)
        {
            println!("{}", serde_json::to_string(&envelope)?);
            return Ok(());
        }
    }
}
