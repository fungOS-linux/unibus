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
pub mod envelope;
pub mod framing;
pub mod grants;
pub mod qos;

pub use auth::{AuthError, NodeClaims, OfflineVerifier};
pub use envelope::{
    Envelope, EnvelopeError, MAX_FRAME, Message, SCHEMA_VERSION_V1, SCHEMA_VERSION_V2,
};
pub use framing::{FrameError, read_frame, write_frame};
pub use grants::{NodeGrant, Role};
pub use qos::Qos;
