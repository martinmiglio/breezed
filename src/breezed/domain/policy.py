"""Speed policies: the feedback-driven seam for alternative control loops."""

from dataclasses import dataclass

from breezed.domain.curve import interpolate
from breezed.domain.settings import Settings
from breezed.domain.types import TempC


@dataclass(frozen=True, slots=True)
class CurvePolicy:
    """Curve-following strategy; satisfies SpeedPolicy structurally — never inherits."""

    def target_pct(self, temp_c: TempC, settings: Settings) -> int | None:
        return interpolate(settings.curve, temp_c)


__all__ = ["CurvePolicy"]
