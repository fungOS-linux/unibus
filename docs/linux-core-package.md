# Linux core package (initial slice)

The reference `unibus-core` Debian payload installs only the router. It does
not install or enable adapters, Mycelium, Canvas, an updater, or a LAN listener.
The public Rust contracts remain library crates; there is not yet a unified
operator CLI package. This is not a production-readiness certification.

The service uses systemd's dynamic unprivileged `unibus` identity. systemd owns
0700 runtime/state directories under `/run/unibus` and `/var/lib/unibus`.
Configuration lives in `/etc/unibus/hub.json` and is a Debian conffile, preserving
operator edits on upgrades. The shipped configuration grants no clients.
Change `installation_id` to the deployment's identity before granting clients.
For private keys/tokens use systemd credentials and an explicit config pointing
at credential paths; do not make secrets world-readable to accommodate the
dynamic user. Discovery never grants authorization.

## Build and inspect (Linux, isolated working directory)

Run from the repository root. The binary must be built from the same revision
as this payload, not copied from a shared target cache. `amd64` below is the
reference target; arm64 needs a matching build and control Architecture field.

```sh
cargo check -p unibus-router
cargo test -p unibus-router
cargo build --release -p unibus-router
stage=$(mktemp -d /tmp/unibus-core-package.XXXXXX)
chmod 0755 "$stage"
cp -R deploy/linux/core/. "$stage/"
install -Dm755 target/release/unibus-router "$stage/usr/bin/unibus-router"
dpkg-deb --root-owner-group --build "$stage" "$stage.deb"
dpkg-deb --info "$stage.deb"
dpkg-deb --contents "$stage.deb"
```

There are deliberately no maintainer scripts: package installation does not
start or enable this service. Image assembly or an operator explicitly runs:

```sh
sudo dpkg -i /absolute/path/to/unibus-core.deb
sudo systemctl daemon-reload
sudo systemctl enable --now unibus-router.service
systemctl status unibus-router.service
journalctl -u unibus-router.service -b
```

Do not enable this alongside an existing hub on port 18790. Migrate ownership,
grants, credentials and the listener deliberately. Legacy `unibus-hub.service`
is untouched. Restart after config changes; no live reload is claimed.

## Verification and remaining gates

The router test suite checks the exact packaged configuration. Before release,
test install/start/client rejection/restart/upgrade/config preservation/removal
in a disposable systemd VM; archive the package digest and build revision.
Rollback means installing the prior package and restarting, not overwriting
configuration. Mycelium remains the optional owner of fleet rollout policy.
Automatic lifecycle tests, a unified CLI/doctor, declared minimum Rust/toolchain,
reproducible builds and architecture-specific dependency/version metadata remain
follow-up gates. Do not treat a successful package archive build as those tests.
