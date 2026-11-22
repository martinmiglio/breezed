# breezed

Curve-based fan controller for Dell PowerEdge servers over iDRAC/IPMI.

`breezed` reads the hottest CPU temperature, maps it through a fan curve, and
sets the iDRAC fan speed with `ipmitool`. Above the top curve point it hands
control back to the iDRAC. Logs are JSON on stdout, and Prometheus metrics are
available on a loopback port when enabled.

## Install

The installer downloads a release, verifies its SHA-256, and runs
`breezed daemon install`:

```sh
curl -fsSL https://raw.githubusercontent.com/martinmiglio/breezed/master/install.sh | bash
```

To pin a version, or to pass flags such as `--dry-run` through to
`daemon install`:

```sh
curl -fsSL https://raw.githubusercontent.com/martinmiglio/breezed/master/install.sh \
  | bash -s -- --version 0.4.2
```

The script needs `curl`, `tar`, and `sha256sum`. Release binaries are static
x86_64 Linux (musl), so they run on both glibc and musl systems. Every install
checks the SHA-256 against the published `SHA256SUMS`. Set
`BREEZED_VERIFY_ATTESTATION=1` to also verify GitHub build provenance, which
requires a recent `gh` with the `attestation` command. Environment variables
must be set on the `bash` side of the pipe:

```sh
curl -fsSL https://raw.githubusercontent.com/martinmiglio/breezed/master/install.sh \
  | BREEZED_VERIFY_ATTESTATION=1 bash
```

Downloads go into a private temp directory and are checked before anything is
installed.

`daemon install` asks for sudo once, copies the binary to
`/usr/local/bin/breezed`, creates the `breezed` user, writes the systemd unit,
and enables and restarts the service. Existing `/etc/breezed/breezed.toml` and
`/etc/breezed.env` are never overwritten.

If you prefer to install from a downloaded tarball:

```sh
VERSION=0.4.2
BASE="https://github.com/martinmiglio/breezed/releases/download/v${VERSION}"
ARCHIVE="breezed-${VERSION}-x86_64-unknown-linux-musl.tar.gz"

curl -fsSLO "${BASE}/${ARCHIVE}"
curl -fsSLO "${BASE}/SHA256SUMS"

# Optional: verify build provenance (needs a recent gh CLI).
gh attestation verify "${ARCHIVE}" --repo martinmiglio/breezed

sha256sum -c SHA256SUMS
tar -xzf "${ARCHIVE}"
sudo "./breezed-${VERSION}-x86_64-unknown-linux-musl/breezed" daemon install
```

## Usage

```sh
breezed run --config /etc/breezed/breezed.toml --metrics-port 9762 -v
breezed set 20
breezed auto
breezed status
breezed validate /etc/breezed/breezed.toml --probe
```

```
run       [-c|--config PATH] [--metrics-port INT] [-v|--verbose]
set PCT   [-c|--config PATH]
auto      [-c|--config PATH]
status    [--config PATH] [--verbose]
validate PATH [--probe]

daemon install   [--dry-run] [--json]
daemon status
daemon uninstall [--dry-run] [--json]
```

Exit codes are `0` for success, `1` for runtime or IPMI failures, and `2` for
usage or config errors. Commands print one JSON object to stdout; errors go to
stderr. `daemon install` and `daemon uninstall` print their steps to stderr and
only emit JSON with `--json`.

## Configuration

A commented example lives at
[`resources/breezed.toml.example`](resources/breezed.toml.example) in the
repository, and ships as `breezed.toml.example` in release archives. The
service installs a copy at `/etc/breezed/breezed.toml` on first run.

Secrets belong in `/etc/breezed.env`, not the config file. `IDRAC_HOST`,
`IDRAC_USER`, and `IDRAC_PASSWORD` override the file when non-empty. The config
reloads when its mtime changes; an invalid reload keeps the last good settings.

Curve points must be strictly ascending in `temp_c`, `fan_pct` must be `1..=100`,
and `metrics_port` must be `1..=65535`.

### Experimental native IPMI backend

Instead of spawning `ipmitool` for every poll, an optional adapter can speak
RMCP+ directly through [`ipmi-rs`](https://github.com/datdenkikniet/ipmi-rs):

```sh
cargo build --release --features native-ipmi
BREEZED_NATIVE_IPMI=1 ./target/release/breezed run --config /etc/breezed/breezed.toml
```

- The switch applies to `run` only; `set`, `auto`, and `status` still use
  `ipmitool`.
- Release archives and `install.sh` are built without the feature, so the
  variable has no effect on released binaries. Build from source to try it.
- The adapter opens one session, loads the SDR repository once at connect, and
  reuses the session for every poll. The iDRAC account still needs the
  `Administrator` role for Dell's OEM fan commands.
- Known limitations: the SDR load happens at connect and can take a few
  seconds; a lost packet during the load forces a reconnect and a full reload;
  there is no release packaging or Prometheus metric for the native path yet.
  The `ipmi-rs` dependency is pinned to a fork revision until the session and
  privilege fixes are released upstream.

## Service

```sh
breezed daemon status        # unit state and version drift
sudo breezed daemon install  # install or upgrade
sudo breezed daemon uninstall
```

`daemon uninstall` keeps `/etc/breezed/` and `/etc/breezed.env`. The unit runs
as the unprivileged `breezed` user with systemd hardening and
`KillMode=mixed`.

With the default unit, metrics are served on `127.0.0.1:9762` at `/metrics`.
The port is part of the unit's `ExecStart`; edit the unit and reload systemd if
you want to run without metrics.

## Safety

You are the thermal management system. A bad curve can overheat components or
leave a server at full speed. `breezed` falls back to iDRAC automatic control
above the top curve point and after repeated sensor failures, but the curve is
yours to choose. `breezed auto` returns control immediately.

## Development

Rust 1.100.0 is pinned in `rust-toolchain.toml`.

```sh
cargo build --release --locked
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

`just ci` runs the same checks. Tests use fake `ipmitool`, `sudo`, and
`systemctl` binaries, so no hardware is needed. The architecture is described in
[`ARCHITECTURE.md`](ARCHITECTURE.md).

## Releasing

Versions are bumped with [`cargo-release`](https://github.com/crate-ci/cargo-release),
which keeps `Cargo.toml`, `Cargo.lock`, and the git tag in sync:

```sh
cargo install cargo-release --locked
cargo release patch        # dry run
just release patch         # commit, tag vX.Y.Z, and push
```

The tag triggers the release workflow: it reruns the checks, builds the
`x86_64-unknown-linux-musl` archive, writes `SHA256SUMS`, attests the artifacts
with GitHub artifact attestation, and creates the GitHub release.

Every commit pushed to a non-draft pull request is also packaged the same way.
The archive is named with the commit's short SHA and uploaded to the workflow
run as an artifact, so a change can be installed and tried before a release.

## License

[MIT](LICENSE)
