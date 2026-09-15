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
use unibus::framing::{read_frame, write_frame};
use unibus::kinds::{self, Control};
use unibus::qos::Qos;

use crate::auth;
use crate::config::{HubConfig, NodeConfig, RouteTarget};
use crate::plugin::{Delivery, HubHandle, HubPlugin, Sink};
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
    writer: Arc<Mutex<TcpStream>>,
}

impl Sink for LiveConnection {
    fn push(&self, envelope: &Envelope, payload: &[u8]) -> Result<()> {
        let mut stream = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("connection mutex poisoned"))?;
        write_frame(&mut *stream, envelope, payload).context("push frame to node")
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
}

impl Hub {
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
        Ok(Self {
            config: Arc::new(config),
            grants: Arc::new(grants),
            routes: Arc::new(routes),
            registry: Arc::new(Mutex::new(NodeRegistry::new())),
            conns: Arc::new(Mutex::new(HashMap::new())),
            plugins: Arc::new(Vec::new()),
        })
    }

    #[must_use]
    pub fn with_plugin(mut self, plugin: Arc<dyn HubPlugin>) -> Self {
        let mut plugins = (*self.plugins).clone();
        plugins.push(plugin);
        self.plugins = Arc::new(plugins);
        self
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

    /// Bind and block forever (daemon entry point).
    pub fn serve(self: Arc<Self>) -> Result<()> {
        self.start()?;
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
        let grant = node_config.to_grant();

        auth::validate_token(&node_config.token_file, &producer_id, &envelope.token)?;

        let claimed: RegisterBody =
            serde_json::from_value(serde_json::Value::Object(envelope.message.body.clone()))
                .context("parse register body")?;
        for capability in &claimed.capabilities {
            ensure!(
                grant.can(capability),
                "node {producer_id} claimed ungranted capability {capability}"
            );
        }

        {
            let mut registry = self
                .registry
                .lock()
                .map_err(|_| anyhow::anyhow!("registry mutex poisoned"))?;
            registry.upsert(EdgeNode::new(&grant));
        }
        let connection = Arc::new(LiveConnection {
            writer: Arc::new(Mutex::new(stream)),
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
            claimed.capabilities, claimed.zones, claimed.screen_anchors
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

        let outcome = self.connection_loop(&producer_id, &connection, reader);

        if let Ok(mut conns) = self.conns.lock() {
            conns.remove(&producer_id);
        }
        if let Ok(mut registry) = self.registry.lock() {
            registry.remove(&producer_id);
        }
        let Err(e) = outcome else {
            return Ok(());
        };
        warn!("node '{producer_id}' stream error: {e:#}");
        Ok(())
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
                Err(unibus::FrameError::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            };
            envelope.validate(&self.config.installation_id, now_ms())?;

            let mut handled_reply = None;
            match kinds::classify(&envelope.message.kind) {
                Control::Heartbeat => {
                    let ok = self
                        .registry
                        .lock()
                        .map_err(|_| anyhow::anyhow!("registry mutex poisoned"))?
                        .heartbeat(producer_id);
                    ensure!(ok, "heartbeat from unregistered node {producer_id}");
                    continue;
                }
                Control::Reply => continue,
                Control::Register => {
                    // Mid-session re-registration: same grant, refresh state.
                    self.registry
                        .lock()
                        .map_err(|_| anyhow::anyhow!("registry mutex poisoned"))?
                        .upsert(EdgeNode::new(
                            &self
                                .grants
                                .get(producer_id)
                                .context("node vanished from grants")?
                                .to_grant(),
                        ));
                    continue;
                }
                Control::Data => {
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
            (delivered == 0).then(|| "no eligible nodes".to_string()),
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
        match selected.and_then(|id| self.connection(&id)) {
            Some(conn) => {
                if conn.push(envelope, payload).is_ok() {
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
