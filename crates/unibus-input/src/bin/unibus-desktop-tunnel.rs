//! Resolve SSH through Mycelium, then own one loopback-only input forward.
use anyhow::{Context, Result, ensure};
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.len() == 4,
        "usage: unibus-desktop-tunnel PEER REMOTE_PORT LOCAL_PORT UNIX_USER"
    );
    let remote: u16 = args[1].parse()?;
    let local: u16 = args[2].parse()?;
    ensure!(remote != 0 && local != 0, "ports must be nonzero");
    let output = std::process::Command::new("mycelium")
        .args(["ssh", &args[0], "--user", &args[3], "--json"])
        .output()
        .context("resolve SSH target through Mycelium")?;
    ensure!(
        output.status.success(),
        "Mycelium SSH discovery failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let mut argv = result["argv"]
        .as_array()
        .context("Mycelium returned no SSH argv")?
        .iter()
        .map(|v| v.as_str().map(String::from).context("invalid SSH argv"))
        .collect::<Result<Vec<_>>>()?;
    let host = argv.pop().context("empty SSH argv")?;
    ensure!(
        !host.starts_with('-') && host.contains('@'),
        "invalid resolved SSH host"
    );
    argv.extend([
        "-N".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "ServerAliveInterval=1".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
        "-L".into(),
        format!("127.0.0.1:{local}:127.0.0.1:{remote}"),
        host,
    ]);
    ensure!(
        std::process::Command::new("ssh")
            .args(argv)
            .status()?
            .success(),
        "desktop input tunnel ended unsuccessfully"
    );
    Ok(())
}
