# T8 — Daemon deploy & docs

## Goal

Make breezed deployable and upgradable as a systemd service with one command, per the
locked decision that **systemd is the only deployment path** (no Docker/Containerfile —
removed from scope):

- `breezed daemon install` — idempotent install/upgrade of the systemd unit from a
  template packaged inside breezed itself; safe to re-run after every `uv tool upgrade`
- `breezed daemon status` / `breezed daemon uninstall`
- Final README rewrite documenting the full flow:
  `uv tool install .` → `sudo breezed daemon install --start`

No other application code changes. Everything here builds on the CLI contract locked
in T7 (the `daemon` Typer sub-app mounts onto the same `app`; see T7's forward-compat
note).

## Depends on

(T1–T7)

## Files

- `src/breezed/daemon.py` (new) — `DaemonInstaller`: all filesystem/systemd logic,
  fully seam-injected for testability (task 2)
- `src/breezed/cli.py` (modify) — add the `daemon` Typer sub-app (`install`,
  `status`, `uninstall`) via `app.add_typer(...)`; thin wrappers over
  `DaemonInstaller`, exit codes per the T7 contract
- `deploy/breezed.service.template` (new) — the unit file template, shipped in the
  package wheel via `[tool.hatch.build.targets.wheel.force-include]` or a packaged
  data dir; version-stamped at render time
- `deploy/breezed.toml.example` (new) — byte-identical to the SPEC "Config example"
- `README.md` (full rewrite — the skeleton is superseded; keep the License section)

## Tasks

### 1. Unit template (`deploy/breezed.service.template`)

```ini
# Installed by breezed {version} on {installed_at}; re-run `breezed daemon install`
# to refresh this file.
[Unit]
Description=breezed - curve-based iDRAC/IPMI fan controller
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={exec_path} run --config /etc/breezed/breezed.toml --metrics-port 9762
EnvironmentFile=-/etc/breezed.env
Restart=always
RestartSec=5

# Hardening — small network daemon that shells out to ipmitool
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

Requirements:

- `{exec_path}` resolves to the *currently running* `breezed` executable
  (`sys.argv[0]` resolved / `shutil.which("breezed")`) so upgrades that move the
  shim path are picked up on the next `daemon install`.
- `{version}`/`{installed_at}` stamp makes drift visible: `daemon status` can show
  "unit installed by 0.1.2 but running binary is 0.1.3" (compare against
  `breezed.__version__`).
- **Dedicated system user (`breezed`), not `DynamicUser`**: `/etc/breezed.env`
  holds the iDRAC password and needs one stable UID owning read perms
  (`chown root:breezed && chmod 640`). breezed keeps no state, so account
  provisioning is the only cost.
- Never `Type=forking`; never `KillMode=process` (default control-group is correct).
- Metrics port is baked into ExecStart deliberately (netdata scrapes it); document
  how to remove it by editing the unit.

### 2. `DaemonInstaller` (seam-injected, testable without root)

```python
@dataclass(frozen=True, slots=True)
class InstallerPaths:
    unit_path: Path = Path("/etc/systemd/system/breezed.service")
    env_path: Path = Path("/etc/breezed.env")
    config_dir: Path = Path("/etc/breezed")

class DaemonInstaller:
    def __init__(
        self,
        paths: InstallerPaths = InstallerPaths(),
        *,
        runner: Callable[[list[str]], None] = subprocess.run_wrapper,
        fs: FileOps = RealFileOps(),          # write_text/mkdir/chown/chmod/stat seams
        exec_path: str | None = None,         # defaults to current executable
        require_root: bool = True,
    ) -> None: ...

    def install(self, *, start: bool = False) -> InstallReport: ...
    def status(self) -> DaemonStatus: ...
    def uninstall(self) -> None: ...
```

`install()` behavior — **every step idempotent**:

1. Root check (`os.geteuid() != 0` ⇒ exit 1 unless `require_root=False` in tests);
   message must say what to run (`sudo breezed daemon install`).
2. Create the `breezed` system user if missing
   (`useradd --system --no-create-home --shell /usr/sbin/nologin breezed`);
   skip silently when it exists (check via `pwd.getpwnam`, no shell-out parsing).
3. Render the packaged template (version + timestamp + exec path); write to
   `unit_path`. Re-running always overwrites — that is the *upgrade* mechanism.
4. Ensure `/etc/breezed/` exists; if `env_path` is absent write a skeleton with
   empty values + comments; **never overwrite an existing env file** (it holds
   secrets). Set `root:breezed` / `0640`.
5. If `/etc/breezed/breezed.toml` is absent, copy `breezed.toml.example` there;
   never overwrite an existing config (hot-reloaded local edits survive upgrades).
6. `systemctl daemon-reload`; if `start`, also `systemctl enable --now breezed`.
7. Return an `InstallReport` (frozen dataclass: created/skipped items per step,
   rendered unit version, started bool) — printed as JSON by the CLI.

`status()`: report installed-unit version stamp vs running `__version__`,
unit file existence, `systemctl is-active/is-enabled` output.
`uninstall()`: `disable --now`, delete unit, `daemon-reload`; leave user/env/config
alone (destructive steps need explicit flags — keep v1 minimal and safe).

All `systemctl` calls go through the injected `runner`; all FS effects through
`fs`. No test ever touches real `/etc`.

### 3. CLI wiring

- `daemon install [--start]` → build installer, call `install(start=...)`,
  print the `InstallReport` as JSON to stdout; failures ⇒ exit 1 (runtime) with
  stderr message per T7's `_fail` contract.
- `daemon status` → `DaemonStatus` as JSON.
- `daemon uninstall` → confirm-less but JSON-reporting; exit codes same rules.
- Mount via a small `typer.Typer()` sub-app registered with `app.add_typer(daemon_app, name="daemon")`.

### 4. `deploy/breezed.toml.example`

Copy the SPEC "Config example" verbatim — four curve points, commented
`metrics_port`, `host = "169.254.0.1"`. Add nothing, remove nothing.

### 5. Final README rewrite

Sections, in order:

1. **Title + what/why** — curve vs static speed, iDRAC AUTO as safety net above
   the curve; multi-point curve w/ linear interpolation; JSON logs; opt-in metrics;
   swappable adapters/policies note (one sentence, ports-and-adapters).
2. **Install**:
   ```sh
   uv tool install .
   breezed validate /etc/breezed/breezed.toml --probe   # reads one live temp, touches no fans
   ```
3. **CLI examples** — one per command incl. `daemon install/status/uninstall`;
   mention exit codes 0/1/2 in one sentence.
4. **Deploy (systemd)** — the happy path, copy-pasteable:
   ```sh
   sudo breezed daemon install --start
   sudo $EDITOR /etc/breezed.env      # fill IDRAC_HOST/IDRAC_USER/IDRAC_PASSWORD
   sudo systemctl restart breezed
   journalctl -u breezed -f           # watch the first hour
   ```
   Plus the upgrade flow: `uv tool upgrade` (or reinstall) → `sudo breezed daemon
   install` → `sudo systemctl restart breezed`; note `daemon status` shows
   version drift between unit stamp and binary.
5. **Config example** — same TOML block; env var names match T3; hot-reload on
   mtime change noted.
6. **netdata metrics note** — port 9762 on 127.0.0.1, scrape URL, metric names.
7. **Safety disclaimer** — manual fan control risks; AUTO fallback above curve top
   and on repeated sensor failures; `breezed auto` to bail out instantly.
8. **Replacing the legacy C# fan-controller on mmsrv** — stop/disable the old
   JDMallen compose stack (`docker compose down`) *before* starting breezed; two
   controllers fighting over the iDRAC flip modes every poll. Never both at once.

## Acceptance criteria

- [ ] `deploy/breezed.service.template` renders to a valid unit: `Type=simple`,
      `EnvironmentFile=-/etc/breezed.env`, `Restart=always`, `RestartSec=5`,
      network-online ordering, hardening directives present, dedicated-user
      decision documented in-ticket
- [ ] Template renders with `{version}`, `{installed_at}`, `{exec_path}` filled;
      rendered output passes `systemd-analyze verify` (environment warnings about
      missing files/users acceptable; directive-level failures are not)
- [ ] `daemon install` is idempotent: second run changes nothing except the unit
      timestamp stamp; existing env file and config are never overwritten
      (asserted by tests)
- [ ] Root requirement enforced with actionable message; bypassed cleanly under
      `require_root=False` in tests
- [ ] `daemon status` reports unit-present/active/enabled + version-stamp drift
- [ ] `daemon uninstall` disables/removes unit + reloads; leaves user/env/config
- [ ] CLI: three subcommands mounted under `daemon`, JSON reports on stdout,
      errors on stderr, exit codes per T7 contract
- [ ] Tests inject fakes exclusively through `InstallerPaths`/`runner`/`fs` seams;
      zero real `/etc` writes, zero root assumptions, no monkeypatching
- [ ] `deploy/breezed.toml.example` is byte-identical to the SPEC "Config example"
- [ ] README renders all commands correctly; every invocation matches the actual
      `--help` surface; upgrade flow documented
- [ ] mmsrv cutover note present (stop old stack first, never both at once)
- [ ] No container artifacts anywhere in repo (grep for `Containerfile`/`docker build`
      returns only the mmsrv-cutover README note about stopping the old stack)
- [ ] All verification commands pass clean:
  - [ ] `uv run pytest` — full suite green
  - [ ] `uvx ty check` (zero errors, zero ignores)
  - [ ] `uvx ruff check .`
  - [ ] `uvx ruff format --check .`
  - [ ] `uvx pre-commit run --all-files` green (fixers cover new deploy files too)

## Notes

- **Why no Docker anymore**: locked decision — systemd only, simplest form. The
  Containerfile ticket content was removed wholesale; if containers ever return,
  resurrect from git history rather than half-recreating.
- **Idempotence is the whole feature**: the upgrade story is "re-run installer";
  every step must be a no-op-or-refresh, never error-on-existing. The report's
  created-vs-skipped breakdown is how users verify that.
- **Secrets**: env file skeleton ships empty; installer chmods `0640 root:breezed`
  and refuses to touch populated files. The README never shows a real password
  example beyond telling users to edit the file.
- **`subprocess.run_wrapper`**: thin module-level wrapper around `subprocess.run`
  so the injected `runner` has one obvious signature; keeps ipmi.py the only
  *other* subprocess importer (SPEC single-boundary rule now has exactly two
  sanctioned sites — ipmi adapter and daemon installer; note this deviation is
  accepted because systemd control is inherently process supervision).
- **Template packaging**: ensure the template lands in the wheel
  (`force-include` or package data) — a missing template at runtime must fail
  with a clear `IpmiError`-style domain error naming the resource, not a raw
  `FileNotFoundError`.
- Docs+CLI diff discipline: do not touch `curve/config/ipmi/controller/metrics`
  modules; if pre-commit fixers rewrite files, commit fixed versions.
