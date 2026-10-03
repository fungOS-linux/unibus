//! Unibus — the wire contract for one household bus.
//!
//! One shared backplane where display, audio, presence, sensor, and control
//! modules are all just grant-bearing slots. This crate is L1: envelopes,
//! framing, grants, and QoS classification. It never parses a message body;
//! anything that understands `canvas.*` or `presence.*` content is an L2
//! contract living with its domain.
//!
//! The wire protocol is compatible with JARVIS router v1
//! (`jarvis-core::edge_transport`), which this contract is being lifted from;
//! v2 generalizes capability/message namespacing without breaking v1 frames.

pub mod auth;
pub mod client;
pub mod direct;
pub mod discovery;
pub mod envelope;
pub mod framing;
pub mod grants;
pub mod kinds;
pub mod peer;
pub mod qos;
pub mod reconnect;

pub use auth::{AuthError, NodeClaims, OfflineVerifier};
pub use direct::{DirectSessionClaims, DirectSessionError, DirectSessionVerifier, PeerBinding};
pub use discovery::{
    EndpointSource, NetworkEndpoint, NetworkEndpointError, StaticEndpointSource, Transport,
};
pub use envelope::{
    DEFAULT_TTL_MS, Envelope, EnvelopeError, MAX_FRAME, MAX_FUTURE_SKEW_MS, Message,
    SCHEMA_VERSION_V1, SCHEMA_VERSION_V2,
};
pub use framing::{FrameError, decode_frame, encode_frame, read_frame, write_frame};
pub use grants::{NodeGrant, Role, is_valid_namespaced_name, is_valid_slug};
pub use kinds::Control;
pub use peer::{
    PeerCandidate, PeerDirectory, PeerError, PeerFuture, PeerSession, PeerTransport, SessionPath,
    StaticPeerDirectory, connect_direct_or_else,
};
pub use qos::Qos;
pub use reconnect::{ReconnectBackoff, ReconnectPolicy};
