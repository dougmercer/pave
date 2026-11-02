"""The version 1 wire format. All on-disk columns are explicitly little endian."""

FORMAT = "osm-road-prep"
VERSION = 1
HIGHWAYS = (
    "motorway",
    "trunk",
    "primary",
    "secondary",
    "tertiary",
    "unclassified",
    "residential",
    "service",
    "track",
    "road",
    "motorway_link",
    "trunk_link",
    "primary_link",
    "secondary_link",
    "tertiary_link",
)
HIGHWAY_CODES = {value: code for code, value in enumerate(HIGHWAYS)}
STRINGS = ("access", "motor_vehicle", "hgv", "surface", "name")
NUMBERS = ("maxspeed_kmh", "maxweight_t", "maxheight_m")
FLAGS = {"bridge": 1, "tunnel": 2, "bridge_present": 4, "tunnel_present": 8}
COLUMNS = {
    "u": "<i8",
    "v": "<i8",
    "osmid": "<i8",
    "width_m": "<f8",
    "highway": "u1",
    **dict.fromkeys(NUMBERS, "<f4"),
    "flags": "u1",
    **dict.fromkeys(STRINGS, "<i4"),
}
