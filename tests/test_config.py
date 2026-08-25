"""Config loader tests: SPEC config cases 1-8 plus the hot-reload contract."""

import os
from dataclasses import fields
from pathlib import Path

import pytest

from breezed.config import (
    DEFAULT_CURVE,
    ConfigError,
    ConfigWatcher,
    Settings,
    load_settings,
)
from breezed.curve import CurvePoint
from breezed.types import FanPercent, TempC

FIXTURE = Path(__file__).parent / "fixtures" / "config_valid.toml"

EXPECTED_FIXTURE_CURVE = (
    CurvePoint(temp_c=TempC(45), fan_pct=FanPercent(6)),
    CurvePoint(temp_c=TempC(60), fan_pct=FanPercent(8)),
    CurvePoint(temp_c=TempC(68), fan_pct=FanPercent(12)),
    CurvePoint(temp_c=TempC(74), fan_pct=FanPercent(18)),
)


@pytest.fixture(autouse=True)
def clean_idrac_env(monkeypatch: pytest.MonkeyPatch) -> None:
    for var in ("IDRAC_HOST", "IDRAC_USER", "IDRAC_PASSWORD"):
        monkeypatch.delenv(var, raising=False)


def write_config(path: Path, content: str) -> Path:
    path.write_text(content)
    return path


def bump_mtime(path: Path, offset_ns: int) -> None:
    mtime_ns = path.stat().st_mtime_ns
    os.utime(path, ns=(mtime_ns + offset_ns, mtime_ns + offset_ns))


VALID_BODY = """
[settings]
host = "192.168.0.10"
user = "admin"
password = "s3cret"

[[curve]]
temp_c = 40
fan_pct = 5

[[curve]]
temp_c = 70
fan_pct = 50
"""


def test_full_valid_toml_loads_frozen_settings() -> None:
    settings = load_settings(FIXTURE)

    assert settings.host == "169.254.0.1"
    assert settings.user == "root"
    assert settings.password == ""
    assert settings.curve == EXPECTED_FIXTURE_CURVE
    assert settings.poll_interval_s == 10
    assert settings.read_failure_limit == 3
    assert settings.step_down_hysteresis_s == 30
    assert settings.metrics_port is None
    assert settings.ipmitool_path == "/usr/bin/ipmitool"
    for field_info in fields(Settings):
        with pytest.raises(AttributeError):
            setattr(settings, field_info.name, object())


def test_missing_host_and_user_raises_naming_field(tmp_path: Path) -> None:
    path = write_config(tmp_path / "no_identity.toml", "[settings]\npoll_interval_s = 5\n")

    with pytest.raises(ConfigError) as excinfo:
        load_settings(path)

    message = str(excinfo.value)
    assert "host" in message
    assert "user" in message


def test_env_overrides_file(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setenv("IDRAC_HOST", "10.9.8.7")
    monkeypatch.setenv("IDRAC_USER", "envuser")
    monkeypatch.setenv("IDRAC_PASSWORD", "envpass")
    path = write_config(
        tmp_path / "with_identity.toml",
        '[settings]\nhost = "file-host"\nuser = "file-user"\npassword = "file-pass"\n',
    )

    settings = load_settings(path)

    assert settings.host == "10.9.8.7"
    assert settings.user == "envuser"
    assert settings.password == "envpass"


def test_empty_env_value_falls_back_to_file(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    path = write_config(
        tmp_path / "with_identity.toml",
        '[settings]\nhost = "file-host"\nuser = "file-user"\n',
    )

    monkeypatch.setenv("IDRAC_HOST", "")
    monkeypatch.setenv("IDRAC_PASSWORD", "")
    settings = load_settings(path)
    assert settings.host == "file-host"
    assert settings.password == ""


def test_empty_env_value_with_missing_file_key_raises(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    path = write_config(tmp_path / "no_user.toml", '[settings]\nhost = "h"\n')

    monkeypatch.setenv("IDRAC_USER", "")
    with pytest.raises(ConfigError) as excinfo:
        load_settings(path)

    assert "IDRAC_USER" in str(excinfo.value)


@pytest.mark.parametrize(
    "field",
    ["poll_interval_s", "read_failure_limit", "step_down_hysteresis_s", "metrics_port"],
)
def test_bool_rejected_for_positive_int_fields(tmp_path: Path, field: str) -> None:
    path = write_config(
        tmp_path / "bool_trap.toml",
        f'[settings]\nhost = "h"\nuser = "u"\n{field} = true\n',
    )

    with pytest.raises(ConfigError) as excinfo:
        load_settings(path)

    assert f"{field}: expected an integer" in str(excinfo.value)


def test_non_ascending_curve_rejected(tmp_path: Path) -> None:
    path = write_config(
        tmp_path / "flat_curve.toml",
        VALID_BODY + "\n[[curve]]\ntemp_c = 70\nfan_pct = 55\n",
    )

    with pytest.raises(ConfigError) as excinfo:
        load_settings(path)

    assert "strictly ascending" in str(excinfo.value)


def test_fan_pct_out_of_range_rejected(tmp_path: Path) -> None:
    path = write_config(
        tmp_path / "bad_pct.toml",
        '[settings]\nhost = "h"\nuser = "u"\n\n[[curve]]\ntemp_c = 40\nfan_pct = 101\n',
    )

    with pytest.raises(ConfigError) as excinfo:
        load_settings(path)

    assert "curve[0].fan_pct" in str(excinfo.value)


@pytest.mark.parametrize(
    "field", ["poll_interval_s", "read_failure_limit", "step_down_hysteresis_s"]
)
@pytest.mark.parametrize("value", [0, -3])
def test_zero_or_negative_interval_rejected(tmp_path: Path, field: str, value: int) -> None:
    path = write_config(
        tmp_path / "bad_interval.toml",
        f'[settings]\nhost = "h"\nuser = "u"\n{field} = {value}\n',
    )

    with pytest.raises(ConfigError) as excinfo:
        load_settings(path)

    assert field in str(excinfo.value)


def test_omitted_curve_falls_back_to_default(tmp_path: Path) -> None:
    path = write_config(tmp_path / "no_curve.toml", '[settings]\nhost = "h"\nuser = "u"\n')
    empty_path = write_config(
        tmp_path / "empty_curve.toml",
        '[settings]\nhost = "h"\nuser = "u"\ncurve = []\n',
    )

    assert load_settings(path).curve == DEFAULT_CURVE
    assert load_settings(empty_path).curve == DEFAULT_CURVE


def test_unknown_keys_ignored(tmp_path: Path) -> None:
    path = write_config(
        tmp_path / "extra_keys.toml",
        'top_level_stray = "ignored"\n'
        '[settings]\nhost = "h"\nuser = "u"\nsomeday_field = true\n\n'
        '[[curve]]\ntemp_c = 40\nfan_pct = 5\nfuture_col = "x"\n',
    )

    settings = load_settings(path)

    assert settings.host == "h"
    assert settings.curve == (CurvePoint(temp_c=TempC(40), fan_pct=FanPercent(5)),)


def test_invalid_toml_and_missing_file_surface_as_config_error(tmp_path: Path) -> None:
    bad_toml = write_config(tmp_path / "broken.toml", "[settings]\nhost = \n")
    missing = tmp_path / "absent.toml"

    with pytest.raises(ConfigError):
        load_settings(bad_toml)
    with pytest.raises(ConfigError):
        load_settings(missing)


def test_hot_reload_picks_up_mtime_change(tmp_path: Path) -> None:
    path = write_config(tmp_path / "hot.toml", '[settings]\nhost = "first"\nuser = "u"\n')
    watcher = ConfigWatcher(path)

    assert not watcher.changed()
    assert watcher.reload().host == "first"

    path.write_text('[settings]\nhost = "second"\nuser = "u"\n')
    bump_mtime(path, 1_000_000)

    assert watcher.changed()
    reloaded = watcher.reload()
    assert reloaded.host == "second"
    assert not watcher.changed()


def test_failed_reload_keeps_last_good_settings_usable(tmp_path: Path) -> None:
    path = write_config(tmp_path / "reload.toml", '[settings]\nhost = "good"\nuser = "u"\n')
    watcher = ConfigWatcher(path)
    last_good = watcher.reload()

    path.write_text("[settings]\nhost = ")
    bump_mtime(path, 1_000_000)
    assert watcher.changed()

    with pytest.raises(ConfigError):
        watcher.reload()

    path.write_text('[settings]\nhost = "recovered"\nuser = "u"\n')
    bump_mtime(path, 2_000_000)

    recovered = watcher.reload()
    assert recovered.host == "recovered"
    assert last_good.host == "good"
    assert last_good.curve == DEFAULT_CURVE


def test_vanished_file_after_load_surfaces_config_error_not_oserror(
    tmp_path: Path,
) -> None:
    path = write_config(tmp_path / "vanish.toml", '[settings]\nhost = "good"\nuser = "u"\n')
    watcher = ConfigWatcher(path)
    watcher.reload()
    assert not watcher.changed()

    path.unlink()
    assert watcher.changed()

    with pytest.raises(ConfigError):
        watcher.reload()


def test_empty_ipmitool_path_falls_back_to_default(tmp_path: Path) -> None:
    path = write_config(
        tmp_path / "empty_tool_path.toml",
        '[settings]\nhost = "h"\nuser = "u"\nipmitool_path = ""\n',
    )

    settings = load_settings(path)

    assert settings.ipmitool_path == "/usr/bin/ipmitool"


def test_settings_field_set_matches_contract() -> None:
    names = [f.name for f in fields(Settings)]
    assert names == [
        "host",
        "user",
        "password",
        "curve",
        "poll_interval_s",
        "read_failure_limit",
        "step_down_hysteresis_s",
        "metrics_port",
        "ipmitool_path",
    ]
