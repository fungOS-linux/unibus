//! Optional reliable peer transport. Iroh authenticates endpoint keys;
//! verified Unibus assertions bind those keys to identities and message scope.
//! One FIN-delimited stream per frame avoids cross-frame cancellation damage.
use iroh::{Endpoint, EndpointAddr, endpoint::Connection};
use std::{
    collections::BTreeSet,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use unibus::{
    DirectSessionClaims, DirectSessionVerifier, Envelope, IrohPeerEndpoint, Message,
    PeerAccessPolicy, PeerCandidate, PeerError, PeerFuture, PeerSession, PeerTransport,
};

const AUTH_LIMIT: usize = 64 * 1024;
const HANDSHAKE: Duration = Duration::from_secs(15);
const IO_LIMIT: Duration = Duration::from_secs(5);
const READY: &str = "unibus.direct.ready.v1";

#[derive(Clone)]
pub struct SessionConfig {
    pub verifier: Arc<dyn DirectSessionVerifier>,
    pub audience: String,
    pub installation_id: String,
    pub local_producer: String,
    /// Local allowlist, not a discovered relay's assertion of trust.
    pub allowed_relays: BTreeSet<String>,
    /// Additional policy may deny, never authorize an unsigned session.
    pub access_policy: Option<Arc<dyn PeerAccessPolicy>>,
}

impl SessionConfig {
    fn verify(
        &self,
        assertion: &str,
        local_key: &str,
        remote_key: &str,
    ) -> Result<DirectSessionClaims, PeerError> {
        if assertion.len() > AUTH_LIMIT / 2 {
            return Err(error("session assertion exceeds bound"));
        }
        let now = now_s();
        let claims = self
            .verifier
            .verify(assertion, now)
            .map_err(|e| error(e.to_string()))?;
        claims
            .validate(
                &self.audience,
                &self.installation_id,
                &self.local_producer,
                now,
            )
            .map_err(|e| error(e.to_string()))?;
        let local = if claims.initiator.producer_id == self.local_producer {
            &claims.initiator
        } else {
            &claims.responder
        };
        let remote = claims
            .peer_for(&self.local_producer)
            .ok_or_else(|| error("local producer not authorized"))?;
        if local.key_binding != format!("iroh:{local_key}")
            || remote.key_binding != format!("iroh:{remote_key}")
            || now >= claims.exp
        {
            return Err(error("Iroh keys do not match authorized session peers"));
        }
        Ok(claims)
    }
}

pub struct IrohTransport {
    endpoint: Endpoint,
    config: SessionConfig,
}

impl IrohTransport {
    /// Endpoint lifecycle and its relay/DNS settings remain deployment-owned.
    /// Use Minimal, not N0, to avoid implicit public relay/DNS publication.
    pub fn new(endpoint: Endpoint, config: SessionConfig) -> Self {
        Self { endpoint, config }
    }
}

impl PeerTransport for IrohTransport {
    fn connect<'a>(
        &'a self,
        candidate: &'a PeerCandidate,
        assertion: &'a str,
    ) -> PeerFuture<'a, Box<dyn PeerSession>> {
        Box::pin(async move {
            candidate.validate(now_s())?;
            let mut failures = Vec::new();
            for hint in &candidate.iroh_endpoints {
                let claims = self.config.verify(
                    assertion,
                    &self.endpoint.id().to_string(),
                    &hint.endpoint_id,
                )?;
                let remote = claims.peer_for(&self.config.local_producer).unwrap();
                if remote.producer_id != candidate.peer_id {
                    return Err(error("candidate is not authorized peer"));
                }
                let address = address(hint, &self.config.allowed_relays)?;
                let result = tokio::time::timeout(HANDSHAKE, async {
                    let connection = self
                        .endpoint
                        .connect(address, crate::PEER_ALPN.as_bytes())
                        .await
                        .map_err(|e| error(e.to_string()))?;
                    let mut admission = AdmissionGuard(Some(connection.clone()));
                    let (mut send, mut recv) = connection
                        .open_bi()
                        .await
                        .map_err(|e| error(e.to_string()))?;
                    let hello = Envelope::new(
                        &self.config.installation_id,
                        &self.config.local_producer,
                        Some(&candidate.peer_id),
                        "",
                        Message::new(
                            unibus::direct::DIRECT_AUTH_KIND,
                            serde_json::json!({"assertion":assertion}),
                        ),
                    );
                    send.write_all(
                        &unibus::encode_frame(&hello, &[]).map_err(|e| error(e.to_string()))?,
                    )
                    .await
                    .map_err(|e| error(e.to_string()))?;
                    send.finish().map_err(|e| error(e.to_string()))?;
                    let bytes = recv
                        .read_to_end(AUTH_LIMIT)
                        .await
                        .map_err(|e| error(e.to_string()))?;
                    let (ack, payload) =
                        unibus::decode_frame(&bytes).map_err(|e| error(e.to_string()))?;
                    ack.validate(&self.config.installation_id, now_ms())
                        .map_err(|e| error(e.to_string()))?;
                    if !payload.is_empty()
                        || !ack.token.is_empty()
                        || ack.message.kind != READY
                        || ack.producer_id != candidate.peer_id
                        || ack.destination.as_deref() != Some(&self.config.local_producer)
                        || ack.message.body["session_id"] != claims.jti
                    {
                        return Err(error("invalid Iroh session admission acknowledgement"));
                    }
                    let session = IrohSession {
                        connection,
                        claims: claims.clone(),
                        config: self.config.clone(),
                        remote: candidate.peer_id.clone(),
                    };
                    session.authorize_admission(false).await?;
                    admission.0 = None;
                    Ok(Box::new(session) as Box<dyn PeerSession>)
                })
                .await;
                match result {
                    Ok(Ok(session)) => return Ok(session),
                    Ok(Err(e)) => failures.push(e.to_string()),
                    Err(_) => failures.push("Iroh handshake timed out".into()),
                }
            }
            Err(error(format!(
                "no Iroh candidate connected: {}",
                failures.join("; ")
            )))
        })
    }
}

pub struct IrohListener {
    endpoint: Endpoint,
    config: SessionConfig,
}

impl IrohListener {
    pub fn new(endpoint: Endpoint, config: SessionConfig) -> Self {
        Self { endpoint, config }
    }

    pub async fn accept(&self) -> Result<IrohSession, PeerError> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or_else(|| error("Iroh endpoint closed"))?;
        tokio::time::timeout(HANDSHAKE, async {
            let connection = incoming.await.map_err(|e| error(e.to_string()))?;
            let mut admission = AdmissionGuard(Some(connection.clone()));
            if connection.alpn() != crate::PEER_ALPN.as_bytes() {
                return Err(error("wrong Iroh application protocol"));
            }
            let (mut send, mut recv) = connection
                .accept_bi()
                .await
                .map_err(|e| error(e.to_string()))?;
            let bytes = recv
                .read_to_end(AUTH_LIMIT)
                .await
                .map_err(|e| error(e.to_string()))?;
            let (hello, payload) =
                unibus::decode_frame(&bytes).map_err(|e| error(e.to_string()))?;
            hello
                .validate(&self.config.installation_id, now_ms())
                .map_err(|e| error(e.to_string()))?;
            if !payload.is_empty()
                || !hello.token.is_empty()
                || hello.message.kind != unibus::direct::DIRECT_AUTH_KIND
                || hello.destination.as_deref() != Some(&self.config.local_producer)
            {
                return Err(error("invalid Iroh session authentication frame"));
            }
            let assertion = hello.message.body["assertion"]
                .as_str()
                .ok_or_else(|| error("missing session assertion"))?;
            let claims = self.config.verify(
                assertion,
                &self.endpoint.id().to_string(),
                &connection.remote_id().to_string(),
            )?;
            let remote = claims
                .peer_for(&self.config.local_producer)
                .unwrap()
                .producer_id
                .clone();
            if hello.producer_id != remote {
                return Err(error(
                    "authenticated producer differs from Iroh session claim",
                ));
            }
            let session = IrohSession {
                connection,
                claims,
                config: self.config.clone(),
                remote,
            };
            session.authorize_admission(true).await?;
            let ack = Envelope::new(
                &self.config.installation_id,
                &self.config.local_producer,
                Some(&session.remote),
                "",
                Message::new(READY, serde_json::json!({"session_id":session.claims.jti})),
            );
            send.write_all(&unibus::encode_frame(&ack, &[]).map_err(|e| error(e.to_string()))?)
                .await
                .map_err(|e| error(e.to_string()))?;
            send.finish().map_err(|e| error(e.to_string()))?;
            admission.0 = None;
            Ok(session)
        })
        .await
        .map_err(|_| error("Iroh session admission timed out"))?
    }
}

pub struct IrohSession {
    connection: Connection,
    claims: DirectSessionClaims,
    config: SessionConfig,
    remote: String,
}

struct AdmissionGuard(Option<Connection>);
impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        if let Some(connection) = &self.0 {
            connection.close(0u32.into(), b"Unibus admission rejected or canceled");
        }
    }
}

impl Drop for IrohSession {
    fn drop(&mut self) {
        self.connection.close(0u32.into(), b"Unibus session closed");
    }
}

impl IrohSession {
    async fn authorize_admission(&self, incoming: bool) -> Result<(), PeerError> {
        let (source, destination) = if incoming {
            (&self.remote, &self.config.local_producer)
        } else {
            (&self.config.local_producer, &self.remote)
        };
        for kind in &self.claims.kinds {
            self.policy(source, destination, kind).await?;
        }
        if now_s() >= self.claims.exp {
            return Err(error("session expired during admission"));
        }
        Ok(())
    }

    async fn policy(&self, source: &str, destination: &str, kind: &str) -> Result<(), PeerError> {
        if let Some(policy) = &self.config.access_policy {
            tokio::time::timeout(
                IO_LIMIT,
                policy.authorize(source, destination, kind, now_s()),
            )
            .await
            .map_err(|_| error("local access policy timed out"))??;
        }
        Ok(())
    }

    async fn authorize(
        &self,
        envelope: &Envelope,
        payload_len: usize,
        source: &str,
        destination: &str,
    ) -> Result<(), PeerError> {
        self.claims
            .authorize_frame(envelope, payload_len, source, now_ms())
            .map_err(|e| error(e.to_string()))?;
        self.policy(source, destination, &envelope.message.kind)
            .await?;
        self.claims
            .authorize_frame(envelope, payload_len, source, now_ms())
            .map_err(|e| error(e.to_string()))
    }
}

impl PeerSession for IrohSession {
    fn peer_id(&self) -> &str {
        &self.remote
    }

    fn send<'a>(&'a mut self, envelope: &'a Envelope, payload: &'a [u8]) -> PeerFuture<'a, ()> {
        Box::pin(async move {
            self.authorize(
                envelope,
                payload.len(),
                &self.config.local_producer,
                &self.remote,
            )
            .await?;
            let bytes =
                unibus::encode_frame(envelope, payload).map_err(|e| error(e.to_string()))?;
            let remaining = Duration::from_secs(self.claims.exp.saturating_sub(now_s()));
            tokio::time::timeout(IO_LIMIT.min(remaining), async {
                let mut stream = self
                    .connection
                    .open_uni()
                    .await
                    .map_err(|e| error(e.to_string()))?;
                stream
                    .write_all(&bytes)
                    .await
                    .map_err(|e| error(e.to_string()))?;
                stream.finish().map_err(|e| error(e.to_string()))
            })
            .await
            .map_err(|_| error("Iroh send timed out or session expired"))?
        })
    }

    fn receive(&mut self) -> PeerFuture<'_, (Envelope, Vec<u8>)> {
        Box::pin(async move {
            let remaining = Duration::from_secs(self.claims.exp.saturating_sub(now_s()));
            let bytes = tokio::time::timeout(IO_LIMIT.min(remaining), async {
                let mut stream = self
                    .connection
                    .accept_uni()
                    .await
                    .map_err(|e| error(e.to_string()))?;
                stream
                    .read_to_end(unibus::MAX_FRAME as usize)
                    .await
                    .map_err(|e| error(e.to_string()))
            })
            .await
            .map_err(|_| error("Iroh receive timed out or session expired"))??;
            let (envelope, payload) =
                unibus::decode_frame(&bytes).map_err(|e| error(e.to_string()))?;
            self.authorize(
                &envelope,
                payload.len(),
                &self.remote,
                &self.config.local_producer,
            )
            .await?;
            Ok((envelope, payload))
        })
    }
}

fn address(hint: &IrohPeerEndpoint, allowed: &BTreeSet<String>) -> Result<EndpointAddr, PeerError> {
    hint.validate()?;
    let mut addr = EndpointAddr::new(
        hint.endpoint_id
            .parse()
            .map_err(|_| error("invalid Iroh public key"))?,
    );
    for ip in &hint.direct_addresses {
        addr = addr.with_ip_addr(*ip);
    }
    for relay in &hint.relay_urls {
        let url = url::Url::parse(relay).map_err(|_| error("invalid Iroh relay URL"))?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
            || !allowed.contains(relay)
        {
            return Err(error("Iroh relay is unsafe or not locally allowed"));
        }
        addr = addr.with_relay_url(
            relay
                .parse()
                .map_err(|_| error("invalid Iroh relay origin"))?,
        );
    }
    Ok(addr)
}

fn now_s() -> u64 {
    now_ms() / 1000
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn error(message: impl Into<String>) -> PeerError {
    PeerError(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    // Test fixture only: production must inject a cryptographic verifier.
    struct FixtureVerifier;
    impl DirectSessionVerifier for FixtureVerifier {
        fn verify(
            &self,
            assertion: &str,
            _: u64,
        ) -> Result<DirectSessionClaims, unibus::DirectSessionError> {
            serde_json::from_str(assertion).map_err(|e| unibus::DirectSessionError(e.to_string()))
        }
    }
    async fn endpoint() -> Endpoint {
        Endpoint::builder(iroh::endpoint::presets::Minimal)
            .relay_mode(iroh::RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .alpns(vec![crate::PEER_ALPN.as_bytes().to_vec()])
            .bind()
            .await
            .unwrap()
    }
    fn config(producer: &str) -> SessionConfig {
        SessionConfig {
            verifier: Arc::new(FixtureVerifier),
            audience: "test".into(),
            installation_id: "test".into(),
            local_producer: producer.into(),
            allowed_relays: BTreeSet::new(),
            access_policy: None,
        }
    }
    #[tokio::test]
    async fn encrypted_loopback_round_trip_and_scope_denial() {
        let a = endpoint().await;
        let b = endpoint().await;
        let binding = |producer: &str, endpoint: &Endpoint| unibus::direct::PeerBinding {
            producer_id: producer.into(),
            key_binding: format!("iroh:{}", endpoint.id()),
            candidates: vec![],
        };
        let claims = DirectSessionClaims {
            schema_version: 1,
            iss: "fixture".into(),
            aud: "test".into(),
            jti: "test-session".into(),
            installation_id: "test".into(),
            iat: now_s(),
            exp: now_s() + 60,
            initiator: binding("a", &a),
            responder: binding("b", &b),
            kinds: vec!["test.packet.v1".into()],
            max_payload_bytes: 1024,
        };
        let candidate = PeerCandidate {
            peer_id: "b".into(),
            site: None,
            endpoints: vec![],
            expires_at_unix_s: now_s() + 60,
            iroh_endpoints: vec![IrohPeerEndpoint {
                endpoint_id: b.id().to_string(),
                direct_addresses: b.addr().ip_addrs().copied().collect(),
                relay_urls: vec![],
            }],
        };
        let transport = IrohTransport::new(a.clone(), config("a"));
        let listener = IrohListener::new(b.clone(), config("b"));
        let assertion = serde_json::to_string(&claims).unwrap();
        let (client, server) =
            tokio::join!(transport.connect(&candidate, &assertion), listener.accept());
        let mut client = client.unwrap();
        let mut server = server.unwrap();
        let mut message = Envelope::new(
            "test",
            "a",
            Some("b"),
            "",
            Message::new("test.packet.v1", serde_json::json!({})),
        );
        message.payload_bytes = 3;
        client.send(&message, &[0, 128, 255]).await.unwrap();
        assert_eq!(server.receive().await.unwrap().1, vec![0, 128, 255]);
        let reply = Envelope::new(
            "test",
            "b",
            Some("a"),
            "",
            Message::new("test.packet.v1", serde_json::json!({})),
        );
        server.send(&reply, &[]).await.unwrap();
        assert_eq!(client.receive().await.unwrap().0.producer_id, "b");
        message.destination = Some("intruder".into());
        assert!(client.send(&message, &[0, 128, 255]).await.is_err());
        drop(client);
        drop(server);
        a.close().await;
        b.close().await;
    }
    #[test]
    fn discovered_relays_do_not_grant_trust() {
        let hint = IrohPeerEndpoint {
            endpoint_id: iroh::SecretKey::generate().public().to_string(),
            direct_addresses: vec![],
            relay_urls: vec!["https://relay.example/".into()],
        };
        assert!(address(&hint, &BTreeSet::new()).is_err());
        let allowed = BTreeSet::from(["https://relay.example/".into()]);
        assert!(address(&hint, &allowed).is_ok());
    }
}
