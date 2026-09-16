//! v1 shared-token authentication, kept for compatibility until fpl-auth
//! assertion verification (`unibus::auth::OfflineVerifier`) ships upstream.
//! Constant-time comparison against a per-node token file, `0600` on write.

use anyhow::{Context, Result, anyhow, ensure};
use subtle::ConstantTimeEq;

/// Constant-time check of a presented token against the node's token file.
pub fn validate_token(token_file: &std::path::Path, producer_id: &str, token: &str) -> Result<()> {
    ensure!(
        token.trim().len() >= 32,
        "token too short for producer {producer_id}"
    );
    let expected = std::fs::read_to_string(token_file)
        .map_err(|e| anyhow!("read token file for {producer_id}: {e}"))?;
    let expected = expected.trim();
    ensure!(
        bool::from(token.trim().as_bytes().ct_eq(expected.as_bytes())),
        "invalid token for producer {producer_id}"
    );
    Ok(())
}

pub fn issue_token(token_path: &std::path::Path, token: &str) -> Result<()> {
    ensure!(
        token.trim().len() >= 32,
        "issued token must be at least 32 characters"
    );
    let dir = token_path
        .parent()
        .ok_or_else(|| anyhow!("token file needs parent directory"))?;
    std::fs::create_dir_all(dir)?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(token_path)?;
    use std::io::Write;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

/// Generate a 256-bit token from the operating system CSPRNG.
pub fn generate_token() -> Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).context("generate token from operating-system entropy")?;
    let mut token = String::with_capacity(bytes.len() * 2);
    use std::fmt::Write as _;
    for byte in bytes {
        write!(&mut token, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_256_bit_lowercase_hex() {
        let token = generate_token().unwrap();
        assert_eq!(token.len(), 64);
        assert!(
            token
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }
}
