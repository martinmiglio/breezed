"""IPMI adapter over ipmitool — the only module in T1-T7 that touches subprocess.

Structurally satisfies the TempReader and FanCommander ports from breezed.ports
(never inherits them). Redaction is by construction: the password exists only in
_invoke's local argv list; error messages reference the ipmitool subcommand args,
never the full argv.
"""

import re
import subprocess
from collections.abc import Callable, Sequence

from breezed.config import Settings
from breezed.ports import IpmiError
from breezed.types import FanPercent, TempC

Runner = Callable[..., subprocess.CompletedProcess[str]]

_TIMEOUT_S = 15

# The SDR text table reports temperature sensors by hex address; only 0Eh/0Fh
# rows are CPU-relevant. Anchored per-line so the capture grabs the full 1-3
# digit temp instead of a greedy two-digit tail (108 -> "08").
_SDR_TEMP_RE = re.compile(
    r"^\s*.+?\|\s*0[EF]h\s*\|.*\|\s*(\d{1,3}) degrees C\s*$",
    re.MULTILINE,
)
_FAN_RPM_RE = re.compile(r"(\d+) RPM")

# Raw Dell iDRAC OEM IPMI byte sequences: netfn 0x30/0x30 toggles dynamic fan
# control (0x01 auto on/off) or sets a manual PWM duty cycle (0x02 + full-byte
# selector). ipmitool treats these as opaque bytes; they must stay byte-exact.
_ENABLE_AUTO_CMD = ["raw", "0x30", "0x30", "0x01", "0x01"]
_DISABLE_AUTO_CMD = ["raw", "0x30", "0x30", "0x01", "0x00"]
_MANUAL_PCT_CMD_PREFIX = ["raw", "0x30", "0x30", "0x02", "0xff"]


def _default_runner(args: Sequence[str]) -> subprocess.CompletedProcess[str]:
    """subprocess.run with capture_output/text/utf-8/replace, timeout, check=False."""
    return subprocess.run(
        args,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=_TIMEOUT_S,
        check=False,
    )


class IpmiClient:
    """Structurally satisfies TempReader + FanCommander — never inherits them."""

    def __init__(self, settings: Settings, *, runner: Runner | None = None) -> None:
        self._settings = settings
        self._runner = runner or _default_runner

    def _invoke(self, args: Sequence[str]) -> subprocess.CompletedProcess[str]:
        argv = [
            self._settings.ipmitool_path,
            "-I",
            "lanplus",
            "-H",
            self._settings.host,
            "-U",
            self._settings.user,
            "-P",
            self._settings.password,
            *args,
        ]
        try:
            return self._runner(argv)
        except subprocess.TimeoutExpired as exc:
            msg = f"ipmitool {' '.join(args)} timed out after {_TIMEOUT_S}s"
            raise IpmiError(msg) from exc

    def _raise_failed(
        self, args: Sequence[str], completed: subprocess.CompletedProcess[str]
    ) -> None:
        stderr = completed.stderr
        if self._settings.password:
            stderr = stderr.replace(self._settings.password, "[redacted]")
        first_line = stderr.strip().splitlines()[0] if stderr.strip() else ""
        msg = f"ipmitool {' '.join(args)} failed (rc={completed.returncode}): {first_line[:200]}"
        raise IpmiError(msg)

    def _run_read(self, args: Sequence[str]) -> str:
        """Query subcommands must produce parseable output; empty stdout is a failure."""
        completed = self._invoke(args)
        if completed.returncode != 0:
            self._raise_failed(args, completed)
        if not completed.stdout.strip():
            msg = f"ipmitool {' '.join(args)}: empty output"
            raise IpmiError(msg)
        return completed.stdout

    def _run_write(self, args: Sequence[str]) -> None:
        """Raw write subcommands report via exit code; empty stdout is success."""
        completed = self._invoke(args)
        if completed.returncode != 0:
            self._raise_failed(args, completed)

    def read_max_cpu_temp(self) -> TempC:
        output = self._run_read(["sdr", "type", "temperature"])
        temps = [int(m.group(1)) for m in _SDR_TEMP_RE.finditer(output)]
        if not temps:
            msg = "ipmitool sdr type temperature: no 0Eh/0Fh temperature rows found"
            raise IpmiError(msg)
        return TempC(max(temps))

    def read_fan_rpms(self) -> list[tuple[str, int]]:
        output = self._run_read(["sdr", "type", "fan"])
        rpms: list[tuple[str, int]] = []
        for line in output.splitlines():
            fields = [field.strip() for field in line.split("|")]
            if len(fields) < 2 or not fields[0]:
                continue
            match = _FAN_RPM_RE.fullmatch(fields[-1])
            if match is None:
                continue
            rpms.append((fields[0], int(match.group(1))))
        return rpms

    def enable_auto(self) -> None:
        self._run_write(_ENABLE_AUTO_CMD)

    def disable_auto(self) -> None:
        self._run_write(_DISABLE_AUTO_CMD)

    def set_manual_pct(self, pct: FanPercent) -> None:
        self._run_write([*_MANUAL_PCT_CMD_PREFIX, f"{pct:#04x}"])


__all__ = ["IpmiError", "Runner", "IpmiClient"]
