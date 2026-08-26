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

(`/etc/breezed/breezed.toml` is created by `daemon install`; before that,
validate a local copy of the example.)

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
breezed daemon install                # stage runtime+unit+config to /tmp, print the sudo commands
breezed daemon status                 # unit state + installed-vs-running version drift
breezed daemon uninstall              # print the commands; keeps user/env/config
```

Exit codes: `0` ok, `1` runtime error (e.g. IPMI failure), `2` usage/config error.
Machine-readable output is always JSON on stdout; errors go to stderr as text.

## Deploy (systemd)

systemd is the only deployment path — no containers. `daemon install` never
escalates itself: it stages everything into `/tmp/breezed-install/`, smoke-tests
the staged runtime, then prints a copy-paste block of plain sudo commands
(bash/zsh/fish safe):

```sh
uv tool install .
breezed daemon install
# review, then paste the printed commands:
sudo systemctl enable --now breezed
sudoedit /etc/breezed.env          # fill IDRAC_HOST/IDRAC_USER/IDRAC_PASSWORD first
sudo systemctl restart breezed     # pick up the credentials you just wrote
journalctl -u breezed -f           # watch the first hour
```

Re-running `daemon install` re-stages a fresh runtime; re-pasting the block
upgrades in place. The command list creates the dedicated `breezed` system
user, installs the unit rendered from the template shipped inside breezed,
writes an env skeleton for `/etc/breezed.env` only if absent — it never
overwrites an existing env file or config — and copies the packaged example
config (`breezed.toml.example`) to `/etc/breezed/breezed.toml` only if that
file does not exist yet.

**Upgrading**: after `uv tool upgrade breezed` (or reinstalling from source),
run `breezed daemon install` again and re-paste the printed block, then
`sudo systemctl restart breezed`. Local edits to `/etc/breezed/breezed.toml`
survive upgrades (the config hot-reloads on mtime change anyway). `breezed daemon status` shows the
version stamped into the installed unit next to the version of the running
binary, so drift between "unit installed by" and "binary running as" is
visible at a glance; if they disagree, re-run `daemon install`.

The metrics port is baked into the unit's `ExecStart` deliberately (netdata
scrapes it). To run without metrics, edit the unit to drop
`--metrics-port 9762` and `systemctl daemon-reload && systemctl restart
breezed`; note your edit will be overwritten by the next `daemon install`.

> **Migrating from the legacy C# controller on mmsrv?** Stop and disable the old
> JDMallen.IPMITempMonitor compose stack first — two controllers fighting over
> one iDRAC flip fan mode on every poll.

## Config example

A fully commented example ships inside the package: see `breezed.toml.example`
in the breezed templates directory (also copied to `/etc/breezed/breezed.toml`
on first install, never overwritten after that).

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

## License

[MIT](LICENSE)
