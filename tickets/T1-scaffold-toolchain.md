# T1 — Scaffold & toolchain

## Goal

Finish the project scaffold so the full toolchain is green end-to-end: uv-managed Python 3.13
project with a `src/breezed` package skeleton, ruff + ty configured, pre-commit hooks wired,
and a minimal pytest proving `uv sync → pytest → ruff → ty → pre-commit` all pass.

## Depends on

(none)

## Current state

Most scaffolding already exists (commit `5ef1052 "spec, license, and toolchain scaffolding (pre-T1)"`). Verified:

- `pyproject.toml` — project metadata (breezed 0.1.0, MIT, Martin Miglio), `typer>=0.12`
  dependency, `[project.scripts] breezed = "breezed.cli:app"`, dev group with `pytest>=8`,
  hatchling build backend (`src/breezed` wheel target), ruff config (line-length 100,
  target py313, rules E/W/F/I/UP/B/SIM), `[tool.ty.src] include = ["src", "tests"]`.
- `.python-version` — contains `3.13`. ✔
- `.pre-commit-config.yaml` — pre-commit-hooks (trailing-whitespace, end-of-file-fixer,
  check-yaml, check-toml @ v6.0.0), ruff-pre-commit @ v0.15.14 (ruff-check --fix +
  ruff-format), ty-pre-commit @ v0.0.73.
- `LICENSE` — MIT, `Copyright (c) 2026 Martin Miglio <contact@martinmiglio.dev>`. ✔
- `README.md` — skeleton exists with short pitch, config teaser, SPEC link, status section.
- `src/breezed/__init__.py` — contains only `__version__ = "0.1.0"`; no docstring, no
  `__all__`, no `cli.py` yet (entry point references it ahead of T7).
- `tests/` — exists but is **empty** (no test file, no `__init__.py`).
- No `uv.lock` present yet.
- `.gitignore` covers `.venv/`, caches, `dist/`, etc.

## Tasks

1. Polish `src/breezed/__init__.py`: add a module docstring (one-liner from pyproject
   description), keep `__version__ = "0.1.0"`, add `__all__ = ["__version__"]`.
2. Review `pyproject.toml` against spec — it already satisfies requirements (typer dep,
   pytest dev group, ruff lint/format config, ty src include). Only touch if a gap shows up
   while running the verification commands. Do not add a `[tool.pytest.ini_options]`
   section unless needed to make discovery work.
3. Verify `.python-version` stays exactly `3.13`.
4. Verify `.pre-commit-config.yaml` revs are current: bump `ruff-pre-commit` rev so its
   bundled ruff matches what `uvx ruff` resolves to at implementation time; pin `ty-pre-commit`
   to the latest tagged release. Keep hook ids/args as-is (`ruff-check --fix`, `ruff-format`, `ty`).
5. Create `tests/test_version.py` with one test asserting `breezed.__version__` is a `str`
   (and non-empty). This wires pytest end-to-end.
6. Run `uv sync` to generate `uv.lock`; confirm lockfile is created and not gitignored.
7. Run the full verification command list below; fix anything they surface.
8. Update README "Status" line to note T1 complete once green.

## Acceptance criteria

- [ ] `src/breezed/__init__.py` exposes `__version__: str` with docstring + `__all__`
- [ ] `tests/test_version.py` passes and asserts `__version__` is a non-empty string
- [ ] `uv.lock` generated and committed (not in `.gitignore`)
- [ ] Pre-commit revs pinned to current releases for ruff-pre-commit and ty-pre-commit
- [ ] All of the following pass clean:
  - [ ] `uv sync`
  - [ ] `uv run pytest`
  - [ ] `uvx ruff check .`
  - [ ] `uvx ruff format --check .`
  - [ ] `uvx ty check`
  - [ ] `uvx pre-commit run --all-files`

## Notes

- **Always use `uvx` for ruff/ty/pre-commit** — the global `ruff` on this system is ancient
  (0.1.5) and will misreport/misformat. Never rely on PATH-installed tooling.
- **ty-pre-commit rev pinning**: the repo's tags track ty releases (`v0.0.x`); pin the latest
  tag explicitly — do not use branch or `main`. If `uvx ty check` and the pre-commit hook
  disagree on findings, the versions differ; align the revs.
- **Commit `uv.lock`** — this is an application, not a library; reproducible envs matter for
  the systemd/Container deployment in T8.
- The `[project.scripts]` entry point points at `breezed.cli:app` which doesn't exist until
  T7. That's fine: hatchling only fails on import when the script is *invoked*, not at
  install/sync time. Don't stub cli.py here.
- `check-yaml` in pre-commit-hooks will parse `.pre-commit-config.yaml` itself; keep it valid YAML.
- Keep ruff's rule set as configured (E/W/F/I/UP/B/SIM); expanding it is out of scope for T1.
