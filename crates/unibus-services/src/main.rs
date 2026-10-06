use anyhow::{Result, bail};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() < 2
        || args.len() > 3
        || args[0] != "inspect"
        || (args.len() == 3 && args[2] != "--json")
    {
        bail!("usage: unibus-services inspect http://PRIVATE_IP:PORT [--json]");
    }
    let inspection = unibus_services::inspect(&args[1])?;
    if args.len() == 3 {
        println!("{}", serde_json::to_string_pretty(&inspection)?);
        return Ok(());
    }
    println!(
        "{}  {:?}",
        inspection.health.service_id, inspection.effective_state
    );
    if let Some(catalog) = inspection.catalog {
        println!(
            "owner: {}  via: {}",
            catalog.owner_reference, catalog.presenter_reference
        );
        for resource in catalog.resources {
            println!(
                "  {}  {}  {}",
                resource.id, resource.kind, resource.display_name
            );
            for stream in resource.stream_ids {
                println!("    stream: {stream}");
            }
            for operation in resource.operations {
                println!(
                    "    {}: {} (requires {})",
                    operation.id, operation.request_kind, operation.requires
                );
            }
        }
    }
    Ok(())
}
