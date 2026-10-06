//! Local, read-only diagnostics. Discovery and updates remain Mycelium-owned.
use anyhow::{Result, ensure};
use serde::Serialize;
use std::{path::Path, process::Command};
use unibus_router::HubConfig;

#[derive(Serialize)]
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

fn command(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program).args(args).output()?;
    ensure!(
        output.status.success(),
        "{program} failed; check local permissions and installation"
    );
    Ok(String::from_utf8(output.stdout)?)
}

fn service_check(properties: &str) -> Check {
    let property = |key: &str| properties.lines().find_map(|line| line.strip_prefix(key));
    let load = property("LoadState=").unwrap_or("unknown");
    let active = property("ActiveState=").unwrap_or("unknown");
    let enabled = property("UnitFileState=").unwrap_or("unknown");
    let ok =
        load == "loaded" && (active == "active" || (active == "inactive" && enabled == "disabled"));
    Check {
        name: "service",
        ok,
        detail: format!(
            "load={load} active={active} enabled={enabled}; disabled/inactive is the packaged default"
        ),
    }
}

fn listener_check(listeners: &str, expected: &str) -> Check {
    let found = listeners
        .lines()
        .any(|line| line.split_whitespace().nth(3) == Some(expected));
    Check {
        name: "tcp_listener",
        ok: found,
        detail: format!(
            "configured endpoint {expected} {} (socket presence only, not process ownership or authorization)",
            if found {
                "is listening"
            } else {
                "is absent; check journalctl -u unibus-router.service -b"
            }
        ),
    }
}

fn doctor(path: &Path) -> Vec<Check> {
    let mut checks = Vec::new();
    let mut expected = None;
    match HubConfig::read(path) {
        Ok(config) => { expected = Some(config.listen.clone()); checks.push(Check {
            name: "config",
            ok: true,
            detail: format!("valid; TCP={} grants={} routes={} QUIC={}; discovery does not grant access",
                config.listen, config.nodes.len(), config.routes.len(), config.quic.is_some()),
        }); },
        // Do not print configuration values or parser errors that may contain tokens.
        Err(_) => checks.push(Check {
            name: "config",
            ok: false,
            detail: "unreadable or invalid config; check path, permissions and HubConfig schema (values redacted)".into(),
        }),
    }
    match command(
        "systemctl",
        &[
            "show",
            "unibus-router.service",
            "--property=LoadState,ActiveState,UnitFileState",
        ],
    ) {
        Ok(properties) => {
            if properties.lines().any(|line| line == "ActiveState=active") {
                if let Some(expected) = expected {
                    match command("ss", &["-ltnH"]) {
                        Ok(listeners) => checks.push(listener_check(&listeners, &expected)),
                        Err(error) => checks.push(Check {
                            name: "tcp_listener",
                            ok: false,
                            detail: error.to_string(),
                        }),
                    }
                }
            }
            checks.push(service_check(&properties));
        }
        Err(error) => checks.push(Check {
            name: "service",
            ok: false,
            detail: error.to_string(),
        }),
    }
    checks
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--version"] {
        println!("unibusctl {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    ensure!(
        args.first().map(String::as_str) == Some("doctor"),
        "usage: unibusctl doctor [--config PATH] [--json]"
    );
    let mut path = "/etc/unibus/hub.json";
    let mut json = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--json" if !json => json = true,
            "--config" if i + 1 < args.len() => {
                i += 1;
                path = &args[i];
            }
            _ => anyhow::bail!("unknown or incomplete option"),
        }
        i += 1;
    }
    let checks = doctor(Path::new(path));
    if json {
        println!("{}", serde_json::to_string_pretty(&checks)?);
    } else {
        for check in &checks {
            println!(
                "{} {}: {}",
                if check.ok { "OK" } else { "FAIL" },
                check.name,
                check.detail
            );
        }
    }
    if checks.iter().any(|check| !check.ok) {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabled_default_and_running_service_are_healthy() {
        assert!(
            service_check("LoadState=loaded\nActiveState=inactive\nUnitFileState=disabled\n").ok
        );
        assert!(service_check("LoadState=loaded\nActiveState=active\nUnitFileState=enabled\n").ok);
    }
    #[test]
    fn missing_failed_or_enabled_but_stopped_fail_loudly() {
        for properties in [
            "",
            "LoadState=not-found\n",
            "LoadState=loaded\nActiveState=failed\nUnitFileState=enabled\n",
            "LoadState=loaded\nActiveState=inactive\nUnitFileState=enabled\n",
        ] {
            assert!(!service_check(properties).ok);
        }
    }
    #[test]
    fn listener_must_match_address_and_port() {
        let sockets = "LISTEN 0 128 127.0.0.1:18790 0.0.0.0:*\n";
        assert!(listener_check(sockets, "127.0.0.1:18790").ok);
        assert!(!listener_check(sockets, "0.0.0.0:18790").ok);
        assert!(!listener_check(sockets, "127.0.0.1:1879").ok);
    }
    #[test]
    fn invalid_configuration_values_are_redacted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hub.json");
        std::fs::write(&path, r#"{"secret":"never-print-this-token"}"#).unwrap();
        let checks = doctor(&path);
        assert!(!checks[0].ok);
        assert!(
            !serde_json::to_string(&checks)
                .unwrap()
                .contains("never-print-this-token")
        );
    }
}
