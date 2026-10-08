//! Read-only selected Holodeck endpoints from Mycelium's service catalog.
use anyhow::{Result, ensure};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
use unibus_holodeck::room_directory::selected_hosts;
use unibus_mycelium::MyceliumServiceDirectory;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let socket = PathBuf::from(args.next().ok_or_else(|| {
        anyhow::anyhow!("usage: holodeck-room-hosts MYCELIUM_SOCKET TRUSTED_ENDPOINT_ID...")
    })?);
    let trusted: BTreeSet<String> = args.collect();
    ensure!(
        !trusted.is_empty() && trusted.len() <= 16,
        "specify 1..16 explicitly trusted room endpoint IDs"
    );
    let directory = MyceliumServiceDirectory::new(socket)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    println!(
        "{}",
        serde_json::to_string_pretty(&selected_hosts(&directory, now, &trusted)?)?
    );
    Ok(())
}
