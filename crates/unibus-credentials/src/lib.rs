//! Credential reference resolution shared by Unibus adapters.
//!
//! Two schemes are accepted:
//!
//! - `env://NAME` — read the variable directly.
//! - `geode://<realm>/<account>/<slot>` — read the variable that geode's
//!   `EnvBackend` uses: `GEODE_<REALM>_<ACCOUNT>_<SLOT>`.
//!
//! The `geode://` mapping is deliberate: it is the only geode backend that can
//! be reached without a host IPC boundary today. When `geoded` (or an in-process
//! host resolver) exists, this module is the one place to swap; configuration and
//! call sites keep the same `geode://` reference.
//!
//! This crate never prints or logs a resolved value.

use anyhow::{Context, Result, bail, ensure};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Resolve a reference to a secret with a generic purpose.
pub fn resolve(reference: &str) -> Result<Option<String>> {
    resolve_with_purpose(reference, "generic.read")
}

/// Resolve a reference to a secret for a named purpose.
///
/// `geode://` uses a running `geoded` socket. Set `GEODE_ENV_FALLBACK=1` to use
/// geode's `EnvBackend` mapping instead (development/tests only).
pub fn resolve_with_purpose(reference: &str, purpose: &str) -> Result<Option<String>> {
    if reference.is_empty() {
        return Ok(None);
    }
    if let Some(name) = reference.strip_prefix("env://") {
        validate_env_name(name)?;
        return read_env(name);
    }
    if !reference.starts_with("geode://") {
        bail!("unsupported credential provider; use env:// or geode://");
    }
    if std::env::var("GEODE_ENV_FALLBACK").as_deref() == Ok("1") {
        return read_env(&geode_env_name(reference)?);
    }
    read_geoded(reference, purpose)
}

fn read_env(name: &str) -> Result<Option<String>> {
    let value = std::env::var(name)
        .with_context(|| format!("read credential environment variable {name}"))?;
    ensure!(!value.is_empty(), "credential is empty");
    Ok(Some(value))
}

#[derive(serde::Serialize)]
struct LeaseRequest<'a> {
    op: &'static str,
    reference: &'a str,
    purpose: &'a str,
    ttl_ms: u64,
}

#[derive(serde::Deserialize)]
struct LeaseResponse {
    ok: bool,
    #[serde(default)]
    secret_b64: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

fn read_geoded(reference: &str, purpose: &str) -> Result<Option<String>> {
    let socket = std::env::var_os("GEODE_SOCKET")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| std::path::PathBuf::from(home).join(".geode/geode.sock"))
        })
        .context("neither GEODE_SOCKET nor HOME is set")?;
    let mut stream = UnixStream::connect(&socket).with_context(|| {
        format!(
            "connect geoded at {} (is it running? set GEODE_ENV_FALLBACK=1 for the env backend)",
            socket.display()
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let request = serde_json::to_vec(&LeaseRequest {
        op: "lease",
        reference,
        purpose,
        ttl_ms: 5_000,
    })?;
    stream.write_all(&request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    ensure!(!line.is_empty(), "geoded closed without a response");
    let response: LeaseResponse = serde_json::from_str(&line).context("parse geoded response")?;
    if !response.ok {
        bail!(
            "geoded refused the lease: {}",
            response.error.unwrap_or_else(|| "unknown error".into())
        );
    }
    let encoded = response
        .secret_b64
        .context("geoded response had no secret")?;
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
        .context("decode geoded secret")?;
    let value = String::from_utf8(bytes).context("geoded secret is not UTF-8")?;
    ensure!(!value.is_empty(), "credential is empty");
    Ok(Some(value))
}

/// The environment variable geode's `EnvBackend` reads for a reference.
///
/// Components are validated and mapped exactly like geode: letters/digits are
/// uppercased, every other byte becomes `_`.
pub fn geode_env_name(reference: &str) -> Result<String> {
    let rest = reference
        .strip_prefix("geode://")
        .context("not a geode reference")?;
    if rest.contains(['@', '?', '#']) {
        bail!("geode reference contains userinfo, query, or fragment syntax");
    }
    let mut parts = rest.split('/');
    let (Some(realm), Some(account), Some(slot), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        bail!("geode reference must be geode://<realm>/<account>/<slot>");
    };
    for (field, value) in [("realm", realm), ("account", account), ("slot", slot)] {
        if value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            bail!("invalid geode {field}");
        }
    }
    let raw = format!("GEODE_{realm}_{account}_{slot}");
    Ok(raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect())
}

fn validate_env_name(name: &str) -> Result<()> {
    let mut bytes = name.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        bail!("invalid environment variable name");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geode_references_map_to_the_env_backend_name() {
        assert_eq!(
            geode_env_name("geode://mail/work/imap").unwrap(),
            "GEODE_MAIL_WORK_IMAP"
        );
        assert_eq!(
            geode_env_name("geode://oauth/google.avery/access").unwrap(),
            "GEODE_OAUTH_GOOGLE_AVERY_ACCESS"
        );
        assert_eq!(
            geode_env_name("geode://a/b/test-secret").unwrap(),
            "GEODE_A_B_TEST_SECRET"
        );
        for invalid in [
            "geode://user:pass@a/b/c",
            "geode://a/b",
            "geode://a/b/c/d",
            "geode:///b/c",
            "geode://a/b/c?x=1",
        ] {
            assert!(geode_env_name(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn resolves_env_and_geode_without_echoing_the_secret() {
        let name = "UNIBUS_CREDENTIALS_TEST_TOKEN";
        // SAFETY: test-local variable with a unique name.
        unsafe { std::env::set_var(name, "secret-value") };
        // SAFETY: test-local variable, removed below.
        unsafe { std::env::set_var("GEODE_TEST_UNIT_TOKEN", "secret-value") };
        unsafe { std::env::set_var("GEODE_ENV_FALLBACK", "1") };
        assert_eq!(
            resolve(&format!("env://{name}")).unwrap().as_deref(),
            Some("secret-value")
        );
        assert_eq!(
            resolve("geode://test/unit/token").unwrap().as_deref(),
            Some("secret-value")
        );
        assert!(resolve("geode://test/unit/missing").is_err());
        assert!(resolve("vault://x/y/z").is_err());
        unsafe { std::env::remove_var(name) };
        unsafe { std::env::remove_var("GEODE_TEST_UNIT_TOKEN") };
        unsafe { std::env::remove_var("GEODE_ENV_FALLBACK") };
    }

    #[test]
    fn env_names_are_bounded_and_valid() {
        assert!(validate_env_name("MAIL_PASSWORD").is_ok());
        assert!(validate_env_name("9BAD").is_err());
        assert!(validate_env_name("bad-name").is_err());
    }
}
