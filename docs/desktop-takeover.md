# Neo → TV shortcut takeover

This uses the existing `input.desktop_state.v1` contract, a separately permissioned
macOS event-tap app, and a Linux receiver. Canvas owns UI focus and injection;
Unibus owns authenticated delivery. Mycelium resolves the SSH route, not input
semantics. No existing Dock Control accessibility helper is replaced.

Press **Ctrl+Option+Space** to toggle TV control; **Escape** always releases to Neo.
Startup is OFF. Keyboard layouts belong to the destination. Pointer motion is
coalesced; ordered keyboard/button edges use a bounded queue. Reports refresh at
least every 50 ms while active and expire after 250 ms. Only acknowledgements of
recent, actually injected reports keep takeover alive. A lost connection, changed
foreground application, secure input, tap failure, or saturated edge queue releases
control. A restarted process never resumes takeover automatically.

## Installation / manual runbook

1. Build `unibus-desktop-capture`, `unibus-desktop-tunnel`, `unibus-desktop-setup`,
   and `unibus-desktop-probe` on Neo. Cross-build `unibus-desktop-edge` for
   `aarch64-unknown-linux-gnu` on Agora. The helpers are `unibus-input` binaries.
2. Run `unibus-desktop-setup` with a **new absolute private directory**. It creates
   typed paired configs and random tokens with 0700 directory / 0600 file modes.
   Never commit these generated credentials.
3. Put `router.json`, `edge.json`, and both token files in
   `/home/ajm/.config/unibus/desktop-control` on the Pi. Install the edge binary in
   `/usr/local/bin`. Install the two supplied `config/desktop-control/*.service`
   units and enable/start them. They use the existing router executable, but a
   separate loopback-only channel so normal audio/video routing is uninterrupted.
4. Run `unibus-desktop-tunnel linux-home-pi 18796 18797 ajm` on Neo. It obtains
   Mycelium's SSH argv and binds the forwarded port to loopback only. Producer
   credentials and reciprocal publish allowlists restrict the two participants.
5. Run `unibus-desktop-probe capture.json`: it sends only empty held input and
   verifies the target compositor acknowledgement. Do not run it during takeover.
6. Install `config/desktop-control/Info.plist` and the capture executable as the
   independent **Unibus TV Control.app**. Grant this app Accessibility and Input
   Monitoring in macOS System Settings. Launch it only once; read its stderr log
   for READY / OFF / failure status. Config defaults to
   `~/Library/Application Support/Unibus TV Control/capture.json`.

The supplied `dev.fpl.unibus-tv-*.plist` LaunchAgents are Neo-specific (adjust
absolute installation paths on other Macs). Bootstrap the tunnel first, then the
control agent in `gui/$(id -u)` with `launchctl bootstrap`. The tunnel retries on
disconnect; the controller retries failures after 30 seconds, always starting OFF.
Its log must say `TV control ready` before using the shortcut. A permission error
means the background app still needs its own grant, even if a terminal-run copy
was trusted. Never run the probe and controller concurrently: they share one
producer identity.

The Pi service runs as `ajm` against its owner-only native compositor socket;
no root input permission changes are necessary. If no compositor socket is
configured, the receiver can instead use Linux uinput for a conventional desktop.

## Current limitations

The installed legacy Pi compositor does **not** advertise wheel input or render a
pointer cursor. Wheel input is suppressed during takeover; use Page Up/Down. These
need compositor capabilities, not fabricated transport support. Foreground safety
detects process changes, not same-application window changes. Physical keyboard
and mouse behavior requires a user test after macOS permission grants. No viewer
or pixel-stream dependency is introduced.

To roll back: quit Unibus TV Control, stop its Neo tunnel/LaunchAgent, and disable
the new `unibus-desktop-edge` / `unibus-desktop-router` services. Existing Canvas,
media services, workspaces, and Dock Control remain untouched. Receiver leases also
release held input after abrupt process termination.
