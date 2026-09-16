//! Unibus hub: the central relay of the household bus.
//!
//! Lifted from `jarvis-router` (JARVIS master) and generalized: the hub
//! routes *opaque* data messages by configuration (routes table, screen
//! anchors, zone + capability) and understands only control-plane kinds
//! (`register`, `heartbeat`, `reply`). Domain behavior that genuinely needs
//! hub-side state — like JARVIS's presence latest-value fan-out — plugs in
//! through [`HubPlugin`] and lives with its domain, not in L1.

pub mod auth;
pub mod config;
pub mod hub;
mod outbound;
pub mod plugin;
pub mod quic;
pub mod registry;

pub use config::{HubConfig, NodeConfig, RouteRule, RouteTarget};
pub use hub::Hub;
pub use plugin::HubPlugin;
pub use plugin::{Delivery, HubHandle, Sink};
pub use registry::{EdgeNode, NodeRegistry};
