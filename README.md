# breezed

Curve-based fan controller for Dell PowerEdge servers via iDRAC/IPMI.

Source: [github.com/martinmiglio/breezed](https://github.com/martinmiglio/breezed)

`breezed` keeps a homelab rack quiet at idle and cool under load. It maps the
maximum CPU temperature to a multi-point fan curve with linear interpolation,
drives the iDRAC through `ipmitool`, and hands control back to the iDRAC's own
automatic thermal management above the top curve point. Logs are JSON on
stdout; a Prometheus-format metrics endpoint is opt-in over a loopback socket.

The implementation is Rust, with a strict onion/hexagonal layout. The binary is
self-contained and has no Python or `uv` runtime dependency.

## Install

Download a release tarball, verify it, and run the installer:

```sh
VERSION=0.3.1
BASE="https://github.com/martinmiglio/breezed/releases/download/v${VERSION}"
curl -fsSLO "${BASE}/breezed-${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
curl -fsSLO "${BASE}/SHA256SUMS"
gh attestation verify "breezed-${VERSION}-x86_64-unknown-linux-gnu.tar.gz" \
  --repo martinmiglio/breezed
sha256sum -c SHA256SUMS
tar -xzf "breezed-${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
sudo "./breezed-${VERSION}-x86_64-unknown-linux-gnu/breezed" daemon install
```

`daemon install` prints the privileged steps, then re-executes itself once via
`sudo` to run a fixed, code-reviewed sequence. Pass `--dry-run` to review it
without escalating, or `--json` for one machine-readable result on stdout.

The installer:

- copies the running binary to `/usr/local/bin/breezed` as `root:root 0755`;
- creates the `breezed` system group and user;
- writes `/etc/systemd/system/breezed.service` (`root:root 0644`);
- writes `/etc/breezed/breezed.toml` and `/etc/breezed.env` on first install
  only, preserving tuned config and secrets on upgrade;
- runs `systemctl daemon-reload`, enables the unit, and restarts it.

After installing, add credentials to `/etc/breezed.env`:

```sh
sudoedit /etc/breezed.env
journalctl -u breezed -f
```

## Build from source

Rust 1.100.0 (pinned in `rust-toolchain.toml`):

```sh
cargo build --release --locked
target/release/breezed --help
```

## CLI

```sh
breezed run --config /etc/breezed/breezed.toml --metrics-port 9762 -v
breezed set 20
breezed auto
breezed status --config /etc/breezed/breezed.toml
breezed validate breezed.toml --probe
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
daemon apply     --owner STR                                             (hidden)
daemon remove                                                            (hidden)
```

Exit codes: `0` ok, `1` runtime/IPMI error, `2` usage/config error.

Other commands emit one JSON object on stdout; errors go to stderr as
`breezed: …`. `daemon install` and `daemon uninstall` print human-readable
steps to stderr and nothing to stdout by default.

## Deploy (systemd)

systemd is the only deployment path; there are no containers.

```sh
sudo breezed daemon install            # install or upgrade
sudo breezed daemon install --dry-run  # print the privileged steps
breezed daemon status                  # unit state and version drift
sudo breezed daemon uninstall          # keeps /etc/breezed and /etc/breezed.env
```

The unit runs as the unprivileged `breezed` user with hardening
(`ProtectSystem=strict`, `PrivateTmp`, `MemoryDenyWriteExecute`,
`SystemCallFilter=@system-service`, and related settings), `Type=exec`,
`KillMode=mixed`, and restart-on-failure.

The metrics port is baked into the unit's `ExecStart` deliberately (netdata
scrapes it). To run without metrics, edit the unit to drop `--metrics-port 9762`
and run `systemctl daemon-reload && systemctl restart breezed`; note that the
next `daemon install` overwrites the unit.

## Config

A commented example ships at [`resources/breezed.toml.example`](resources/breezed.toml.example)
and is copied to `/etc/breezed/breezed.toml` on first install.

Secrets never go in the config file. `IDRAC_HOST`, `IDRAC_USER`, and
`IDRAC_PASSWORD` come from the environment (`/etc/breezed.env`); host and user
may also live in the config, but non-empty environment variables win. The config
hot-reloads when its mtime changes; an invalid reload keeps the last good
configuration.

Every curve must be strictly ascending in `temp_c`, with `fan_pct` in `1..=100`.
`metrics_port` must be in `1..=65535`.

## Metrics

With the default unit, `breezed` serves Prometheus text format on
`127.0.0.1:9762`:

```
http://127.0.0.1:9762/metrics
```

Metric names: `breezed_temp_c`, `breezed_fan_percent`, `breezed_mode`,
`breezed_ipmi_errors_total`, `breezed_polls_total`.

## Safety

Manually controlling server fans means you are the thermal management system: a
bad curve can cook components or leave a server roaring at 100%. `breezed`
mitigates this by deferring to iDRAC automatic control above the curve top and
forcing AUTO again after repeated sensor-read failures — but you chose the curve
points, so choose them carefully. If anything looks wrong, `breezed auto` hands
control back to the iDRAC immediately.

## Releases

Tag `vX.Y.Z` only when `Cargo.toml` has the same version. The release workflow
runs the checks, builds `breezed-VERSION-x86_64-unknown-linux-gnu.tar.gz`, writes
`SHA256SUMS`, attests the tarball and checksums with GitHub build provenance,
and publishes the GitHub release.

Attestations use GitHub's Sigstore-backed service, so there is no long-lived
signing key to manage. The tarball also ships the unit, example config, README,
migration notes, and LICENSE for offline inspection.

## Testing

No live iDRAC is required. The test suite drives the real binary against a
shell-script fake `ipmitool` and injects fakes at adapter seams:

```sh
cargo test --all-features
```

Coverage includes:

- curve interpolation and rounding, validated settings and config precedence,
  controller hysteresis/failure fallbacks, IPMI argv/env and redaction, JSON log
  fields, metrics rendering and the HTTP endpoint;
- end-to-end CLI output, exit codes, a graceful SIGTERM shutdown, invalid
  identity types, and metrics-port bounds;
- daemon planning/idempotence/sudo argv with fake `sudo`/`systemctl`, and the
  fixed binary-install sequence;
- an architecture guard for layer boundaries and `unsafe`.

## Tooling

```sh
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

`just ci` runs the same sequence. CI runs fmt, clippy, tests, and docs on every
push and pull request. `deny.toml` is provided for an optional local
`cargo deny check`.

## Layout

```
src/
  domain/        types, curve, settings, events, ports        (no I/O)
  application/   controller control loop                     (ports only)
  adapters/      ipmi, config, watcher, logs, metrics        (all I/O)
  entry/         clap CLI, runtime loop, daemon deployment   (composition root)
tests/           end-to-end CLI/daemon tests + architecture guard
resources/       packaged systemd unit and example config
```

Dependencies point inward only: `domain` ← `application` ← `adapters` ← `entry`.
See [`ARCHITECTURE.md`](ARCHITECTURE.md); `tests/architecture.rs` enforces the
direction mechanically.

## Migrating from Python

The Python implementation was replaced by this crate. See
[`MIGRATION.md`](MIGRATION.md) for the upgrade path and the small set of
observable differences.

## License

[MIT](LICENSE)
