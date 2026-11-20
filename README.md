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
  | bash -s -- --version 0.3.1
```

The script needs `curl`, `tar`, and `sha256sum`. Set
`BREEZED_VERIFY_ATTESTATION=1` to also verify GitHub build provenance; that
requires the `gh` CLI. Downloads go into a private temp directory and are
checked before anything is installed.

`daemon install` asks for sudo once, copies the binary to
`/usr/local/bin/breezed`, creates the `breezed` user, writes the systemd unit,
and enables and restarts the service. Existing `/etc/breezed/breezed.toml` and
`/etc/breezed.env` are never overwritten.

If you prefer to install from a downloaded tarball:

```sh
VERSION=0.3.1
gh attestation verify "breezed-${VERSION}-x86_64-unknown-linux-gnu.tar.gz" \
  --repo martinmiglio/breezed
sha256sum -c SHA256SUMS
tar -xzf "breezed-${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
sudo "./breezed-${VERSION}-x86_64-unknown-linux-gnu/breezed" daemon install
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

A commented example is in
[`resources/breezed.toml.example`](resources/breezed.toml.example). The service
installs a copy at `/etc/breezed/breezed.toml` on first run.

Secrets belong in `/etc/breezed.env`, not the config file. `IDRAC_HOST`,
`IDRAC_USER`, and `IDRAC_PASSWORD` override the file when non-empty. The config
reloads when its mtime changes; an invalid reload keeps the last good settings.

Curve points must be strictly ascending in `temp_c`, `fan_pct` must be `1..=100`,
and `metrics_port` must be `1..=65535`.

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

## License

[MIT](LICENSE)
