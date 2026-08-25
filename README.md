# breezed

Curve-based fan controller for Dell PowerEdge servers via iDRAC/IPMI.
Python 3.13 · uv · ruff · ty.

breezed replaces a single static fan speed with a multi-point temperature
curve and linear interpolation between points, so homelab racks stay quiet at
idle and cool under load. Above the top curve point it hands control back to
iDRAC's own automatic thermal management — the safety net is always one degree
away. Logs are JSON on stdout; a Prometheus-format metrics socket is opt-in;
adapters (IPMI transport) and speed policies (curve today, PID later) sit
behind small Protocols, so either can be swapped without touching the core.

## Install

```sh
uv tool install .
breezed validate /etc/breezed/breezed.toml --probe   # reads one live temp, touches no fans
```

## CLI examples

```sh
breezed run --config /etc/breezed/breezed.toml --metrics-port 9762 -v  # foreground daemon
breezed set 20            # one-shot 20% duty cycle
breezed auto              # one-shot back to iDRAC automatic control
breezed status --config /etc/breezed/breezed.toml   # snapshot: temps, RPMs, curve target
breezed validate breezed.toml --probe               # check config; probe reads live temp
```

The `daemon` subcommands manage the systemd deployment:

```sh
sudo breezed daemon install --start   # idempotent install/upgrade of the unit
breezed daemon status                 # unit state + installed-vs-running version drift
sudo breezed daemon uninstall         # disable/remove the unit; keeps user/env/config
```

Exit codes: `0` ok, `1` runtime error (e.g. IPMI failure), `2` usage/config error.
Machine-readable output is always JSON on stdout; errors go to stderr as text.

## Deploy (systemd)

systemd is the only deployment path — no containers. One command installs or
upgrades everything:

```sh
uv tool install .
sudo breezed daemon install --start
sudo $EDITOR /etc/breezed.env      # fill IDRAC_HOST/IDRAC_USER/IDRAC_PASSWORD
sudo systemctl restart breezed     # pick up the credentials you just wrote
journalctl -u breezed -f           # watch the first hour
```

`daemon install` is idempotent and safe to re-run at any time. It creates the
dedicated `breezed` system user, renders `/etc/systemd/system/breezed.service`
from a template shipped inside breezed itself, writes an empty skeleton for
`/etc/breezed.env` (`root:breezed`, mode `0640`) only if absent — it never
overwrites an existing env file or config — and copies
`deploy/breezed.toml.example` to `/etc/breezed/breezed.toml` only if that file
does not exist yet.

**Upgrading**: after `uv tool upgrade breezed` (or reinstalling from source),
run `sudo breezed daemon install` again — this re-renders the unit so its
`ExecStart` points at the new binary shim — then `sudo systemctl restart
breezed`. Local edits to `/etc/breezed/breezed.toml` survive upgrades (the
config hot-reloads on mtime change anyway). `breezed daemon status` shows the
version stamped into the installed unit next to the version of the running
binary, so drift between "unit installed by" and "binary running as" is
visible at a glance; if they disagree, re-run `daemon install`.

The metrics port is baked into the unit's `ExecStart` deliberately (netdata
scrapes it). To run without metrics, edit the unit to drop
`--metrics-port 9762` and `systemctl daemon-reload && systemctl restart
breezed`; note your edit will be overwritten by the next `daemon install`.

## Config example

`/etc/breezed/breezed.toml`:

```toml
[settings]
host = "169.254.0.1"
poll_interval_s = 10
read_failure_limit = 3
step_down_hysteresis_s = 30
# metrics_port = 9762

[[curve]]
temp_c = 45
fan_pct = 6

[[curve]]
temp_c = 60
fan_pct = 8

[[curve]]
temp_c = 68
fan_pct = 12

[[curve]]
temp_c = 74
fan_pct = 18
```

Secrets never go in the config file: `IDRAC_HOST`, `IDRAC_USER`, and
`IDRAC_PASSWORD` come from the environment (`/etc/breezed.env`); host and user
may also live in the config, but environment variables win. The config
hot-reloads when its mtime changes; an invalid reload keeps the last good
config.

## Metrics (netdata)

With the default unit, breezed serves Prometheus text format on
`127.0.0.1:9762`. Scrape URL for netdata's `prometheus` collector:
`http://127.0.0.1:9762/metrics`. Metric names: `breezed_temp_c`,
`breezed_fan_percent`, `breezed_mode`, `breezed_ipmi_errors_total`,
`breezed_polls_total`.

## Safety

Manually controlling server fans means you are the thermal management system:
a bad curve can cook components or leave a server roaring at 100%. breezed
mitigates this by deferring to iDRAC automatic control above the curve top and
forcing AUTO again after repeated sensor-read failures — but you chose the
curve points, so choose them carefully. If anything looks wrong,
`breezed auto` hands control back to the iDRAC instantly.

## Replacing the legacy C# fan-controller on mmsrv

Stop and disable the old JDMallen.IPMITempMonitor compose stack **before**
starting breezed:

```sh
docker compose down && docker compose rm   # in the legacy project directory
```

Two controllers fighting over the same iDRAC flip the fan mode back and forth
on every poll. Never run both at once.

## License

[MIT](LICENSE)
