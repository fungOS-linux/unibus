//! Destructive package checks, restricted to a disposable QEMU systemd VM.
//! See docs/linux-core-package.md. Never run this against a fleet node.
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    net::TcpStream,
    process::Command,
    thread,
    time::{Duration, Instant},
};
use unibus::{
    envelope::{Envelope, Message},
    framing::{FrameError, read_frame, write_frame},
    kinds,
};

const UNIT: &str = "unibus-router.service";
const CONFIG: &str = "/etc/unibus/hub.json";
const MARKER: &str = "unibus-package-validation";

fn output(program: &str, args: &[&str]) -> Result<String> {
    let result = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("execute {program}"))?;
    ensure!(
        result.status.success(),
        "{program} {args:?}: {}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(String::from_utf8(result.stdout)?.trim().to_owned())
}

fn disposable_guard(product: &str, marker: &str, virtualization: &str, uid: &str) -> Result<()> {
    ensure!(uid == "0", "run as root inside the disposable VM");
    ensure!(
        product.trim() == MARKER && marker.trim() == MARKER,
        "refusing to change packages outside the marked disposable VM"
    );
    ensure!(
        matches!(virtualization.trim(), "kvm" | "qemu"),
        "QEMU/KVM guest required"
    );
    Ok(())
}

fn pid() -> Result<String> {
    output(
        "systemctl",
        &["show", UNIT, "--property=MainPID", "--value"],
    )
}

fn wait_ready(previous_pid: Option<&str>) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let current = pid()?;
        if current != "0" && previous_pid != Some(current.as_str()) {
            if let Ok(stream) = TcpStream::connect("127.0.0.1:18790") {
                drop(stream);
                ensure!(
                    output("systemctl", &["is-active", UNIT])? == "active",
                    "router inactive"
                );
                return Ok(current);
            }
        }
        ensure!(Instant::now() < deadline, "router failed to become ready");
        thread::sleep(Duration::from_millis(100));
    }
}

fn assert_local_unprivileged() -> Result<()> {
    let current = wait_ready(None)?;
    let status = fs::read_to_string(format!("/proc/{current}/status"))?;
    let uid = status
        .lines()
        .find(|line| line.starts_with("Uid:"))
        .context("process uid missing")?;
    ensure!(
        uid.split_whitespace().skip(1).all(|value| value != "0"),
        "root router: {uid}"
    );
    ensure!(
        fs::read_link(format!("/proc/{current}/exe"))?
            == std::path::Path::new("/usr/bin/unibus-router"),
        "running executable is not the packaged router"
    );
    let sockets = output("ss", &["-H", "-ltnp"])?;
    let router: Vec<_> = sockets
        .lines()
        .filter(|line| line.contains("unibus-router"))
        .collect();
    ensure!(
        router.len() == 1 && router[0].contains("127.0.0.1:18790"),
        "expected exactly one loopback router socket: {router:?}"
    );
    let config: serde_json::Value = serde_json::from_slice(&fs::read(CONFIG)?)?;
    ensure!(
        config["nodes"].as_array().is_some_and(Vec::is_empty),
        "default must have no grants"
    );
    let installation = config["installation_id"]
        .as_str()
        .context("installation missing")?;
    let mut stream = TcpStream::connect("127.0.0.1:18790")?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let request = Envelope::new(
        installation,
        "ungranted-package-probe",
        None,
        "0123456789abcdef0123456789abcdef",
        Message::new(
            kinds::V1_REGISTER,
            serde_json::json!({"capabilities":["display"],"zones":["default"]}),
        ),
    );
    write_frame(&mut stream, &request, &[])?;
    match read_frame(&mut stream) {
        Err(FrameError::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
            ) => {}
        other => bail!("ungranted client was not promptly disconnected: {other:?}"),
    }
    ensure!(
        output("systemctl", &["is-active", UNIT])? == "active",
        "probe crashed router"
    );
    Ok(())
}

fn installed_version() -> Result<String> {
    output("dpkg-query", &["-W", "-f=${Version}", "unibus-core"])
}

fn run(old: &str, upgrade: &str) -> Result<()> {
    disposable_guard(
        &fs::read_to_string("/sys/class/dmi/id/product_name")?,
        &fs::read_to_string("/run/unibus-package-validation")?,
        &output("systemd-detect-virt", &["--vm"])?,
        &output("id", &["-u"])?,
    )?;
    ensure!(
        !std::path::Path::new(CONFIG).exists(),
        "guest already has Unibus config"
    );
    let old_version = output("dpkg-deb", &["-f", old, "Version"])?;
    let new_version = output("dpkg-deb", &["-f", upgrade, "Version"])?;
    ensure!(
        old_version != new_version,
        "upgrade fixture must have a distinct version"
    );
    output(
        "dpkg",
        &["--compare-versions", &new_version, "gt", &old_version],
    )?;
    for package in [old, upgrade] {
        ensure!(
            output("dpkg-deb", &["-f", package, "Package"])? == "unibus-core",
            "wrong package"
        );
    }
    let extracted = tempfile::tempdir()?;
    let old_root = extracted.path().join("old");
    let new_root = extracted.path().join("upgrade");
    output(
        "dpkg-deb",
        &[
            "--extract",
            old,
            old_root.to_str().context("old fixture path")?,
        ],
    )?;
    output(
        "dpkg-deb",
        &[
            "--extract",
            upgrade,
            new_root.to_str().context("upgrade fixture path")?,
        ],
    )?;
    ensure!(
        fs::read(old_root.join("etc/unibus/hub.json"))?
            != fs::read(new_root.join("etc/unibus/hub.json"))?,
        "upgrade must change the packaged conffile to exercise preservation"
    );
    output("dpkg", &["-i", old])?;
    output("systemctl", &["daemon-reload"])?;
    ensure!(
        pid()? == "0",
        "installation unexpectedly started the router"
    );
    ensure!(
        output(
            "systemctl",
            &["show", UNIT, "--property=UnitFileState", "--value"]
        )? == "disabled",
        "installation unexpectedly enabled the router"
    );
    println!("PASS install: disabled, not running");
    output(
        "systemd-analyze",
        &["verify", "/usr/lib/systemd/system/unibus-router.service"],
    )?;
    output("systemctl", &["enable", "--now", UNIT])?;
    assert_local_unprivileged()?;
    println!("PASS start: unprivileged, loopback-only, ungranted registration rejected");
    let before = pid()?;
    output("systemctl", &["restart", UNIT])?;
    wait_ready(Some(&before))?;
    assert_local_unprivileged()?;
    println!("PASS explicit restart");
    let before = pid()?;
    output(
        "systemctl",
        &["kill", "--kill-whom=main", "--signal=SIGKILL", UNIT],
    )?;
    wait_ready(Some(&before))?;
    assert_local_unprivileged()?;
    println!("PASS crash recovery");
    let mut config: serde_json::Value = serde_json::from_slice(&fs::read(CONFIG)?)?;
    config["installation_id"] = serde_json::json!("operator-customized-guest");
    fs::write(CONFIG, serde_json::to_vec_pretty(&config)?)?;
    let preserved = fs::read(CONFIG)?;
    for (label, package, expected) in [
        ("upgrade", upgrade, &new_version),
        ("rollback", old, &old_version),
    ] {
        output("dpkg", &["--force-confold", "-i", package])?;
        ensure!(
            installed_version()? == *expected,
            "{label}: wrong installed version"
        );
        ensure!(
            fs::read(CONFIG)? == preserved,
            "{label}: operator config changed"
        );
        output("systemctl", &["daemon-reload"])?;
        output("systemctl", &["restart", UNIT])?;
        assert_local_unprivileged()?;
        println!("PASS {label}: version {expected}, operator config preserved");
    }
    output("systemctl", &["disable", "--now", UNIT])?;
    output("dpkg", &["--remove", "unibus-core"])?;
    ensure!(
        fs::read(CONFIG)? == preserved,
        "remove lost operator config"
    );
    ensure!(
        !std::path::Path::new("/usr/bin/unibus-router").exists(),
        "remove left binary"
    );
    ensure!(
        !std::path::Path::new("/usr/lib/systemd/system/unibus-router.service").exists(),
        "remove left unit"
    );
    output("dpkg", &["--purge", "unibus-core"])?;
    ensure!(!std::path::Path::new(CONFIG).exists(), "purge left config");
    println!("PASS remove preserves configuration; purge removes it");
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 3,
        "usage: linux_package_lifecycle OLD.deb UPGRADE.deb"
    );
    run(&args[1], &args[2])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refuses_hosts_and_unmarked_guests() {
        assert!(disposable_guard(MARKER, MARKER, "kvm", "0").is_ok());
        for (product, marker, virt, uid) in [
            ("Agora", MARKER, "kvm", "0"),
            (MARKER, "", "kvm", "0"),
            (MARKER, MARKER, "docker", "0"),
            (MARKER, MARKER, "kvm", "1000"),
        ] {
            assert!(disposable_guard(product, marker, virt, uid).is_err());
        }
    }
}
