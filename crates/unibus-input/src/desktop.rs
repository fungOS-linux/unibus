//! Shared physical-key mapping and monotonic lease enforcement for desktop adapters.
use crate::{ContractError, DesktopInputState};
use serde::{Deserialize, Serialize};

pub const ACK_CONTRACT: &str = "input.desktop_ack.v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeConfig {
    pub bus: unibus::client::ClientConfig,
    pub session_id: String,
    pub controller_id: String,
    pub target_id: String,
    pub authorized_producer: String,
    #[serde(default)]
    pub compositor_socket: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureConfig {
    pub bus: unibus::client::ClientConfig,
    pub destination: String,
    pub session_id: String,
    pub controller_id: String,
    pub target_id: String,
    pub width: u32,
    pub height: u32,
    #[serde(default = "default_hotkey")]
    pub hotkey_keycode: u16,
    #[serde(default)]
    pub scroll_enabled: bool,
}
fn default_hotkey() -> u16 {
    49
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ack {
    pub session_id: String,
    pub controller_id: String,
    pub target_id: String,
    pub sequence: u64,
}
impl From<&DesktopInputState> for Ack {
    fn from(s: &DesktopInputState) -> Self {
        Self {
            session_id: s.session_id.clone(),
            controller_id: s.controller_id.clone(),
            target_id: s.target_id.clone(),
            sequence: s.sequence,
        }
    }
}

/// (macOS physical keycode, USB keyboard usage, Linux evdev keycode).
/// Text/layout translation belongs to the destination OS, not the transport.
pub const KEYS: &[(u16, u16, u16)] = &[
    (0, 4, 30),
    (11, 5, 48),
    (8, 6, 46),
    (2, 7, 32),
    (14, 8, 18),
    (3, 9, 33),
    (5, 10, 34),
    (4, 11, 35),
    (34, 12, 23),
    (38, 13, 36),
    (40, 14, 37),
    (37, 15, 38),
    (46, 16, 50),
    (45, 17, 49),
    (31, 18, 24),
    (35, 19, 25),
    (12, 20, 16),
    (15, 21, 19),
    (1, 22, 31),
    (17, 23, 20),
    (32, 24, 22),
    (9, 25, 47),
    (13, 26, 17),
    (7, 27, 45),
    (16, 28, 21),
    (6, 29, 44),
    (18, 30, 2),
    (19, 31, 3),
    (20, 32, 4),
    (21, 33, 5),
    (23, 34, 6),
    (22, 35, 7),
    (26, 36, 8),
    (28, 37, 9),
    (25, 38, 10),
    (29, 39, 11),
    (36, 40, 28),
    (53, 41, 1),
    (51, 42, 14),
    (48, 43, 15),
    (49, 44, 57),
    (27, 45, 12),
    (24, 46, 13),
    (33, 47, 26),
    (30, 48, 27),
    (42, 49, 43),
    (41, 51, 39),
    (39, 52, 40),
    (50, 53, 41),
    (43, 54, 51),
    (47, 55, 52),
    (44, 56, 53),
    (57, 57, 58),
    (122, 58, 59),
    (120, 59, 60),
    (99, 60, 61),
    (118, 61, 62),
    (96, 62, 63),
    (97, 63, 64),
    (98, 64, 65),
    (100, 65, 66),
    (101, 66, 67),
    (109, 67, 68),
    (103, 68, 87),
    (111, 69, 88),
    (114, 73, 110),
    (115, 74, 102),
    (116, 75, 104),
    (117, 76, 111),
    (119, 77, 107),
    (121, 78, 109),
    (124, 79, 106),
    (123, 80, 105),
    (125, 81, 108),
    (126, 82, 103),
    (71, 83, 69),
    (75, 84, 98),
    (67, 85, 55),
    (78, 86, 74),
    (69, 87, 78),
    (76, 88, 96),
    (83, 89, 79),
    (84, 90, 80),
    (85, 91, 81),
    (86, 92, 75),
    (87, 93, 76),
    (88, 94, 77),
    (89, 95, 71),
    (91, 96, 72),
    (92, 97, 73),
    (82, 98, 82),
    (65, 99, 83),
    (59, 224, 29),
    (56, 225, 42),
    (58, 226, 56),
    (55, 227, 125),
    (62, 228, 97),
    (60, 229, 54),
    (61, 230, 100),
    (54, 231, 126),
];
pub fn mac_to_hid(key: u16) -> Option<u16> {
    KEYS.iter().find(|k| k.0 == key).map(|k| k.1)
}
pub fn hid_to_evdev(key: u16) -> Option<u16> {
    KEYS.iter().find(|k| k.1 == key).map(|k| k.2)
}

#[derive(Debug)]
pub struct Lease {
    session: String,
    controller: String,
    target: String,
    sequence: Option<u64>,
    deadline: Option<u64>,
}
impl Lease {
    pub fn new(session: String, controller: String, target: String) -> Result<Self, ContractError> {
        if [&session, &controller, &target]
            .iter()
            .any(|id| !crate::slug(id))
        {
            return Err(ContractError("invalid desktop receiver identity".into()));
        }
        Ok(Self {
            session,
            controller,
            target,
            sequence: None,
            deadline: None,
        })
    }
    /// Wall time validates freshness; monotonic time controls release. A wall
    /// clock correction cannot extend the lifetime of already-held controls.
    pub fn accept(
        &mut self,
        state: &DesktopInputState,
        wall_ms: u64,
        mono_ms: u64,
    ) -> Result<bool, ContractError> {
        state.validate()?;
        if state.session_id != self.session
            || state.controller_id != self.controller
            || state.target_id != self.target
        {
            return Err(ContractError("unauthorized desktop input identity".into()));
        }
        if state.keys.iter().any(|k| hid_to_evdev(*k).is_none()) {
            return Err(ContractError("unsupported physical keyboard usage".into()));
        }
        if state.sampled_at_unix_ms > wall_ms.saturating_add(50)
            || state.valid_until_unix_ms <= wall_ms
        {
            return Ok(false);
        }
        if self.sequence.is_some_and(|s| state.sequence <= s) {
            return Ok(false);
        }
        let remaining = (state.valid_until_unix_ms - wall_ms).min(crate::MAX_DESKTOP_LEASE_MS);
        self.sequence = Some(state.sequence);
        self.deadline = Some(mono_ms.saturating_add(remaining));
        Ok(true)
    }
    pub fn expired(&mut self, mono_ms: u64) -> bool {
        if self.deadline.is_some_and(|deadline| mono_ms >= deadline) {
            self.deadline = None;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> DesktopInputState {
        DesktopInputState {
            schema_version: 1,
            session_id: "neo-tv".into(),
            controller_id: "neo".into(),
            target_id: "home-pi".into(),
            sequence: 1,
            sampled_at_unix_ms: 1000,
            valid_until_unix_ms: 1250,
            keys: vec![4, 224],
            pointer_x: 0,
            pointer_y: 0,
            pointer_buttons: 1,
            scroll_x: 0,
            scroll_y: 1,
        }
    }
    fn lease() -> Lease {
        Lease::new("neo-tv".into(), "neo".into(), "home-pi".into()).unwrap()
    }
    #[test]
    fn mapping_is_bijective_and_common_keys_are_physical() {
        for column in 0..3 {
            let mut seen = std::collections::BTreeSet::new();
            for k in KEYS {
                assert!(seen.insert([k.0, k.1, k.2][column]));
            }
        }
        assert_eq!(mac_to_hid(0), Some(4));
        assert_eq!(hid_to_evdev(4), Some(30));
        assert_eq!(hid_to_evdev(224), Some(29));
        assert_eq!(mac_to_hid(255), None);
    }
    #[test]
    fn duplicates_do_not_replay_scroll_or_extend_lease() {
        let mut l = lease();
        assert!(l.accept(&state(), 1000, 50).unwrap());
        assert!(!l.accept(&state(), 1100, 150).unwrap());
        assert!(!l.expired(299));
        assert!(l.expired(300));
        assert!(!l.expired(301));
        assert!(!l.accept(&state(), 1100, 301).unwrap());
    }
    #[test]
    fn unauthorized_future_expired_and_unknown_keys_cannot_take_control() {
        let mut l = lease();
        let mut s = state();
        s.target_id = "other".into();
        assert!(l.accept(&s, 1000, 0).is_err());
        s = state();
        s.keys = vec![65535];
        assert!(l.accept(&s, 1000, 0).is_err());
        assert!(!l.accept(&state(), 949, 0).unwrap());
        assert!(!l.accept(&state(), 1250, 0).unwrap());
        assert!(l.accept(&state(), 1000, 0).unwrap());
    }
}
