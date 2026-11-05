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

/// Resolve a reference to a secret. An empty reference means "no secret".
pub fn resolve(reference: &str) -> Result<Option<String>> {
    if reference.is_empty() {
        return Ok(None);
    }
    let name = if let Some(name) = reference.strip_prefix("env://") {
        validate_env_name(name)?;
        name.to_owned()
    } else if reference.starts_with("geode://") {
        geode_env_name(reference)?
    } else {
        bail!("unsupported credential provider; use env:// or geode://");
    };
    let value = std::env::var(&name)
        .with_context(|| format!("read credential environment variable {name}"))?;
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
    }

    #[test]
    fn env_names_are_bounded_and_valid() {
        assert!(validate_env_name("MAIL_PASSWORD").is_ok());
        assert!(validate_env_name("9BAD").is_err());
        assert!(validate_env_name("bad-name").is_err());
    }
}
