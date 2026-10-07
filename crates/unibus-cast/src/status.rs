//! Local receive health, not proof that a remote display rendered a frame.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MirrorPhase {
    WaitingSource,
    WaitingBootstrap,
    Streaming,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MirrorStatus {
    pub schema_version: u32,
    pub phase: MirrorPhase,
    pub process_live: bool,
    pub source_live: bool,
    /// Monotonic age of last received video TS payload, not presentation age.
    pub video_age_ms: Option<u64>,
    pub active_viewers: usize,
    pub waiting_viewers: usize,
    pub bootstrap_ready: bool,
    pub cache_bytes: usize,
    pub normalize_video: bool,
}
