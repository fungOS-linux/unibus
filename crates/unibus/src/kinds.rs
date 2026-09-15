//! Control-plane message kinds. Everything else is opaque, routed data.
//! v1 JARVIS tags and v2 namespaced spellings classify identically, so a
//! mixed-version deployment cannot disagree about what the hub must
//! understand.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Register,
    Heartbeat,
    Reply,
    /// Opaque payload; L1 routes it, never parses it.
    Data,
}

pub const V1_REGISTER: &str = "register";
pub const V1_HEARTBEAT: &str = "heartbeat";
pub const V1_REPLY: &str = "reply";

pub const REGISTER: &str = "unibus.register";
/// Hub-pushed producer liveness: `{producer_id, online}`. Consumers use it
/// to invalidate state a dead producer left behind (orphan pills, stale
/// sources). Latest-value truth about one producer.
pub const NODE_STATE: &str = "unibus.node_state";
pub const HEARTBEAT: &str = "unibus.heartbeat";
pub const REPLY: &str = "unibus.reply";

pub fn classify(kind: &str) -> Control {
    match kind {
        V1_REGISTER | REGISTER => Control::Register,
        V1_HEARTBEAT | HEARTBEAT => Control::Heartbeat,
        V1_REPLY | REPLY => Control::Reply,
        _ => Control::Data,
    }
}

/// The `reply` body shape both versions emit: `{success, message?, request_id?}`.
pub fn reply_body(
    success: bool,
    message: Option<String>,
    request_id: Option<String>,
) -> serde_json::Value {
    serde_json::json!({
        "success": success,
        "message": message,
        "request_id": request_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_classify_alike() {
        assert_eq!(classify("register"), classify(REGISTER));
        assert_eq!(classify("heartbeat"), classify(HEARTBEAT));
        assert_eq!(classify("reply"), classify(REPLY));
        assert_eq!(classify("canvas_command"), Control::Data);
        assert_eq!(classify("anything.novel"), Control::Data);
    }
}
