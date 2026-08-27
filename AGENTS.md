# AGENTS.md

You are working in **breezed**, a 4-layer onion/hexagonal Python project
(domain → application → adapters → entry). Full context lives in
`ARCHITECTURE.md` — read it first. This file is the short, agent-friendly
version: the rules, plus recipes for the three common tasks.

## Hard rules

**DO**
- Keep dependencies pointing inward: `domain` ← `application` ← `adapters` ← `entry`.
- Put new business types, enums, and Protocols in `domain/`.
- Inject collaborators via constructors; only `entry/` wires concrete classes.
- Wrap adapter / low-level errors into `DomainError` subclasses with field-name-only messages.
- Add new events to `EventType` (in `domain/types.py`) first, then handle in every sink.
- Conform to Protocols structurally (the adapter "is-a" port without subclassing).

**DON'T**
- Import an outer layer from an inner one (no `domain` → `adapters`, no `application` → `entry`).
- Put business rules or validation in adapters — that belongs in the domain.
- Call `sys.exit` outside `entry/`; let secrets reach logs or error text.
- Subclass a Protocol to implement it.
- Store secrets in config; they come from the environment and win over the file.

## Boundary quick-ref

| Layer | May import | May do I/O |
| --- | --- | --- |
| domain | only `domain` | no |
| application | only `domain` | only via injected ports |
| adapters | `domain` + sibling adapters | yes |
| entry | anything (composition root) | yes |

## Recipes

### Add a CLI command
In `entry/runtime.py` (runtime cmds) or `entry/daemon_cli.py` (daemon cmds):
1. Decorate with `@app.command()`; load settings via `_load_settings_or_fail(config)`.
2. Build the client via `deps.build_client(settings)`; guard IPMI calls and, on
   `IpmiError`, call `_fail(err, code=1)`.
3. On config / usage errors call `_fail(err, code=2)`.
4. Print machine output as JSON to **stdout**; send errors / human text to **stderr**.

### Add an adapter
1. New module in `adapters/`. Define a class/function that structurally satisfies
   a `domain/ports` Protocol.
2. Keep all validation in the domain; wrap I/O errors into the typed
   `DomainError` subclass (field-name-only messages, no secrets).
3. Wire the concrete into `entry/` (composition root) — never self-wire inside the adapter.

### Add an event
1. Add the member to `EventType` in `domain/types.py`.
2. Emit it from `Controller` via `self._emit(EventType.X, **fields)`.
3. Ensure every `EventSink` (`LoggingEventSink`, `MetricsState`) handles or ignores it.

## Known cleanup targets
- `EventSink` should move from `application/controller.py` into `domain/ports.py`.
- `Controller` should depend on a `SpeedPolicy` Protocol (implemented by
  `curve.interpolate`) rather than calling `interpolate` directly.
See `ARCHITECTURE.md` → Target-state cleanup for rationale.
