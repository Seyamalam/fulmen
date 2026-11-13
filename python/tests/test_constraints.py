"""Direct tests for the constraint/markers module (DESIGN.md §5.3).

These were previously exercised only through annotation compilation;
constructing and rejecting bad arguments directly pins the public
contract the spec builder consumes.
"""

from __future__ import annotations

import pytest
from fulmen.constraints import (
    UNSET,
    AfterValidator,
    Alias,
    Default,
    Deprecated,
    Description,
    Discriminator,
    Example,
    Field,
    Len,
    Lower,
    MultipleOf,
    Pattern,
    Range,
    Strip,
    Unique,
    Upper,
)


def test_field_rejects_default_and_factory_together() -> None:
    with pytest.raises(ValueError, match="both `default` and `default_factory`"):
        Field(1, default_factory=list)


def test_field_builds_a_spec_with_defaults() -> None:
    spec = Field(5, alias="wire", description="the number", ge=0, le=10, unique=True)
    assert spec.default == 5
    assert spec.default_factory is None
    assert spec.alias == "wire"
    assert spec.description == "the number"
    assert spec.ge == 0
    assert spec.le == 10
    assert spec.unique is True
    assert spec.example is UNSET


def test_field_factory_form() -> None:
    spec = Field(default_factory=list, deprecated=True, example=[1])
    assert spec.default is UNSET
    assert spec.default_factory is list
    assert spec.deprecated is True
    assert spec.example == [1]


def test_marker_construction() -> None:
    assert Alias("x").name == "x"
    assert Default(3).value == 3
    assert Description("d").text == "d"
    assert Example(9).value == 9
    assert Discriminator("kind").field == "kind"
    assert Len(min=1, max=2).min == 1
    assert Range(ge=0).ge == 0
    assert MultipleOf(3).value == 3
    assert Pattern("^a$").pattern == "^a$"
    assert Unique().enabled is True
    assert Deprecated() is not None
    assert Strip() is not None
    assert Lower() is not None
    assert Upper() is not None


def test_after_validator_holds_the_function() -> None:
    def double(v: int) -> int:
        return v * 2

    assert AfterValidator(double).fn is double
