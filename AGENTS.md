# AGENTS.md — breezed

`breezed` is a Rust fan controller for Dell PowerEdge servers over
iDRAC/IPMI. The architecture is onion/hexagonal, with a strict dependency
direction documented in `ARCHITECTURE.md` and enforced by
`tests/architecture.rs`.

## Hard rules

- Dependencies point inward: `domain` ← `application` ← `adapters` ← `entry`.
- Domain has no I/O and no imports from outer layers; application only uses
  `domain` and injected ports; adapters never import `entry`.
- Inject collaborators through constructors/traits. Only `entry/` wires concrete
  types together.
- Business rules live in `domain/`; adapters parse shape and wrap errors.
- Events are the typed `domain::events::Event` enum. Add a variant and its
  `EventType` member together, then handle it in `adapters::logs` and
  `adapters::metrics`.
- Never put a secret on argv or in a log/error string. `SecretString` masks it;
  `ipmitool` receives it via `IPMI_PASSWORD`.
- No `unsafe` (forbidden in `Cargo.toml` and checked by a test).
- No `println!`/I/O in `domain` or `application`.

## Commands

```sh
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

`just ci` runs the same sequence.

## Recipes

### Add a CLI command

1. Add a `Command` variant and an `Args` struct in `entry/cli.rs`.
2. Implement the handler in `entry/runtime.rs` or `entry/daemon_cli.rs`.
   Daemon deploy steps are data-driven (`InstallAction`/`RemoveAction`) in
   `entry/daemon.rs`; add a variant there rather than a closure.
3. Return `CliError::usage(...)` for config/usage failures (`2`) and
   `CliError::runtime(...)` for IPMI/runtime failures (`1`).
4. Print machine output to stdout with `serde_json`; errors go to stderr.

### Add an adapter

1. Define a type in `adapters/` that implements the relevant `domain::ports`
   trait.
2. Keep validation in `domain`; wrap low-level failures in the adapter error
   type (field-name-only messages).
3. Wire it in `entry/`. `daemon install` copies the running binary; the
   privileged `apply` validates the config owner against `SUDO_USER` and
   resolves its primary group.

### Add an event

1. Add a variant to `domain::events::Event` and an `EventType` member.
2. Emit it from `application::controller` (take `&Event` in `emit`).
3. Handle or ignore it in `adapters::logs` and `adapters::metrics`; add the
   field names to the event-schema tests.
