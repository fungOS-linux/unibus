use anyhow::{Result, ensure};
use unibus_canvas::fleet::{Action, Config, endpoint_status, format_status};
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "status") {
        return status(&args[1..]).await;
    }
    ensure!(
        (args.len() == 3 && args[2] == "inspect") || (args.len() == 4 && args[2] == "switch"),
        "usage: canvas-fleet CONFIG GROUP_OR_TARGETS inspect | switch WORKSPACE"
    );
    let file = std::fs::File::open(&args[0])?;
    ensure!(
        file.metadata()?.len() <= 65536,
        "fleet config exceeds bound"
    );
    let mut config: Config = serde_json::from_reader(file)?;
    if let Some(bus) = config.unibus.as_mut() {
        bus.resolve_token_file(std::path::Path::new(&args[0]));
    }
    let targets = config.select(&args[1])?;
    let results = if args[2] == "inspect" {
        config.run(&targets, Action::Inspect).await
    } else {
        let observed = config.run(&targets, Action::Inspect).await;
        let mut revisions = std::collections::BTreeMap::new();
        let mut errors = Vec::new();
        for mut result in observed {
            if result.ok
                && let Some(revision) = result.reply.as_ref().and_then(|r| r["revision"].as_u64())
            {
                revisions.insert(result.target, revision);
                continue;
            }
            if result.ok {
                result.ok = false;
                result.error = Some("inspection did not return a revision; switch not sent".into());
            }
            errors.push(result);
        }
        let ready: Vec<_> = targets
            .into_iter()
            .filter(|name| revisions.contains_key(name))
            .collect();
        errors.extend(
            config
                .run(
                    &ready,
                    Action::Switch {
                        name: args[3].clone(),
                        revisions,
                    },
                )
                .await,
        );
        errors
    };
    println!("{}", serde_json::to_string_pretty(&results)?);
    if results.iter().any(|result| !result.ok) {
        std::process::exit(1)
    }
    Ok(())
}

async fn status(args: &[String]) -> Result<()> {
    let mut selector = None;
    let mut json = false;
    let mut path = None;
    let mut arguments = args.iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--json" => json = true,
            "--config" => {
                path = Some(
                    arguments
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--config requires a path"))?
                        .clone(),
                )
            }
            value if !value.starts_with('-') && selector.is_none() => {
                selector = Some(value.to_owned())
            }
            _ => anyhow::bail!(
                "usage: canvas-fleet status [GROUP_OR_TARGETS] [--json] [--config PATH]"
            ),
        }
    }
    let path = match path {
        Some(path) => std::path::PathBuf::from(path),
        None => std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config"))
            })
            .ok_or_else(|| anyhow::anyhow!("no config directory; supply --config"))?
            .join("unibus/canvas-fleet.json"),
    };
    let file = std::fs::File::open(&path)?;
    ensure!(
        file.metadata()?.len() <= 65536,
        "fleet config exceeds bound"
    );
    let mut config: Config = serde_json::from_reader(file)?;
    if let Some(bus) = config.unibus.as_mut() {
        bus.resolve_token_file(&path);
    }
    let targets = match selector {
        Some(selector) => config.select(&selector)?,
        None => {
            ensure!(
                !config.targets.is_empty() && config.targets.len() <= 32,
                "fleet needs 1..=32 targets"
            );
            config.targets.keys().cloned().collect()
        }
    };
    let results = config.run(&targets, Action::Inspect).await;
    let statuses: Vec<_> = results.iter().map(endpoint_status).collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&statuses)?);
    } else {
        for status in &statuses {
            println!("{}", format_status(status));
        }
    }
    if statuses.iter().any(|s| !s.ok) {
        std::process::exit(1);
    }
    Ok(())
}
