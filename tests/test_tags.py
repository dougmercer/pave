import math

import pytest

from pave.tags import direction, flags, scalar, width_m


@pytest.mark.parametrize(
    "highway,expected",
    [
        ("motorway", 11),
        ("trunk", 9),
        ("primary", 7.5),
        ("secondary", 7),
        ("tertiary", 6.5),
        ("unclassified", 5.5),
        ("residential", 5.5),
        ("road", 5.5),
        ("service", 4),
        ("track", 3.5),
        ("living_street", 4),
        ("unknown", 4),
        ("motorway_link", 6.6),
        ("trunk_link", 5.4),
        ("primary_link", 4.5),
        ("secondary_link", 4.2),
        ("tertiary_link", 3.9),
    ],
)
def test_default_widths(highway, expected):
    assert width_m({"highway": highway}) == pytest.approx(expected)


@pytest.mark.parametrize(
    "tags,expected",
    [
        ({"width": "6.2", "lanes": "4", "service": "driveway"}, 6.2),
        ({"width": "0", "lanes": "2", "service": "driveway"}, 7),
        ({"width": "-2", "lanes": "0", "service": "driveway"}, 3),
        ({"width": "garbage", "lanes": "2"}, 7),
        ({"width": "nan", "lanes": "inf", "service": "parking_aisle"}, 5.5),
        ({"service": "alley"}, 3.5),
        ({"service": "other"}, 4),
        ({"width": "12 ft"}, 3.6576),
        ({"width": "3.6 m"}, 3.6),
        ({"lanes": "2;3"}, 4),
        ({"width": "3-5"}, 4),
    ],
)
def test_width_precedence(tags, expected):
    assert width_m({"highway": "service", **tags}) == pytest.approx(expected)


@pytest.mark.parametrize(
    "value,kind,expected",
    [
        ("30 mph", "speed", 48.28032),
        ("70 km/h", "speed", 70),
        ("0", "speed", 0),
        ("3.5", "weight", 3.5),
        ("3500 kg", "weight", 3.5),
        ("2 t", "weight", 2),
        ("2204.62262185 lbs", "weight", 1),
        ("2 st", "weight", 1.81436948),
        ("4.2 m", "length", 4.2),
        ("12'6\"", "length", 3.81),
        (" 4.2 ", "length", 4.2),
        ("420 cm", "length", 4.2),
    ],
)
def test_explicit_units(value, kind, expected):
    assert scalar(value, kind) == pytest.approx(expected)


@pytest.mark.parametrize(
    "value",
    [
        None,
        "none",
        "signals",
        "GB:nsl_single",
        "walk",
        "30;50",
        "50 @ (wet)",
        "-1",
        "inf",
        "nan",
        "",
        "5 bananas",
    ],
)
def test_no_inferred_numeric_values(value):
    assert math.isnan(scalar(value, "speed"))


@pytest.mark.parametrize(
    "tags,expected",
    [
        ({}, 0),
        ({"oneway": "yes"}, 1),
        ({"oneway": "true"}, 1),
        ({"oneway": "1"}, 1),
        ({"oneway": "-1"}, -1),
        ({"junction": "roundabout"}, 1),
        ({"junction": "circular"}, 1),
        ({"highway": "motorway"}, 1),
        ({"highway": "motorway_link"}, 1),
        ({"highway": "trunk"}, 0),
        ({"highway": "motorway", "oneway": "no"}, 0),
        ({"junction": "roundabout", "oneway": "no"}, 0),
        ({"highway": "motorway", "oneway": "-1"}, -1),
        ({"highway": "motorway", "oneway": "false"}, 0),
        ({"highway": "motorway", "oneway": "reversible"}, 0),
        ({"highway": "motorway_link", "oneway": "alternating"}, 0),
        ({"junction": "roundabout", "oneway": "reversible"}, 0),
    ],
)
def test_direction(tags, expected):
    assert direction(tags) == expected


@pytest.mark.parametrize(
    "tags,expected",
    [
        ({}, 0),
        ({"bridge": "no"}, 4),
        ({"bridge": "yes"}, 5),
        ({"tunnel": "no"}, 8),
        ({"tunnel": "culvert"}, 10),
        ({"bridge": "no", "tunnel": "yes"}, 14),
        ({"bridge": "false", "tunnel": "0"}, 15),
        ({"bridge": ""}, 5),
    ],
)
def test_boolean_values_and_nulls(tags, expected):
    assert flags(tags) == expected
