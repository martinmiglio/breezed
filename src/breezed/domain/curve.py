"""Fan curve model: validated points and linear interpolation."""

from bisect import bisect_left
from collections.abc import Sequence
from dataclasses import dataclass
from operator import attrgetter

from breezed.domain.types import FanPercent, TempC


@dataclass(frozen=True, slots=True)
class CurvePoint:
    temp_c: TempC
    fan_pct: FanPercent


_TEMP_C = attrgetter("temp_c")


def validate_curve(points: Sequence[CurvePoint]) -> tuple[CurvePoint, ...]:
    """Empty -> ValueError; any adjacent pair with temp_c[i] >= temp_c[i+1] -> ValueError.

    Returns a normalized tuple copy.
    """
    if not points:
        msg = "curve must contain at least one point"
        raise ValueError(msg)
    normalized = tuple(points)
    for i, (a, b) in enumerate(zip(normalized, normalized[1:], strict=False)):
        if a.temp_c >= b.temp_c:
            msg = (
                f"curve temp_c must be strictly ascending: "
                f"points[{i}].temp_c={a.temp_c} >= points[{i + 1}].temp_c={b.temp_c}"
            )
            raise ValueError(msg)
    return normalized


def interpolate(curve: Sequence[CurvePoint], temp_c: TempC) -> int | None:
    """At-or-above top point -> None (AUTO signal); below first -> first fan_pct.

    Raises ValueError for an empty curve.

    Otherwise linear interpolation between bracketing points, rounded with built-in
    round() (banker's rounding — never int() truncation, which would bias every
    fractional result downward).
    """
    if not curve:
        msg = "curve must contain at least one point"
        raise ValueError(msg)
    if temp_c >= curve[-1].temp_c:
        return None
    if temp_c <= curve[0].temp_c:
        return curve[0].fan_pct
    # Settings stores validated curves as tuples, so bisection avoids scanning
    # every segment on each daemon poll while preserving the same interval rule.
    index = bisect_left(curve, temp_c, key=_TEMP_C)
    if index == 0 or index == len(curve):
        raise ValueError("curve not strictly ascending")
    a = curve[index - 1]
    b = curve[index]
    if a.temp_c >= b.temp_c:
        raise ValueError("curve not strictly ascending")
    span = b.temp_c - a.temp_c
    frac = (temp_c - a.temp_c) / span
    return round(a.fan_pct + (b.fan_pct - a.fan_pct) * frac)


__all__ = ["CurvePoint", "interpolate", "validate_curve"]
