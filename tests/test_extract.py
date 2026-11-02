import itertools
import json
import math
import random
import subprocess
import sys
import tempfile
from pathlib import Path

import numpy as np
import osmium
import pytest

from osm_road_prep import iter_bbox_tiles, read_bbox
from osm_road_prep.preprocess import preprocess
from osm_road_prep.schema import COLUMNS, HIGHWAYS


def edge_rows(columns, way_id):
    return [
        (
            int(columns["u"][i]),
            int(columns["v"][i]),
            columns["vertices"][columns["offsets"][i] : columns["offsets"][i + 1]],
        )
        for i in np.flatnonzero(columns["osmid"] == way_id)
    ]


def test_junctions_shapes_tags_and_cross_tile_ids(pbf, tmp_path):
    nodes = {
        1: (0.1, 0.1),
        2: (0.2, 0.12),
        3: (0.4, 0.1),
        4: (0.6, 0.14),
        5: (0.8, 0.1),
        6: (0.4, -0.1),
        7: (0.4, 0.3),
        8: (0.2, 0.3),
        9: (0.9, 0.1),
    }
    tags = {
        "highway": "residential",
        "width": "6.1",
        "lanes": "4",
        "access": "private",
        "motor_vehicle": "destination",
        "hgv": "delivery",
        "surface": "asphalt",
        "name": "Rue de l’Été 🚗",
        "maxspeed": "30 mph",
        "maxweight": "7.5",
        "maxheight": "12'6\"",
        "bridge": "no",
        "tunnel": "building_passage",
    }
    path = pbf(
        nodes,
        [
            (100, [1, 2, 3, 4, 5], tags),
            (101, [6, 3, 7], {"highway": "primary", "oneway": "-1"}),
            (102, [8, 2], {"highway": "footway"}),
            (103, [5, 9], {"highway": "service", "access": "no", "name": ""}),
        ],
    )
    out = tmp_path / "roads"
    report = preprocess(path, out)
    data = read_bbox(out, -180, -90, 180, 90)
    assert report["edges"] == 8
    expected = {(1, 3): [1, 2, 3], (3, 1): [3, 2, 1], (3, 5): [3, 4, 5], (5, 3): [5, 4, 3]}
    for u, v, geometry in edge_rows(data, 100):
        np.testing.assert_array_equal(geometry, [nodes[node] for node in expected[(u, v)]])
    assert {(u, v) for u, v, _ in edge_rows(data, 101)} == {(3, 6), (7, 3)}
    assert len(edge_rows(data, 102)) == 0
    assert len(edge_rows(data, 103)) == 2  # Access restrictions remain raw; never filter them.
    assert set(data["u"]) | set(data["v"]) == {1, 3, 5, 6, 7, 9}
    for i in np.flatnonzero(data["osmid"] == 100):
        assert data["width_m"][i] == 6.1
        assert data["maxspeed_kmh"][i] == pytest.approx(48.28032)
        assert data["maxweight_t"][i] == 7.5
        assert data["maxheight_m"][i] == pytest.approx(3.81)
        assert data["flags"][i] == 14
        for column in ("access", "motor_vehicle", "hgv", "surface", "name"):
            assert data["dictionaries"][column][data[column][i]] == tags[column]
    missing = data["osmid"] == 101
    assert np.isnan(data["maxspeed_kmh"][missing]).all()
    assert (data["name"][missing] == -1).all()
    assert (data["flags"][missing] == 0).all()
    assert data["dictionaries"]["name"][-1] is None
    assert all(
        data["dictionaries"]["name"][code] == "" for code in data["name"][data["osmid"] == 103]
    )
    manifest = json.loads((out / "manifest.json").read_text())
    assert manifest["source_pbf_timestamp"] == "2026-01-02T03:04:05Z"
    assert manifest["highway_enum"][str(int(data["highway"][0]))] in HIGHWAYS
    assert len(manifest["tiles"]) > 1
    assert data["offsets"].shape == (9,)
    assert data["offsets"][-1] == len(data["vertices"])
    for column, dtype in COLUMNS.items():
        assert data[column].dtype == np.dtype(dtype)
    assert data["vertices"].dtype == np.dtype("float64")
    assert data["vertices"].shape[1] == 2
    # Selection uses geometry envelopes, even if the start lies in a different cell.
    selected = read_bbox(out, 0.595, 0.135, 0.605, 0.145)
    assert {(3, 5), (5, 3)} <= {(u, v) for u, v, _ in edge_rows(selected, 100)}
    for tile in iter_bbox_tiles(out, -180, -90, 180, 90):
        mapped = (value for name, value in tile.items() if name != "offsets")
        assert all(isinstance(value, np.memmap) for value in mapped)
        assert all(not value.flags.writeable for value in tile.values())
        assert (np.diff(tile["offsets"]) >= 2).all()


@pytest.mark.parametrize("highway", HIGHWAYS)
def test_each_drivable_class(pbf, tmp_path, highway):
    path = pbf({1: (0, 0), 2: (0.01, 0)}, [(1, [1, 2], {"highway": highway})])
    out = tmp_path / "roads"
    preprocess(path, out)
    result = read_bbox(out, 0, 0, 0.1, 0.1)
    assert len(result["u"]) == (1 if highway in ("motorway", "motorway_link") else 2)
    assert all(result["highway_enum"][code] == highway for code in result["highway"])
    assert isinstance(result["vertices"], np.memmap)


@pytest.mark.parametrize(
    "extra,expected",
    [
        ({"oneway": "yes"}, {(1, 3)}),
        ({"oneway": "true"}, {(1, 3)}),
        ({"oneway": "1"}, {(1, 3)}),
        ({"oneway": "-1"}, {(3, 1)}),
        ({"junction": "circular"}, {(1, 3)}),
        ({"junction": "roundabout"}, {(1, 3)}),
        ({"highway": "motorway", "oneway": "no"}, {(1, 3), (3, 1)}),
        ({"highway": "motorway_link", "oneway": "no"}, {(1, 3), (3, 1)}),
    ],
)
def test_directed_geometry(pbf, tmp_path, extra, expected):
    nodes = {1: (0.01, 0), 2: (0.03, 0.01), 3: (0.05, 0)}
    path = pbf(nodes, [(1, [1, 2, 3], {"highway": "residential", **extra})])
    out = tmp_path / "roads"
    preprocess(path, out)
    rows = edge_rows(read_bbox(out, -180, -90, 180, 90), 1)
    assert {(u, v) for u, v, _ in rows} == expected
    for u, _, coords in rows:
        np.testing.assert_array_equal(
            coords, [nodes[n] for n in ([1, 2, 3] if u == 1 else [3, 2, 1])]
        )


def test_closed_and_self_touching_ways_count_distinct_ways(pbf, tmp_path):
    nodes = {i: (0.001 * i, 0) for i in range(1, 8)}
    path = pbf(
        nodes,
        [
            (1, [1, 2, 3, 2, 4], {"highway": "road"}),
            (2, [5, 6, 7, 5], {"highway": "road", "junction": "roundabout"}),
        ],
    )
    out = tmp_path / "roads"
    preprocess(path, out)
    data = read_bbox(out, -180, -90, 180, 90)
    assert len(data["u"]) == 3
    assert {(u, v) for u, v, _ in edge_rows(data, 1)} == {(1, 4), (4, 1)}
    assert [len(geom) for _, _, geom in edge_rows(data, 1)] == [5, 5]
    assert [(u, v, len(geom)) for u, v, geom in edge_rows(data, 2)] == [(5, 5, 4)]


def test_int64_long_ways_and_boundary_coordinates(pbf, tmp_path):
    base = 2**40
    nodes = {base + i: (179 + i / 1600, 90) for i in range(1601)}
    path = pbf(nodes, [(base + 10000, list(nodes), {"highway": "road"})])
    out = tmp_path / "roads"
    preprocess(path, out)
    result = read_bbox(out, 179, 89, 180, 90)
    assert set(zip(result["u"], result["v"], strict=True)) == {
        (base, base + 1600),
        (base + 1600, base),
    }
    assert (result["osmid"] == base + 10000).all()
    assert (np.diff(result["offsets"]) == 1601).all()


def test_antimeridian(pbf, tmp_path):
    path = pbf({1: (179.9, 1), 2: (-179.9, 1)}, [(1, [1, 2], {"highway": "road"})])
    out = tmp_path / "roads"
    preprocess(path, out)
    result = read_bbox(out, 179.95, 0.9, -179.95, 1.1)
    assert len(result["u"]) == 2
    assert len(set(result["tile_ids"])) == len(result["tile_ids"]) == 2
    assert len(read_bbox(out, 179.98, 0.9, 180, 1.1)["u"]) == 2


def test_missing_nodes_are_fatal_and_output_is_atomic(pbf, tmp_path):
    path = pbf({1: (0, 0)}, [(1, [1, 2], {"highway": "road"})])
    out = tmp_path / "roads"
    with pytest.raises(ValueError, match="missing node"):
        preprocess(path, out)
    assert not out.exists()
    assert not list(tmp_path.glob(".osm-road-output-*"))


@pytest.mark.parametrize("missing", [3, 6, 10])
def test_missing_nodes_are_named_wherever_they_fall_among_the_ids(pbf, tmp_path, missing):
    path = pbf({5: (0, 0), 7: (0.01, 0), 9: (0.02, 0)}, [(1, [5, missing, 9], {"highway": "road"})])
    with pytest.raises(ValueError, match=f"way 1 references missing node {missing}"):
        preprocess(path, tmp_path / "roads")


def test_missing_node_on_a_one_node_way_leaves_other_coordinates_alone(pbf, tmp_path):
    nodes = {1: (0.01, 0), 3: (0.03, 0), 5: (0.05, 0)}
    ways = [(1, [2], {"highway": "road"}), (2, [1, 3, 5], {"highway": "road"})]
    out = tmp_path / "roads"
    preprocess(pbf(nodes, ways), out)
    data = read_bbox(out, -180, -90, 180, 90)
    assert len(data["u"]) == 2
    np.testing.assert_allclose(
        edge_rows(data, 2)[0][2], [nodes[1], nodes[3], nodes[5]], rtol=0, atol=1e-14
    )


@pytest.mark.parametrize(
    "ids", [(-7, -2, 4), (1, 2**33, 2**33 + 5), (2**40, 2**40 + 4096, 2**40 + 2**33)]
)
def test_sparse_and_negative_node_ids(pbf, tmp_path, ids):
    nodes = {node: (i * 0.01, 0) for i, node in enumerate(ids)}
    out = tmp_path / "roads"
    preprocess(pbf(nodes, [(1, list(ids), {"highway": "road"})]), out)
    data = read_bbox(out, -180, -90, 180, 90)
    assert set(zip(data["u"].tolist(), data["v"].tolist(), strict=True)) == {
        (ids[0], ids[-1]),
        (ids[-1], ids[0]),
    }
    np.testing.assert_allclose(
        edge_rows(data, 1)[0][2], [nodes[node] for node in ids], rtol=0, atol=1e-14
    )


def test_absurdly_wide_node_ids_are_refused(pbf, tmp_path):
    path = pbf({1: (0, 0), 2**41: (0.01, 0)}, [(1, [1, 2**41], {"highway": "road"})])
    with pytest.raises(ValueError, match="too wide"):
        preprocess(path, tmp_path / "roads")


def test_invalid_coordinates_are_fatal(pbf, tmp_path):
    osmium_location = osmium.osm.Location(200, 0)
    path = pbf({1: (0, 0), 2: osmium_location}, [(1, [1, 2], {"highway": "road"})])
    with pytest.raises(ValueError, match="invalid WGS84 coordinates at node 2"):
        preprocess(path, tmp_path / "roads")


@pytest.mark.parametrize(
    "options", [",pbf_dense_nodes=false", ",pbf_compression=none", ",add_metadata=false"]
)
def test_other_pbf_encodings_give_the_same_dataset(pbf, tmp_path, options):
    nodes = {1: (0.01, 0.02), 2: (0.03, 0.04), 3: (0.05, 0.06)}
    ways = [(1, [1, 2, 3], {"highway": "road", "name": "Rua é"})]
    node_tags = {2: {"barrier": "gate", "maxheight": "3"}}
    plain = tmp_path / "plain"
    preprocess(pbf(nodes, ways, node_tags=node_tags), plain)
    other = tmp_path / "other"
    preprocess(pbf(nodes, ways, node_tags=node_tags, options=options), other)
    plain_files, other_files = dataset_files(plain), dataset_files(other)
    del plain_files[Path("manifest.json")], other_files[Path("manifest.json")]  # Names the source.
    assert plain_files == other_files
    assert read_bbox(other, -180, -90, 180, 90)["dictionaries"]["name"][0] == "Rua é"


@pytest.mark.parametrize(
    "damage",
    [
        lambda data: data[:-10],
        lambda data: data[:3],
        lambda data: data[:40] + b"\xff" * 40 + data[80:],
        lambda data: data[:-40] + b"\xff" * 20 + data[-20:],
        lambda data: b"\xff" * len(data),
    ],
)
def test_damaged_files_fail_cleanly(pbf, tmp_path, damage):
    nodes = {i: (i * 0.001, 0) for i in range(1, 200)}
    path = pbf(nodes, [(1, list(nodes), {"highway": "road", "name": "A road"})])
    path.write_bytes(damage(path.read_bytes()))
    out = tmp_path / "roads"
    with pytest.raises(ValueError):
        preprocess(path, out)
    assert not out.exists()


def test_empty_graph_missing_timestamp_and_ignored_relations(pbf, tmp_path):
    path = pbf(
        {1: (0, 0)},
        [(1, [1, 2], {"highway": "footway"})],
        timestamp=None,
        relations=[osmium.osm.mutable.Relation(id=1, tags={"type": "restriction"})],
    )
    out = tmp_path / "roads"
    out.mkdir()
    report = preprocess(path, out)
    assert report["edges"] == 0
    assert report["source"]["timestamp"] is None
    result = read_bbox(out, -180, -90, 180, 90)
    assert result["vertices"].shape == (0, 2)
    np.testing.assert_array_equal(result["offsets"], [0])
    assert len(result["name"]) == 0
    assert all(len(dictionary) == 0 for dictionary in result["dictionaries"].values())


def test_empty_bbox_and_refuse_existing_dataset(pbf, tmp_path):
    path = pbf({1: (0, 0), 2: (0.1, 0.1)}, [(1, [1, 2], {"highway": "road"})])
    out = tmp_path / "roads"
    preprocess(path, out)
    original = (out / "manifest.json").read_bytes()
    with pytest.raises(FileExistsError):
        preprocess(path, out)
    assert (out / "manifest.json").read_bytes() == original
    result = read_bbox(out, 40, 40, 41, 41)
    assert result["vertices"].shape == (0, 2)
    np.testing.assert_array_equal(result["offsets"], [0])
    with pytest.raises(ValueError):
        read_bbox(out, 0, 91, 1, 92)


def test_cli(pbf, tmp_path):
    path = pbf({1: (0, 0), 2: (0.1, 0.1)}, [(1, [1, 2], {"highway": "road"})])
    command = [
        sys.executable,
        "-m",
        "osm_road_prep.cli",
        str(path),
        str(tmp_path / "roads"),
        "--quiet",
    ]
    run = subprocess.run(command, check=True, capture_output=True, text=True)
    assert json.loads(run.stdout)["edges"] == 2
    assert run.stderr == ""
    failed = subprocess.run(command, capture_output=True, text=True)
    assert failed.returncode == 1
    assert "absent or empty" in failed.stderr


def test_tile_handle_eviction_and_revisit(pbf, tmp_path):
    nodes = {}
    ways = []
    for i in range(45):
        nodes[2 * i + 1] = (i + 0.01, 0.01)
        nodes[2 * i + 2] = (i + 0.02, 0.01)
        ways.append((i + 1, [2 * i + 1, 2 * i + 2], {"highway": "road", "name": "shared"}))
    for i in range(45):
        ways.append((i + 100, [2 * i + 1, 2 * i + 2], {"highway": "road", "name": "revisit"}))
    out = tmp_path / "roads"
    preprocess(pbf(nodes, ways), out, threads=1)
    data = read_bbox(out, -180, -90, 180, 90)
    assert len(data["tile_ids"]) == 45
    assert len(data["u"]) == 180
    assert data["offsets"][-1] == 360
    for tile in iter_bbox_tiles(out, -180, -90, 180, 90):
        assert len(tile["u"]) == 4
        assert [data["dictionaries"]["name"][code] for code in tile["name"]] == [
            "shared",
            "shared",
            "revisit",
            "revisit",
        ]


def test_chunked_columns_and_repeated_dictionary_values(pbf, tmp_path):
    # Cross the native 32,768-row conversion boundary, revisiting every string.
    count = 33000
    path = pbf(
        {1: (0, 0), 2: (0.01, 0)},
        [
            (i + 1, [1, 2], {"highway": "road", "oneway": "yes", "name": f"road {i % 9000}"})
            for i in range(count)
        ],
    )
    out = tmp_path / "roads"
    preprocess(path, out)
    data = read_bbox(out, -180, -90, 180, 90)
    np.testing.assert_array_equal(data["offsets"], np.arange(count + 1) * 2)
    np.testing.assert_array_equal(data["osmid"], np.arange(1, count + 1))
    assert len(data["dictionaries"]["name"]) == 9000
    for i in [0, 8192, 8999, 9000, 32767, 32768, 32999]:
        assert data["dictionaries"]["name"][data["name"][i]] == f"road {i % 9000}"


def test_native_matches_independent_graph_oracle(pbf, tmp_path):
    from collections import Counter
    from random import Random

    rng = Random(71)
    nodes = {i: (i * 0.01, (i % 5) * 0.01) for i in range(1, 61)}
    ways = []
    for i in range(80):
        refs = rng.sample(list(nodes), rng.randint(2, 8))
        if i % 7 == 0:
            refs.append(refs[0])
        ways.append(
            (
                i + 1,
                refs,
                {
                    "highway": "footway" if i % 9 == 0 else "residential",
                    "oneway": rng.choice(["yes", "no", "-1"]),
                },
            )
        )
    drivable = [(wid, refs, tags) for wid, refs, tags in ways if tags["highway"] != "footway"]
    incidence = Counter(node for _, refs, _ in drivable for node in set(refs))
    junctions = {node for node, count in incidence.items() if count >= 2}
    junctions.update(node for _, refs, _ in drivable for node in (refs[0], refs[-1]))
    expected = []
    for wid, refs, tags in drivable:
        first = 0
        for end in range(1, len(refs)):
            if refs[end] in junctions:
                segment = refs[first : end + 1]
                if tags["oneway"] != "-1":
                    expected.append((wid, segment))
                if tags["oneway"] != "yes":
                    expected.append((wid, segment[::-1]))
                first = end
    out = tmp_path / "roads"
    preprocess(pbf(nodes, ways), out)
    data = read_bbox(out, -180, -90, 180, 90)
    actual = []
    for i, wid in enumerate(data["osmid"]):
        geom = data["vertices"][data["offsets"][i] : data["offsets"][i + 1]]
        refs = [round(lon * 100) for lon, _ in geom]
        assert (data["u"][i], data["v"][i]) == (refs[0], refs[-1])
        np.testing.assert_allclose(geom, [nodes[node] for node in refs], rtol=0, atol=1e-14)
        actual.append((int(wid), refs))
    assert sorted(actual) == sorted(expected)


def dataset_files(root):
    return {
        path.relative_to(root): path.read_bytes()
        for path in sorted(root.rglob("*"))
        if path.is_file() and path.name != "stats.json"
    }


def test_default_settings(pbf, tmp_path):
    path = pbf({1: (0, 0), 2: (0.1, 0.1)}, [(1, [1, 2], {"highway": "road"})])
    settings = preprocess(path, tmp_path / "roads")["settings"]
    assert settings["threads"] == 8
    assert settings["tile_degrees"] == 0.1


def test_cli_uses_the_same_default_tile_size(pbf, tmp_path):
    path = pbf({1: (0, 0), 2: (0.1, 0.1)}, [(1, [1, 2], {"highway": "road"})])
    command = [sys.executable, "-m", "osm_road_prep.cli", str(path), str(tmp_path / "roads")]
    run = subprocess.run([*command, "--quiet"], check=True, capture_output=True, text=True)
    assert json.loads(run.stdout)["settings"]["tile_degrees"] == 0.1


def test_small_buffers_spill_without_changing_output(pbf, tmp_path):
    # Both directions of 200,000 vertices overflow a 1 MiB tile buffer.
    count = 200_000
    nodes = {i + 1: ((i % 700) * 0.01, (i // 700) * 0.01) for i in range(count)}
    # Enough ways for several blocks, and shuffled IDs make them arrive out of spatial order,
    # so each block adds to most tiles and they are flushed repeatedly.
    ids = random.Random(11).sample(range(1, count), count // 5 - 1)
    ways = [
        (ids[i], [*range(i * 5 + 1, i * 5 + 7)], {"highway": "road", "name": f"w{i % 7}"})
        for i in range(len(ids))
    ]
    path = pbf(nodes, ways)
    small = preprocess(path, tmp_path / "small", spool_mb=1)
    large = preprocess(path, tmp_path / "large", spool_mb=64)
    assert small["buffers"]["spool_flushes"] > large["buffers"]["spool_flushes"]
    assert small["settings"]["spool_mb"] == 1
    assert dataset_files(tmp_path / "small") == dataset_files(tmp_path / "large")


@pytest.mark.parametrize("setting", ["spool_mb", "threads"])
def test_buffer_settings_must_be_positive(pbf, tmp_path, setting):
    path = pbf({1: (0, 0), 2: (0.1, 0.1)}, [(1, [1, 2], {"highway": "road"})])
    with pytest.raises(ValueError, match="positive"):
        preprocess(path, tmp_path / "roads", **{setting: 0})


def test_cli_passes_buffer_settings(pbf, tmp_path):
    path = pbf({1: (0, 0), 2: (0.1, 0.1)}, [(1, [1, 2], {"highway": "road"})])
    command = [
        sys.executable,
        "-m",
        "osm_road_prep.cli",
        str(path),
        str(tmp_path / "roads"),
        "--quiet",
        "--spool-mb",
        "5",
    ]
    run = subprocess.run(command, check=True, capture_output=True, text=True)
    assert json.loads(run.stdout)["settings"]["spool_mb"] == 5


def test_scratch_defaults_beside_the_output(pbf, tmp_path, monkeypatch):
    # The system temp directory can be RAM-backed, so it must not be used implicitly.
    path = pbf({1: (0, 0), 2: (0.1, 0.1)}, [(1, [1, 2], {"highway": "road"})])
    monkeypatch.setattr(tempfile, "tempdir", str(tmp_path / "no-such-directory"))
    assert preprocess(path, tmp_path / "roads")["edges"] == 2


def spherical_metres(points):
    """Independent haversine over consecutive [lon, lat] points."""
    radius = 6371008.8
    total = 0.0
    for (lon1, lat1), (lon2, lat2) in itertools.pairwise(points):
        phi1, phi2 = math.radians(lat1), math.radians(lat2)
        a = (
            math.sin((phi2 - phi1) / 2) ** 2
            + math.cos(phi1) * math.cos(phi2) * math.sin(math.radians(lon2 - lon1) / 2) ** 2
        )
        total += 2 * radius * math.asin(math.sqrt(a))
    return total


def test_length_sums_every_segment_in_both_directions(pbf, tmp_path):
    nodes = {1: (0.01, 50), 2: (0.02, 50.01), 3: (0.05, 50)}
    out = tmp_path / "roads"
    preprocess(pbf(nodes, [(1, [1, 2, 3], {"highway": "residential"})]), out)
    data = read_bbox(out, -180, -90, 180, 90)
    assert len(data["u"]) == 2
    expected = spherical_metres([nodes[1], nodes[2], nodes[3]])
    assert data["length_m"].dtype == np.dtype("<f4")
    np.testing.assert_allclose(data["length_m"], expected, rtol=1e-6)


def test_length_crosses_the_antimeridian_the_short_way(pbf, tmp_path):
    nodes = {1: (179.99, 0), 2: (-179.99, 0)}
    out = tmp_path / "roads"
    preprocess(pbf(nodes, [(1, [1, 2], {"highway": "road", "oneway": "yes"})]), out)
    data = read_bbox(out, -180, -90, 180, 90)
    np.testing.assert_allclose(data["length_m"], 2223.9, rtol=1e-3)


def directed(data, u, v):
    return int(np.flatnonzero((data["u"] == u) & (data["v"] == v))[0])


def test_extra_raw_tags_are_stored(pbf, tmp_path):
    nodes = {1: (0.01, 0), 2: (0.02, 0)}
    tags = {"highway": "road", "vehicle": "destination", "maxwidth": "2.2", "maxlength": "40 ft"}
    out = tmp_path / "roads"
    preprocess(pbf(nodes, [(1, [1, 2], tags)]), out)
    data = read_bbox(out, -180, -90, 180, 90)
    assert data["dictionaries"]["vehicle"][data["vehicle"][0]] == "destination"
    assert data["maxwidth_m"][0] == pytest.approx(2.2)
    assert data["maxlength_m"][0] == pytest.approx(12.192)


def test_absent_extra_tags_are_null(pbf, tmp_path):
    out = tmp_path / "roads"
    preprocess(pbf({1: (0.01, 0), 2: (0.02, 0)}, [(1, [1, 2], {"highway": "road"})]), out)
    data = read_bbox(out, -180, -90, 180, 90)
    assert (data["vehicle"] == -1).all()
    assert np.isnan(data["maxwidth_m"]).all() and np.isnan(data["maxlength_m"]).all()


@pytest.mark.parametrize(
    "tags,forward,backward",
    [
        ({"maxspeed:forward": "50", "maxspeed:backward": "30"}, 50, 30),
        ({"maxspeed": "70", "maxspeed:backward": "30"}, 70, 30),
        ({"maxspeed": "70", "maxspeed:forward": "50"}, 50, 70),
        ({"maxspeed": "70", "maxspeed:forward": "signals"}, 70, 70),
        ({"maxspeed:forward": "30 mph"}, 48.28032, None),
    ],
)
def test_directional_speed_limits_apply_per_direction(pbf, tmp_path, tags, forward, backward):
    nodes = {1: (0.01, 0), 2: (0.02, 0)}
    out = tmp_path / "roads"
    preprocess(pbf(nodes, [(1, [1, 2], {"highway": "road", **tags})]), out)
    data = read_bbox(out, -180, -90, 180, 90)
    assert data["maxspeed_kmh"][directed(data, 1, 2)] == pytest.approx(forward)
    reverse = data["maxspeed_kmh"][directed(data, 2, 1)]
    assert np.isnan(reverse) if backward is None else reverse == pytest.approx(backward)


def test_reversed_oneway_takes_the_backward_limit(pbf, tmp_path):
    nodes = {1: (0.01, 0), 2: (0.02, 0)}
    tags = {
        "highway": "road",
        "oneway": "-1",
        "maxspeed:forward": "50",
        "maxspeed:backward": "30",
    }
    out = tmp_path / "roads"
    preprocess(pbf(nodes, [(1, [1, 2], tags)]), out)
    data = read_bbox(out, -180, -90, 180, 90)
    assert (int(data["u"][0]), int(data["v"][0])) == (2, 1)
    assert data["maxspeed_kmh"][0] == 30


def text(data, column, index):
    return data["dictionaries"][column][int(data[column][index])]


def test_barrier_on_a_shape_point_blocks_both_directions_without_splitting(pbf, tmp_path):
    nodes = {1: (0.01, 0), 2: (0.02, 0), 3: (0.03, 0)}
    path = pbf(nodes, [(1, [1, 2, 3], {"highway": "road"})], node_tags={2: {"barrier": "bollard"}})
    out = tmp_path / "roads"
    preprocess(path, out)
    data = read_bbox(out, -180, -90, 180, 90)
    assert {(int(u), int(v)) for u, v in zip(data["u"], data["v"], strict=True)} == {
        (1, 3),
        (3, 1),
    }
    assert text(data, "barrier", 0) == text(data, "barrier", 1) == "bollard"
    assert (data["barrier_at_v"] == -1).all()


def test_barriers_on_one_edge_join_in_travel_order(pbf, tmp_path):
    nodes = {i: (i * 0.01, 0) for i in range(1, 5)}
    tags = {2: {"barrier": "gate"}, 3: {"barrier": "bollard"}}
    out = tmp_path / "roads"
    preprocess(pbf(nodes, [(1, [1, 2, 3, 4], {"highway": "road"})], node_tags=tags), out)
    data = read_bbox(out, -180, -90, 180, 90)
    assert text(data, "barrier", directed(data, 1, 4)) == "gate;bollard"
    assert text(data, "barrier", directed(data, 4, 1)) == "bollard;gate"


def test_barrier_at_a_junction_marks_only_edges_arriving_there(pbf, tmp_path):
    nodes = {1: (0.01, 0), 2: (0.02, 0), 3: (0.03, 0)}
    ways = [(1, [1, 2], {"highway": "road"}), (2, [2, 3], {"highway": "road"})]
    out = tmp_path / "roads"
    preprocess(pbf(nodes, ways, node_tags={2: {"barrier": "gate"}}), out)
    data = read_bbox(out, -180, -90, 180, 90)
    assert text(data, "barrier_at_v", directed(data, 1, 2)) == "gate"
    assert text(data, "barrier_at_v", directed(data, 3, 2)) == "gate"
    assert text(data, "barrier_at_v", directed(data, 2, 1)) is None
    assert text(data, "barrier_at_v", directed(data, 2, 3)) is None
    assert (data["barrier"] == -1).all()


def test_node_tags_off_the_road_network_are_ignored(pbf, tmp_path):
    nodes = {1: (0.01, 0), 2: (0.02, 0), 3: (0.03, 0), 4: (0.04, 0)}
    ways = [(1, [1, 2], {"highway": "road"}), (2, [3, 4], {"highway": "footway"})]
    tags = {3: {"barrier": "gate", "maxheight": "2"}, 4: {"barrier": "stile"}}
    out = tmp_path / "roads"
    preprocess(pbf(nodes, ways, node_tags=tags), out)
    data = read_bbox(out, -180, -90, 180, 90)
    assert len(data["dictionaries"]["barrier"]) == 0
    assert np.isnan(data["maxheight_m"]).all()


@pytest.mark.parametrize(
    "way,node,expected",
    [
        ({}, {"maxheight": "3"}, 3),
        ({"maxheight": "4"}, {"maxheight": "3"}, 3),
        ({"maxheight": "3"}, {"maxheight": "4"}, 3),
        ({"maxheight": "3"}, {"maxheight": "default"}, 3),
    ],
)
def test_limits_on_a_node_tighten_edges_through_it(pbf, tmp_path, way, node, expected):
    nodes = {1: (0.01, 0), 2: (0.02, 0), 3: (0.03, 0)}
    path = pbf(nodes, [(1, [1, 2, 3], {"highway": "road", **way})], node_tags={2: node})
    out = tmp_path / "roads"
    preprocess(path, out)
    data = read_bbox(out, -180, -90, 180, 90)
    np.testing.assert_allclose(data["maxheight_m"], expected)


def test_weight_and_width_on_a_node_tighten_their_own_columns(pbf, tmp_path):
    nodes = {1: (0.01, 0), 2: (0.02, 0), 3: (0.03, 0)}
    tags = {2: {"maxweight": "7.5", "maxwidth": "2"}}
    out = tmp_path / "roads"
    preprocess(pbf(nodes, [(1, [1, 2, 3], {"highway": "road"})], node_tags=tags), out)
    data = read_bbox(out, -180, -90, 180, 90)
    np.testing.assert_allclose(data["maxweight_t"], 7.5)
    np.testing.assert_allclose(data["maxwidth_m"], 2)
    assert np.isnan(data["maxheight_m"]).all()


def test_limit_at_a_junction_applies_only_to_edges_arriving_there(pbf, tmp_path):
    nodes = {1: (0.01, 0), 2: (0.02, 0), 3: (0.03, 0)}
    ways = [(1, [1, 2], {"highway": "road"}), (2, [2, 3], {"highway": "road"})]
    out = tmp_path / "roads"
    preprocess(pbf(nodes, ways, node_tags={2: {"maxheight": "3"}}), out)
    data = read_bbox(out, -180, -90, 180, 90)
    assert data["maxheight_m"][directed(data, 1, 2)] == 3
    assert np.isnan(data["maxheight_m"][directed(data, 2, 1)])
