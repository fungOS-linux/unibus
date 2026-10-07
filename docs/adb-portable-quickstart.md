# Portable ADB bridge (James / another machine)

Supported validation targets: macOS and Linux. Both sides use the same
`unibus-adb` binary; it includes its isolated hub. Mycelium is **optional**.
There is no requirement for sibling repositories, Canvas, or our Mac paths.
Use current stable Rust, Git, and a native C build toolchain (Xcode Command
Line Tools on macOS; `build-essential` on Debian/Ubuntu) for TLS dependencies.
Install Android platform-tools separately
for the ordinary `adb` client/server. Linux USB access needs the distro's
Android udev rules and appropriate user permissions; do not run the bridge or
ADB as root to bypass them.

## Build on each machine

```sh
git clone https://github.com/FuturePresentLabs/unibus.git
cd unibus
cargo check --locked -p unibus-adb
cargo test --locked -p unibus-adb
cargo install --locked --path crates/unibus-adb
unibus-adb --help
```

The repository requires its normal GitHub access rights. Other cross-project
Rust dependencies use public HTTPS Git URLs pinned to revisions; Cargo fetches
them automatically. For this ADB build, no ALSA, MIDI or Lua development
packages are needed. Full Linux workspace builds additionally need
`pkg-config` and `libasound2-dev`.

## USB host

Plug in the phone, enable USB debugging, and approve **this host's** ADB key.

```sh
adb start-server
adb devices -l
unibus-adb init ./adb-host --server-id james-usb --client-id james-client --installation-id james-adb
unibus-adb hub ./adb-host/router.json
```

In a second terminal:

```sh
unibus-adb ./adb-host/serve.json
```

Init creates a new private directory (0700 on Unix), independent 256-bit
tokens (0600), and config files. It never overwrites an existing directory.
Without `--adb-binary`, you start ADB yourself as above. Optionally supply an
existing trusted **absolute executable path** with `--adb-binary` to start it
automatically. No Homebrew location is assumed. The default historical
`studio-adb` / `neo-adb` IDs remain compatible; the options above avoid those
names on a new deployment. Use a distinct client identity per concurrent
consumer; init configures one consumer, not a public multi-user ADB service.

## Client handoff

Securely copy only `attach.json` and `james-client.token` from the USB host
into a private directory on the client. **Do not copy** `james-usb.token`,
the router config, or the observer token. For example, substituting your
SSH account, host and actual source directory:

```sh
mkdir -m 700 ./adb-client
scp USER@USB_HOST:/ABSOLUTE/adb-host/attach.json ./adb-client/
scp USER@USB_HOST:/ABSOLUTE/adb-host/james-client.token ./adb-client/
chmod 600 ./adb-client/james-client.token
```

Token filenames are resolved relative to their config file, **not** the
current working directory or originating user's home. Absolute token paths
in older installations remain supported. Do not print or commit tokens.

In one terminal, open the authenticated tunnel (SSH host-key verification
and SSH credentials work normally):

```sh
unibus-adb tunnel --ssh USER@USB_HOST
```

This forwards client loopback `19893` to host loopback `18793`; no LAN listener
is opened. Alternatively, `unibus-adb tunnel MYCELIUM_TARGET` uses Mycelium's
existing SSH plan. Discovery does not grant access.

In a second terminal:

```sh
unibus-adb ./adb-client/attach.json
```

Then use normal ADB in a third terminal:

```sh
adb -H 127.0.0.1 -P 5038 devices -l
adb -H 127.0.0.1 -P 5038 -s SERIAL shell getprop ro.product.model
unibus-adb inventory 127.0.0.1:5038
```

Keep `-H` and `-P` explicit: local USB ADB on port 5037 is untouched. Authorized
clients get **full host ADB-server privileges**, including every device that
server knows. The bridge is not a per-device sandbox or generic USB-over-IP.
`adb forward` sockets live on the USB host and are not automatically tunneled;
scrcpy requires an explicit capture/port arrangement.

## Troubleshooting / shutdown

- `unauthorized`: approve the debugging prompt on the phone for the USB host.
- `offline` / no devices: check USB power, cable, host ADB and Linux USB permissions.
- Missing token: copy the matching client token beside `attach.json`.
- SSH failure: verify the account, host key and access to the USB host. Mycelium
  is not necessary for the static SSH command.
- Port occupied: stop the conflicting instance or change matching loopback
  endpoints in the configs and use an equivalent explicit `ssh -N -L` command.
- Stop each foreground process with Ctrl-C. Interrupted ADB commands fail
  visibly; reconnect and reissue them. Nothing is replayed automatically.

The existing launchd files are site-specific deployment examples, not installers
for James. Use the foreground commands first; a service manager can run those
same commands under the owning user's identity once verified.
