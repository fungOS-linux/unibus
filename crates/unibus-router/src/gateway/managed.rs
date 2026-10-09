//! Optional Mycelium authority adapter. Local rules bind exact resources/actions;
//! Mycelium owns cryptographic validation and revocation, never Unibus traffic.
use super::GatewayAuthority;
use anyhow::{Result, ensure};
use serde::Deserialize;
use std::{collections::BTreeSet, path::PathBuf, sync::Mutex};
use unibus::gateway::{GatewayInvoke, ResourceId, identifier};
use unibus_mycelium::access::MyceliumAccessAuthority;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedConfig {
    pub socket: PathBuf,
    pub rules: Vec<Binding>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub caller_producer: String,
    pub auth_subject: String,
    pub resource: ResourceId,
    pub operation: String,
    pub source_peer: String,
    pub destination_peer: String,
    pub principal: String,
    pub scope: String,
}
pub struct MyceliumGatewayAuthority {
    config: ManagedConfig,
    queries: Mutex<()>,
}
impl MyceliumGatewayAuthority {
    pub fn new(config: ManagedConfig) -> Result<Self> {
        ensure!(
            config.socket.is_absolute() && !config.rules.is_empty() && config.rules.len() <= 512,
            "invalid managed gateway configuration"
        );
        let mut seen = BTreeSet::new();
        for rule in &config.rules {
            rule.resource.validate()?;
            for name in [
                &rule.caller_producer,
                &rule.operation,
                &rule.source_peer,
                &rule.destination_peer,
            ] {
                identifier(name)?;
            }
            for name in [&rule.auth_subject, &rule.principal, &rule.scope] {
                ensure!(
                    !name.trim().is_empty() && name.len() <= 512,
                    "invalid managed gateway identity/scope"
                );
            }
            ensure!(
                seen.insert((
                    &rule.caller_producer,
                    &rule.auth_subject,
                    &rule.resource,
                    &rule.operation
                )),
                "ambiguous managed gateway binding"
            );
        }
        Ok(Self {
            config,
            queries: Mutex::new(()),
        })
    }
}
impl GatewayAuthority for MyceliumGatewayAuthority {
    fn grant_expiry(
        &self,
        source: &str,
        subject: &str,
        invocation: &GatewayInvoke,
        now: u64,
    ) -> Result<u64> {
        let rule = self
            .config
            .rules
            .iter()
            .find(|rule| {
                rule.caller_producer == source
                    && rule.auth_subject == subject
                    && rule.resource == invocation.resource
                    && rule.operation == invocation.operation
            })
            .ok_or_else(|| anyhow::anyhow!("no managed resource binding"))?;
        // No queued IPC backlog during outages; callers retry explicitly.
        let _query = self
            .queries
            .try_lock()
            .map_err(|_| anyhow::anyhow!("gateway authority busy/unavailable"))?;
        MyceliumAccessAuthority::new(self.config.socket.clone()).peer_scope_expiry(
            &rule.source_peer,
            &rule.destination_peer,
            &rule.principal,
            &rule.scope,
            now,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_rules_and_missing_bindings_deny() {
        let rule = serde_json::json!({"caller_producer":"caller","auth_subject":"subject","resource":{"scope":{"kind":"peer","id":"host"},"key":"canvas"},"operation":"canvas.inspect","source_peer":"neo","destination_peer":"host","principal":"peer:neo","scope":"canvas:read:host"});
        let config = serde_json::from_value(
            serde_json::json!({"socket":"/nonexistent.sock","rules":[rule.clone(),rule.clone()]}),
        )
        .unwrap();
        assert!(MyceliumGatewayAuthority::new(config).is_err());
        let config = serde_json::from_value(
            serde_json::json!({"socket":"/nonexistent.sock","rules":[rule]}),
        )
        .unwrap();
        let authority = MyceliumGatewayAuthority::new(config).unwrap();
        let invoke: GatewayInvoke = serde_json::from_value(serde_json::json!({"schema_version":1,"request_id":"test-1","resource":{"scope":{"kind":"peer","id":"host"},"key":"canvas"},"generation":1,"operation":"canvas.inspect","input":{}})).unwrap();
        assert!(
            authority
                .grant_expiry("other", "subject", &invoke, 100)
                .is_err()
        );
        assert!(
            authority
                .grant_expiry("caller", "subject", &invoke, 100)
                .is_err()
        );
    }
    #[cfg(unix)]
    #[test]
    fn existing_mycelium_rpc_supplies_expiry_and_revocation() {
        use std::{
            io::{BufRead, BufReader, Write},
            os::unix::net::UnixListener,
        };
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("authority.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            for active in [true, false] {
                for method in ["peer_list", "access_list"] {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut line = String::new();
                    BufReader::new(stream.try_clone().unwrap())
                        .read_line(&mut line)
                        .unwrap();
                    assert_eq!(
                        serde_json::from_str::<serde_json::Value>(&line).unwrap()["method"],
                        method
                    );
                    let result = if method == "peer_list" {
                        serde_json::json!([{"origin":"neo","hello":{"node_id":"neo"}}, {"origin":"host","hello":{"node_id":"host"}}])
                    } else {
                        serde_json::json!({"grants":[{"active":active,"revoked_by":[],"record":{"statement":{"kind":"grant","principal":"peer:neo","not_before":90,"not_after":200,"scopes":["canvas:read:host"]}}}]})
                    };
                    writeln!(stream, "{}", serde_json::json!({"ok":true,"result":result})).unwrap();
                }
            }
        });
        let config = serde_json::from_value(serde_json::json!({"socket":socket,"rules":[{"caller_producer":"caller","auth_subject":"subject","resource":{"scope":{"kind":"peer","id":"host"},"key":"canvas"},"operation":"canvas.inspect","source_peer":"neo","destination_peer":"host","principal":"peer:neo","scope":"canvas:read:host"}]})).unwrap();
        let authority = MyceliumGatewayAuthority::new(config).unwrap();
        let invoke: GatewayInvoke = serde_json::from_value(serde_json::json!({"schema_version":1,"request_id":"test-1","resource":{"scope":{"kind":"peer","id":"host"},"key":"canvas"},"generation":1,"operation":"canvas.inspect","input":{}})).unwrap();
        assert_eq!(
            authority
                .grant_expiry("caller", "subject", &invoke, 100)
                .unwrap(),
            200
        );
        assert!(
            authority
                .grant_expiry("caller", "subject", &invoke, 100)
                .is_err()
        );
        server.join().unwrap();
    }
}
