//! The hub: authenticates nodes against grants, tracks live registrations,
//! and relays opaque data messages by screen anchor, route table, or plugin.

use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use tracing::{debug, info, warn};
use unibus::envelope::{Envelope, Message, now_ms};
use unibus::framing::read_frame;
use unibus::kinds::{self, Control};
use unibus::qos::Qos;

use crate::auth;
use crate::config::{HubConfig, NodeConfig, RouteTarget};
use crate::outbound::OutboundScheduler;
use crate::plugin::{Delivery, HubHandle, HubPlugin, Sink};
use crate::quic::{QuicEndpoint, QuicPeer};
use crate::registry::{EdgeNode, NodeRegistry};

const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_ZONE: &str = "default";

#[derive(Debug, Deserialize)]
struct RegisterBody {
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    zones: Vec<String>,
    #[serde(default)]
    screen_anchors: Vec<String>,
}

struct LiveConnection {
    outbound: OutboundScheduler,
    schema_version: std::sync::atomic::AtomicU32,
}

struct LiveQuicConnection {
    sender: tokio::sync::mpsc::Sender<(Envelope, Vec<u8>)>,
    schema_version: u32,
}

impl Sink for LiveQuicConnection {
    fn push(&self, envelope: &Envelope, payload: &[u8]) -> Result<()> {
        let mut envelope = envelope.clone();
        envelope.token.clear();
        self.sender
            .try_send((envelope, payload.to_vec()))
            .map_err(|error| anyhow::anyhow!("QUIC outbound queue rejected frame: {error}"))
    }

    fn peer_schema(&self) -> u32 {
        self.schema_version
    }
}

impl Sink for LiveConnection {
    fn push(&self, envelope: &Envelope, payload: &[u8]) -> Result<()> {
        self.outbound.push(envelope, payload)
    }
    fn peer_schema(&self) -> u32 {
        self.schema_version
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// The hub daemon.
pub struct Hub {
    config: Arc<HubConfig>,
    grants: Arc<HashMap<String, NodeConfig>>,
    routes: Arc<HashMap<String, RouteTarget>>,
    registry: Arc<Mutex<NodeRegistry>>,
    conns: Arc<Mutex<HashMap<String, Arc<dyn Sink>>>>,
    plugins: Arc<Vec<Arc<dyn HubPlugin>>>,
    verifier: Option<Arc<dyn unibus::OfflineVerifier>>,
    #[cfg(feature = "viewer")]
    widget_store: Arc<crate::viewer::widgets::Store>,
}

impl Hub {
    #[cfg(feature = "viewer")]
    pub fn configure_widgets(
        &self,
        definitions: &[crate::viewer::widgets::Definition],
    ) -> Result<()> {
        self.widget_store.configure(definitions)
    }
    #[cfg(feature = "viewer")]
    pub fn viewer_widget(
        &self,
        principal: &str,
        definition: &crate::viewer::widgets::Definition,
    ) -> Result<Option<coherence_widget_contract::Snapshot>> {
        let grant = self
            .grants
            .get(principal)
            .context("viewer principal has no grant")?
            .to_grant();
        self.widget_store.get(definition, &grant)
    }
    #[cfg(feature = "viewer")]
    pub fn viewer_snapshot(&self, producer_id: &str) -> Result<crate::viewer::Snapshot> {
        let grant = self
            .grants
            .get(producer_id)
            .context("viewer principal has no grant")?
            .to_grant();
        ensure!(
            grant
                .capabilities
                .iter()
                .any(|c| c == crate::viewer::VIEWER_CAPABILITY),
            "principal lacks viewer capability"
        );
        let registry = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("registry unavailable"))?;
        crate::viewer::project(
            &self.config.installation_id,
            &grant,
            registry.all(),
            &self.config.routes,
        )
    }
    fn admitted_registration(&self, producer_id: &str, envelope: &Envelope) -> Result<EdgeNode> {
        ensure!(
            envelope.producer_id == producer_id,
            "authenticated producer {producer_id} attempted to send as {}",
            envelope.producer_id
        );
        let grant = self
            .grants
            .get(producer_id)
            .with_context(|| format!("ungranted producer '{producer_id}'"))?
            .to_grant();
        let claimed: RegisterBody =
            serde_json::from_value(serde_json::Value::Object(envelope.message.body.clone()))
                .context("parse register body")?;
        ensure!(
            claimed
                .capabilities
                .iter()
                .all(|value| grant.capabilities.contains(value)),
            "node {producer_id} claimed an ungranted capability"
        );
        ensure!(
            claimed
                .zones
                .iter()
                .all(|value| grant.zones.contains(value)),
            "node {producer_id} claimed an ungranted zone"
        );
        ensure!(
            claimed
                .screen_anchors
                .iter()
                .all(|value| grant.screen_anchors.contains(value)),
            "node {producer_id} claimed an ungranted screen anchor"
        );
        let mut node = EdgeNode::from_claims(
            &grant,
            claimed.zones,
            claimed.capabilities,
            claimed.screen_anchors,
        );
        node.schema_version = envelope.schema_version;
        Ok(node)
    }

    fn authorize_publish(&self, producer_id: &str, envelope: &Envelope) -> Result<()> {
        let grant = self
            .grants
            .get(producer_id)
            .context("authenticated producer vanished from grants")?
            .to_grant();
        // Frozen schema-v1 peers predate publish scopes. Schema v2 is the
        // fail-closed boundary where explicit ingress policy is mandatory.
        if envelope.schema_version == unibus::SCHEMA_VERSION_V1 && grant.publishes.is_empty() {
            return Ok(());
        }
        ensure!(
            grant.may_publish(&envelope.message.kind),
            "node {producer_id} may not publish {}",
            envelope.message.kind
        );
        if let Some(anchor) = &envelope.target_screen {
            ensure!(
                grant.publish_screen_anchors.contains(anchor),
                "node {producer_id} may not target screen {anchor}"
            );
        } else if let Some(target) = self.routes.get(&envelope.message.kind) {
            match target {
                RouteTarget::Capability { .. } => {
                    let zone = envelope.destination.as_deref().unwrap_or(DEFAULT_ZONE);
                    ensure!(
                        grant.publish_zones.iter().any(|allowed| allowed == zone),
                        "node {producer_id} may not target zone {zone}"
                    );
                }
                RouteTarget::Producer => {
                    let target = envelope
                        .destination
                        .as_deref()
                        .context("producer-addressed publication requires destination")?;
                    ensure!(
                        grant
                            .publish_producers
                            .iter()
                            .any(|allowed| allowed == target),
                        "node {producer_id} may not target producer {target}"
                    );
                }
                RouteTarget::AnyHolder { .. } => {}
            }
        }
        Ok(())
    }

    pub fn new(config: HubConfig) -> Result<Self> {
        let grants = config
            .nodes
            .iter()
            .map(|n| (n.producer_id.clone(), n.clone()))
            .collect();
        let routes = config
            .routes
            .iter()
            .map(|r| (r.kind.clone(), r.target.clone()))
            .collect();
        #[cfg(feature = "viewer")]
        let widget_store = Arc::new(crate::viewer::widgets::Store::default());
        Ok(Self {
            config: Arc::new(config),
            grants: Arc::new(grants),
            routes: Arc::new(routes),
            registry: Arc::new(Mutex::new(NodeRegistry::new())),
            conns: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(not(feature = "viewer"))]
            plugins: Arc::new(Vec::new()),
            #[cfg(feature = "viewer")]
            plugins: Arc::new(vec![widget_store.clone() as Arc<dyn HubPlugin>]),
            #[cfg(feature = "viewer")]
            widget_store,
            verifier: None,
        })
    }

    #[must_use]
    pub fn with_plugin(mut self, plugin: Arc<dyn HubPlugin>) -> Self {
        let mut plugins = (*self.plugins).clone();
        plugins.push(plugin);
        self.plugins = Arc::new(plugins);
        self
    }

    /// Install the fpl-auth signature verifier used by grants with an
    /// `auth_subject`. Shared-token grants do not consult it.
    #[must_use]
    pub fn with_offline_verifier(mut self, verifier: Arc<dyn unibus::OfflineVerifier>) -> Self {
        self.verifier = Some(verifier);
        self
    }

    fn authenticate_registration(&self, config: &NodeConfig, envelope: &Envelope) -> Result<()> {
        if let Some(subject) = config
            .auth_subject
            .as_deref()
            .filter(|subject| !subject.trim().is_empty())
        {
            let verifier = self
                .verifier
                .as_ref()
                .context("node requires fpl-auth but no OfflineVerifier is installed")?;
            let now_s = now_ms() / 1_000;
            let claims = verifier.verify(&envelope.token, now_s)?;
            claims.validate(
                &format!("unibus:{}", self.config.installation_id),
                &self.config.installation_id,
                now_s,
            )?;
            ensure!(claims.sub == subject, "fpl-auth subject mismatch");
            ensure!(
                claims.unibus_producer_id == envelope.producer_id,
                "fpl-auth producer mismatch"
            );
            let grant = config.to_grant();
            ensure!(
                claims
                    .unibus_capabilities
                    .iter()
                    .all(|value| grant.capabilities.contains(value))
                    && claims
                        .unibus_zones
                        .iter()
                        .all(|value| grant.zones.contains(value))
                    && claims
                        .unibus_screen_anchors
                        .iter()
                        .all(|value| grant.screen_anchors.contains(value)),
                "fpl-auth assertion scope exceeds grant"
            );
            Ok(())
        } else {
            let token_file = config
                .token_file
                .as_deref()
                .context("shared-token grant has no token_file")?;
            auth::validate_token(token_file, &envelope.producer_id, &envelope.token)
        }
    }

    /// Bind the listener and start the accept loop on a background thread.
    /// Returns the actual local address (useful with port 0 in tests).
    pub fn start(self: &Arc<Self>) -> Result<SocketAddr> {
        let listener = TcpListener::bind(&self.config.listen)
            .with_context(|| format!("bind hub to {}", self.config.listen))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        info!(
            installation = %self.config.installation_id,
            "unibus hub listening on {addr}"
        );
        let hub = self.clone();
        thread::Builder::new()
            .name("unibus-hub".into())
            .spawn(move || hub.accept_loop(listener))
            .context("start hub accept thread")?;
        Ok(addr)
    }

    /// Start the same hub over QUIC/TLS. Certificate and trust policy are
    /// explicit caller inputs; application identity remains the register
    /// assertion validated by the normal session path.
    pub fn start_quic(
        self: &Arc<Self>,
        address: SocketAddr,
        config: quinn::ServerConfig,
    ) -> Result<SocketAddr> {
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let hub = self.clone();
        thread::Builder::new()
            .name("unibus-quic".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Runtime::new() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(anyhow::anyhow!(error)));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let endpoint = match QuicEndpoint::server(address, config) {
                        Ok(endpoint) => endpoint,
                        Err(error) => {
                            let _ = ready_tx.send(Err(error));
                            return;
                        }
                    };
                    let local_addr = endpoint.local_addr();
                    if ready_tx.send(local_addr).is_err() {
                        return;
                    }
                    loop {
                        match endpoint.accept().await {
                            Ok(peer) => {
                                let hub = hub.clone();
                                tokio::spawn(async move {
                                    if let Err(error) = hub.handle_quic_peer(peer).await {
                                        debug!("QUIC connection closed: {error:#}");
                                    }
                                });
                            }
                            Err(error) => {
                                warn!("QUIC accept error: {error:#}");
                                return;
                            }
                        }
                    }
                });
            })
            .context("start QUIC hub thread")?;
        ready_rx.recv().context("QUIC startup thread stopped")?
    }

    /// Bind and block forever (daemon entry point).
    pub fn serve(self: Arc<Self>) -> Result<()> {
        let quic = self
            .config
            .quic
            .as_ref()
            .map(|quic| {
                let address = quic
                    .listen
                    .parse()
                    .with_context(|| format!("parse QUIC listen address {}", quic.listen))?;
                let tls = crate::quic::server_config_from_pem(&quic.cert_file, &quic.key_file)?;
                Ok::<_, anyhow::Error>((address, tls))
            })
            .transpose()?;
        self.start()?;
        if let Some((address, tls)) = quic {
            let actual = self.start_quic(address, tls)?;
            info!("unibus QUIC/TLS hub listening on {actual}");
        }
        loop {
            thread::sleep(Duration::from_secs(3600));
        }
    }

    fn accept_loop(self: &Arc<Self>, listener: TcpListener) {
        let mut prune_at = Instant::now();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let hub = self.clone();
                    if thread::Builder::new()
                        .name("unibus-hub-conn".into())
                        .spawn(move || {
                            if let Err(e) = hub.handle_connection(stream) {
                                debug!("connection closed: {e:#}");
                            }
                        })
                        .is_err()
                    {
                        warn!("failed to spawn connection thread; dropping");
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if prune_at.elapsed() >= Duration::from_secs(5) {
                        self.prune_dead();
                        prune_at = Instant::now();
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    warn!("hub accept error: {e:#}");
                    return;
                }
            }
        }
    }

    fn handle_connection(&self, stream: TcpStream) -> Result<()> {
        // Accepted sockets inherit the listener's non-blocking flag on macOS;
        // the connection loop is a blocking read with timeouts.
        stream
            .set_nonblocking(false)
            .context("clear non-blocking")?;
        stream
            .set_read_timeout(Some(READ_TIMEOUT))
            .context("set read timeout")?;
        stream
            .set_write_timeout(Some(WRITE_TIMEOUT))
            .context("set write timeout")?;
        let peer = stream.peer_addr()?;
        // Interactive byte protocols (ADB, HID, RPC) must not incur Nagle's
        // small-frame delay on the hub-to-client side.
        stream.set_nodelay(true).context("set TCP_NODELAY")?;
        debug!("incoming connection from {peer}");

        let mut reader = stream.try_clone()?;
        let (envelope, _payload) = read_frame(&mut reader).context("read register frame")?;
        let installation = self.config.installation_id.clone();
        envelope.validate(&installation, now_ms())?;
        ensure!(
            kinds::classify(&envelope.message.kind) == Control::Register,
            "first message must be register, got {}",
            envelope.message.kind
        );
        let producer_id = envelope.producer_id.clone();

        let node_config = self
            .grants
            .get(&producer_id)
            .with_context(|| format!("ungranted producer '{producer_id}'"))?;
        self.authenticate_registration(node_config, &envelope)?;
        let node = self.admitted_registration(&producer_id, &envelope)?;

        {
            let mut registry = self
                .registry
                .lock()
                .map_err(|_| anyhow::anyhow!("registry mutex poisoned"))?;
            registry.upsert(node.clone());
        }
        let connection = Arc::new(LiveConnection {
            outbound: OutboundScheduler::spawn(stream)?,
            schema_version: std::sync::atomic::AtomicU32::new(envelope.schema_version),
        });
        {
            let mut conns = self
                .conns
                .lock()
                .map_err(|_| anyhow::anyhow!("connections mutex poisoned"))?;
            conns.insert(producer_id.clone(), connection.clone() as Arc<dyn Sink>);
        }
        info!(
            "node '{producer_id}' registered: capabilities {:?}, zones {:?}, anchors {:?}",
            node.capabilities, node.zones, node.screen_anchors
        );

        let reply = Envelope::new(
            &installation,
            &producer_id,
            None,
            "",
            Message::new(
                kinds::V1_REPLY,
                kinds::reply_body(true, Some("registered".into()), Some(envelope.request_id)),
            ),
        );
        connection.push(&reply, &[])?;
        self.broadcast_node_state(&producer_id, true);

        let outcome = self.connection_loop(&producer_id, &connection, reader);

        // Ownership-checked removal: a node may hold several connections
        // (JARVIS Sound dials status + remote input under one producer id).
        // A dying connection must never evict the registration or socket
        // slot of a connection that replaced it; registrations expire via
        // heartbeat TTL, not via this cleanup.
        let was_owner = {
            match self.conns.lock() {
                Ok(mut conns) => {
                    let owned = conns.get(&producer_id).is_some_and(|current| {
                        Arc::ptr_eq(current, &(connection.clone() as Arc<dyn Sink>))
                    });
                    if owned {
                        conns.remove(&producer_id);
                    }
                    owned
                }
                Err(_) => false,
            }
        };
        // Only the connection that still owns the slot may declare the
        // producer offline; a replaced connection must not evict a live one.
        if was_owner {
            self.broadcast_node_state(&producer_id, false);
        }
        let Err(e) = outcome else {
            return Ok(());
        };
        warn!("node '{producer_id}' stream error: {e:#}");
        Ok(())
    }

    async fn handle_quic_peer(&self, peer: QuicPeer) -> Result<()> {
        let (envelope, _payload) = peer.recv().await.context("read QUIC register frame")?;
        envelope.validate(&self.config.installation_id, now_ms())?;
        ensure!(
            kinds::classify(&envelope.message.kind) == Control::Register,
            "first QUIC message must be register"
        );
        let producer_id = envelope.producer_id.clone();
        let node_config = self
            .grants
            .get(&producer_id)
            .with_context(|| format!("ungranted producer '{producer_id}'"))?;
        self.authenticate_registration(node_config, &envelope)?;
        let node = self.admitted_registration(&producer_id, &envelope)?;

        let (sender, mut receiver) = tokio::sync::mpsc::channel::<(Envelope, Vec<u8>)>(128);
        let outbound_peer = peer.clone();
        tokio::spawn(async move {
            while let Some((envelope, payload)) = receiver.recv().await {
                if outbound_peer.send(&envelope, &payload).await.is_err() {
                    return;
                }
            }
        });
        let connection = Arc::new(LiveQuicConnection {
            sender,
            schema_version: envelope.schema_version,
        });
        self.registry
            .lock()
            .map_err(|_| anyhow::anyhow!("registry mutex poisoned"))?
            .upsert(node);
        self.conns
            .lock()
            .map_err(|_| anyhow::anyhow!("connections mutex poisoned"))?
            .insert(producer_id.clone(), connection.clone());

        let reply = Envelope::new(
            &self.config.installation_id,
            &producer_id,
            None,
            "",
            Message::new(
                kinds::V1_REPLY,
                kinds::reply_body(true, Some("registered".into()), Some(envelope.request_id)),
            ),
        );
        connection.push(&reply, &[])?;
        self.broadcast_node_state(&producer_id, true);

        let outcome = loop {
            match peer.recv().await {
                Ok((envelope, payload)) => {
                    if let Err(error) = self.handle_registered_frame(
                        &producer_id,
                        connection.as_ref(),
                        envelope,
                        payload,
                    ) {
                        break Err(error);
                    }
                }
                Err(error) => break Err(error),
            }
        };

        let was_owner = self.conns.lock().is_ok_and(|mut conns| {
            let current: Arc<dyn Sink> = connection.clone();
            let owned = conns
                .get(&producer_id)
                .is_some_and(|registered| Arc::ptr_eq(registered, &current));
            if owned {
                conns.remove(&producer_id);
            }
            owned
        });
        if was_owner {
            self.broadcast_node_state(&producer_id, false);
        }
        outcome
    }

    fn connection_loop(
        &self,
        producer_id: &str,
        connection: &Arc<LiveConnection>,
        mut reader: TcpStream,
    ) -> Result<()> {
        loop {
            let (envelope, payload) = match read_frame(&mut reader) {
                Ok(frame) => frame,
                // macOS reports an SO_RCVTIMEO expiry as EAGAIN/WouldBlock,
                // not TimedOut: idle polls are normal, only EOF ends a
                // session politely.
                Err(unibus::FrameError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    self.prune_dead();
                    continue;
                }
                Err(unibus::FrameError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            };
            self.handle_registered_frame(producer_id, connection.as_ref(), envelope, payload)?;
        }
    }

    fn handle_registered_frame(
        &self,
        producer_id: &str,
        connection: &dyn Sink,
        mut envelope: Envelope,
        payload: Vec<u8>,
    ) -> Result<()> {
        envelope.validate(&self.config.installation_id, now_ms())?;
        ensure!(
            envelope.producer_id == producer_id,
            "authenticated producer {producer_id} attempted to send as {}",
            envelope.producer_id
        );
        envelope.token.clear();

        let mut handled_reply = None;
        match kinds::classify(&envelope.message.kind) {
            Control::Heartbeat => {
                let ok = self
                    .registry
                    .lock()
                    .map_err(|_| anyhow::anyhow!("registry mutex poisoned"))?
                    .heartbeat(producer_id);
                ensure!(ok, "heartbeat from unregistered node {producer_id}");
                return Ok(());
            }
            Control::Reply => return Ok(()),
            Control::Register => {
                // Mid-session re-registration repeats the same subset
                // validation as the initial registration.
                let node = self.admitted_registration(producer_id, &envelope)?;
                self.registry
                    .lock()
                    .map_err(|_| anyhow::anyhow!("registry mutex poisoned"))?
                    .upsert(node);
                return Ok(());
            }
            Control::Data => {
                if let Err(error) = self.authorize_publish(producer_id, &envelope) {
                    warn!("publication rejected for '{producer_id}': {error:#}");
                    handled_reply = Some((false, Some(error.to_string())));
                } else {
                    let hub = self.handle();
                    let mut plugin_err = None;
                    let mut plugin_hit = false;
                    for plugin in self.plugins.iter() {
                        match plugin.handle(&hub, producer_id, &envelope, &payload) {
                            Ok(true) => {
                                plugin_hit = true;
                                break;
                            }
                            Ok(false) => {}
                            Err(e) => {
                                warn!("plugin failed for '{producer_id}': {e:#}");
                                plugin_err = Some(e.to_string());
                                break;
                            }
                        }
                    }
                    if let Some(message) = plugin_err {
                        handled_reply = Some((false, Some(message)));
                    } else if !plugin_hit {
                        handled_reply =
                            Some(self.route_default(producer_id, &envelope, &payload)?);
                    }
                }
            }
        }

        // Ack policy mirrors jarvis-router v1: stream data is
        // fire-and-forget, everything else gets a reply.
        let is_stream = Qos::of_kind(&envelope.message.kind) == Qos::Stream;
        if !is_stream {
            let (success, message) = handled_reply.unwrap_or((true, None));
            let reply = Envelope::new(
                &self.config.installation_id,
                producer_id,
                None,
                "",
                Message::new(
                    kinds::V1_REPLY,
                    kinds::reply_body(success, message, Some(envelope.request_id.clone())),
                ),
            );
            connection.push(&reply, &[])?;
        }
        Ok(())
    }

    /// Producer liveness, pushed to every connected node so consumers can
    /// invalidate state a dead producer left behind (e.g. a voice brain's
    /// "thinking" pill). Latest-value truth about one producer.
    fn broadcast_node_state(&self, producer_id: &str, online: bool) {
        let message = Message::new(
            unibus::kinds::NODE_STATE,
            serde_json::json!({"producer_id": producer_id, "online": online}),
        );
        let envelope = Envelope::new(
            &self.config.installation_id,
            "unibus-hub",
            None,
            "",
            message,
        );
        let conns = match self.conns.lock() {
            Ok(conns) => conns,
            Err(_) => return,
        };
        for (node, conn) in conns.iter() {
            // v1 peers carry a closed message enum: an unknown kind would
            // kill their read loop. Only v2 (routed-opaque) peers may see
            // hub advisories.
            if node != producer_id && conn.peer_schema() >= 2 {
                let _ = conn.push(&envelope, &[]);
            }
        }
    }

    fn route_default(
        &self,
        source: &str,
        envelope: &Envelope,
        payload: &[u8],
    ) -> Result<(bool, Option<String>)> {
        let delivered = if let Some(anchor) = &envelope.target_screen {
            self.handle().send_to_screen(anchor, envelope, payload)
        } else if let Some(target) = self.routes.get(&envelope.message.kind) {
            let hub = self.handle();
            match target {
                RouteTarget::Capability { name } => {
                    let zone = envelope.destination.as_deref().unwrap_or(DEFAULT_ZONE);
                    hub.send_to_capability(name, zone, source, envelope, payload)
                }
                RouteTarget::AnyHolder { name } => {
                    hub.send_to_holders(name, source, envelope, payload)
                }
                RouteTarget::Producer => match envelope.destination.as_deref() {
                    Some(target) => {
                        if hub.send_to_producer(target, envelope, payload).is_ok() {
                            1
                        } else {
                            0
                        }
                    }
                    None => 0,
                },
            }
        } else {
            warn!(
                "no route for kind '{}' from '{source}'",
                envelope.message.kind
            );
            return Ok((
                false,
                Some(format!(
                    "no route for message kind {}",
                    envelope.message.kind
                )),
            ));
        };
        if delivered == 0 {
            warn!(
                "kind '{}' from '{source}' matched no live node",
                envelope.message.kind
            );
        }
        Ok((
            delivered > 0,
            (delivered == 0).then(|| kinds::REPLY_NO_ELIGIBLE_NODES.to_string()),
        ))
    }

    fn handle(&self) -> HubHandle {
        HubHandle {
            installation_id: self.config.installation_id.clone(),
            grants: self.grants.clone(),
            registry: self.registry.clone(),
            conns: self.conns.clone(),
        }
    }

    fn prune_dead(&self) {
        if let Ok(mut registry) = self.registry.lock() {
            let removed = registry.prune_dead();
            if removed > 0 {
                info!("pruned {removed} dead nodes");
            }
        }
    }
}

impl HubHandle {
    fn live_for(
        &self,
        capability: &str,
        zone: Option<&str>,
        exclude: Option<&str>,
    ) -> Vec<(Delivery, Arc<dyn Sink>)> {
        let registry = match self.registry.lock() {
            Ok(registry) => registry,
            Err(_) => return Vec::new(),
        };
        registry
            .live()
            .filter(|node| {
                node.can(capability)
                    && zone.map(|z| node.in_zone(z)).unwrap_or(true)
                    && Some(node.producer_id.as_str()) != exclude
            })
            .filter_map(|node| {
                self.connection(&node.producer_id).map(|conn| {
                    (
                        Delivery {
                            producer_id: node.producer_id.clone(),
                            site: node.site.clone(),
                            zones: node.zones.clone(),
                        },
                        conn,
                    )
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod delivery_liveness_tests {
    use super::*;

    struct AcceptingSink;

    impl Sink for AcceptingSink {
        fn push(&self, _: &Envelope, _: &[u8]) -> Result<()> {
            Ok(())
        }

        fn peer_schema(&self) -> u32 {
            2
        }
    }

    #[test]
    fn successful_delivery_refreshes_receive_only_node() {
        let mut registry = NodeRegistry::new();
        registry.upsert(EdgeNode {
            producer_id: "receiver".into(),
            zones: vec!["default".into()],
            capabilities: vec!["audio_input".into()],
            screen_anchors: Vec::new(),
            site: "default".into(),
            schema_version: 2,
            connected_at: Instant::now(),
            last_heartbeat: Instant::now() - Duration::from_secs(9),
            token_authenticated: true,
        });
        let registry = Arc::new(Mutex::new(registry));
        let handle = HubHandle {
            installation_id: "home".into(),
            grants: Arc::new(HashMap::new()),
            registry: registry.clone(),
            conns: Arc::new(Mutex::new(HashMap::new())),
        };
        let delivered = handle.deliver(
            vec![(
                Delivery {
                    producer_id: "receiver".into(),
                    site: "default".into(),
                    zones: vec!["default".into()],
                },
                Arc::new(AcceptingSink),
            )],
            &Envelope::new(
                "home",
                "sender",
                Some("default".into()),
                "",
                Message::new("audio_capture_chunk", serde_json::json!({})),
            ),
            &[],
        );

        assert_eq!(delivered, 1);
        assert!(
            registry
                .lock()
                .unwrap()
                .get("receiver")
                .unwrap()
                .last_heartbeat
                .elapsed()
                < Duration::from_secs(1)
        );
    }
}

impl HubHandle {
    /// Every live holder, installation-wide, excluding `exclude` (the source).
    pub fn send_to_holders(
        &self,
        capability: &str,
        exclude: &str,
        envelope: &Envelope,
        payload: &[u8],
    ) -> usize {
        self.deliver(
            self.live_for(capability, None, Some(exclude)),
            envelope,
            payload,
        )
    }
}

/// Internal helpers with concrete connection types (trait objects can't
/// name them and the sink signature needs the subtraction helper).
impl HubHandle {
    fn record_delivery(&self, producer_id: &str) {
        // A successful enqueue proves that this connection is still usable.
        // Receive-only peers (notably JARVIS remote audio input) otherwise
        // expire despite continuously draining frames from the hub.
        if let Ok(mut registry) = self.registry.lock() {
            let _ = registry.heartbeat(producer_id);
        }
    }

    fn deliver(
        &self,
        targets: Vec<(Delivery, Arc<dyn Sink>)>,
        envelope: &Envelope,
        payload: &[u8],
    ) -> usize {
        let mut delivered = 0;
        for (node, conn) in targets {
            let mut forwarded = envelope.clone();
            forwarded.destination = Some(node.producer_id.clone());
            if conn.push(&forwarded, payload).is_ok() {
                self.record_delivery(&node.producer_id);
                delivered += 1;
            }
        }
        delivered
    }

    pub fn send_to_screen(&self, anchor: &str, envelope: &Envelope, payload: &[u8]) -> usize {
        let selected = self.registry.lock().ok().and_then(|registry| {
            registry
                .select_for_screen(anchor)
                .map(|n| n.producer_id.clone())
        });
        match selected.and_then(|id| self.connection(&id).map(|conn| (id, conn))) {
            Some((producer_id, conn)) => {
                if conn.push(envelope, payload).is_ok() {
                    self.record_delivery(&producer_id);
                    1
                } else {
                    0
                }
            }
            None => {
                warn!("no live node claims screen anchor '{anchor}'");
                0
            }
        }
    }

    pub fn send_to_capability(
        &self,
        capability: &str,
        zone: &str,
        exclude: &str,
        envelope: &Envelope,
        payload: &[u8],
    ) -> usize {
        self.deliver(
            self.live_for(capability, Some(zone), Some(exclude)),
            envelope,
            payload,
        )
    }
}
