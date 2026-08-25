# T8 — Deploy & docs

## Goal

Ship the two deployment artifacts and the final README so breezed can replace the legacy
C# fan-controller on mmsrv today:

- **Primary:** `deploy/breezed.service` — a hardened `Type=simple` systemd unit running
  the installed `breezed run --config /etc/breezed/breezed.toml`, with secrets injected
  from an environment file and AUTO-restore on shutdown guaranteed by T5/T7.
- **Secondary:** `deploy/Containerfile` — a runtime-agnostic OCI image that builds
  identically under `docker build` and `podman build`.
- `deploy/breezed.toml.example` — byte-identical to the SPEC "Config example".
- A final `README.md` rewrite covering what/why, quickstart (`uv tool install`), CLI,
  config, systemd steps, container run, netdata metrics, safety disclaimer, and the
  mmsrv cutover note.

No application code changes — everything here consumes the CLI contract locked in T7.

## Depends on

(T1–T7)

## Files

- `deploy/breezed.service` (new)
- `deploy/Containerfile` (new)
- `deploy/breezed.toml.example` (new)
- `README.md` (full rewrite — the T1 skeleton is superseded; keep the License section)

## Tasks

### 1. `deploy/breezed.service`

```ini
[Unit]
Description=breezed - curve-based iDRAC/IPMI fan controller
Documentation=https://github.com/martinmiglio/breezed
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/breezed run --config /etc/breezed/breezed.toml
EnvironmentFile=-/etc/breezed.env
Restart=always
RestartSec=5

# Hardening — small network daemon that shells out to ipmitool
DynamicUser=no
User=breezed
Group=breezed
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
RestrictNamespaces=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
RestrictSUIDSGID=yes
CapabilityBoundingSet=
AmbientCapabilities=

[Install]
WantedBy=multi-user.target
```

**User decision: dedicated system user, not `DynamicUser=yes`.**
Justification: `/etc/breezed.env` holds the iDRAC password and must be readable by
exactly one stable UID; with a dedicated user the permission story is plain
`chown root:breezed /etc/breezed.env && chmod 640` on any distro. A `DynamicUser`
would force either world-readable secrets (unacceptable) or `LoadCredential`
machinery that outguns a homelab daemon. breezed keeps no state, so account
provisioning is the only cost — one `useradd` line documented in the README.

Unit requirements:

- `Type=simple` — breezed foregrounds by design (SPEC); never `Type=forking`.
- **ExecStart path contract**: `/usr/local/bin/breezed` assumes a *system-wide*
  install. `uv tool install .` defaults to the user's `~/.local/bin`, which the
  systemd service can't see. The README must therefore show either
  `UV_TOOL_BIN_DIR=/usr/local/bin uv tool install .` (run as root) or an explicit
  symlink step (`sudo ln -s ~/.local/bin/breezed /usr/local/bin/breezed`) — pick
  one and make the quickstart and this path agree. Do not silently leave the
  mismatch.
- `EnvironmentFile=-` prefix keeps boot resilient when the env file is absent
  (host/user may live in the TOML; only `IDRAC_PASSWORD` realistically requires it).
- `After=`/`Wants=network-online.target` — first sensor read needs the route to
  the iDRAC's link-local address; T5's failure-limit fallback means a race here
  degrades safely, but don't rely on it.
- `ProtectSystem=strict` with an empty `ReadWritePaths=` works because breezed
  writes nothing to disk (logs → stdout/journal). If a future need arises, add
  paths there rather than weakening `ProtectSystem`.

### 2. `deploy/Containerfile`

```dockerfile
FROM ghcr.io/astral-sh/uv:python3.13-bookworm-slim

# ipmitool is the only runtime dependency outside Python
RUN apt-get update \
    && apt-get install -y --no-install-recommends ipmitool \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY pyproject.toml uv.lock README.md ./
COPY src ./src
RUN uv sync --frozen --no-dev

USER nobody
ENTRYPOINT ["breezed"]
CMD ["run", "--config", "/etc/breezed/breezed.toml"]
```

Requirements:

- Base image exactly `ghcr.io/astral-sh/uv:python3.13-bookworm-slim` (see Notes on
  tag pinning).
- `uv sync --frozen --no-dev` against the committed `uv.lock` — no lock drift between
  dev and container (this is why T1 commits the lockfile).
- Runs as `nobody` (non-root). No volume mounts needed; the config is baked/mounted
  read-only at `/etc/breezed/breezed.toml`.
- Must build **identically** under `docker build` and `podman build`: stick to OCI
  BuildKit-portable syntax — no `DOCKER_BUILDKIT` conditionals, no podman-only flags,
  no `--mount=type=cache` run mounts. Verify both before checking the AC box.

### 3. `deploy/breezed.toml.example`

Copy the SPEC "Config example" verbatim — all four curve points, commented
`metrics_port`, `host = "169.254.0.1"`. Add nothing, remove nothing (the SPEC block
is normative; README shows the same content inline).

### 4. Final `README.md` rewrite

Sections, in order:

1. **Title + what/why** — keep the existing pitch (curve vs static speed, iDRAC AUTO
   as the safety net above the curve), plus: multi-point curve with linear interpolation,
   safe-by-default fallback behavior, JSON logs, opt-in metrics.
2. **Quickstart (uv tool install)**:
   ```sh
   uv tool install .
   breezed validate breezed.toml --probe   # reads one live temp, touches no fans
   ```
3. **CLI examples** — one per command, exactly per SPEC shape: `run` (with
   `--metrics-port 9762` example), `set 12`, `auto`, `status`, `validate --probe`;
   mention exit codes 0/1/2 in one sentence.
4. **Config example** — same TOML block as the `.example` file; note env vars
   `IDRAC_HOST`, `IDRAC_USER`, `IDRAC_PASSWORD` (env wins) and hot-reload on mtime change.
5. **systemd install** — copy unit + example config into place, create the user,
   provision `/etc/breezed.env`, enable/start, journalctl hint:
   ```sh
   sudo useradd --system --no-create-home --shell /usr/sbin/nologin breezed
   sudo install -m644 deploy/breezed.toml.example /etc/breezed/breezed.toml
   printf 'IDRAC_HOST=169.254.0.1\nIDRAC_USER=root\nIDRAC_PASSWORD=calvin\n' \
       | sudo tee /etc/breezed.env >/dev/null
   sudo chown root:breezed /etc/breezed.env && sudo chmod 640 /etc/breezed.env
   sudo install -m644 deploy/breezed.service /etc/systemd/system/
   sudo systemctl daemon-reload && sudo systemctl enable --now breezed
   ```
6. **Container run** — `docker build`-first example, then the identical `podman build`,
   both with `--network=host` (see Notes). The config file must be mounted — the
   image contains none and `load_settings` fails hard without it (exit 2):
   ```sh
   docker build -t breezed -f deploy/Containerfile .
   docker run --network=host \
       --env IDRAC_HOST=169.254.0.1 --env IDRAC_USER=root --env IDRAC_PASSWORD=calvin \
       --volume ./breezed.toml:/etc/breezed/breezed.toml:ro \
       breezed
   ```
7. **netdata metrics note** — short paragraph: start `breezed run --metrics-port 9762`
   (or uncomment in config); binds `127.0.0.1`; scrape via a netdata go.d/python.d job
   pointing at `http://127.0.0.1:9762/metrics`; list the metric names from SPEC.
8. **Safety disclaimer** — you are taking manual control of server fans; above the top
   curve point (and on repeated sensor failures) breezed hands control back to iDRAC
   AUTOMATIC, but validate with `breezed validate --probe` and watch the first hour of
   `journalctl -u breezed`. Fans not spinning up under load = stop and `breezed auto`.
9. **Replacing the legacy C# fan-controller on mmsrv** — brief note: stop and disable
   the old JDMallen.IPMITempMonitor compose stack (`docker compose down` /
   `systemctl disable`) *before* starting breezed — two controllers fighting over the
   iDRAC flip fan modes every poll. Never run both simultaneously.

## Acceptance criteria

- [ ] `deploy/breezed.service`: `Type=simple`, `ExecStart=… breezed run --config
      /etc/breezed/breezed.toml`, `EnvironmentFile=-/etc/breezed.env`, `Restart=always`,
      `RestartSec=5`, `After=`+`Wants=network-online.target`, hardening directives present,
      dedicated-user decision documented in-ticket
- [ ] `systemd-analyze verify deploy/breezed.service` passes (exit 0; warnings about the
      missing `/etc/breezed.env` or unresolvable user on the dev box are acceptable if the
      directive syntax itself verifies clean)
- [ ] `deploy/Containerfile`: pinned `ghcr.io/astral-sh/uv:python3.13-bookworm-slim` base,
      `apt-get install ipmitool`, `uv sync --frozen --no-dev` against the committed lock,
      non-root `USER`, `ENTRYPOINT breezed`, `CMD run --config …`
- [ ] `docker build -f deploy/Containerfile .` succeeds end-to-end AND the
      docker/podman-compatible claim is verified/documented (same Dockerfile builds under
      `podman build`; note this in README)
- [ ] `deploy/breezed.toml.example` is byte-identical to the SPEC "Config example" TOML
- [ ] README renders all commands correctly: fenced code blocks with language tags, every
      CLI invocation matches T7's actual `--help` surface, config/env var names match T3
      (`IDRAC_HOST`, `IDRAC_USER`, `IDRAC_PASSWORD`), systemd steps copy-pasteable
- [ ] mmsrv cutover note present (stop old stack first, never both at once)
- [ ] All verification commands pass clean:
  - [ ] `uvx ruff check .` still green (docs ticket must not regress lint)
  - [ ] `uv run pytest` — full suite green
  - [ ] `uvx ruff format --check .` and `uvx ty check` still green
  - [ ] `uvx pre-commit run --all-files` green (trailing-whitespace/end-of-file fixers
        cover the new deploy files too)

## Notes

- **Container networking is the #1 gotcha**: the iDRAC dedicated NIC lives on a
  link-local `169.254.x.x` subnet reachable from the *host*. A default bridge network
  will not route there, so the README container example **must** use `--network=host`
  (and say why in one sentence). Under host networking the container's ipmitool reaches
  the BMC exactly like the host's would. Also note SELinux contexts if someone mounts
  the config read-only under enforcing mode (`:z`/`Z` or `--volume …:ro` with proper
  label) — one sentence suffices.
- **Never `Type=forking`**: breezed is a foreground process by design (SPEC); forking
  would break signal-driven AUTO restore on shutdown. Likewise do not add
  `KillMode=process` — default `control-group` is correct so a wedged ipmitool child
  dies with the unit.
- **uv image tag pinning**: `python3.13-bookworm-slim` is itself a floating tag; for a
  reproducible appliance-style image consider pinning further (e.g. digest or the dated
  tags astral publishes) at implementation time — document whichever choice lands in a
  comment in the Containerfile. Rebuilds should pick up patched Pythons deliberately,
  not silently.
- **`uv sync --frozen --no-dev` runs as root during build but drops to `nobody` after**
  — ensure the synced `.venv` inside the image is world-readable/executable (default
  umask in the uv images is fine; verify `USER nobody ... ENTRYPOINT` actually execs,
  e.g. via `docker run --rm breezed --help`).
- **`systemd-analyze verify` on a dev machine** will warn about the missing env file
  path or nonexistent `breezed` user — those are environment warnings, not syntax
  errors; the AC passes when there are no directive-level failures.
- **Env file trailing newline**: the `printf` snippet in the README ends each variable;
  remind readers `EnvironmentFile` parsing is line-based — no quotes needed, no spaces
  around `=`.
- **Secret hygiene in the image/container**: pass secrets via `-e`/env-file at
  `docker run`, never bake them into the image; the Containerfile contains none. The
  README example uses `calvin` — flag it as the well-known default that must be changed.
- Docs-only diff discipline: no changes to `src/`, `tests/`, or tooling configs. If
  pre-commit's end-of-file-fixer rewrites a new file, commit the fixed version.
