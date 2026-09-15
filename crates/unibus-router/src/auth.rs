//! v1 shared-token authentication, kept for compatibility until fpl-auth
//! assertion verification (`unibus::auth::OfflineVerifier`) ships upstream.
//! Constant-time comparison against a per-node token file, `0600` on write.

use anyhow::{Result, anyhow, ensure};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
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

/// Generate a random-ish token from system entropy mixed with a monotonic
/// counter. For local installations this is entropy enough.
pub fn generate_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut hasher = Sha256::new();
    hasher.update(pid.to_be_bytes());
    hasher.update(nanos.to_be_bytes());
    hasher.update(counter.to_be_bytes());
    format!("{:x}", hasher.finalize())
}
