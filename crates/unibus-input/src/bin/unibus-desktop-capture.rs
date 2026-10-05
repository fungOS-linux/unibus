//! Independently permissioned macOS shortcut takeover. Never captures at startup.
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("unibus-desktop-capture requires macOS");
    std::process::exit(1);
}
#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    mac::run()
}

#[cfg(target_os = "macos")]
mod mac {
    use anyhow::{Context, Result, ensure};
    use std::{
        collections::BTreeSet,
        ffi::c_void,
        ptr,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc::{self, SyncSender},
        },
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };
    use unibus::{Message, client::Client};
    use unibus_input::{
        DESKTOP_STATE_CONTRACT, DesktopInputState,
        desktop::{ACK_CONTRACT, Ack, mac_to_hid},
    };

    type Event = *mut c_void;
    #[repr(C)]
    #[derive(Clone, Copy, PartialEq, Eq)]
    struct Front {
        high: u32,
        low: u32,
    }
    type Callback = unsafe extern "C" fn(*mut c_void, u32, Event, *mut c_void) -> Event;
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn GetFrontProcess(process: *mut Front) -> i16;
        fn IsSecureEventInputEnabled() -> bool;
    }
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
        fn CGEventTapCreate(
            location: u32,
            placement: u32,
            options: u32,
            mask: u64,
            callback: Callback,
            context: *mut c_void,
        ) -> *mut c_void;
        fn CGEventTapEnable(tap: *mut c_void, enabled: bool);
        fn CGEventGetIntegerValueField(event: Event, field: u32) -> i64;
        fn CGEventGetFlags(event: Event) -> u64;
        fn CGEventSourceKeyState(source: i32, key: u16) -> bool;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopCommonModes: *const c_void;
        fn CFMachPortCreateRunLoopSource(
            allocator: *const c_void,
            port: *mut c_void,
            order: isize,
        ) -> *mut c_void;
        fn CFRunLoopGetCurrent() -> *mut c_void;
        fn CFRunLoopAddSource(runloop: *mut c_void, source: *mut c_void, mode: *const c_void);
        fn CFRunLoopRun();
        fn CFRunLoopStop(runloop: *mut c_void);
        fn CFRelease(value: *const c_void);
    }

    use unibus_input::desktop::CaptureConfig as Config;
    #[derive(Clone)]
    struct Report {
        epoch: u64,
        keys: Vec<u16>,
        x: u16,
        y: u16,
        buttons: u8,
        scroll_x: i16,
        scroll_y: i16,
    }
    struct State {
        active: bool,
        epoch: u64,
        keys: BTreeSet<u16>,
        buttons: u8,
        x: f64,
        y: f64,
        blocked: BTreeSet<u16>,
        last_ack: Instant,
        ready: bool,
        pointer_dirty: bool,
        front: Option<Front>,
        notice: Option<&'static str>,
    }
    impl State {
        fn release(&mut self) {
            if !self.active && self.keys.is_empty() && self.buttons == 0 {
                return;
            }
            self.active = false;
            self.epoch = self.epoch.wrapping_add(1);
            self.keys.clear();
            self.buttons = 0;
            self.front = None;
        }
        fn move_pointer(&mut self, dx: f64, dy: f64, width: f64, height: f64) {
            self.x = (self.x + dx * 65535.0 / width).clamp(0.0, 65535.0);
            self.y = (self.y + dy * 65535.0 / height).clamp(0.0, 65535.0);
            self.pointer_dirty = true;
        }
        fn report(&self, sx: i16, sy: i16) -> Report {
            Report {
                epoch: self.epoch,
                keys: self.keys.iter().copied().collect(),
                x: self.x as u16,
                y: self.y as u16,
                buttons: self.buttons,
                scroll_x: sx,
                scroll_y: sy,
            }
        }
    }
    struct ContextState {
        state: Arc<Mutex<State>>,
        queue: SyncSender<Report>,
        alive: Arc<AtomicBool>,
        width: f64,
        height: f64,
        hotkey: u16,
        scroll_enabled: bool,
    }

    /// Safety: CoreGraphics invokes this serially on the owning run loop. `data`
    /// points to the stable boxed ContextState retained until the tap is released.
    /// Events are borrowed OS objects: never retain, mutate, or free them. No I/O
    /// occurs here. Null suppresses the event only during explicit takeover.
    unsafe extern "C" fn callback(
        _proxy: *mut c_void,
        kind: u32,
        event: Event,
        data: *mut c_void,
    ) -> Event {
        if event.is_null() {
            return event;
        }
        let ctx = unsafe { &*(data as *const ContextState) };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut s = ctx.state.lock().unwrap();
            if kind >= 0xffff_fffe {
                s.release();
                s.ready = false;
                s.notice = Some("TV control released: event tap disabled");
                return event;
            }
            if !ctx.alive.load(Ordering::Acquire)
                || (s.active && s.last_ack.elapsed() > Duration::from_millis(500))
            {
                s.release();
                s.ready = false;
            }
            let key = unsafe { CGEventGetIntegerValueField(event, 9) } as u16;
            let flags = unsafe { CGEventGetFlags(event) };
            if kind == 10
                && key == ctx.hotkey
                && flags & ((1 << 18) | (1 << 19)) == ((1 << 18) | (1 << 19))
            {
                // Autorepeat must not toggle the mode repeatedly.
                if s.blocked.insert(key) {
                    if s.active {
                        s.release();
                        s.notice = Some("TV control OFF");
                    } else if s.ready {
                        s.active = true;
                        s.epoch = s.epoch.wrapping_add(1);
                        s.keys.clear();
                        s.buttons = 0;
                        s.last_ack = Instant::now();
                        s.pointer_dirty = true;
                        s.notice = Some("TV control ON · Escape returns to Neo");
                    } else {
                        s.notice = Some(
                            "TV control unavailable: receiver disconnected or permission lost",
                        );
                    }
                }
                return ptr::null_mut();
            }
            if kind == 11 && s.blocked.remove(&key) {
                return ptr::null_mut();
            }
            if s.active && kind == 10 && key == 53 {
                s.blocked.insert(key);
                s.release();
                s.notice = Some("TV control OFF · Escape");
                return ptr::null_mut();
            }
            if !s.active {
                return event;
            }
            let mut sx = 0;
            let mut sy = 0;
            match kind {
                10..=12 => {
                    if let Some(hid) = mac_to_hid(key) {
                        if key == 57 {
                            // Caps Lock is a toggle; do not hold it.
                            s.keys.insert(hid);
                            if ctx.queue.try_send(s.report(0, 0)).is_err() {
                                s.release();
                                return ptr::null_mut();
                            }
                            s.keys.remove(&hid);
                            if ctx.queue.try_send(s.report(0, 0)).is_err() {
                                s.release();
                            }
                            return ptr::null_mut();
                        }
                        let down = if kind == 12 {
                            unsafe { CGEventSourceKeyState(1, key) }
                        } else {
                            kind == 10
                        };
                        if down {
                            s.keys.insert(hid);
                        } else {
                            s.keys.remove(&hid);
                        }
                    }
                }
                1 | 2 | 3 | 4 | 25 | 26 => {
                    let bit = match kind {
                        1 | 2 => 1,
                        3 | 4 => 2,
                        _ => {
                            let b = unsafe { CGEventGetIntegerValueField(event, 3) };
                            match b {
                                2 => 4,
                                3 => 8,
                                4 => 16,
                                _ => 0,
                            }
                        }
                    };
                    if matches!(kind, 1 | 3 | 25) {
                        s.buttons |= bit;
                    } else {
                        s.buttons &= !bit;
                    }
                }
                5 | 6 | 7 | 27 => {
                    s.move_pointer(
                        unsafe { CGEventGetIntegerValueField(event, 4) } as f64,
                        unsafe { CGEventGetIntegerValueField(event, 5) } as f64,
                        ctx.width,
                        ctx.height,
                    );
                    // Coalesce motion only. Key/button edges keep their ordered
                    // bounded queue, so short taps cannot disappear.
                    return ptr::null_mut();
                }
                22 => {
                    if !ctx.scroll_enabled {
                        s.notice =
                            Some("TV wheel input unavailable on this compositor; use Page Up/Down");
                        return ptr::null_mut();
                    }
                    sy = unsafe { CGEventGetIntegerValueField(event, 11) }
                        .clamp(i16::MIN as i64, i16::MAX as i64) as i16;
                    sx = unsafe { CGEventGetIntegerValueField(event, 12) }
                        .clamp(i16::MIN as i64, i16::MAX as i64) as i16;
                }
                _ => return event,
            }
            if s.keys.len() > unibus_input::MAX_DESKTOP_KEYS
                || ctx.queue.try_send(s.report(sx, sy)).is_err()
            {
                s.release();
                s.notice = Some("TV control released: input queue saturated");
            }
            // Modifier changes pass locally too, so the takeover chord cannot
            // leave Neo applications believing Ctrl/Option remain depressed.
            if kind == 12 { event } else { ptr::null_mut() }
        }));
        match result {
            Ok(value) => value,
            Err(_) => {
                ctx.alive.store(false, Ordering::Release);
                event
            }
        }
    }

    pub fn run() -> Result<()> {
        let path = std::env::args()
            .nth(1)
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| {
                    std::path::PathBuf::from(home)
                        .join("Library/Application Support/Unibus TV Control/capture.json")
                })
            })
            .context("usage: unibus-desktop-capture config.json")?;
        let mut config: Config = serde_json::from_slice(&std::fs::read(path)?)?;
        ensure!(
            config.width > 0 && config.height > 0,
            "target dimensions must be positive"
        );
        ensure!(
            mac_to_hid(config.hotkey_keycode).is_some(),
            "unsupported shortcut key"
        );
        unibus_input::desktop::Lease::new(
            config.session_id.clone(),
            config.controller_id.clone(),
            config.target_id.clone(),
        )
        .map_err(anyhow::Error::msg)?;
        ensure!(
            !config.destination.is_empty(),
            "receiver producer must not be empty"
        );
        ensure!(
            unsafe { AXIsProcessTrusted() },
            "Enable Accessibility for Unibus TV Control in macOS System Settings; the existing Dock Control grant is not reused"
        );
        config.bus.read_timeout_ms = 1;
        let client = Client::connect(config.bus.clone())?;
        let state = Arc::new(Mutex::new(State {
            active: false,
            epoch: 0,
            keys: BTreeSet::new(),
            buttons: 0,
            x: 32768.0,
            y: 32768.0,
            blocked: BTreeSet::new(),
            last_ack: Instant::now(),
            ready: true,
            pointer_dirty: false,
            front: None,
            notice: None,
        }));
        let alive = Arc::new(AtomicBool::new(true));
        let (tx, rx) = mpsc::sync_channel::<Report>(64);
        let ctx = Box::new(ContextState {
            state: state.clone(),
            queue: tx,
            alive: alive.clone(),
            width: config.width as f64,
            height: config.height as f64,
            hotkey: config.hotkey_keycode,
            scroll_enabled: config.scroll_enabled,
        });
        let worker_state = state.clone();
        let worker_alive = alive.clone();
        // CoreFoundation run loops may be stopped from another thread. Retain
        // only its opaque address; the main thread owns the run loop lifetime.
        let owner_loop = unsafe { CFRunLoopGetCurrent() } as usize;
        std::thread::spawn(move || {
            let result = (|| -> Result<()> {
                let mut sequence = SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros() as u64;
                let mut ack_sequence = 0;
                let mut was_active = false;
                let mut last_sent = Instant::now() - Duration::from_secs(1);
                let mut pending = std::collections::VecDeque::<(u64, Instant)>::new();
                let mut epoch = 0;
                let mut focus_poll = Instant::now() - Duration::from_secs(1);
                loop {
                    let queued = rx.recv_timeout(Duration::from_millis(5)).ok();
                    let focus = if focus_poll.elapsed() >= Duration::from_millis(50) {
                        let mut front = Front { high: 0, low: 0 };
                        focus_poll = Instant::now();
                        Some(if unsafe { GetFrontProcess(&mut front) } == 0 {
                            Some(front)
                        } else {
                            None
                        })
                    } else {
                        None
                    };
                    let (active, report, emit, notice) = {
                        let mut s = worker_state.lock().unwrap();
                        if unsafe { IsSecureEventInputEnabled() } {
                            s.release();
                            s.ready = false;
                        }
                        if s.active && s.last_ack.elapsed() > Duration::from_millis(500) {
                            s.release();
                            s.notice =
                                Some("TV control released: receiver acknowledgements stopped");
                        }
                        if s.active {
                            if let Some(front) = focus {
                                if front.is_none() || (s.front.is_some() && s.front != front) {
                                    s.release();
                                    s.notice = Some("TV control released: Neo foreground changed");
                                } else {
                                    s.front = front;
                                }
                            }
                        }
                        let queued = queued.filter(|r| r.epoch == s.epoch);
                        let emit = (s.active
                            && (queued.is_some()
                                || (s.pointer_dirty
                                    && last_sent.elapsed() >= Duration::from_millis(12))
                                || last_sent.elapsed() >= Duration::from_millis(50)))
                            || (!s.active && was_active);
                        if emit && queued.is_none() {
                            s.pointer_dirty = false;
                        }
                        (
                            s.active,
                            queued.unwrap_or_else(|| s.report(0, 0)),
                            emit,
                            s.notice.take(),
                        )
                    };
                    if let Some(notice) = notice {
                        eprintln!("{notice}");
                    }
                    if report.epoch != epoch {
                        pending.clear();
                        epoch = report.epoch;
                    }
                    if emit {
                        sequence = sequence
                            .checked_add(1)
                            .context("input sequence exhausted")?;
                        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
                        let input = DesktopInputState {
                            schema_version: 1,
                            session_id: config.session_id.clone(),
                            controller_id: config.controller_id.clone(),
                            target_id: config.target_id.clone(),
                            sequence,
                            sampled_at_unix_ms: now,
                            valid_until_unix_ms: now + 250,
                            keys: if active { report.keys } else { Vec::new() },
                            pointer_x: report.x,
                            pointer_y: report.y,
                            pointer_buttons: if active { report.buttons } else { 0 },
                            scroll_x: if active { report.scroll_x } else { 0 },
                            scroll_y: if active { report.scroll_y } else { 0 },
                        };
                        input.validate().map_err(anyhow::Error::msg)?;
                        let envelope = client.envelope(
                            Some(&config.destination),
                            Message::new(DESKTOP_STATE_CONTRACT, serde_json::to_value(&input)?),
                        )?;
                        client.send(&envelope, &[])?;
                        if pending.len() == 128 {
                            pending.pop_front();
                        }
                        pending.push_back((sequence, Instant::now()));
                        was_active = active;
                        last_sent = Instant::now();
                    }
                    if let Some((envelope, payload)) = client.try_read()? {
                        if envelope.message.kind == ACK_CONTRACT
                            && envelope.producer_id == config.destination
                            && payload.is_empty()
                        {
                            let ack: Ack = serde_json::from_value(serde_json::Value::Object(
                                envelope.message.body,
                            ))?;
                            if ack.session_id == config.session_id
                                && ack.controller_id == config.controller_id
                                && ack.target_id == config.target_id
                                && ack.sequence > ack_sequence
                                && ack.sequence <= sequence
                            {
                                ack_sequence = ack.sequence;
                                if pending.iter().any(|(seq, sent)| {
                                    *seq == ack.sequence
                                        && sent.elapsed() <= Duration::from_millis(250)
                                }) {
                                    worker_state.lock().unwrap().last_ack = Instant::now();
                                }
                                pending.retain(|(seq, sent)| {
                                    *seq > ack.sequence
                                        && sent.elapsed() <= Duration::from_millis(500)
                                });
                            }
                        }
                    }
                }
            })();
            if let Err(error) = result {
                eprintln!("TV control stopped: {error:#}");
            }
            worker_alive.store(false, Ordering::Release);
            if let Ok(mut s) = worker_state.lock() {
                s.release();
                s.ready = false;
            }
            unsafe { CFRunLoopStop(owner_loop as *mut c_void) };
        });
        // SAFETY: callback context is boxed and remains alive for the entire
        // synchronous run loop; CF objects follow create/release ownership.
        unsafe {
            let mask = [1, 2, 3, 4, 5, 6, 7, 10, 11, 12, 22, 25, 26, 27]
                .iter()
                .fold(0u64, |m, k| m | (1u64 << k));
            let tap = CGEventTapCreate(
                0,
                0,
                0,
                mask,
                callback,
                (&*ctx as *const ContextState).cast_mut().cast(),
            );
            ensure!(
                !tap.is_null(),
                "macOS refused the event tap; grant Input Monitoring and Accessibility to Unibus TV Control"
            );
            let source = CFMachPortCreateRunLoopSource(ptr::null(), tap, 0);
            if source.is_null() {
                CFRelease(tap);
                anyhow::bail!("cannot create input run-loop source");
            }
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
            CGEventTapEnable(tap, true);
            eprintln!(
                "TV control ready · Ctrl+Option+Space toggles · Escape releases · starts OFF"
            );
            CFRunLoopRun();
            CGEventTapEnable(tap, false);
            CFRelease(source);
            CFRelease(tap);
        }
        state.lock().unwrap().release();
        ensure!(
            alive.load(Ordering::Acquire),
            "input transport stopped; takeover released"
        );
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        fn state() -> State {
            State {
                active: false,
                epoch: 0,
                keys: BTreeSet::new(),
                buttons: 0,
                x: 32768.0,
                y: 32768.0,
                blocked: BTreeSet::new(),
                last_ack: Instant::now(),
                ready: true,
                pointer_dirty: false,
                front: None,
                notice: None,
            }
        }
        #[test]
        fn starts_off_and_release_invalidates_held_reports() {
            let mut s = state();
            assert!(!s.active);
            s.active = true;
            s.keys.insert(4);
            s.buttons = 1;
            let old = s.report(0, 0);
            s.release();
            assert!(!s.active);
            assert!(s.keys.is_empty());
            assert_eq!(s.buttons, 0);
            assert_ne!(s.epoch, old.epoch);
            assert_eq!((s.x, s.y), (32768.0, 32768.0));
            let epoch = s.epoch;
            s.release();
            assert_eq!(s.epoch, epoch);
        }
        #[test]
        fn pointer_motion_is_normalized_and_clamped() {
            let mut s = state();
            s.move_pointer(192.0, -108.0, 1920.0, 1080.0);
            assert!((s.x - 39321.5).abs() < 0.01);
            assert!((s.y - 26214.5).abs() < 0.01);
            assert!(s.pointer_dirty);
            s.move_pointer(1e6, -1e6, 1920.0, 1080.0);
            assert_eq!((s.x, s.y), (65535.0, 0.0));
        }
    }
}
