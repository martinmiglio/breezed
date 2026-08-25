# T9 — Privilege UX: plan-and-script installer

## Goal

Rework T8's installer so **nothing breezed does requires root**. `breezed daemon
install` runs fully unprivileged: it validates, renders the unit, and emits a
self-contained, idempotent POSIX-sh install script plus printed instructions
(`sudo sh <path>`). The user reviews and runs it themselves. This replaces the
in-process privileged execution design (which failed live: `sudo breezed` breaks
on PATH, and the hardened unit's `ProtectHome=yes` hides binaries under `/home`
— solved by the script installing a venv copy under `/opt/breezed`).

Secondary goals: fix the ExecPath drift found at cutover (binary must live
outside `/home`); make uninstall follow the same pattern; keep `status`
unprivileged end-to-end.

## Depends on

(T1–T8)

## Design

### Command flow (all unprivileged)

```
$ breezed daemon install [--start] [--output PATH]
✔ config valid, template rendered (version 0.1.0)
Wrote install script: /tmp/breezed-install-a1b2c3.sh
Review it, then run:  sudo sh /tmp/breezed-install-a1b2c3.sh

$ sudo sh /tmp/breezed-install-a1b2c3.sh
[breezed-install] creating system user breezed
[breezed-install] copying runtime to /opt/breezed
...
[breezed-install] done — systemctl status breezed
```

- `install` NEVER executes privileged operations. It produces the script via a
  new pure function `DaemonInstaller.render_install_script(start: bool) -> str`.
- `uninstall` mirrors it: `render_uninstall_script() -> str`, same output flow.
- `status`: unchanged, already unprivileged.
- Exit codes unchanged (0/1/2 per T7 contract).

### What the generated script does (each step guarded/idempotent, `[breezed-install]` prefix)

1. Root check: `if [ "$(id -u)" -ne 0 ]`; actionable message naming the exact
   command to re-run.
2. Create `breezed` system user if missing
   (`useradd --system --no-create-home --shell /usr/sbin/nologin breezed`).
3. Install runtime: copy the *current* venv (`sys.prefix`, resolved at generation
   time and embedded as `$SRC_VENV`) to `/opt/breezed` —
   `rm -rf /opt/breezed.old; [ -d /opt/breezed ] && mv /opt/breezed /opt/breezed.old`;
   `cp -a "$SRC_VENV" /opt/breezed`. Upgrade-safe: old runtime kept one
   generation back. Symlink `/usr/local/bin/breezed -> /opt/breezed/bin/breezed`
   (replace existing link/file).
4. Write the rendered unit to `/etc/systemd/system/breezed.service` (heredoc,
   quoted delimiter). Placeholders `{version}`, `{installed_at}`, and
   `ExecStart=/usr/local/bin/breezed run --config /etc/breezed/breezed.toml
   --metrics-port 9762` are filled at GENERATION time (unit content ships inside
   the script, so the script alone reproduces the install).
5. Env skeleton: if `/etc/breezed.env` absent → write skeleton with empty values;
   **never overwrite**; always ensure `root:breezed` / `0640`.
6. Config: if `/etc/breezed/breezed.toml` absent → install the packaged
   `breezed.toml.example` there; never overwrite.
7. `systemctl daemon-reload`; if start requested: `systemctl enable --now breezed`.
8. Final line: `echo "[breezed-install] done — systemctl status breezed"`.

### Code changes

- `src/breezed/daemon.py`: add `render_install_script(start: bool) -> str` and
  `render_uninstall_script() -> str` to `DaemonInstaller` (pure functions over
  injected paths/template/loader seams — NO runner/fs execution in these paths);
  keep existing `install/status/uninstall` execution methods only where still
  exercised (status stays; install/uninstall execution paths become
  script-generation only — remove the privileged execution code paths and their
  FileOps/user_lookup/runner machinery where orphaned).
- `src/breezed/cli.py`: `daemon install [--start] [--output PATH]` writes script
  (default `<tempdir>/breezed-install-<8-hex>.sh`, mode 0644) and prints the
  review-and-run instruction; `daemon uninstall [--output PATH]` same pattern.
  JSON summary of planned actions to stdout alongside the human instruction.
- Template placeholder change: `{exec_path}` → fixed `/usr/local/bin/breezed`
  (the shim), eliminating drift between installed shim and unit.
- Update README deploy section to the two-command flow.

### Security properties

- Generated scripts contain NO secrets (env skeleton has empty values).
- Scripts are plain POSIX sh, reviewable, deterministic given inputs (embedded
  timestamp/version excepted).
- breezed never spawns sudo/pkexec; no privilege-escalation code in-process.

## Acceptance criteria

- [ ] `daemon install` runs to completion as non-root, writing only its script +
      stdout output (test asserts no other filesystem effects via tmp_path cwd)
- [ ] `render_install_script` output is valid POSIX sh (`sh -n` passes in a test)
      and idempotent: running the script twice leaves env/config byte-identical
      (tested by executing generated script against a fake root dir via env-var
      override of install targets — script must honor `BREEZED_INSTALL_ROOT`
      prefix for testability, defaulting to `/`)
- [ ] Second run of script preserves pre-existing env file contents (secret-
      preservation test at script level)
- [ ] Unit inside script uses `/usr/local/bin/breezed`; no `/home` paths anywhere
      in rendered unit
- [x] `sh -n` clean; `shellcheck` findings triaged — none above INFO severity
      (document in Notes; do not add shellcheck to CI)
- [ ] CLI: `--output` honored; instruction line names exact script path; exit
      codes 0/1/2 contract holds (e.g., unwritable output dir ⇒ exit 1)
- [ ] `daemon status` remains unprivileged (no regression)
- [ ] Old privileged execution machinery removed; `uvx ty check` zero errors,
      no dead exports in `__all__`
- [ ] README deploy section rewritten to two-command flow; upgrade path updated
      (rerun install → regenerate script → rerun script → restart)
- [ ] All verification commands pass clean:
  - [ ] `uv run pytest`
  - [ ] `uvx ty check`
  - [ ] `uvx ruff check .`
  - [ ] `uvx ruff format --check .`
  - [ ] `uvx pre-commit run --all-files`

## Notes

- `BREEZED_INSTALL_ROOT` env prefix in generated scripts exists purely so tests
  execute real scripts against sandboxed trees without root; production runs
  unset it. Document at top of generated script.
- The venv-copy approach assumes the source venv relocates cleanly (uv tools'
  venvs use relative shebangs after `bin/breezed` shim rewrite; verify
  `/opt/breezed/bin/breezed` runs standalone — add a script step that smoke-tests
  `/opt/breezed/bin/breezed --version` before enabling the unit).
- If `/opt/breezed/bin/breezed --version` fails in the script, abort BEFORE
  daemon-reload and point at `/opt/breezed.old` restore hint.
- Keep `DaemonError` semantics for missing packaged resources during generation.
- Config vs secrets permissions: every install run (fresh or drift-repair)
  sets `/etc/breezed/breezed.toml` to `CONFIG_OWNER` (`BREEZED_CONFIG_OWNER`,
  else `$SUDO_USER`, else root) + `0664` so agents/admins can tune it live
  (hot-reload picks edits up); the env file stays `root:breezed` `0640` and is
  never content-touched.
- Live finding (mmsrv): config edits never hot-reloaded because mtime was only
  checked when SIGHUP arrived; the run loop now checks `watcher.changed()` every
  tick, so SIGHUP is an optional force-check rather than the trigger.
- Live finding (mmsrv): `MemoryDenyWriteExecute=yes`/`RestrictNamespaces=yes`
  block thread creation at metrics-server startup under the service sandbox —
  both directives removed from the unit template with an explanatory comment.
- Live finding (mmsrv): uncaught exceptions bypassed the JSON pipeline; `run`
  now emits a structured `fatal` event (`error`/`detail`/`traceback`) via the
  logging pipeline before propagating exit 1.
- Generation guard: `render_install_script`/`render_uninstall_script` refuse to
  run from inside the deploy target (sys.prefix resolving under
  `<BREEZED_INSTALL_ROOT>/opt/breezed*`, e.g. when the `/usr/local/bin` shim
  shadows the dev install) and the rendered script itself re-checks
  `SRC_VENV != $ROOT/opt/breezed` before copying.
- The install script relocates the runtime fully off `/home` (rewrites venv
  shebangs to `#!/<root>/opt/breezed/bin/python3`, dereferences interpreter
  symlinks, copies the base CPython next door, and repoints `pyvenv.cfg home`)
  so `ProtectHome=yes` cannot break exec, and always prints a next-steps block
  before the done line: secret-fill/restart when it created a fresh env
  skeleton (`FRESH_ENV=1`), restart-only on upgrades/pre-existing env.
- When the install run creates a fresh env skeleton (`FRESH_ENV=1`), the script
  prints secret-fill/restart instructions (`sudoedit /etc/breezed.env`,
  `systemctl restart breezed`) right before the final done line; reruns over a
  pre-existing env file skip those lines.
- Shellcheck triage: `shellcheck -s sh` on both rendered scripts — zero findings at any severity (verified during audit with 0.11; implementer used shellcheck-py, also clean).
