//! Consume Mycelium's authority-filtered access view; do not duplicate signatures or revocation.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{path::PathBuf, time::Duration};

pub struct MyceliumAccessAuthority {
    socket: PathBuf,
}
impl MyceliumAccessAuthority {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    /// Peer IDs must be canonical IDs, not hostnames or display-name aliases.
    pub fn require_peer_scope(
        &self,
        source: &str,
        destination: &str,
        principal: &str,
        scope: &str,
        now_unix_s: u64,
    ) -> Result<()> {
        self.peer_scope_expiry(source, destination, principal, scope, now_unix_s)
            .map(|_| ())
    }

    /// Return the authority-filtered grant's expiry, so asynchronous consumers
    /// can reject a decision which expires while the IPC query is in flight.
    pub fn peer_scope_expiry(
        &self,
        source: &str,
        destination: &str,
        principal: &str,
        scope: &str,
        now_unix_s: u64,
    ) -> Result<u64> {
        ensure!(
            !source.is_empty()
                && !destination.is_empty()
                && !principal.is_empty()
                && !scope.is_empty(),
            "empty peer ACL binding"
        );
        let peers = self.request("peer_list")?;
        let nodes = peers
            .as_array()
            .context("Mycelium peer view is not an array")?;
        for peer in [source, destination] {
            ensure!(
                nodes
                    .iter()
                    .any(|n| n.get("origin").and_then(Value::as_str) == Some(peer)
                        && n.pointer("/hello/node_id").and_then(Value::as_str) == Some(peer)),
                "Mycelium does not know canonical peer {peer}"
            );
        }
        let access = self.request("access_list")?;
        permitted_until(&access, principal, scope, now_unix_s)?
            .context("Mycelium access scope denied")
    }

    #[cfg(unix)]
    fn request(&self, method: &str) -> Result<Value> {
        use std::{
            io::{BufRead, BufReader, Read, Write},
            os::unix::net::UnixStream,
        };
        let mut stream =
            UnixStream::connect(&self.socket).context("connect Mycelium access authority")?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        serde_json::to_writer(&mut stream, &serde_json::json!({"method": method}))?;
        stream.write_all(b"\n")?;
        let mut line = String::new();
        BufReader::new(stream)
            .take(1_048_577)
            .read_line(&mut line)?;
        ensure!(
            line.len() <= 1_048_576 && line.ends_with('\n'),
            "invalid or oversized Mycelium authority response"
        );
        let response: Value = serde_json::from_str(&line)?;
        ensure!(
            response.get("ok").and_then(Value::as_bool) == Some(true),
            "Mycelium authority request failed"
        );
        response
            .get("result")
            .cloned()
            .context("missing Mycelium authority result")
    }
    #[cfg(not(unix))]
    fn request(&self, _: &str) -> Result<Value> {
        anyhow::bail!("Mycelium authority requires Unix IPC")
    }
}

#[cfg(test)]
fn permits(view: &Value, principal: &str, scope: &str, now: u64) -> Result<bool> {
    Ok(permitted_until(view, principal, scope, now)?.is_some())
}

fn permitted_until(view: &Value, principal: &str, scope: &str, now: u64) -> Result<Option<u64>> {
    let grants = view
        .get("grants")
        .and_then(Value::as_array)
        .context("invalid Mycelium access view")?;
    Ok(grants
        .iter()
        .filter(|grant| {
            let statement = &grant["record"]["statement"];
            grant["active"].as_bool() == Some(true)
                && grant["revoked_by"].as_array().is_some_and(|r| r.is_empty())
                && statement["kind"].as_str() == Some("grant")
                && statement["principal"].as_str() == Some(principal)
                && statement["not_before"].as_u64().is_some_and(|t| t <= now)
                && statement["not_after"].as_u64().is_some_and(|t| now < t)
                && statement["scopes"]
                    .as_array()
                    .is_some_and(|scopes| scopes.iter().any(|s| s.as_str() == Some(scope)))
        })
        .filter_map(|grant| grant["record"]["statement"]["not_after"].as_u64())
        .max())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn view() -> Value {
        serde_json::json!({"grants":[{"active":true,"revoked_by":[],"record":{"statement":{"kind":"grant","principal":"peer:neo","not_before":10,"not_after":20,"scopes":["unibus:re-analysis:studio"]}}}]})
    }
    #[test]
    fn broad_scope_does_not_depend_on_tool_names() {
        assert!(permits(&view(), "peer:neo", "unibus:re-analysis:studio", 15).unwrap());
        assert!(!permits(&view(), "peer:other", "unibus:re-analysis:studio", 15).unwrap());
        assert!(!permits(&view(), "peer:neo", "unibus:admin:studio", 15).unwrap());
    }
    #[test]
    fn inactive_revoked_expired_and_missing_views_deny() {
        for now in [9, 20] {
            assert!(!permits(&view(), "peer:neo", "unibus:re-analysis:studio", now).unwrap());
        }
        let mut value = view();
        value["grants"][0]["active"] = false.into();
        assert!(!permits(&value, "peer:neo", "unibus:re-analysis:studio", 15).unwrap());
        value = view();
        value["grants"][0]["revoked_by"] = serde_json::json!(["revoke"]);
        assert!(!permits(&value, "peer:neo", "unibus:re-analysis:studio", 15).unwrap());
        assert!(permits(&serde_json::json!({}), "peer:neo", "scope", 15).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn authority_queries_existing_rpc_and_rejects_unknown_peers() {
        use std::{
            io::{BufRead, BufReader, Write},
            os::unix::net::UnixListener,
            sync::atomic::{AtomicU64, Ordering},
        };
        static SEQ: AtomicU64 = AtomicU64::new(0);
        for known in [true, false] {
            let path = std::env::temp_dir().join(format!(
                "ub-acl-{}-{}.sock",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let listener = UnixListener::bind(&path).unwrap();
            let server = std::thread::spawn(move || {
                let methods = if known {
                    vec!["peer_list", "access_list"]
                } else {
                    vec!["peer_list"]
                };
                for method in methods {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut line = String::new();
                    BufReader::new(stream.try_clone().unwrap())
                        .read_line(&mut line)
                        .unwrap();
                    assert_eq!(
                        serde_json::from_str::<Value>(&line).unwrap()["method"],
                        method
                    );
                    let result = if method == "peer_list" {
                        serde_json::json!([{"origin":"neo","hello":{"node_id":"neo"}}, {"origin":"studio","hello":{"node_id":"studio"}}])
                    } else {
                        view()
                    };
                    writeln!(stream, "{}", serde_json::json!({"ok":true,"result":result})).unwrap();
                }
            });
            let result = MyceliumAccessAuthority::new(path.clone()).require_peer_scope(
                if known { "neo" } else { "spoofed" },
                "studio",
                "peer:neo",
                "unibus:re-analysis:studio",
                15,
            );
            assert_eq!(result.is_ok(), known);
            server.join().unwrap();
            std::fs::remove_file(path).unwrap();
        }
    }
}
