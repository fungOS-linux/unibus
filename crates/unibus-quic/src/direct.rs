//! mTLS-bound direct Unibus sessions.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use unibus::{
    DirectSessionClaims, DirectSessionVerifier, Envelope, Message, NetworkEndpoint, PeerCandidate,
    PeerError, PeerFuture, PeerSession, PeerTransport, Transport,
};

use crate::{QuicEndpoint, QuicPeer, certificate_key_binding};

pub const DIRECT_AUTH_KIND: &str = "unibus.direct.authenticate.v1";

pub struct DirectQuicTransport {
    bind_address: SocketAddr,
    client_config: quinn::ClientConfig,
    verifier: Arc<dyn DirectSessionVerifier>,
    expected_audience: String,
    installation_id: String,
    local_producer: String,
}

impl DirectQuicTransport {
    pub fn new(
        bind_address: SocketAddr,
        client_config: quinn::ClientConfig,
        verifier: Arc<dyn DirectSessionVerifier>,
        expected_audience: impl Into<String>,
        installation_id: impl Into<String>,
        local_producer: impl Into<String>,
    ) -> Self {
        Self {
            bind_address,
            client_config,
            verifier,
            expected_audience: expected_audience.into(),
            installation_id: installation_id.into(),
            local_producer: local_producer.into(),
        }
    }
}

impl PeerTransport for DirectQuicTransport {
    fn connect<'a>(
        &'a self,
        candidate: &'a PeerCandidate,
        session_assertion: &'a str,
    ) -> PeerFuture<'a, Box<dyn PeerSession>> {
        Box::pin(async move {
            let now = now_unix_s();
            candidate.validate(now)?;
            let claims = self
                .verifier
                .verify(session_assertion, now)
                .map_err(|error| PeerError(error.to_string()))?;
            claims
                .validate(
                    &self.expected_audience,
                    &self.installation_id,
                    &self.local_producer,
                    now,
                )
                .map_err(|error| PeerError(error.to_string()))?;
            let remote = claims
                .peer_for(&self.local_producer)
                .ok_or_else(|| PeerError("local producer is not in direct session".into()))?;
            let remote_producer = remote.producer_id.clone();
            let remote_key_binding = remote.key_binding.clone();
            if remote_producer != candidate.peer_id {
                return Err(PeerError("candidate does not match authorized peer".into()));
            }

            let mut failures = Vec::new();
            for endpoint in candidate
                .endpoints
                .iter()
                .filter(|endpoint| endpoint.transport == Transport::Quic)
            {
                match connect_endpoint(self.bind_address, self.client_config.clone(), endpoint)
                    .await
                {
                    Ok((owner, peer)) => {
                        let certificate = peer
                            .peer_certificate()
                            .map_err(|error| PeerError(error.to_string()))?;
                        if certificate_key_binding(&certificate) != remote_key_binding {
                            return Err(PeerError(
                                "peer TLS certificate does not match session key binding".into(),
                            ));
                        }
                        let handshake = handshake(
                            &self.installation_id,
                            &self.local_producer,
                            &remote_producer,
                            session_assertion,
                        );
                        peer.send(&handshake, &[]).await.map_err(|error| {
                            PeerError(format!("send direct authentication: {error}"))
                        })?;
                        return Ok(Box::new(DirectQuicSession {
                            _endpoint: owner,
                            peer,
                            claims,
                            local_producer: self.local_producer.clone(),
                            remote_producer,
                        }) as Box<dyn PeerSession>);
                    }
                    Err(error) => failures.push(error.to_string()),
                }
            }
            Err(PeerError(format!(
                "no direct QUIC candidate connected: {}",
                failures.join("; ")
            )))
        })
    }
}

pub struct DirectQuicListener {
    endpoint: Arc<QuicEndpoint>,
    verifier: Arc<dyn DirectSessionVerifier>,
    expected_audience: String,
    installation_id: String,
    local_producer: String,
}

impl DirectQuicListener {
    pub fn bind(
        address: SocketAddr,
        server_config: quinn::ServerConfig,
        verifier: Arc<dyn DirectSessionVerifier>,
        expected_audience: impl Into<String>,
        installation_id: impl Into<String>,
        local_producer: impl Into<String>,
    ) -> Result<Self> {
        let endpoint = Arc::new(QuicEndpoint::server(address, server_config)?);
        Ok(Self {
            endpoint,
            verifier,
            expected_audience: expected_audience.into(),
            installation_id: installation_id.into(),
            local_producer: local_producer.into(),
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    pub async fn accept(&self) -> Result<DirectQuicSession> {
        let peer = self.endpoint.accept().await?;
        let (envelope, payload) = peer.recv().await?;
        ensure!(
            payload.is_empty(),
            "direct authentication carried a payload"
        );
        ensure!(
            envelope.message.kind == DIRECT_AUTH_KIND,
            "first direct frame was not authentication"
        );
        let assertion = envelope
            .message
            .body
            .get("assertion")
            .and_then(|value| value.as_str())
            .context("direct authentication has no assertion")?;
        let now = now_unix_s();
        let claims = self.verifier.verify(assertion, now)?;
        claims.validate(
            &self.expected_audience,
            &self.installation_id,
            &self.local_producer,
            now,
        )?;
        let remote = claims
            .peer_for(&self.local_producer)
            .context("local producer is not in direct session")?;
        let remote_producer = remote.producer_id.clone();
        let remote_key_binding = remote.key_binding.clone();
        ensure!(
            envelope.installation_id == self.installation_id
                && envelope.producer_id == remote_producer
                && envelope.destination.as_deref() == Some(self.local_producer.as_str()),
            "direct authentication identity mismatch"
        );
        let certificate = peer.peer_certificate()?;
        ensure!(
            certificate_key_binding(&certificate) == remote_key_binding,
            "peer TLS certificate does not match session key binding"
        );
        Ok(DirectQuicSession {
            _endpoint: self.endpoint.clone(),
            peer,
            claims,
            local_producer: self.local_producer.clone(),
            remote_producer,
        })
    }
}

pub struct DirectQuicSession {
    _endpoint: Arc<QuicEndpoint>,
    peer: QuicPeer,
    claims: DirectSessionClaims,
    local_producer: String,
    remote_producer: String,
}

impl PeerSession for DirectQuicSession {
    fn peer_id(&self) -> &str {
        &self.remote_producer
    }

    fn send<'a>(&'a mut self, envelope: &'a Envelope, payload: &'a [u8]) -> PeerFuture<'a, ()> {
        Box::pin(async move {
            self.authorize(envelope, payload, &self.local_producer)?;
            self.peer
                .send(envelope, payload)
                .await
                .map_err(|error| PeerError(format!("send direct frame: {error}")))?;
            Ok(())
        })
    }

    fn receive(&mut self) -> PeerFuture<'_, (Envelope, Vec<u8>)> {
        Box::pin(async move {
            let (envelope, payload) = self
                .peer
                .recv()
                .await
                .map_err(|error| PeerError(format!("receive direct frame: {error}")))?;
            self.authorize(&envelope, &payload, &self.remote_producer)?;
            Ok((envelope, payload))
        })
    }
}

impl DirectQuicSession {
    fn authorize(
        &self,
        envelope: &Envelope,
        payload: &[u8],
        expected_producer: &str,
    ) -> Result<(), PeerError> {
        if envelope.installation_id != self.claims.installation_id
            || envelope.producer_id != expected_producer
            || !self.claims.kinds.contains(&envelope.message.kind)
            || payload.len() > self.claims.max_payload_bytes as usize
            || !envelope.token.is_empty()
        {
            return Err(PeerError(
                "frame exceeds direct-session identity or scope".into(),
            ));
        }
        envelope
            .validate(&self.claims.installation_id, now_unix_ms())
            .map_err(|error| PeerError(error.to_string()))
    }
}

async fn connect_endpoint(
    bind_address: SocketAddr,
    config: quinn::ClientConfig,
    endpoint: &NetworkEndpoint,
) -> Result<(Arc<QuicEndpoint>, QuicPeer)> {
    let server_name = endpoint
        .server_name
        .as_deref()
        .unwrap_or(endpoint.host.as_str());
    let addresses = if let Ok(ip) = endpoint.host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, endpoint.port)]
    } else {
        (endpoint.host.as_str(), endpoint.port)
            .to_socket_addrs()?
            .collect()
    };
    let owner = Arc::new(QuicEndpoint::client(bind_address, config)?);
    let mut failure = None;
    for address in addresses {
        match owner.connect(address, server_name).await {
            Ok(peer) => return Ok((owner, peer)),
            Err(error) => failure = Some(error),
        }
    }
    Err(failure.unwrap_or_else(|| anyhow::anyhow!("endpoint resolved no addresses")))
}

fn handshake(
    installation_id: &str,
    local_producer: &str,
    remote_producer: &str,
    assertion: &str,
) -> Envelope {
    Envelope::new(
        installation_id,
        local_producer,
        Some(remote_producer),
        "",
        Message::new(
            DIRECT_AUTH_KIND,
            serde_json::json!({"assertion": assertion}),
        ),
    )
}

fn now_unix_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::RootCertStore;
    use rustls::pki_types::PrivateKeyDer;
    use unibus::{DirectSessionError, PeerBinding};

    use crate::{certificate_key_binding, mutual_client_config, mutual_server_config};

    struct JsonVerifier;

    impl DirectSessionVerifier for JsonVerifier {
        fn verify(
            &self,
            assertion: &str,
            _now_unix_s: u64,
        ) -> Result<DirectSessionClaims, DirectSessionError> {
            serde_json::from_str(assertion)
                .map_err(|error| DirectSessionError(format!("invalid test assertion: {error}")))
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn direct_mtls_round_trip_binds_claims_and_skips_hub() {
        let CertifiedKey {
            cert: server_cert,
            signing_key: server_key,
        } = generate_simple_self_signed(vec!["server.test".into()]).unwrap();
        let CertifiedKey {
            cert: client_cert,
            signing_key: client_key,
        } = generate_simple_self_signed(vec!["client.test".into()]).unwrap();

        let mut client_roots = RootCertStore::empty();
        client_roots.add(client_cert.der().clone()).unwrap();
        let listener = DirectQuicListener::bind(
            "127.0.0.1:0".parse().unwrap(),
            mutual_server_config(
                vec![server_cert.der().clone()],
                PrivateKeyDer::Pkcs8(server_key.serialize_der().into()),
                client_roots,
            )
            .unwrap(),
            Arc::new(JsonVerifier),
            "unibus:direct:test",
            "test",
            "server",
        )
        .unwrap();
        let server_address = listener.local_addr().unwrap();

        let now = now_unix_s();
        let endpoint = |producer: &str, key_binding: String| PeerBinding {
            producer_id: producer.into(),
            key_binding,
            candidates: vec![NetworkEndpoint {
                transport: Transport::Quic,
                host: format!("{producer}.test"),
                port: 18792,
                server_name: None,
                priority: 0,
                weight: 0,
            }],
        };
        let claims = DirectSessionClaims {
            schema_version: unibus::direct::DIRECT_SESSION_SCHEMA_V1,
            iss: "test-authority".into(),
            aud: "unibus:direct:test".into(),
            jti: "direct-test-1".into(),
            installation_id: "test".into(),
            iat: now,
            exp: now + 60,
            initiator: endpoint("client", certificate_key_binding(client_cert.der())),
            responder: endpoint("server", certificate_key_binding(server_cert.der())),
            kinds: vec!["test.message.v1".into()],
            max_payload_bytes: 1024,
        };
        let assertion = serde_json::to_string(&claims).unwrap();

        let server = tokio::spawn(async move {
            let mut session = listener.accept().await.unwrap();
            let (received, payload) = session.receive().await.unwrap();
            assert_eq!(received.message.kind, "test.message.v1");
            assert_eq!(payload, b"hello");
            let mut reply = Envelope::new(
                "test",
                "server",
                Some("client"),
                "",
                Message::new("test.message.v1", serde_json::Value::Null),
            );
            reply.payload_bytes = 5;
            session.send(&reply, b"world").await.unwrap();
        });

        let mut server_roots = RootCertStore::empty();
        server_roots.add(server_cert.der().clone()).unwrap();
        let transport = DirectQuicTransport::new(
            "127.0.0.1:0".parse().unwrap(),
            mutual_client_config(
                vec![client_cert.der().clone()],
                PrivateKeyDer::Pkcs8(client_key.serialize_der().into()),
                server_roots,
            )
            .unwrap(),
            Arc::new(JsonVerifier),
            "unibus:direct:test",
            "test",
            "client",
        );
        let candidate = PeerCandidate {
            peer_id: "server".into(),
            site: Some("test-site".into()),
            endpoints: vec![NetworkEndpoint {
                transport: Transport::Quic,
                host: server_address.ip().to_string(),
                port: server_address.port(),
                server_name: Some("server.test".into()),
                priority: 0,
                weight: 0,
            }],
            expires_at_unix_s: now + 30,
        };
        let mut session = transport.connect(&candidate, &assertion).await.unwrap();
        let mut message = Envelope::new(
            "test",
            "client",
            Some("server"),
            "",
            Message::new("test.message.v1", serde_json::Value::Null),
        );
        message.payload_bytes = 5;
        session.send(&message, b"hello").await.unwrap();
        let (_, payload) = session.receive().await.unwrap();
        assert_eq!(payload, b"world");
        server.await.unwrap();
    }
}
