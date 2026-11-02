import json
import random
import subprocess
import sys

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
        assert all(isinstance(value, np.memmap) for value in tile.values())
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
    preprocess(pbf(nodes, ways), out, cache_mb=1, threads=1)
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


def test_chunked_columns_and_dictionary_cache_eviction(pbf, tmp_path):
    # Cross the native 32,768-row conversion boundary and 8,192-value dictionary cache.
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


def test_threads_default_to_eight(pbf, tmp_path):
    path = pbf({1: (0, 0), 2: (0.1, 0.1)}, [(1, [1, 2], {"highway": "road"})])
    assert preprocess(path, tmp_path / "roads")["settings"]["threads"] == 8


def test_small_buffers_spill_without_changing_output(pbf, tmp_path):
    # 1 MiB holds 131,072 sort keys, so 200,000 references need several sorted runs,
    # and both directions of 200,000 vertices overflow a 1 MiB tile buffer.
    count = 200_000
    nodes = {i + 1: ((i % 700) * 0.01, (i // 700) * 0.01) for i in range(count)}
    # Shuffled IDs make ways arrive out of spatial order, so tiles are flushed repeatedly.
    ids = random.Random(11).sample(range(1, count), count // 50 - 1)
    ways = [
        (ids[i], [*range(i * 50 + 1, i * 50 + 52)], {"highway": "road", "name": f"w{i % 7}"})
        for i in range(len(ids))
    ]
    path = pbf(nodes, ways)
    small = preprocess(path, tmp_path / "small", sort_mb=1, spool_mb=1)
    large = preprocess(path, tmp_path / "large", sort_mb=64, spool_mb=64)
    assert small["buffers"]["sort_runs"] > 1
    assert small["buffers"]["spool_flushes"] > large["buffers"]["spool_flushes"]
    assert small["settings"]["sort_mb"] == 1
    assert small["settings"]["spool_mb"] == 1
    assert dataset_files(tmp_path / "small") == dataset_files(tmp_path / "large")


@pytest.mark.parametrize("setting", ["cache_mb", "sort_mb", "spool_mb", "threads"])
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
        "--sort-mb",
        "3",
        "--spool-mb",
        "5",
    ]
    run = subprocess.run(command, check=True, capture_output=True, text=True)
    settings = json.loads(run.stdout)["settings"]
    assert (settings["sort_mb"], settings["spool_mb"]) == (3, 5)
