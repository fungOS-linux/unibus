//! Operator-authorized exact-call grants. Pipe stdout directly to the MCP
//! endpoint: it contains a short-lived capability, never log or commit it.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    io::Read,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

fn now_ms() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

fn request(
    policy_path: &Path,
    provider: &str,
    tool: &str,
    arguments: Value,
    now: u64,
) -> Result<Value> {
    struct Lock(std::path::PathBuf);
    impl Drop for Lock {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let lock_path = policy_path.with_extension("grant-lock");
    let lock_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&lock_path)
        .context("another operator is issuing a grant; inspect any stale lock before retrying")?;
    let _lock = Lock(lock_path);
    drop(lock_file);
    ensure!(arguments.is_object(), "arguments must be an object");
    ensure!(
        !provider.is_empty() && !tool.is_empty(),
        "provider and tool are required"
    );
    let metadata = fs::symlink_metadata(policy_path)?;
    ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o077 == 0,
        "policy must be an existing private regular file"
    );
    ensure!(metadata.len() <= 1_048_576, "policy too large");
    let bytes = fs::read(policy_path)?;
    let mut policy: Value =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid policy"))?;
    ensure!(policy["schema_version"] == 1, "unsupported policy version");
    let grants = policy["grants"].as_array_mut().context("missing grants")?;
    grants.retain(|g| {
        g["expires_at_unix_ms"]
            .as_u64()
            .is_some_and(|expiry| expiry > now)
    });
    ensure!(grants.len() < 256, "too many grants");
    let mut entropy = [0u8; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut entropy)?;
    let token = format!(
        "unibus-scoped-v1:{}",
        entropy
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    grants.push(json!({"encoded":token,"provider_id":provider,"tool_name":tool,
        "arguments":arguments,"issued_at_unix_ms":now,"expires_at_unix_ms":now.checked_add(300_000).context("time overflow")?}));
    let parent = policy_path.parent().context("policy needs parent")?;
    let staged = parent.join(format!(".scoped-grant-{}.json", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staged)?;
    serde_json::to_writer(&mut file, &policy)?;
    file.sync_all()?;
    // Fail on another operator's concurrent edit rather than clobbering grants.
    ensure!(
        fs::read(policy_path)? == bytes,
        "policy changed during grant issuance"
    );
    fs::rename(staged, policy_path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(
        json!({"jsonrpc":"2.0","id":now,"method":"tools/call","params":{
        "name":format!("{}__{tool}",provider.replace('-',"_")),"arguments":arguments,
        "_meta":{"unibus_authorization":token}}}),
    )
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ensure!(args.len() == 4,
        "usage: unibus-mcp-authorize POLICY_FILE PROVIDER TOOL ARGUMENTS_JSON_FILE (pipe stdout to MCP)");
    let arguments: Value = serde_json::from_slice(&fs::read(&args[3])?)?;
    let output = request(
        Path::new(&args[0]),
        &args[1],
        &args[2],
        arguments,
        now_ms()?,
    )?;
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grant_is_private_exact_and_short_lived() {
        let directory =
            std::env::temp_dir().join(format!("unibus-grant-test-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("policy.json");
        fs::write(&path, b"{\"schema_version\":1,\"grants\":[]}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let arguments = json!({"channelId":"selected","content":"approved"});
        let rpc = request(
            &path,
            "fungos-discord",
            "discord_send_message",
            arguments.clone(),
            1000,
        )
        .unwrap();
        let policy: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(policy["grants"][0]["arguments"], arguments);
        assert_eq!(policy["grants"][0]["expires_at_unix_ms"], 301000);
        assert_eq!(
            rpc["params"]["_meta"]["unibus_authorization"],
            policy["grants"][0]["encoded"]
        );
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        assert!(request(&path, "x", "y", Value::Null, 1000).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(request(&path, "x", "y", json!({}), 1000).is_err());
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
