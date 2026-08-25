# breezed

Curve-based fan controller for Dell PowerEdge servers via iDRAC/IPMI.
Python 3.13 · uv · ruff · ty.

Quiet homelab servers with a proper fan curve instead of one static speed —
with iDRAC's own thermal control kept as the safety net above the curve.

```toml
# breezed.toml
[[curve]]
temp_c = 45
fan_pct = 6
```

See [SPEC.md](SPEC.md) for full behavior, acceptance criteria, and tickets.

## Status

T1 scaffold & toolchain complete; T2 domain types & curve engine complete — CI gates (pre-commit, pytest) green.

## License

[MIT](LICENSE)
