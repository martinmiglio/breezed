# breezed

Curve-based fan controller for Dell PowerEdge servers via iDRAC/IPMI.
Python 3.13 · uv · ruff · ty.

Source: [github.com/martinmiglio/breezed](https://github.com/martinmiglio/breezed)

breezed is a curve-based fan controller for Dell PowerEdge servers, driving
iDRAC/IPMI with a multi-point temperature curve and linear interpolation between
points so homelab racks stay quiet at idle and cool under load. Above the top curve point it hands control back to
iDRAC's own automatic thermal management — the safety net is always one degree
away. Logs are JSON on stdout; a Prometheus-format metrics socket is opt-in;
adapters (IPMI transport) and speed policies (curve today, PID later) sit
behind small Protocols, so either can be swapped without touching the core.

## Install

From PyPI:

```sh
pip install breezed
# or
uv tool install breezed
```

To deploy as a systemd service (the `breezed` binary needs privileges to talk to
iDRAC; `daemon install` stages the unit and re-execs under sudo once), run one
command and enter your password once:

```sh
breezed daemon install
```

`daemon install` stages the unit/env/config, then re-executes itself via sudo to
perform the fixed privileged steps (create the service user, install the runtime
under `/opt`, write the systemd unit, enable and start the service). Pass
`--dry-run` to print the steps without escalating. The user-level `uv` only
invokes breezed from the checkout; it is not copied into the system installation
and is not a service dependency.

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
breezed daemon install         # one command, one sudo prompt
breezed daemon install --dry-run      # print the privileged steps without escalating
breezed daemon status                 # report unit state and binary version
breezed daemon uninstall              # one command, one sudo prompt; keeps env/config
```

Exit codes: `0` ok, `1` runtime error (e.g. IPMI failure), `2` usage/config error.
Machine-readable output is always JSON on stdout; errors go to stderr as text.

## Deploy (systemd)

systemd is the only deployment path — no containers. `daemon install` stages
`breezed.service`, `breezed.env`, and `breezed.toml` into a private temp
directory, prints the privileged steps, then re-executes itself once via sudo
to perform them. uv installs and manages the runtime directly under `/opt`:

```sh
breezed daemon install
# one sudo password prompt; then:
sudoedit /etc/breezed.env
journalctl -u breezed -f
```

`daemon install --dry-run` prints the same steps without escalating, for review.
The sudo re-exec runs only a fixed, code-reviewed sequence — no free-form
commands, no shell — using the absolute path of the running binary, so it is
immune to sudo's `secure_path`.

The runtime step sets `UV_TOOL_DIR=/opt/breezed`,
`UV_TOOL_BIN_DIR=/usr/local/bin`, and
`UV_PYTHON_INSTALL_DIR=/opt/breezed-python`, then installs the current checkout
with `--reinstall`. The sequence creates the non-login `breezed` system user and
installs files with explicit ownership and permissions:

- `/etc/systemd/system/breezed.service`: `root:root`, mode `0644`
- `/etc/breezed/breezed.toml`: invoking user and group, mode `0664`
- `/etc/breezed.env`: `root:breezed`, mode `0640`

The config and environment install steps run on first installation only and are
skipped when those files already exist, so upgrades never overwrite tuned config
or secrets.

**Upgrading**: update the checkout and run `breezed daemon install` again.
The uv `tool install --reinstall` step replaces the runtime in `/opt`; the config
and environment steps are skipped because those files already exist. `breezed
daemon uninstall` mirrors install: one sudo prompt to disable and remove the
service and runtime, while `/etc/breezed/` and `/etc/breezed.env` remain.

The metrics port is baked into the unit's `ExecStart` deliberately (netdata
scrapes it). To run without metrics, edit the unit to drop
`--metrics-port 9762` and `systemctl daemon-reload && systemctl restart
breezed`; note your edit will be overwritten by the next `daemon install`.

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
