# Updating Unibus with Mycelium

Unibus owns its binary and `_self-check`. Mycelium owns signed artifact
distribution and executes the target host's local activation policy. The
service unit and router configuration remain Unibus deployment state.

Build the target artifact, then copy and edit the example release-set version:

```sh
cargo zigbuild --release --target aarch64-unknown-linux-gnu \
  -p unibus-router --bin unibus-router
cp deploy/mycelium/unibus-router.release-set.example.json /tmp/unibus-release.json
mycelium releases publish-set --manifest /tmp/unibus-release.json \
  --signing-key /secure/mycelium-release.key --write
```

Install the reviewed policy once on the target. It is intentionally not
transported in the release:

```sh
sudo install -d -m 0755 /etc/mycelium/components
sudo install -m 0644 deploy/mycelium/unibus-router.policy.json \
  /etc/mycelium/components/unibus-router.json
```

When the peer reports the artifact as ready, activate it:

```sh
mycelium update status --component unibus-router --channel canary
sudo --preserve-env=MYCELIUM_HOME,MYCELIUM_SOCKET \
  mycelium update apply --component unibus-router --channel canary \
  --policy /etc/mycelium/components/unibus-router.json --write
```

Mycelium stages the candidate beside the installed binary, calls the
Unibus-owned `_self-check`, restarts the locally named service, and polls
systemd. Failure restores the previous binary and rechecks the restored
service. Mycelium never reads `router.json`, grants, routes, or Unibus message
contracts.
