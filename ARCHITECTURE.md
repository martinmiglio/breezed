# Architecture

`breezed` uses a strict 4-layer **onion / hexagonal (ports & adapters)** design.
Dependencies point **inward only**: outer layers know about inner layers; inner
layers know nothing about outer ones. `tests/architecture.rs` enforces the
dependency direction mechanically.

## Layers

| Layer | Modules | Responsibility | Touches I/O? |
| --- | --- | --- | --- |
| **Domain** (innermost) | `domain/types`, `domain/ports`, `domain/curve`, `domain/settings`, `domain/events` | Vocabulary and business rules: `TempC`/`FanPercent`/`Curve`, operating mode and event enums, capability traits, validated `Settings`. | No |
| **Application** | `application/controller` | The control-loop use case: read → decide → command → emit. | Only via injected ports |
| **Adapters** | `adapters/ipmi`, `adapters/config`, `adapters/watcher`, `adapters/logs`, `adapters/metrics` | Concrete I/O: `ipmitool` subprocess, TOML plus environment loading, mtime hot-reload, JSON logging, Prometheus socket. | Yes |
| **Entry** (composition root) | `entry/cli`, `entry/runtime`, `entry/daemon`, `entry/daemon_cli`, `entry/common` | CLI parsing, dependency wiring, control-loop orchestration, systemd deployment, exit-code mapping. | Yes (only here) |

## Dependency direction

- `domain` imports nothing outside `crate::domain`.
- `application` imports only `crate::domain`.
- `adapters` import `crate::domain` and, where useful, sibling adapters.
- `entry` imports anything; it is the only place that constructs concrete
  adapters and wires them together.

If an inner layer needs a type from an outer one, that type belongs in the
domain.

## Ports and adapters

Capability traits live in `domain::ports`:

- `TempReader` / `FanCommander`, combined as `FanDevice`;
- `EventSink`;
- `SpeedPolicy`;
- `Clock`.

Adapters implement these traits structurally and are erased behind
`Arc<dyn …>` in `entry`. `Controller` depends only on the traits, so the IPMI
transport, clock, policy, and sinks can be replaced without touching the core.
Tests inject fakes at these seams; no hardware is required.

## Application core

`Controller` is the single use case. It is constructed with a `FanDevice`, a
`SpeedPolicy`, validated `Settings`, a slice of `EventSink`s, and a `Clock`. It
performs no I/O of its own: every observable action goes through an injected
port or is broadcast to the sinks.

Events fan out through `EventSink`. The core does not know whether a sink logs,
updates metrics, or records events for a test.

## Cross-cutting rules

1. **Errors are values.** `DomainError` is the shared base for expected
   failures. Adapters wrap low-level failures into `IpmiError` or
   `ConfigError`; `entry` maps them to the exit-code contract. Inner layers
   return `Result`, never call `process::exit`.
2. **Exit-code contract.** Owned by `entry` only: `0` ok, `1` runtime/IPMI
   error, `2` usage/config error.
3. **Event vocabulary.** `domain::events::Event` and `EventType` are the closed
   source of event names. Add the variant and the enum member together, then
   handle it in every sink.
4. **Secrets discipline.** The iDRAC password reaches `ipmitool` through the
   child environment (`IPMI_PASSWORD` / `-E`), never argv. `SecretString` masks
   it in `Debug`/`Display`. Errors name fields and subcommand arguments, never
   the full argv or secret values. Config never holds secrets: `IDRAC_*`
   environment variables win over the file.
5. **Single composition root.** Only `entry/` builds concrete collaborators.
   `RuntimeDeps` exists so the control loop can be driven with fakes in tests.

## Adapter conventions

- Implement the relevant `domain::ports` trait; do not add a parallel
  abstraction.
- Hold zero business rules — validation lives in `domain`.
- Wrap low-level failures into the layer's typed error with field-name-only
  messages.
- Sibling adapter imports are allowed as long as every participant still
  depends only inward (for example, `watcher` uses `config`).

## Testing

Unit tests live beside the code. End-to-end tests in `tests/` drive the real
binary against a shell-script fake `ipmitool`, fake `sudo`, and fake
`systemctl`. Domain and application tests inject fakes for every port and never
touch real I/O. `tests/architecture.rs` asserts the layer direction and the
`unsafe` prohibition.
