//! Authorized leased desktop input -> a Linux virtual keyboard/pointer.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("unibus-desktop-edge requires Linux");
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
        AbsInfo, AbsoluteAxisCode as Abs, AttributeSet, InputEvent, KeyCode, PropType,
        RelativeAxisCode as Rel, UinputAbsSetup, uinput::VirtualDevice,
    };
    use std::{
        collections::BTreeSet,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };
    use unibus::{Message, client::Client};
    use unibus_input::{
        DESKTOP_STATE_CONTRACT, DesktopInputState,
        desktop::{ACK_CONTRACT, Ack, KEYS, Lease, hid_to_evdev},
        pointer_button,
    };

    use unibus_input::desktop::EdgeConfig as Config;
    struct Compositor {
        stream: std::os::unix::net::UnixStream,
        reader: std::io::BufReader<std::os::unix::net::UnixStream>,
        sequence: u64,
        x: u16,
        y: u16,
    }
    impl Compositor {
        fn new(path: &std::path::Path) -> Result<Self> {
            let stream = std::os::unix::net::UnixStream::connect(path)?;
            stream.set_read_timeout(Some(Duration::from_millis(100)))?;
            stream.set_write_timeout(Some(Duration::from_millis(100)))?;
            Ok(Self {
                reader: std::io::BufReader::new(stream.try_clone()?),
                stream,
                sequence: SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros() as u64,
                x: 32768,
                y: 32768,
            })
        }
        fn input(
            &mut self,
            lease_ms: u64,
            keys: &[u16],
            x: u16,
            y: u16,
            buttons: u8,
        ) -> Result<()> {
            use std::io::{BufRead, Read, Write};
            self.sequence = self
                .sequence
                .checked_add(1)
                .context("compositor sequence exhausted")?;
            self.x = x;
            self.y = y;
            let sequence = self.sequence;
            let command = serde_json::json!({"command":"input","sequence":sequence,"lease_ms":lease_ms,"keys":keys,"pointer_x":x,"pointer_y":y,"pointer_buttons":buttons});
            writeln!(self.stream, "{command}")?;
            let mut line = String::new();
            self.reader.by_ref().take(8192).read_line(&mut line)?;
            let reply: serde_json::Value = serde_json::from_str(&line)?;
            ensure!(
                reply["ok"] == true && reply["result"]["sequence"] == sequence,
                "compositor refused input: {reply}"
            );
            Ok(())
        }
        fn apply(&mut self, s: &DesktopInputState) -> Result<()> {
            ensure!(
                s.scroll_x == 0 && s.scroll_y == 0,
                "this Canvas compositor has no advertised wheel input capability"
            );
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
            let remaining = s.valid_until_unix_ms.saturating_sub(now);
            ensure!(
                remaining > 0,
                "desktop input expired before compositor delivery"
            );
            self.input(
                remaining.min(unibus_input::MAX_DESKTOP_LEASE_MS),
                &s.keys,
                s.pointer_x,
                s.pointer_y,
                s.pointer_buttons,
            )
        }
        fn release(&mut self) -> Result<()> {
            self.input(1, &[], self.x, self.y, 0)
        }
    }
    impl Drop for Compositor {
        fn drop(&mut self) {
            let _ = self.release();
        }
    }
    enum Backend {
        Uinput(Device),
        Canvas(Compositor),
    }
    impl Backend {
        fn apply(&mut self, s: &DesktopInputState) -> Result<()> {
            match self {
                Self::Uinput(d) => d.apply(s),
                Self::Canvas(c) => c.apply(s),
            }
        }
        fn release(&mut self) -> Result<()> {
            match self {
                Self::Uinput(d) => d.release(),
                Self::Canvas(c) => c.release(),
            }
        }
    }
    const BUTTONS: [(u8, u16); 5] = [
        (pointer_button::LEFT, 272),
        (pointer_button::RIGHT, 273),
        (pointer_button::MIDDLE, 274),
        (pointer_button::BACK, 275),
        (pointer_button::FORWARD, 276),
    ];
    struct Device {
        keyboard: VirtualDevice,
        pointer: VirtualDevice,
        held: BTreeSet<u16>,
        buttons: u8,
    }
    impl Device {
        fn create() -> Result<Self> {
            let mut keys = AttributeSet::<KeyCode>::new();
            for key in KEYS {
                keys.insert(KeyCode::new(key.2));
            }
            let keyboard = VirtualDevice::builder()?
                .name("Unibus Neo keyboard")
                .with_keys(&keys)?
                .build()?;
            let mut buttons = AttributeSet::<KeyCode>::new();
            for (_, key) in BUTTONS {
                buttons.insert(KeyCode::new(key));
            }
            let mut rel = AttributeSet::<Rel>::new();
            rel.insert(Rel::REL_WHEEL);
            rel.insert(Rel::REL_HWHEEL);
            let mut properties = AttributeSet::<PropType>::new();
            properties.insert(PropType::POINTER);
            let axis = AbsInfo::new(0, 0, 65535, 0, 0, 1);
            let pointer = VirtualDevice::builder()?
                .name("Unibus Neo pointer")
                .with_keys(&buttons)?
                .with_properties(&properties)?
                .with_relative_axes(&rel)?
                .with_absolute_axis(&UinputAbsSetup::new(Abs::ABS_X, axis))?
                .with_absolute_axis(&UinputAbsSetup::new(Abs::ABS_Y, axis))?
                .build()?;
            Ok(Self {
                keyboard,
                pointer,
                held: BTreeSet::new(),
                buttons: 0,
            })
        }
        fn apply(&mut self, state: &DesktopInputState) -> Result<()> {
            let next: BTreeSet<_> = state
                .keys
                .iter()
                .map(|k| hid_to_evdev(*k).expect("lease validated mapping"))
                .collect();
            let mut events = Vec::new();
            for key in self.held.difference(&next) {
                events.push(InputEvent::new(1, *key, 0));
            }
            for key in next.difference(&self.held) {
                events.push(InputEvent::new(1, *key, 1));
            }
            self.keyboard.emit(&events)?;
            self.held = next;
            let mut events = vec![
                InputEvent::new(3, Abs::ABS_X.0, i32::from(state.pointer_x)),
                InputEvent::new(3, Abs::ABS_Y.0, i32::from(state.pointer_y)),
            ];
            for (mask, key) in BUTTONS {
                if (self.buttons ^ state.pointer_buttons) & mask != 0 {
                    events.push(InputEvent::new(
                        1,
                        key,
                        i32::from(state.pointer_buttons & mask != 0),
                    ));
                }
            }
            if state.scroll_y != 0 {
                events.push(InputEvent::new(
                    2,
                    Rel::REL_WHEEL.0,
                    i32::from(state.scroll_y),
                ));
            }
            if state.scroll_x != 0 {
                events.push(InputEvent::new(
                    2,
                    Rel::REL_HWHEEL.0,
                    i32::from(state.scroll_x),
                ));
            }
            self.pointer.emit(&events)?;
            self.buttons = state.pointer_buttons;
            Ok(())
        }
        fn release(&mut self) -> Result<()> {
            let keys: Vec<_> = self
                .held
                .iter()
                .map(|k| InputEvent::new(1, *k, 0))
                .collect();
            let buttons: Vec<_> = BUTTONS
                .iter()
                .filter(|(mask, _)| self.buttons & mask != 0)
                .map(|(_, key)| InputEvent::new(1, *key, 0))
                .collect();
            self.keyboard.emit(&keys)?;
            self.pointer.emit(&buttons)?;
            self.held.clear();
            self.buttons = 0;
            Ok(())
        }
    }
    impl Drop for Device {
        fn drop(&mut self) {
            let _ = self.release();
        }
    }
    pub fn run() -> Result<()> {
        let path = std::env::args()
            .nth(1)
            .context("usage: unibus-desktop-edge config.json")?;
        let mut config: Config = serde_json::from_slice(&std::fs::read(path)?)?;
        // Housekeeping must not block longer than the input lease.
        config.bus.read_timeout_ms = 10;
        let mut lease = Lease::new(config.session_id, config.controller_id, config.target_id)
            .map_err(anyhow::Error::msg)?;
        let mut device = if let Some(path) = config.compositor_socket {
            Backend::Canvas(
                Compositor::new(&path).context("connect owner-local Canvas input socket")?,
            )
        } else {
            Backend::Uinput(Device::create().context("create authorized virtual input devices")?)
        };
        let client = Client::connect(config.bus)?;
        let epoch = Instant::now();
        loop {
            if lease.expired(epoch.elapsed().as_millis() as u64) {
                device.release()?;
            }
            if let Some((envelope, payload)) = client.try_read()? {
                if envelope.message.kind != DESKTOP_STATE_CONTRACT {
                    continue;
                }
                if envelope.producer_id != config.authorized_producer {
                    eprintln!("desktop input rejected: unauthorized producer");
                    continue;
                }
                ensure!(
                    payload.is_empty(),
                    "desktop state cannot contain binary payload"
                );
                let state: DesktopInputState =
                    serde_json::from_value(serde_json::Value::Object(envelope.message.body))?;
                let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
                match lease.accept(&state, now, epoch.elapsed().as_millis() as u64) {
                    Ok(true) => {
                        device.apply(&state)?;
                        let ack = client.envelope(
                            Some(&envelope.producer_id),
                            Message::new(ACK_CONTRACT, serde_json::to_value(Ack::from(&state))?),
                        )?;
                        client.send(&ack, &[])?;
                    }
                    Ok(false) => {}
                    Err(error) => eprintln!("desktop input rejected: {error}"),
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn compositor_preserves_geometry_and_release_sequence() {
            use std::io::{BufRead, Write};
            let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
            let server_thread = std::thread::spawn(move || {
                let mut writer = server.try_clone().unwrap();
                let mut reader = std::io::BufReader::new(server);
                for (sequence, held) in [(101, true), (102, false)] {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let command: serde_json::Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(command["sequence"], sequence);
                    assert_eq!(command["pointer_x"], 1234);
                    assert_eq!(command["pointer_y"], 5678);
                    assert_eq!(
                        command["keys"],
                        if held {
                            serde_json::json!([4])
                        } else {
                            serde_json::json!([])
                        }
                    );
                    assert_eq!(command["pointer_buttons"], if held { 1 } else { 0 });
                    if held {
                        assert!(command["lease_ms"].as_u64().unwrap() <= 200);
                    }
                    writeln!(
                        writer,
                        "{}",
                        serde_json::json!({"ok":true,"result":{"sequence":sequence}})
                    )
                    .unwrap();
                }
            });
            let mut compositor = Compositor {
                reader: std::io::BufReader::new(client.try_clone().unwrap()),
                stream: client,
                sequence: 100,
                x: 0,
                y: 0,
            };
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let input = DesktopInputState {
                schema_version: 1,
                session_id: "test".into(),
                controller_id: "neo".into(),
                target_id: "tv".into(),
                sequence: 1,
                sampled_at_unix_ms: now,
                valid_until_unix_ms: now + 200,
                keys: vec![4],
                pointer_x: 1234,
                pointer_y: 5678,
                pointer_buttons: 1,
                scroll_x: 0,
                scroll_y: 0,
            };
            compositor.apply(&input).unwrap();
            compositor.release().unwrap();
            server_thread.join().unwrap();
        }
    }
}
