"""SPEC curve test cases 1-8, one test function per case."""

from collections.abc import Sequence

import pytest

from breezed.curve import CurvePoint, interpolate, validate_curve
from breezed.types import FanPercent, TempC


def make_curve(*points: tuple[int, int]) -> Sequence[CurvePoint]:
    return [CurvePoint(TempC(t), FanPercent(p)) for t, p in points]


DEFAULT_CURVE = [
    CurvePoint(TempC(45), FanPercent(6)),
    CurvePoint(TempC(60), FanPercent(8)),
    CurvePoint(TempC(68), FanPercent(12)),
    CurvePoint(TempC(74), FanPercent(18)),
]


def test_below_first_point_clamps():
    curve = validate_curve(DEFAULT_CURVE)
    assert interpolate(curve, TempC(30)) == 6


def test_exactly_on_point_returns_that_points_pct():
    curve = validate_curve(DEFAULT_CURVE)
    assert interpolate(curve, TempC(60)) == 8
    assert interpolate(curve, TempC(45)) == 6


@pytest.mark.parametrize(
    ("curve", "temp", "expected"),
    [
        pytest.param(DEFAULT_CURVE, TempC(52), 7, id="quarter"),
        pytest.param(DEFAULT_CURVE, TempC(64), 10, id="midpoint"),
        pytest.param(
            make_curve((0, 7), (10, 8)),
            TempC(5),
            round(7.5),
            id="half_step_rounds_to_even",
        ),
    ],
)
def test_interpolates_linearly(curve: Sequence[CurvePoint], temp: TempC, expected: int):
    assert interpolate(validate_curve(curve), temp) == expected


def test_at_or_above_top_returns_none():
    curve = validate_curve(DEFAULT_CURVE)
    assert interpolate(curve, TempC(74)) is None
    assert interpolate(curve, TempC(80)) is None


def test_empty_curve_raises_value_error():
    with pytest.raises(ValueError, match="curve must contain at least one point"):
        interpolate([], TempC(50))


def test_single_point_curve():
    curve = validate_curve([CurvePoint(TempC(50), FanPercent(10))])
    assert interpolate(curve, TempC(40)) == 10
    assert interpolate(curve, TempC(50)) is None
    assert interpolate(curve, TempC(60)) is None


def test_non_monotonic_curve_rejected_by_validator():
    descending = make_curve((45, 6), (60, 8), (55, 12))
    with pytest.raises(ValueError, match="strictly ascending"):
        validate_curve(descending)
    equal_adjacent = make_curve((45, 6), (60, 8), (60, 12))
    with pytest.raises(ValueError, match="strictly ascending"):
        validate_curve(equal_adjacent)


def test_validate_curve_returns_normalized_tuple_copy():
    source = make_curve((45, 6), (60, 8))
    result = validate_curve(source)
    assert isinstance(result, tuple)
    assert result == tuple(source)
