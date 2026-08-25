"""Speed policies: the feedback-driven seam for alternative control loops."""

from dataclasses import dataclass

from breezed.config import Settings
from breezed.curve import interpolate
from breezed.types import TempC


@dataclass(frozen=True, slots=True)
class CurvePolicy:
    """Shipped v1 strategy; satisfies SpeedPolicy STRUCTURALLY — never inherits."""

    def target_pct(self, temp_c: TempC, settings: Settings) -> int | None:
        return interpolate(settings.curve, temp_c)


__all__ = ["CurvePolicy"]
