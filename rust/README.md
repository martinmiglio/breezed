# breezed (Rust)

A Rust port of the Python `breezed` fan controller. The Python package remains
the reference implementation. The Rust port has passed a live iDRAC round
(validate/status/set/run/metrics/systemd install) on the reference host and is
installed there as the `breezed` service.

The architecture mirrors the original onion/hexagonal layers, with Rust idioms
(typed events, traits for ports, `Result` errors) throughout. The CLI surface,
JSON field names and shapes, event vocabulary, IPMI argv/env, config keys,
metric names, exit codes, and systemd behaviour match the Python CLI. The
systemd installer now installs the Rust binary instead of a Python/uv runtime.
Machine output is produced by `serde_json`, so whitespace and key order are not
byte-compatible; see `PARITY.md` for the exact contract.

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
`tests/architecture.rs` enforces that mechanically.

## Build and run

Rust 1.100.0 (see `rust-toolchain.toml`).

```sh
cargo build --release
target/release/breezed --help

cargo run -- status --config breezed.toml
cargo run -- set 20
cargo run -- auto
cargo run -- run --config breezed.toml --metrics-port 9762
```

Commands and flags match the Python CLI:

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

## Releases

Tag `vX.Y.Z` only when `Cargo.toml` has the same version. The release workflow
runs the checks, builds `breezed-VERSION-x86_64-unknown-linux-gnu.tar.gz`,
writes `SHA256SUMS`, attests the tarball with GitHub build provenance, and
publishes the GitHub release.

Verify a downloaded release:

```sh
VERSION=0.3.1
gh attestation verify "breezed-${VERSION}-x86_64-unknown-linux-gnu.tar.gz" \
  --repo martinmiglio/breezed
sha256sum -c SHA256SUMS
tar -xzf "breezed-${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
sudo "./breezed-${VERSION}-x86_64-unknown-linux-gnu/breezed" daemon install
```

Attestations use GitHub's Sigstore-backed service, so there is no long-lived
signing key to manage. They require a public repository (or GitHub Enterprise
Cloud for private repositories). The tarball also ships the unit, example
config, README, parity notes, and LICENSE for offline inspection.

## Testing

No live iDRAC is required. The test suite injects fake `CommandRunner`s at the
adapter seam and drives the real binary against a shell-script fake `ipmitool`:

```sh
cargo test
```

Coverage includes:

- curve interpolation and ties-to-even rounding, validated settings and config
  precedence, controller hysteresis/failure fallbacks, IPMI argv/env and
  redaction, JSON log fields, metrics rendering and HTTP endpoint;
- end-to-end CLI output/exit-code compatibility, including a graceful SIGTERM
  shutdown with a live metrics scrape, invalid identity types, and metrics-port
  bounds;
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

`just` recipes wrap the same commands (`just ci`). CI runs fmt, clippy, and tests
on every push/PR via `.github/workflows/rust.yml`. `deny.toml` is provided for
an optional local `cargo deny check`.

## Live-testing checklist

When you are ready to point it at real hardware, in increasing order of risk:

1. `breezed validate /etc/breezed/breezed.toml` and `--probe` (read-only).
2. `breezed status` (read-only).
3. `breezed set 20` then `breezed auto` (two writes; verify fans audibly react).
4. `breezed run --config ... --metrics-port 9762` in the foreground; watch JSON
   events and `curl -s 127.0.0.1:9762/metrics`; Ctrl-C and confirm `auto`.
5. `target/release/breezed daemon install --dry-run`, then the real install only
   when 1–4 pass. The installer copies the *running* binary, so use the release
   build, not `cargo run`.

Validated live on 2026-11-20: probe/status against the real iDRAC, `set 20`,
`auto`, a short 1 s `run` with a live metrics scrape, Ctrl-C AUTO restore, and a
real `daemon install`/restart. The installed service reports `unit_version:
0.3.1`, two tasks, zero IPMI errors, and ~8.7 MiB peak cgroup memory.

The Rust and Python binaries can be compared side by side by pointing
`ipmitool_path` at a logging wrapper; see `PARITY.md` for the exact contract.

## Known intentional differences

- Machine JSON uses `serde_json`: field names and shapes are stable, but spacing,
  escaping, and key order are not byte-identical to Python's `json.dumps`.
- Validation reports native Rust field errors (and may list several at once)
  instead of pydantic prose.
- Help is rendered by clap rather than Typer: same command names, different
  spacing/punctuation, clap adds `-h`/`-V`, and the daemon installer flags are
  deliberately smaller (`--dry-run`/`--json`, plus hidden `apply --owner`).
- TOML syntax-error prose comes from the Rust parser; the `config: invalid TOML
  in PATH:` prefix and exit code are unchanged, and source snippets that could
  echo a password are stripped.
- Rust backtraces appear in fatal `traceback` fields instead of Python
  tracebacks.
- Metrics HTTP headers (`Server`, `Date`) are minimal; the body is byte-identical.
- `--metrics-port` is parsed as a `u16`, and config-file `metrics_port` must be
  in `1..=65535`; out-of-range values are usage errors (`2`) instead of late
  bind-time crashes.
- Curve temperatures outside `i32` are rejected; Python accepted any integer.
- A missing `ipmitool` binary is a retryable `IpmiError` (fail-safe to AUTO)
  rather than an uncaught exception.
- The binary version is pinned to `0.3.1` in `Cargo.toml` for now.
- `daemon install` is Rust-native: it copies the running binary to
  `/usr/local/bin/breezed` with one sudo prompt. It intentionally diverges from
  the Python installer's `uv tool install …` backend, since no Python runtime is
  needed. Config and env are still first-run-only; uninstall keeps them.
