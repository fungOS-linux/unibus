//! Optional Mycelium CLI boundary. Static configurations work without it.
use anyhow::{Context, Result, ensure};
use std::process::Command;
fn mycelium() -> Command {
    Command::new(std::env::var_os("MYCELIUM_BIN").unwrap_or_else(|| "mycelium".into()))
}
pub fn discover() -> Result<()> {
    let output = mycelium().args(["topology", "--json"]).output()?;
    ensure!(
        output.status.success(),
        "Mycelium service discovery failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let catalog: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let now = unibus::envelope::now_ms() / 1000;
    let bridges = bridge_advertisements(&catalog, now)?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"schema_version":1,"access":"ssh (discovery is not authorization)","observations":bridges})
        )?
    );
    Ok(())
}
/// Keep the actual DNS-SD TXT contract intact. Mycelium's generic service
/// projection on older daemons doesn't retain access/protocol metadata.
fn bridge_advertisements(catalog: &serde_json::Value, now: u64) -> Result<Vec<&serde_json::Value>> {
    let advertisements = catalog
        .get("advertisements")
        .and_then(|v| v.as_object())
        .context("Mycelium topology missing advertisements")?;
    Ok(advertisements
        .values()
        .filter(|o| {
            let txt = o.get("txt").and_then(|v| v.as_array());
            let has = |text: &str| {
                txt.is_some_and(|values| values.iter().any(|v| v.as_str() == Some(text)))
            };
            let last = o.get("last_seen").and_then(|v| v.as_u64());
            let ttl = o
                .get("ttl")
                .and_then(|v| v.as_u64())
                .unwrap_or(900)
                .min(86400);
            has("purpose=unibus-adb")
                && has("contract=adb.socket.v1")
                && has("access=ssh")
                && o.get("service_type").and_then(|v| v.as_str()) == Some("_ssh._tcp")
                && last.is_some_and(|t| t <= now && t.saturating_add(ttl) > now)
        })
        .collect())
}

pub fn tunnel(target: &str) -> Result<()> {
    let output = mycelium().args(["ssh", target, "--json"]).output()?;
    ensure!(
        output.status.success(),
        "Mycelium SSH planning failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let argv = plan
        .get("argv")
        .and_then(|v| v.as_array())
        .context("Mycelium SSH plan missing argv")?;
    let args = argv
        .iter()
        .map(|v| v.as_str().context("invalid SSH plan argument"))
        .collect::<Result<Vec<_>>>()?;
    run_ssh(&args)
}

pub fn static_tunnel(target: &str) -> Result<()> {
    ensure!(
        !target.is_empty() && !target.starts_with('-') && !target.chars().any(char::is_whitespace),
        "SSH target must be HOST or USER@HOST, not options"
    );
    run_ssh(&[target])
}

fn run_ssh(args: &[&str]) -> Result<()> {
    let mut ssh = ssh_command(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(ssh.exec().into())
    }
    #[cfg(not(unix))]
    {
        ensure!(ssh.status()?.success(), "SSH tunnel failed");
        Ok(())
    }
}

fn ssh_command(args: &[&str]) -> Command {
    let mut ssh = Command::new("ssh");
    ssh.args([
        "-N",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-L",
        "127.0.0.1:19893:127.0.0.1:18793",
    ])
    .args(args);
    ssh
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn static_ssh_is_portable_and_rejects_option_injection() {
        for target in ["", "-oProxyCommand=evil", "user@host extra", "host\n"] {
            assert!(static_tunnel(target).is_err());
        }
        let command = ssh_command(&["james@usb-host"]);
        assert_eq!(command.get_program(), "ssh");
        let args: Vec<_> = command.get_args().collect();
        assert!(args.contains(&std::ffi::OsStr::new("127.0.0.1:19893:127.0.0.1:18793")));
        assert_eq!(
            args.last().unwrap(),
            &std::ffi::OsStr::new("james@usb-host")
        );
    }
    #[test]
    fn discovery_requires_contract_and_freshness_not_a_fuzzy_name() {
        let ad = serde_json::json!({"service_type":"_ssh._tcp","last_seen":100,"ttl":20,
            "txt":["purpose=unibus-adb","contract=adb.socket.v1","access=ssh"]});
        let catalog = serde_json::json!({"advertisements":{"bridge":ad}});
        assert_eq!(bridge_advertisements(&catalog, 119).unwrap().len(), 1);
        assert!(bridge_advertisements(&catalog, 120).unwrap().is_empty());
        assert!(bridge_advertisements(&catalog, 99).unwrap().is_empty());
        let fake = serde_json::json!({"advertisements":{"fake":{"instance":"Unibus Studio ADB", "last_seen":100}}});
        assert!(bridge_advertisements(&fake, 101).unwrap().is_empty());
    }
}
