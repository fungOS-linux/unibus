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

Run from a clean tracked-source checkout with the workspace's existing sibling
Isochrone checkout available. The binary and packager must be built from the
same revision, not copied from an unrelated shared target cache. An actively
modified workspace may legitimately require a different lockfile; do not
overwrite someone else's dependency work to make this gate pass.

```sh
cargo metadata --locked --no-deps --format-version 1
cargo check --locked -p unibus-router --all-targets
cargo test --locked -p unibus-router
cargo test --locked -p unibus-router --example linux_core_deb
cargo test --locked -p unibus-router --example linux_core_deb -- --ignored
cargo build --locked --release -p unibus-router --bin unibus-router --example linux_core_deb
package_out=$(mktemp -d /var/tmp/unibus-core-package.XXXXXX)
target/release/examples/linux_core_deb target/release/unibus-router "$package_out"
dpkg-deb --info "$package_out/unibus-core_0.1.0_amd64.deb"
dpkg-deb --contents "$package_out/unibus-core_0.1.0_amd64.deb"
```

The Rust packager uses the shared payload assets, derives architecture from the
ELF header (`amd64` or `arm64`), and calculates the libc floor from required GLIBC
symbol versions using `readelf`. It rejects unknown runtime libraries and refuses
to overwrite an existing package. It publishes only a completed archive, with
fixed payload permissions independent of the builder's umask. Version comes
from Cargo's package version. The explicit `--ignored` command runs its Debian
archive integration check, which requires `readelf` and `dpkg-deb`.
The static control file remains the amd64 reference template; generated packages
do not trust its architecture/version/libc values. `readelf` and `dpkg-deb` are
build dependencies, not runtime requirements. This does not yet guarantee
bit-for-bit reproducibility or validate that a supplied executable matches the
packager's source revision; artifact provenance remains the build owner's job.

### ARM64 cross-build on Agora

Use the same clean checkout and locked dependency graph. With the GNU AArch64
cross linker/sysroot and QEMU user runner installed:

```sh
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='qemu-aarch64 -L /usr/aarch64-linux-gnu'
cargo check --locked -p unibus-router --target aarch64-unknown-linux-gnu
cargo test --locked -p unibus-router --target aarch64-unknown-linux-gnu
cargo build --locked --release -p unibus-router --target aarch64-unknown-linux-gnu --bin unibus-router --example linux_package_lifecycle
target/release/examples/linux_core_deb target/aarch64-unknown-linux-gnu/release/unibus-router "$package_out"
```

User-mode emulation checks Rust behavior, not systemd. Repeat the lifecycle
checker in a real ARM64 systemd guest before qualifying an ARM package. Neither
step is deployment to home-pi or a measurement of native Pi performance.

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

## Disposable-VM lifecycle check

`crates/unibus-router/examples/linux_package_lifecycle.rs` is the reusable Rust
checker. It refuses to run unless the effective user is root, virtualization is
QEMU/KVM, the DMI product is `unibus-package-validation`, and the guest has
`/run/unibus-package-validation` containing that same string. These are guardrails
against an accidental host invocation, not a security boundary against root.
Use a fresh guest without an existing Unibus installation; never mark a real node.

Build the checker on Linux using the same source revision as the package:

```sh
cargo check -p unibus-router --example linux_package_lifecycle
cargo test -p unibus-router --example linux_package_lifecycle
cargo build --release -p unibus-router --example linux_package_lifecycle
```

For the upgrade fixture, copy the package staging directory. Change its control
Version to `0.1.0+test1` and its default configuration's `installation_id` to
`changed-upstream-default`, then build it with `dpkg-deb`. Both fixtures use the
same router executable: this checks Debian package lifecycle and configuration
handling, not compatibility between different router implementations.

Boot an official Ubuntu 24.04 amd64 cloud image with a new writable QEMU overlay,
1 GiB RAM, one vCPU, and this additional QEMU option:

```sh
-smbios type=1,product=unibus-package-validation
```

Provide a cloud-init NoCloud seed with a temporary SSH key, password login
disabled, and this `write_files` entry:

```yaml
write_files:
  - path: /run/unibus-package-validation
    content: unibus-package-validation
    permissions: '0600'
```

Use user-mode networking with SSH forwarded to an unused **host loopback** port.
Do not use a host bridge, shared filesystem, host cgroup mounts, or a host daemon
socket. Copy the two packages and checker into the guest. Inside the guest:

```sh
sudo ./linux_package_lifecycle ./old.deb ./upgrade.deb
```

The checker fails on the first unmet assertion. For failures inspect:

```sh
systemctl status unibus-router.service
journalctl -u unibus-router.service -b
```

It checks disabled/non-running installation, systemd unit validation, non-root
process identity, loopback-only listening, rejection of an ungranted registration,
explicit restart, automatic crash recovery, version changes on upgrade/rollback,
byte-for-byte operator-config preservation, removal, and purge. Upgrade/rollback
explicitly reload systemd and restart the service. Removal explicitly stops and
disables it first; automatic maintainer-script lifecycle handling is not provided.
Finally shut down the guest and retain its test output and package hashes.

### Verified 2026-10-06 on Agora

The checker passed all seven lifecycle stages in a disposable Ubuntu 24.04.5
amd64 QEMU/KVM guest (systemd 255, kernel 6.8.0-142-generic). No host router or
TV services were changed. The tracked router source from `0b67b6a`, with this
slice's package dependency metadata, passed 39 router tests; the checker guard
test passed separately. Both release executables built successfully.

The tested artifacts are retained on Agora under
`/var/tmp/unibus-lifecycle.giuGkV/`:

| Artifact | SHA-256 |
| --- | --- |
| `old.deb` (0.1.0) | `df57397ad92960807cedfc006a7a31c52865d12a4da590e012290c230cd300e6` |
| `upgrade.deb` (0.1.0+test1 fixture) | `565a0b791a08d3489d2fdd47a0e5cc9be886a82a8e32278f96baec21c984f939` |

The shipped executable's highest observed GLIBC requirement was 2.34; the
package now declares that floor. Recalculate dependencies when changing the
toolchain rather than assuming all later builds have the same requirements.

At that initial checkpoint, a clean full-workspace `--locked` build was **not a
passing release gate**: the committed workspace referenced the untracked
`unibus-fix` crate, and its committed lockfile needed reconciliation. Validation used a tracked-source
archive with that unused workspace member omitted and an offline-resolved
lockfile in the isolated copy. No source changes from other ongoing work were
included in the final router executable. The resolved validation source/lockfile
is retained alongside the artifacts; this is a lifecycle proof, not a
reproducible release build or an ARM test.

### Workspace repair and ARM64 qualification

`09473df` includes the verified FIX adapter and reconciles the lockfile against
the tracked workspace, without consuming other in-progress adapter/discovery
changes. A fresh source archive passed `cargo metadata --locked`, core/FIX
`cargo check --locked --all-targets`, and 49 router/FIX tests. Those same 49 tests
also passed as ARM64 executables under QEMU user emulation. The archive packager
passed four unit tests and its explicitly invoked Debian integration test.
This verifies the selected core/FIX targets, not compilation of every adapter.

The ARM64 package then passed all seven lifecycle stages in a full-system,
software-emulated Cortex-A72 Ubuntu 24.04.5 guest with systemd 255 and kernel
6.8.0-142-generic. This includes the actual packaged service, not just Rust tests.
No Pi deployment or native Pi performance claim is implied.
The final amd64 candidate passed the same seven stages in a fresh KVM guest.
Both disposable guests were shut down; live router and TV services were untouched.

The final generated candidates are retained on Agora in
`/var/tmp/unibus-lifecycle.giuGkV/final-packages/`:

| Artifact | SHA-256 |
| --- | --- |
| `unibus-core_0.1.0_amd64.deb` | `93f63ee80cefdfc8751a407b1988fcc01d7a1bc974e24d06c21bd3131e33842d` |
| `unibus-core_0.1.0_arm64.deb` | `e781bf8063e0651b21eeda1cd593fef964236679490995dce40d4968e2c05b15` |
| `upgrade.deb` (ARM64 test fixture) | `a0e04188004d19f28132785dfc37e9e9eabe8a69ca7934e809997dfd4dc684f1` |

These remain unsigned validation candidates, not published releases. The
workspace member/lockfile gate is repaired, but the toolchain and sibling-repo
inputs are not yet pinned for bit-for-bit release reproducibility.

## Verification and remaining gates

The router test suite checks the exact packaged configuration. Before release,
test install/start/client rejection/restart/upgrade/config preservation/removal
in a disposable systemd VM; archive the package digest and build revision.
Rollback means installing the prior package and restarting, not overwriting
configuration. Mycelium remains the optional owner of fleet rollout policy.
The initial VM lifecycle checker is available; continuous automated execution,
maintainer-script lifecycle policy, a unified CLI/doctor, declared minimum Rust/toolchain,
reproducible builds and architecture-specific dependency/version metadata remain
follow-up gates. Do not treat a successful package archive build as those tests.
