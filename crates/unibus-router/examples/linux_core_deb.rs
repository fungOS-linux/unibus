//! Build the shared core payload with metadata derived from a Linux executable.
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const CONTROL: &str = include_str!("../../../deploy/linux/core/DEBIAN/control");
const CONFFILES: &str = include_str!("../../../deploy/linux/core/DEBIAN/conffiles");
const CONFIG: &str = include_str!("../../../deploy/linux/core/etc/unibus/hub.json");
const UNIT: &str =
    include_str!("../../../deploy/linux/core/usr/lib/systemd/system/unibus-router.service");

fn architecture(header: &[u8]) -> Result<&'static str> {
    ensure!(
        header.len() >= 20 && &header[..4] == b"\x7fELF",
        "expected ELF executable"
    );
    ensure!(
        header[4] == 2 && header[5] == 1,
        "expected 64-bit little-endian ELF"
    );
    match u16::from_le_bytes([header[18], header[19]]) {
        62 => Ok("amd64"),
        183 => Ok("arm64"),
        other => bail!("unsupported ELF machine {other}"),
    }
}

fn glibc_floor(versions: &str) -> Result<String> {
    let mut maximum: Option<Vec<u32>> = None;
    for word in versions.split_whitespace() {
        let Some(value) = word.strip_prefix("GLIBC_") else {
            continue;
        };
        let value = value.trim_end_matches(|c: char| !c.is_ascii_digit() && c != '.');
        if !value.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let parts: Vec<u32> = value
            .split('.')
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()?;
        if maximum.as_ref().is_none_or(|current| parts > *current) {
            maximum = Some(parts);
        }
    }
    let parts =
        maximum.context("no GLIBC requirements found; static binaries need a separate profile")?;
    Ok(parts
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join("."))
}

fn validate_libraries(dynamic: &str) -> Result<()> {
    let mut count = 0;
    for line in dynamic.lines().filter(|line| line.contains("(NEEDED)")) {
        let library = line
            .split_once('[')
            .and_then(|(_, tail)| tail.split_once(']'))
            .map(|(name, _)| name)
            .context("malformed readelf NEEDED output")?;
        ensure!(
            matches!(
                library,
                "libc.so.6"
                    | "libm.so.6"
                    | "libpthread.so.0"
                    | "libdl.so.2"
                    | "librt.so.1"
                    | "libgcc_s.so.1"
                    | "ld-linux-aarch64.so.1"
                    | "ld-linux-x86-64.so.2"
            ),
            "unmapped runtime library {library}; update dependency mapping deliberately"
        );
        count += 1;
    }
    ensure!(count > 0, "expected dynamically linked Linux executable");
    Ok(())
}

fn control(arch: &str, glibc: &str) -> Result<String> {
    let mut managed = [0; 3];
    let mut lines = Vec::new();
    for line in CONTROL.lines() {
        let updated = if line.starts_with("Version:") {
            managed[0] += 1;
            format!("Version: {}", env!("CARGO_PKG_VERSION"))
        } else if line.starts_with("Architecture:") {
            managed[1] += 1;
            format!("Architecture: {arch}")
        } else if let Some(dependencies) = line.strip_prefix("Depends:") {
            managed[2] += 1;
            let mut replaced = 0;
            let parts = dependencies
                .split(',')
                .map(|part| {
                    let part = part.trim();
                    if part.starts_with("libc6 ") || part == "libc6" {
                        replaced += 1;
                        format!("libc6 (>= {glibc})")
                    } else {
                        part.to_owned()
                    }
                })
                .collect::<Vec<_>>();
            ensure!(
                replaced == 1,
                "control template must declare libc6 exactly once"
            );
            format!("Depends: {}", parts.join(", "))
        } else {
            line.to_owned()
        };
        lines.push(updated);
    }
    ensure!(managed == [1, 1, 1], "ambiguous control template");
    Ok(format!("{}\n", lines.join("\n")))
}

fn output(program: &str, args: &[&std::ffi::OsStr]) -> Result<String> {
    let result = Command::new(program).args(args).output()?;
    ensure!(
        result.status.success(),
        "{program}: {}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(String::from_utf8(result.stdout)?)
}

fn write(stage: &Path, path: &str, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let destination = stage.join(path);
    fs::create_dir_all(destination.parent().context("payload parent missing")?)?;
    fs::write(&destination, bytes)?;
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o644))?;
    for directory in destination.ancestors().skip(1) {
        if !directory.starts_with(stage) {
            break;
        }
        fs::set_permissions(directory, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn build(binary: &Path, directory: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let arch = architecture(&fs::read(binary)?)?;
    let versions = output("readelf", &["--version-info".as_ref(), binary.as_os_str()])?;
    let dynamic = output("readelf", &["--dynamic".as_ref(), binary.as_os_str()])?;
    validate_libraries(&dynamic)?;
    let glibc = glibc_floor(&versions)?;
    fs::create_dir_all(directory)?;
    let destination = directory.join(format!(
        "unibus-core_{}_{arch}.deb",
        env!("CARGO_PKG_VERSION")
    ));
    ensure!(
        !destination.exists(),
        "refusing to overwrite {}",
        destination.display()
    );
    let workspace = tempfile::tempdir_in(directory)?;
    let stage = workspace.path().join("root");
    fs::create_dir(&stage)?;
    fs::set_permissions(&stage, fs::Permissions::from_mode(0o755))?;
    write(&stage, "DEBIAN/control", control(arch, &glibc)?.as_bytes())?;
    write(&stage, "DEBIAN/conffiles", CONFFILES.as_bytes())?;
    write(&stage, "etc/unibus/hub.json", CONFIG.as_bytes())?;
    write(
        &stage,
        "usr/lib/systemd/system/unibus-router.service",
        UNIT.as_bytes(),
    )?;
    fs::create_dir_all(stage.join("usr/bin"))?;
    fs::set_permissions(stage.join("usr/bin"), fs::Permissions::from_mode(0o755))?;
    let staged_binary = stage.join("usr/bin/unibus-router");
    fs::copy(binary, &staged_binary)?;
    fs::set_permissions(staged_binary, fs::Permissions::from_mode(0o755))?;
    let archive = workspace.path().join("package.deb");
    let result = Command::new("dpkg-deb")
        .args(["--root-owner-group", "--build"])
        .arg(&stage)
        .arg(&archive)
        .env("TMPDIR", workspace.path())
        .output()?;
    ensure!(
        result.status.success(),
        "dpkg-deb: {}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    // Publish only a complete archive, atomically and without replacing another
    // builder's output. Temporary files live on the destination filesystem.
    fs::set_permissions(&archive, fs::Permissions::from_mode(0o644))?;
    fs::hard_link(&archive, &destination)
        .context("publish complete package without overwriting")?;
    println!(
        "{} architecture={arch} libc6>={glibc}",
        destination.display()
    );
    Ok(destination)
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    ensure!(
        args.len() == 3,
        "usage: linux_core_deb ROUTER_EXECUTABLE OUTPUT_DIRECTORY"
    );
    build(Path::new(&args[1]), Path::new(&args[2]))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn derives_architecture_and_rejects_bad_headers() {
        let mut header = [0; 20];
        assert!(architecture(&header).is_err());
        header[..4].copy_from_slice(b"\x7fELF");
        header[4] = 2;
        header[5] = 1;
        for (machine, expected) in [(62u16, "amd64"), (183, "arm64")] {
            header[18..20].copy_from_slice(&machine.to_le_bytes());
            assert_eq!(architecture(&header).unwrap(), expected);
        }
        header[18] = 3;
        assert!(architecture(&header).is_err());
        header[4] = 1;
        assert!(architecture(&header).is_err());
    }
    #[test]
    fn uses_numeric_glibc_order_and_rejects_static_input() {
        assert_eq!(
            glibc_floor("Name: GLIBC_2.9 Name: GLIBC_2.34 (4) GLIBC_PRIVATE").unwrap(),
            "2.34"
        );
        assert!(glibc_floor("GLIBC_PRIVATE").is_err());
    }
    #[test]
    fn preserves_profile_and_rejects_unmapped_libraries() {
        let rendered = control("arm64", "2.17").unwrap();
        assert!(rendered.contains("Architecture: arm64\n"));
        assert!(rendered.contains("libc6 (>= 2.17), libgcc-s1, systemd (>= 235)"));
        assert!(validate_libraries("(NEEDED) Shared library: [libc.so.6]").is_ok());
        assert!(validate_libraries("(NEEDED) Shared library: [libsecret.so.1]").is_err());
        assert!(validate_libraries("").is_err());
    }

    #[test]
    fn payload_permissions_do_not_depend_on_the_builders_umask() {
        use std::os::unix::fs::PermissionsExt;
        let stage = tempfile::tempdir().unwrap();
        write(stage.path(), "etc/unibus/hub.json", CONFIG.as_bytes()).unwrap();
        assert_eq!(
            fs::metadata(stage.path().join("etc/unibus/hub.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        for directory in [
            stage.path().to_owned(),
            stage.path().join("etc"),
            stage.path().join("etc/unibus"),
        ] {
            assert_eq!(
                fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
    }

    #[test]
    #[ignore = "requires Linux readelf and dpkg-deb; run explicitly in package validation"]
    fn archive_is_complete_permissioned_and_cannot_be_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        // Packaging mechanics only: use this test's ELF, not a live service.
        let binary = std::env::current_exe().unwrap();
        let archive = build(&binary, directory.path()).unwrap();
        let before = fs::read(&archive).unwrap();
        assert!(build(&binary, directory.path()).is_err());
        assert_eq!(fs::read(&archive).unwrap(), before);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        let extracted = tempfile::tempdir().unwrap();
        output(
            "dpkg-deb",
            &[
                "--extract".as_ref(),
                archive.as_os_str(),
                extracted.path().as_os_str(),
            ],
        )
        .unwrap();
        for path in [
            "etc",
            "etc/unibus",
            "usr",
            "usr/bin",
            "usr/lib",
            "usr/lib/systemd",
            "usr/lib/systemd/system",
        ] {
            assert_eq!(
                fs::metadata(extracted.path().join(path))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755,
                "{path}"
            );
        }
        assert_eq!(
            fs::read(extracted.path().join("etc/unibus/hub.json")).unwrap(),
            CONFIG.as_bytes()
        );
        assert_eq!(
            fs::read(
                extracted
                    .path()
                    .join("usr/lib/systemd/system/unibus-router.service")
            )
            .unwrap(),
            UNIT.as_bytes()
        );
        assert_eq!(
            fs::metadata(extracted.path().join("usr/bin/unibus-router"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(&archive).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }
}
