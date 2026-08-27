# Architecture

breezed uses a strict 4-layer **onion / hexagonal (ports & adapters)** design.
Dependencies point **inward only**: outer layers know about inner layers; inner
layers know nothing about outer ones. This document is the source of truth for
that contract. It is advisory — enforced in code review, not by tooling.

## Layers

| Layer | Modules | Responsibility | Touches I/O? |
| --- | --- | --- | --- |
| **Domain** (innermost) | `domain/types`, `domain/ports`, `domain/curve`, `domain/settings` | Vocabulary & business rules: `FanPercent`/`TempC` types, `OperatingMode`/`EventType` enums, the port **Protocols**, curve interpolation, validated `Settings`. | No |
| **Application** | `application/controller` | The control-loop use case: read → decide → command → emit. Owns the `EventSink` port. | Only via injected ports |
| **Adapters** | `adapters/ipmi`, `adapters/config`, `adapters/watcher`, `adapters/logs`, `adapters/metrics` | Concrete I/O: `ipmitool` subprocess, TOML+env loading, mtime hot-reload, JSON logging, Prometheus socket. | Yes |
| **Entry** (composition root) | `entry/runtime`, `entry/daemon`, `entry/daemon_cli`, `entry/common` | CLI parsing, dependency wiring, systemd deployment. | Yes (only here) |

## Core invariant: dependency direction

- `domain` imports **nothing** outside `breezed.domain.*`.
- `application` imports **only** `breezed.domain.*`.
- `adapters` import `breezed.domain.*` (and may import sibling adapters — see Adapter conventions).
- `entry` imports anything; it is the only place that knows concrete classes and wires them together.

If you find yourself needing a type from an outer layer inside an inner one,
that is a signal the type belongs in the domain.

## Ports & adapters

- All **Protocols** (the app's interface vocabulary) live in `domain/ports.py`:
  `TempReader`, `FanCommander`. The output-side `EventSink` **should also live
  here** (see Cleanup backlog).
- Adapters conform to ports **structurally** (`runtime_checkable` Protocols); they
  never inherit them. `IpmiClient` satisfies both `TempReader` and `FanCommander`
  without naming them.
- The application depends on the abstract port, never on a concrete adapter.
  Swapping IPMI for a mock or a different transport touches only `entry`, not
  the core.

## Application core

`Controller` is the single use case. It is constructed with its collaborators
injected: `reader: TempReader`, `commander: FanCommander`, `settings: Settings`,
`sinks: Sequence[EventSink]`, and a `clock`. It performs no I/O of its own —
every observable action goes through an injected port or is broadcast to the
sinks.

Events fan out: `_emit` broadcasts an `EventType` to every sink (logs + metrics
today). The core does not know which sinks exist.

## Cross-cutting rules

1. **Error hierarchy.** `DomainError` (a `ValueError` subclass) in
   `domain/types.py` is the shared base for all expected failures. Layer-specific
   subclasses: `IpmiError` (transport failures, raised by the ipmi adapter),
   `ConfigError` (adapters/config), `DaemonError` (entry/daemon). Adapters wrap
   low-level errors (`OSError`, `subprocess`, TOML, pydantic `ValidationError`)
   into the typed error. Never let a raw `ValueError`/`Exception` escape an outer
   boundary as if it were domain data.
2. **Exit-code contract.** Owned by `entry` only: `0` ok, `1` runtime error
   (IPMI failure), `2` usage/config error. Only `entry` translates exceptions
   into exit codes via `_fail`; no inner layer calls `sys.exit`.
3. **Event vocabulary.** `EventType` (in `domain/types.py`) is the single,
   closed source of event names. To add an event: add the member to `EventType`
   first, then make every `EventSink` handle (or explicitly ignore) it. Emit
   sites pass the enum; sinks never invent names.
4. **Secrets discipline.** The iDRAC password exists only inside
   `IpmiClient._invoke`'s local argv; it is never stored, logged, or echoed.
   Error messages reference subcommand args, never the full argv, and redact on
   stderr. Config never holds secrets — `IDRAC_*` come from the environment and
   win over the file.
5. **Single composition root.** Only `entry/` builds and injects dependencies
   (`AppDeps` in `entry/runtime.py`). Inner layers accept ports/values through
   constructors; they never import a concrete collaborator to self-wire.

## Adapter conventions

- Implement a domain/application Protocol **structurally**; do not subclass it.
- Hold **zero business rules** — all validation lives in the domain
  (`make_fan_pct`, `validate_curve`, pydantic validators). Adapters parse shape
  and wrap errors.
- Wrap low-level failures into the layer's typed `DomainError` subclass with
  **field-name-only** messages (never secret values).
- **Adapter → adapter imports are allowed**, as long as every participant still
  depends only inward (example: `watcher` → `config`). Prefer passing results
  across the `entry` boundary when practical, but do not contort code to avoid a
  sibling import.

## Testing

Tests mirror the source layering (`tests/domain`, `tests/application`,
`tests/adapters`, `tests/entry`). Unit tests for domain/application must not
touch real I/O — inject fakes for ports (`clock`, `reader`/`commander`,
`FileOps`, `CommandRunner`). This mirroring is the proof the boundaries hold.

## Target-state cleanup (backlog)

These describe the intended end state; current code diverges slightly. Treat
them as the rule to converge on, not as optional.

- **Move `EventSink` into `domain/ports.py`.** Today it is defined in
  `application/controller.py`. As an output port it belongs with the other
  Protocols.
- **Introduce a `SpeedPolicy` port.** `Controller` currently calls
  `curve.interpolate` directly. Extract a `SpeedPolicy` Protocol (in
  `domain/ports.py`) that `interpolate` implements, so a future PID policy can be
  swapped without touching the core. *Note: this reverses the earlier "collapse
  SpeedPolicy seam" refactor — only do it when a second policy is actually
  planned.*
- **Known current divergences:** `EventSink` still lives in
  `application/controller.py`; `SpeedPolicy` is not yet a port. These are gaps to
  close, not precedents to follow.
