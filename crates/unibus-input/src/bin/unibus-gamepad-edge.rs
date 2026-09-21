#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("unibus-gamepad-edge requires Linux uinput");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    linux::run()
}

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{Context, Result, ensure};
    use evdev::{
        AbsInfo, AbsoluteAxisCode, AbsoluteAxisEvent, AttributeSet, InputEvent, KeyCode, KeyEvent,
        UinputAbsSetup, uinput::VirtualDevice,
    };
    use serde::Deserialize;
    use std::{
        collections::BTreeMap,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };
    use unibus::client::{Client, ClientConfig};
    use unibus_input::{GAMEPAD_STATE_CONTRACT, GamepadState, button};

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Config {
        bus: ClientConfig,
        session_id: String,
        controllers: BTreeMap<String, u8>,
    }

    struct Slot {
        device: VirtualDevice,
        last_sequence: Option<u64>,
        release_at: Option<Instant>,
        released: bool,
    }

    pub fn run() -> Result<()> {
        let path = std::env::args()
            .nth(1)
            .context("usage: unibus-gamepad-edge <config.json>")?;
        let config: Config =
            serde_json::from_slice(&std::fs::read(&path).with_context(|| format!("read {path}"))?)?;
        ensure!(
            !config.controllers.is_empty(),
            "at least one controller is required"
        );
        let mut slots = BTreeMap::new();
        for (&slot, _) in config.controllers.iter().map(|(id, slot)| (slot, id)) {
            ensure!((1..=4).contains(&slot), "player slot must be 1..=4");
            slots.entry(slot).or_insert(Slot {
                device: create_gamepad(slot)?,
                last_sequence: None,
                release_at: None,
                released: true,
            });
        }

        let client = Client::connect(config.bus)?;
        loop {
            if let Some((envelope, payload)) = client.try_read()? {
                if envelope.message.kind != GAMEPAD_STATE_CONTRACT {
                    release_expired(&mut slots)?;
                    continue;
                }
                ensure!(
                    payload.is_empty(),
                    "gamepad state must not contain a binary payload"
                );
                let state: GamepadState =
                    serde_json::from_value(serde_json::Value::Object(envelope.message.body))?;
                state.validate().map_err(anyhow::Error::msg)?;
                if state.session_id != config.session_id {
                    continue;
                }
                let Some(&authorized_slot) = config.controllers.get(&state.controller_id) else {
                    eprintln!(
                        "unibus-gamepad-edge: rejected unknown controller {}",
                        state.controller_id
                    );
                    continue;
                };
                if authorized_slot != state.player_slot {
                    eprintln!(
                        "unibus-gamepad-edge: rejected slot mismatch for {}",
                        state.controller_id
                    );
                    continue;
                }
                let now_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
                if state.valid_until_unix_ms <= now_ms {
                    continue;
                }
                let slot = slots
                    .get_mut(&state.player_slot)
                    .expect("validated slot exists");
                if slot
                    .last_sequence
                    .is_some_and(|last| state.sequence <= last)
                {
                    continue;
                }
                emit_state(&mut slot.device, &state)?;
                slot.last_sequence = Some(state.sequence);
                slot.release_at = Some(
                    Instant::now() + Duration::from_millis(state.valid_until_unix_ms - now_ms),
                );
                slot.released = false;
            }
            release_expired(&mut slots)?;
        }
    }

    fn create_gamepad(slot: u8) -> Result<VirtualDevice> {
        let name = format!("Unibus Gamepad P{slot}");
        let mut keys = AttributeSet::<KeyCode>::new();
        for (_, key) in BUTTONS {
            keys.insert(*key);
        }
        let stick = AbsInfo::new(0, i16::MIN as i32, i16::MAX as i32, 256, 1024, 1);
        let trigger = AbsInfo::new(0, 0, u16::MAX as i32, 128, 512, 1);
        let mut builder = VirtualDevice::builder()
            .context("open /dev/uinput")?
            .name(&name)
            .with_keys(&keys)?;
        for axis in [
            AbsoluteAxisCode::ABS_X,
            AbsoluteAxisCode::ABS_Y,
            AbsoluteAxisCode::ABS_RX,
            AbsoluteAxisCode::ABS_RY,
        ] {
            builder = builder.with_absolute_axis(&UinputAbsSetup::new(axis, stick))?;
        }
        for axis in [AbsoluteAxisCode::ABS_Z, AbsoluteAxisCode::ABS_RZ] {
            builder = builder.with_absolute_axis(&UinputAbsSetup::new(axis, trigger))?;
        }
        builder.build().context("create virtual gamepad")
    }

    const BUTTONS: &[(u32, KeyCode)] = &[
        (button::SOUTH, KeyCode::BTN_SOUTH),
        (button::EAST, KeyCode::BTN_EAST),
        (button::WEST, KeyCode::BTN_WEST),
        (button::NORTH, KeyCode::BTN_NORTH),
        (button::LEFT_SHOULDER, KeyCode::BTN_TL),
        (button::RIGHT_SHOULDER, KeyCode::BTN_TR),
        (button::SELECT, KeyCode::BTN_SELECT),
        (button::START, KeyCode::BTN_START),
        (button::MODE, KeyCode::BTN_MODE),
        (button::LEFT_THUMB, KeyCode::BTN_THUMBL),
        (button::RIGHT_THUMB, KeyCode::BTN_THUMBR),
        (button::DPAD_UP, KeyCode::BTN_DPAD_UP),
        (button::DPAD_DOWN, KeyCode::BTN_DPAD_DOWN),
        (button::DPAD_LEFT, KeyCode::BTN_DPAD_LEFT),
        (button::DPAD_RIGHT, KeyCode::BTN_DPAD_RIGHT),
    ];

    fn emit_state(device: &mut VirtualDevice, state: &GamepadState) -> Result<()> {
        let mut events = Vec::<InputEvent>::with_capacity(BUTTONS.len() + 6);
        for (mask, key) in BUTTONS {
            events.push(*KeyEvent::new(*key, i32::from(state.buttons & mask != 0)));
        }
        for (axis, value) in [
            (AbsoluteAxisCode::ABS_X, state.axes.left_x as i32),
            (AbsoluteAxisCode::ABS_Y, state.axes.left_y as i32),
            (AbsoluteAxisCode::ABS_RX, state.axes.right_x as i32),
            (AbsoluteAxisCode::ABS_RY, state.axes.right_y as i32),
            (AbsoluteAxisCode::ABS_Z, state.axes.left_trigger as i32),
            (AbsoluteAxisCode::ABS_RZ, state.axes.right_trigger as i32),
        ] {
            events.push(*AbsoluteAxisEvent::new(axis, value));
        }
        device.emit(&events).context("emit virtual gamepad state")
    }

    fn release_expired(slots: &mut BTreeMap<u8, Slot>) -> Result<()> {
        let now = Instant::now();
        for slot in slots.values_mut() {
            if !slot.released && slot.release_at.is_some_and(|deadline| deadline <= now) {
                let neutral = GamepadState {
                    schema_version: 1,
                    session_id: "neutral".into(),
                    controller_id: "neutral".into(),
                    player_slot: 1,
                    sequence: 0,
                    sampled_at_unix_ms: 0,
                    valid_until_unix_ms: 1,
                    buttons: 0,
                    axes: Default::default(),
                };
                emit_state(&mut slot.device, &neutral)?;
                slot.released = true;
                slot.release_at = None;
            }
        }
        Ok(())
    }
}
