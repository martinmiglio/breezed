"""T9 daemon deploy tests: pure script rendering + unprivileged status only.

render_* functions are exercised directly (no execution seams involved);
generated scripts are validated with `sh -n` and executed against sandboxed
BREEZED_INSTALL_ROOT trees. CLI daemon commands are exercised by monkeypatching
the module-level renderers.
"""

import getpass
import json
import os
import re
import stat
import subprocess
import sys
import tempfile
from importlib import resources
from pathlib import Path

import pytest
from typer.testing import CliRunner

import breezed
from breezed import cli
from breezed.cli import app
from breezed.daemon import (
    EXEC_PATH,
    DaemonError,
    DaemonStatus,
    InstallerPaths,
    daemon_status,
    render_install_script,
    render_uninstall_script,
)

UNIT_NAME = "breezed.service"
STAMPED_UNIT = f"""\
# Installed by breezed 9.9.9 on 2026-01-01T00:00:00+00:00; re-run `breezed daemon install`
# to refresh this file.
[Service]
ExecStart={EXEC_PATH} run
"""


class FakeFileOps:
    def __init__(self, files: dict[str, str] | None = None) -> None:
        self.files: dict[Path, str] = {Path(name): body for name, body in (files or {}).items()}

    def read_text(self, path: Path) -> str:
        return self.files[path]

    def stat(self, path: Path) -> os.stat_result | None:
        if path not in self.files:
            return None
        return os.stat_result((0o100644, 0, 0, 0, 0, 0, 0, 0, 0, 0))


class FakeRunner:
    def __init__(
        self,
        outputs: dict[str, str] | None = None,
        errors: dict[str, str] | None = None,
    ) -> None:
        self.argvs: list[list[str]] = []
        self.outputs = outputs or {}
        self.errors = errors or {}

    def __call__(self, argv: list[str]) -> str:
        self.argvs.append(argv)
        key = " ".join(argv)
        if key in self.errors:
            raise DaemonError(self.errors[key])
        return self.outputs.get(key, "")


@pytest.fixture
def paths(tmp_path: Path) -> InstallerPaths:
    return InstallerPaths(
        unit_path=tmp_path / UNIT_NAME,
        env_path=tmp_path / "breezed.env",
        config_dir=tmp_path / "breezed",
    )


@pytest.fixture
def cli_runner():
    return CliRunner()


def test_status_reports_version_drift_when_stamped_unit_differs(paths: InstallerPaths):
    fs = FakeFileOps({str(paths.unit_path): STAMPED_UNIT})
    fake_runner = FakeRunner(
        {"systemctl is-active breezed": "active\n", "systemctl is-enabled breezed": "enabled\n"}
    )

    status = daemon_status(paths, runner=fake_runner, fs=fs)

    assert status.unit_present is True
    assert status.active is True
    assert status.enabled is True
    assert status.unit_version == "9.9.9"
    assert status.binary_version == breezed.__version__
    assert status.unit_version != status.binary_version


def test_status_absent_unit_reports_all_clear(paths: InstallerPaths):
    fs, fake_runner = FakeFileOps(), FakeRunner()

    status = daemon_status(paths, runner=fake_runner, fs=fs)

    assert status == DaemonStatus(
        unit_present=False,
        active=False,
        enabled=False,
        unit_version=None,
        binary_version=breezed.__version__,
    )


def test_status_is_unprivileged_and_runs_no_privileged_commands(paths: InstallerPaths):
    fs, fake_runner = FakeFileOps(), FakeRunner()
    daemon_status(paths, runner=fake_runner, fs=fs)
    for argv in fake_runner.argvs:
        assert argv[0] == "systemctl" and argv[1] in ("is-active", "is-enabled")


def test_status_probe_failure_reports_inactive(paths: InstallerPaths):
    fs = FakeFileOps({str(paths.unit_path): STAMPED_UNIT})
    fake_runner = FakeRunner(errors={"systemctl is-active breezed": "systemctl is-active failed"})

    status = daemon_status(paths, runner=fake_runner, fs=fs)

    assert status.unit_present is True
    assert status.active is False


@pytest.mark.parametrize("start", [True, False])
def test_render_install_script_is_valid_posix_sh(start: bool):
    script = render_install_script(start=start)
    completed = subprocess.run(["sh", "-n"], input=script, capture_output=True, text=True)
    assert completed.returncode == 0, completed.stderr


def test_render_uninstall_script_is_valid_posix_sh():
    script = render_uninstall_script()
    completed = subprocess.run(["sh", "-n"], input=script, capture_output=True, text=True)
    assert completed.returncode == 0, completed.stderr


def test_generation_refused_when_running_from_deployed_runtime(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    sandbox = tmp_path / "sandbox"
    (sandbox / "opt").mkdir(parents=True)
    (sandbox / "opt" / "breezed").symlink_to(Path(sys.prefix).resolve(), target_is_directory=True)
    monkeypatch.setenv("BREEZED_INSTALL_ROOT", str(sandbox))
    with pytest.raises(DaemonError, match="deployed runtime"):
        render_install_script(start=False)
    with pytest.raises(DaemonError, match="deployed runtime"):
        render_uninstall_script()


def test_generation_allowed_for_development_prefix(monkeypatch: pytest.MonkeyPatch):
    monkeypatch.setenv("BREEZED_INSTALL_ROOT", str(Path(sys.prefix) / "nowhere-near-opt"))
    render_install_script(start=False)


def test_install_script_contains_self_copy_guard():
    script = render_install_script(start=False)
    assert "refusing to copy the deployed runtime onto itself" in script
    assert 'for deployed in "$ROOT/opt/breezed" "$ROOT/opt/breezed-python"' in script
    assert "source runtime not found" in script


def test_install_script_self_copy_guard_triggers_in_sandbox(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    root = tmp_path / "sandbox"
    (root / "opt").mkdir(parents=True)
    (root / "opt" / "breezed").symlink_to(Path(sys.prefix).resolve(), target_is_directory=True)
    script_path = tmp_path / "install.sh"
    script_path.write_text(render_install_script(start=False))

    result = _run_script(script_path, root)

    assert result.returncode == 1
    assert "refusing to copy the deployed runtime onto itself" in result.stderr
    assert not (root / "etc").exists()


def test_install_script_is_prefixed_and_documents_root_override():
    script = render_install_script(start=False)
    assert script.startswith("#!/bin/sh")
    assert "POSIX sh + GNU coreutils" in script
    assert "BREEZED_INSTALL_ROOT" in script
    assert "[breezed-install] done" in script


def test_install_script_root_check_names_exact_rerun_command():
    script = render_install_script(start=False)
    assert "root privileges are required" in script
    assert "sudo sh $0" in script


def test_start_flag_controls_enable_now_line():
    started = render_install_script(start=True)
    stopped = render_install_script(start=False)
    assert "systemctl enable --now breezed" in started
    assert "systemctl enable --now" not in stopped
    assert "systemctl daemon-reload" in started and "systemctl daemon-reload" in stopped


def test_fresh_env_with_start_warns_unit_will_fail_until_secrets_set():
    script = render_install_script(start=True)
    note_pos = script.find("the unit will fail until secrets are set")
    fill_pos = script.find("secrets are EMPTY")
    assert note_pos != -1 and fill_pos != -1 and note_pos < fill_pos


def test_uninstall_script_sandbox_end_to_end(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    root = tmp_path / "sandbox"
    unit = root / "etc" / "systemd" / "system" / UNIT_NAME
    shim = root / "usr" / "local" / "bin" / "breezed"
    env_file = root / "etc" / "breezed.env"
    config = root / "etc" / "breezed" / "breezed.toml"
    for target in (unit, shim, env_file, config):
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text("# placeholder\n")

    script_path = tmp_path / "uninstall.sh"
    script_path.write_text(render_uninstall_script())

    result = _run_script(script_path, root)

    assert result.returncode == 0, result.stderr
    assert not unit.exists()
    assert not shim.exists()
    assert env_file.exists() and config.exists()


def test_rendered_unit_uses_fixed_exec_path_and_no_home_paths():
    script = render_install_script(start=False)
    assert f"ExecStart={EXEC_PATH} run --config /etc/breezed/breezed.toml" in script
    unit = script.split("<<'BREEZED_UNIT_EOF'\n", 1)[1].split("\nBREEZED_UNIT_EOF\n", 1)[0]
    assert "/home" not in unit
    assert "/usr/local/bin/breezed" in unit


def test_rendered_unit_drops_thread_blocking_directives_keeps_rest():
    script = render_install_script(start=False)
    assert "MemoryDenyWriteExecute=yes" not in script
    assert "RestrictNamespaces=yes" not in script
    for directive in (
        "NoNewPrivileges=yes",
        "ProtectSystem=strict",
        "ProtectHome=yes",
        "PrivateTmp=yes",
        "PrivateDevices=yes",
        "ProtectKernelTunables=yes",
        "ProtectKernelModules=yes",
        "ProtectControlGroups=yes",
        "LockPersonality=yes",
        "RestrictSUIDSGID=yes",
        "CapabilityBoundingSet=",
        "RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX",
    ):
        assert directive in script


def _write_stubs(directory: Path, names: tuple[str, ...]) -> Path:
    directory.mkdir(parents=True, exist_ok=True)
    for name in names:
        stub = directory / name
        stub.write_text("#!/bin/sh\nexit 0\n")
        stub.chmod(0o755)
    return directory


def _run_script(
    script_path: Path,
    root: Path,
    extra_env: dict[str, str] | None = None,
    stubs: Path | None = None,
) -> subprocess.CompletedProcess[str]:
    path_value = f"{stubs}:{os.environ['PATH']}" if stubs is not None else os.environ["PATH"]
    env = {
        "PATH": path_value,
        "BREEZED_INSTALL_ROOT": str(root),
        **(extra_env or {}),
    }
    return subprocess.run(
        ["sh", str(script_path)], capture_output=True, text=True, env=env, timeout=300
    )


def test_install_script_double_run_is_idempotent_in_sandbox(tmp_path: Path):
    root = tmp_path / "sandbox"
    stubs = _write_stubs(tmp_path / "stubs", ("useradd",))
    script_path = tmp_path / "install.sh"
    script_path.write_text(render_install_script(start=True))
    sudo_user = getpass.getuser()

    first = _run_script(script_path, root, {"SUDO_USER": sudo_user}, stubs)
    assert first.returncode == 0, first.stderr

    env_file = root / "etc" / "breezed.env"
    config_file = root / "etc" / "breezed" / "breezed.toml"
    unit_file = root / "etc" / "systemd" / "system" / UNIT_NAME
    shim = root / "usr" / "local" / "bin" / "breezed"
    env_before, config_before = env_file.read_bytes(), config_file.read_bytes()
    example = resources.files("breezed").joinpath("templates").joinpath("breezed.toml.example")
    assert config_file.read_bytes().rstrip(b"\n") == example.read_bytes().rstrip(b"\n")
    assert stat.S_IMODE(config_file.stat().st_mode) == 0o664
    assert config_file.stat().st_uid == os.getuid()
    assert f"config owner: {sudo_user} (0664)" in first.stdout

    second = _run_script(script_path, root, {"SUDO_USER": sudo_user}, stubs)

    assert second.returncode == 0, second.stderr
    assert env_file.read_bytes() == env_before
    assert config_file.read_bytes() == config_before
    assert stat.S_IMODE(config_file.stat().st_mode) == 0o664
    assert config_file.stat().st_uid == os.getuid()
    assert unit_file.is_file()
    assert shim.is_symlink()
    assert (root / "opt" / "breezed.old").is_dir()
    assert "secrets are EMPTY — fill them:" in first.stdout
    assert "sudoedit" in first.stdout
    assert "[breezed-install]   sudo systemctl restart breezed" in first.stdout
    assert first.stdout.strip().splitlines()[-1] == (
        "[breezed-install] done — systemctl status breezed"
    )
    assert "secrets are EMPTY" not in second.stdout
    assert "runtime updated — restart to pick it up:" in second.stdout
    assert "[breezed-install]   sudo systemctl restart breezed" in second.stdout
    assert second.stdout.strip().splitlines()[-1] == (
        "[breezed-install] done — systemctl status breezed"
    )
    assert stat.S_IMODE(env_file.stat().st_mode) == 0o640


def test_install_script_config_owner_override(tmp_path: Path):
    root = tmp_path / "sandbox"
    stubs = _write_stubs(tmp_path / "stubs", ("useradd",))
    script_path = tmp_path / "install.sh"
    script_path.write_text(render_install_script(start=False))

    result = _run_script(
        script_path,
        root,
        {"BREEZED_CONFIG_OWNER": getpass.getuser(), "SUDO_USER": "someone-else"},
        stubs,
    )

    assert result.returncode == 0, result.stderr
    config_file = root / "etc" / "breezed" / "breezed.toml"
    assert stat.S_IMODE(config_file.stat().st_mode) == 0o664
    assert config_file.stat().st_uid == os.getuid()
    assert f"config owner: {getpass.getuser()} (0664)" in result.stdout


def test_install_script_contains_shebang_relocation_loop():
    script = render_install_script(start=False)
    assert 'sed -i "1s|.*|#!${ROOT}/opt/breezed/bin/python3|" "$shim"' in script
    assert '[breezed-install] relocated shebang: $shim"' in script
    assert "home = ${ROOT}/opt/breezed-python/bin|" in script
    assert 'cp -a "$BASE_DIR" "$ROOT/opt/breezed-python"' in script
    assert "activate|activate.*" in script


def test_install_script_rewrites_hostile_shebang_to_relocated_python(tmp_path: Path):
    root = tmp_path / "sandbox"
    stubs = _write_stubs(tmp_path / "stubs", ("useradd",))
    script_path = tmp_path / "install.sh"
    script_path.write_text(render_install_script(start=False))

    result = _run_script(script_path, root, stubs=stubs)

    assert result.returncode == 0, result.stderr
    shim = root / "opt" / "breezed" / "bin" / "breezed"
    first_line = shim.read_text().splitlines()[0]
    assert first_line == f"#!{root}/opt/breezed/bin/python3"
    python_bin = root / "opt" / "breezed" / "bin" / "python3"
    assert python_bin.is_file() and not python_bin.is_symlink()
    pyvenv = (root / "opt" / "breezed" / "pyvenv.cfg").read_text()
    assert f"home = {root}/opt/breezed-python/bin" in pyvenv
    assert "relocated shebang:" in result.stdout


def test_install_script_preserves_pre_existing_env_secrets(tmp_path: Path):
    root = tmp_path / "sandbox"
    secret_env = b"IDRAC_HOST=10.0.0.9\nIDRAC_USER=admin\nIDRAC_PASSWORD=hunter2\n"
    (root / "etc").mkdir(parents=True)
    (root / "etc" / "breezed.env").write_bytes(secret_env)
    stubs = _write_stubs(tmp_path / "stubs", ("useradd",))
    script_path = tmp_path / "install.sh"
    script_path.write_text(render_install_script(start=False))

    result = _run_script(script_path, root, stubs=stubs)

    assert result.returncode == 0, result.stderr
    assert (root / "etc" / "breezed.env").read_bytes() == secret_env
    assert "keeping existing" in result.stdout
    assert "secrets are EMPTY" not in result.stdout
    assert "runtime updated — restart to pick it up:" in result.stdout
    assert "[breezed-install]   sudo systemctl restart breezed" in result.stdout


def test_daemon_install_runs_unprivileged_writing_only_its_script(
    tmp_path: Path,
):
    output_path = tmp_path / "install.sh"
    result = CliRunner().invoke(
        app,
        ["daemon", "install", "--output", str(output_path)],
        catch_exceptions=False,
    )
    assert result.exit_code == 0
    assert result.stderr.count("sudo sh") == 1


class _RendererState:
    def __init__(self) -> None:
        self.install_script = "#!/bin/sh\nexit 0\n"
        self.uninstall_script = "#!/bin/sh\nexit 0\n"
        self.error: DaemonError | None = None
        self.start_arg: bool | None = None


@pytest.fixture
def patch_renderers(monkeypatch: pytest.MonkeyPatch) -> _RendererState:
    state = _RendererState()

    def fake_install(*, start: bool) -> str:
        state.start_arg = start
        if state.error is not None:
            raise state.error
        return state.install_script

    def fake_uninstall() -> str:
        if state.error is not None:
            raise state.error
        return state.uninstall_script

    monkeypatch.setattr(cli, "render_install_script", fake_install)
    monkeypatch.setattr(cli, "render_uninstall_script", fake_uninstall)
    return state


def test_help_lists_daemon_subcommands(cli_runner: CliRunner):
    result = cli_runner.invoke(app, ["daemon", "--help"], catch_exceptions=False)
    assert result.exit_code == 0
    for name in ("install", "status", "uninstall"):
        assert name in result.output


def test_daemon_install_writes_default_tempdir_script_mode_0644(cli_runner: CliRunner):
    result = cli_runner.invoke(app, ["daemon", "install"], catch_exceptions=False)
    assert result.exit_code == 0
    payload = json.loads(result.stdout)
    path = Path(payload["script"])
    try:
        assert payload["start"] is False
        assert path.parent == Path(tempfile.gettempdir())
        assert re.fullmatch(r"breezed-install-[a-z0-9_]+\.sh", path.name)
        assert path.read_text().startswith("#!/bin/sh")
        assert stat.S_IMODE(path.stat().st_mode) == 0o644
        assert f"sudo sh {path}" in result.stderr
    finally:
        path.unlink(missing_ok=True)


def test_daemon_install_honors_output_and_start_flag(
    cli_runner: CliRunner, tmp_path: Path, patch_renderers
):
    out = tmp_path / "custom.sh"
    result = cli_runner.invoke(
        app, ["daemon", "install", "--start", "--output", str(out)], catch_exceptions=False
    )
    assert result.exit_code == 0
    assert patch_renderers.start_arg is True
    assert out.read_text() == "#!/bin/sh\nexit 0\n"
    assert json.loads(result.stdout) == {
        "event": "install_script_written",
        "script": str(out),
        "start": True,
    }
    assert f"sudo sh {out}" in result.stderr


def test_daemon_install_unwritable_output_exits_1(cli_runner: CliRunner, tmp_path: Path):
    result = cli_runner.invoke(
        app, ["daemon", "install", "--output", str(tmp_path / "missing-dir" / "x.sh")]
    )
    assert result.exit_code == 1
    assert result.stdout.strip() == ""


def test_daemon_install_deployed_runtime_error_exits_1(cli_runner: CliRunner, patch_renderers):
    patch_renderers.error = DaemonError("this is the deployed runtime (/opt/breezed)")
    result = cli_runner.invoke(app, ["daemon", "install"])
    assert result.exit_code == 1
    assert "deployed runtime" in result.stderr


def test_daemon_uninstall_writes_script_and_instruction(
    cli_runner: CliRunner, tmp_path: Path, patch_renderers
):
    patch_renderers.uninstall_script = "#!/bin/sh\necho bye\n"
    out = tmp_path / "bye.sh"
    result = cli_runner.invoke(
        app, ["daemon", "uninstall", "--output", str(out)], catch_exceptions=False
    )
    assert result.exit_code == 0
    assert out.read_text() == "#!/bin/sh\necho bye\n"
    assert json.loads(result.stdout) == {
        "event": "uninstall_script_written",
        "script": str(out),
    }
    assert f"sudo sh {out}" in result.stderr


def test_daemon_status_prints_report_json_exit_0(cli_runner: CliRunner, monkeypatch):
    def fake_status(**kwargs: object) -> DaemonStatus:
        return DaemonStatus(True, False, True, "0.1.0", "0.2.0")

    monkeypatch.setattr(cli, "daemon_status", fake_status)
    result = cli_runner.invoke(app, ["daemon", "status"], catch_exceptions=False)
    assert result.exit_code == 0
    payload = json.loads(result.stdout)
    assert payload == {
        "unit_present": True,
        "active": False,
        "enabled": True,
        "unit_version": "0.1.0",
        "binary_version": "0.2.0",
    }
