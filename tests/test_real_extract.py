"""Independent pyosmium oracle on the vendored, real Monaco PBF (test-only RAM use)."""

from collections import Counter
from pathlib import Path

import numpy as np
import osmium

from pave import iter_bbox_tiles, read_bbox
from pave.preprocess import preprocess

DRIVABLE = {
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
    "living_street",
}


def test_real_monaco_against_independent_parser(tmp_path):
    class Source(osmium.SimpleHandler):
        def __init__(self):
            super().__init__()
            self.nodes = {}
            self.barriers = {}
            self.ways = []

        def node(self, node):
            self.nodes[node.id] = (round(node.lon * 1e7), round(node.lat * 1e7))
            if "barrier" in node.tags:
                self.barriers[node.id] = node.tags["barrier"]

        def way(self, way):
            if way.tags.get("highway") in DRIVABLE:
                self.ways.append((way.id, [node.ref for node in way.nodes], dict(way.tags)))

    source = Source()
    path = Path(__file__).parent / "data" / "monaco.osm.pbf"
    source.apply_file(path)
    incidence = Counter(node for _, refs, _ in source.ways for node in set(refs))
    junctions = {node for node, count in incidence.items() if count >= 2}
    junctions.update(node for _, refs, _ in source.ways if refs for node in (refs[0], refs[-1]))
    expected = Counter()
    for way_id, refs, tags in source.ways:
        oneway = tags.get("oneway", "").strip().lower()
        if oneway in ("yes", "true", "1", "-1"):
            directions = [-1] if oneway == "-1" else [1]
        elif oneway in ("no", "false", "0"):
            directions = [1, -1]
        elif tags.get("junction") in ("roundabout", "circular") or tags["highway"] in (
            "motorway",
            "motorway_link",
        ):
            directions = [1]
        else:
            directions = [1, -1]
        start = 0
        for end in range(1, len(refs)):
            if refs[end] not in junctions:
                continue
            for step in directions:
                nodes = refs[start : end + 1][::step]
                interior = [source.barriers[n] for n in nodes[1:-1] if n in source.barriers]
                expected[
                    (
                        way_id,
                        nodes[0],
                        nodes[-1],
                        tags["highway"],
                        tuple(source.nodes[node] for node in nodes),
                        ";".join(interior) if interior else None,
                        source.barriers.get(nodes[-1]),
                    )
                ] += 1
            start = end
    out = tmp_path / "monaco"
    stats = preprocess(path, out)
    result = read_bbox(out, -180, -90, 180, 90)
    actual = Counter()
    for tile in iter_bbox_tiles(out, -180, -90, 180, 90):
        for i, way_id in enumerate(tile["osmid"]):
            coords = tile["vertices"][tile["offsets"][i] : tile["offsets"][i + 1]]
            radians = np.radians(coords)
            half = np.diff(radians, axis=0) / 2
            cosines = np.cos(radians[:-1, 1]) * np.cos(radians[1:, 1])
            chord = np.sin(half[:, 1]) ** 2 + cosines * np.sin(half[:, 0]) ** 2
            length = (2 * 6371008.8 * np.arcsin(np.sqrt(chord))).sum()
            assert np.isclose(tile["length_m"][i], length, rtol=1e-5, atol=1e-3)
            integer_coords = tuple(map(tuple, np.rint(coords * 1e7).astype(np.int64)))
            actual[
                (
                    int(way_id),
                    int(tile["u"][i]),
                    int(tile["v"][i]),
                    result["highway_enum"][int(tile["highway"][i])],
                    integer_coords,
                    result["dictionaries"]["barrier"][int(tile["barrier"][i])],
                    result["dictionaries"]["barrier_at_v"][int(tile["barrier_at_v"][i])],
                )
            ] += 1
    assert actual == expected
    assert source.barriers  # Otherwise the comparison proves nothing about barriers.
    assert stats["implementation"] == "rust"
    assert stats["edges"] == 2354
    assert stats["source"]["timestamp"] == "2026-11-01T21:21:36Z"
