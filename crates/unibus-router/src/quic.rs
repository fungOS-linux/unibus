//! QUIC/TLS transport primitives using the same Unibus frame codec.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use quinn::{Connection, Endpoint};
use rustls::RootCertStore;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use unibus::envelope::{Envelope, MAX_FRAME};
use unibus::framing::{decode_frame, encode_frame};
use unibus::qos::Qos;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuicDelivery {
    Datagram,
    OneWayStream,
}

pub fn server_config(
    certificates: Vec<CertificateDer<'static>>,
    private_key: PrivateKeyDer<'static>,
) -> Result<quinn::ServerConfig> {
    quinn::ServerConfig::with_single_cert(certificates, private_key)
        .context("build QUIC TLS server configuration")
}

pub fn client_config(root_certificates: RootCertStore) -> Result<quinn::ClientConfig> {
    quinn::ClientConfig::with_root_certificates(Arc::new(root_certificates))
        .context("build QUIC TLS client configuration")
}

pub struct QuicEndpoint {
    endpoint: Endpoint,
}

impl QuicEndpoint {
    pub fn server(address: SocketAddr, config: quinn::ServerConfig) -> Result<Self> {
        let socket = UdpSocket::bind(address).context("bind QUIC UDP socket")?;
        let runtime = quinn::default_runtime().ok_or_else(|| anyhow!("no Quinn runtime found"))?;
        let endpoint = Endpoint::new(
            quinn::EndpointConfig::default(),
            Some(config),
            socket,
            runtime,
        )?;
        Ok(Self { endpoint })
    }

    pub fn client(address: SocketAddr, config: quinn::ClientConfig) -> Result<Self> {
        let mut endpoint = Endpoint::client(address)?;
        endpoint.set_default_client_config(config);
        Ok(Self { endpoint })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint
            .local_addr()
            .context("read QUIC local address")
    }

    pub async fn accept(&self) -> Result<QuicPeer> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .context("QUIC endpoint closed")?;
        let connection = incoming.await.context("complete QUIC TLS handshake")?;
        Ok(QuicPeer { connection })
    }

    pub async fn connect(&self, address: SocketAddr, server_name: &str) -> Result<QuicPeer> {
        let connection = self
            .endpoint
            .connect(address, server_name)
            .context("start QUIC connection")?
            .await
            .context("complete QUIC TLS handshake")?;
        Ok(QuicPeer { connection })
    }
}

#[derive(Clone)]
pub struct QuicPeer {
    connection: Connection,
}

impl QuicPeer {
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn delivery_for(&self, envelope: &Envelope, encoded_len: usize) -> QuicDelivery {
        if Qos::of_kind(&envelope.message.kind).datagram_capable()
            && self
                .connection
                .max_datagram_size()
                .is_some_and(|maximum| encoded_len <= maximum)
        {
            QuicDelivery::Datagram
        } else {
            QuicDelivery::OneWayStream
        }
    }

    pub async fn send(&self, envelope: &Envelope, payload: &[u8]) -> Result<QuicDelivery> {
        let encoded = encode_frame(envelope, payload)?;
        let delivery = self.delivery_for(envelope, encoded.len());
        match delivery {
            QuicDelivery::Datagram => self
                .connection
                .send_datagram(encoded.into())
                .context("send QUIC datagram")?,
            QuicDelivery::OneWayStream => {
                let mut stream = self
                    .connection
                    .open_uni()
                    .await
                    .context("open QUIC one-way stream")?;
                stream
                    .write_all(&encoded)
                    .await
                    .context("write QUIC frame")?;
                stream.finish().context("finish QUIC frame")?;
            }
        }
        Ok(delivery)
    }

    pub async fn recv(&self) -> Result<(Envelope, Vec<u8>)> {
        let encoded = tokio::select! {
            datagram = self.connection.read_datagram() => {
                datagram.context("read QUIC datagram")?.to_vec()
            }
            stream = self.connection.accept_uni() => {
                let mut stream = stream.context("accept QUIC one-way stream")?;
                stream.read_to_end(MAX_FRAME as usize).await.context("read QUIC frame")?
            }
        };
        decode_frame(&encoded).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use unibus::envelope::Message;

    #[test]
    fn tls_builders_reject_an_empty_server_chain() {
        let key = PrivateKeyDer::Pkcs8(vec![1, 2, 3].into());
        assert!(server_config(Vec::new(), key).is_err());
    }

    #[test]
    fn client_builder_rejects_an_empty_root_store() {
        assert!(client_config(RootCertStore::empty()).is_err());
    }

    #[test]
    fn quic_uses_the_shared_frame_codec() {
        let envelope = Envelope::new(
            "home",
            "node",
            None,
            "",
            Message::new("canvas.command", serde_json::Value::Null),
        );
        let encoded = encode_frame(&envelope, &[]).unwrap();
        let (decoded, payload) = decode_frame(&encoded).unwrap();
        assert_eq!(decoded, envelope);
        assert!(payload.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tls_round_trip_uses_streams_and_datagrams() {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let server = QuicEndpoint::server(
            "127.0.0.1:0".parse().unwrap(),
            server_config(
                vec![cert.der().clone()],
                PrivateKeyDer::Pkcs8(signing_key.serialize_der().into()),
            )
            .unwrap(),
        )
        .unwrap();
        let address = server.local_addr().unwrap();

        let receive = tokio::spawn(async move {
            let peer = server.accept().await.unwrap();
            let first = peer.recv().await.unwrap();
            let second = peer.recv().await.unwrap();
            vec![first, second]
        });

        let mut roots = RootCertStore::empty();
        roots.add(cert.der().clone()).unwrap();
        let client = QuicEndpoint::client(
            "127.0.0.1:0".parse().unwrap(),
            client_config(roots).unwrap(),
        )
        .unwrap();
        let peer = client.connect(address, "localhost").await.unwrap();
        let command = Envelope::new(
            "home",
            "node",
            None,
            "",
            Message::new("canvas.command", serde_json::Value::Null),
        );
        assert_eq!(
            peer.send(&command, &[]).await.unwrap(),
            QuicDelivery::OneWayStream
        );
        let stream = Envelope::new(
            "home",
            "node",
            None,
            "",
            Message::new("audio.playback_chunk", serde_json::Value::Null),
        );
        assert_eq!(
            peer.send(&stream, &[]).await.unwrap(),
            QuicDelivery::Datagram
        );

        let received = receive.await.unwrap();
        let kinds: std::collections::HashSet<_> = received
            .iter()
            .map(|(envelope, _)| envelope.message.kind.as_str())
            .collect();
        assert_eq!(
            kinds,
            std::collections::HashSet::from(["canvas.command", "audio.playback_chunk"])
        );
    }
}
