import json
import random
import shutil
import subprocess
import sys

import numpy as np
import pytest

from osm_road_prep import iter_bbox_tiles, read_bbox
from osm_road_prep.preprocess import preprocess
from osm_road_prep.schema import COLUMNS


@pytest.fixture
def scattered(pbf, tmp_path):
    """A dataset spread across many tiles, including both sides of the antimeridian."""
    rng = random.Random(3)
    nodes, ways = {}, []
    for way in range(120):
        lon = rng.choice([rng.uniform(-179.9, -178), rng.uniform(178, 179.9), rng.uniform(-4, 4)])
        lat = rng.uniform(-3, 3)
        first, second = way * 2 + 1, way * 2 + 2
        nodes[first] = (lon, lat)
        nodes[second] = (lon + 0.02, lat + 0.02)
        ways.append((way + 1, [first, second], {"highway": "road", "name": f"road {way % 5}"}))
    out = tmp_path / "scattered"
    preprocess(pbf(nodes, ways), out)
    return out


def envelope_hits(tile, west, south, east, north):
    tw, ts, te, tn = tile["bbox"]
    ranges = [(west, east)] if west <= east else [(west, 180), (-180, east)]
    return ts <= north and tn >= south and any(tw <= e and te >= w for w, e in ranges)


def test_tile_selection_matches_envelope_rule_on_random_boxes(scattered):
    manifest = json.loads((scattered / "manifest.json").read_text())
    rng = random.Random(9)
    for _ in range(200):
        west, east = rng.uniform(-180, 180), rng.uniform(-180, 180)
        south, north = sorted((rng.uniform(-5, 5), rng.uniform(-5, 5)))
        expected = [
            tile["id"]
            for tile in manifest["tiles"]
            if envelope_hits(tile, west, south, east, north)
        ]
        result = read_bbox(scattered, west, south, east, north, columns=("osmid",))
        assert result["tile_ids"] == expected


def test_columns_returns_only_requested_edge_columns(scattered):
    result = read_bbox(scattered, -180, -90, 180, 90, columns=("u", "maxspeed_kmh"))
    assert set(result) & {"vertices", "offsets", *COLUMNS} == {"u", "maxspeed_kmh"}
    full = read_bbox(scattered, -180, -90, 180, 90)
    np.testing.assert_array_equal(result["u"], full["u"])
    assert result["u"].dtype == np.dtype(COLUMNS["u"])
    assert result["tile_ids"] == full["tile_ids"]


def test_requesting_vertices_includes_rebased_offsets(scattered):
    full = read_bbox(scattered, -180, -90, 180, 90)
    result = read_bbox(scattered, -180, -90, 180, 90, columns=("vertices",))
    assert set(result) & {"vertices", "offsets", *COLUMNS} == {"vertices", "offsets"}
    np.testing.assert_array_equal(result["vertices"], full["vertices"])
    np.testing.assert_array_equal(result["offsets"], full["offsets"])


def test_columns_narrow_a_single_tile_read(scattered):
    tile_id = read_bbox(scattered, -180, -90, 180, 90)["tile_ids"][0]
    manifest = json.loads((scattered / "manifest.json").read_text())
    west, south, east, north = next(t for t in manifest["tiles"] if t["id"] == tile_id)["bbox"]
    result = read_bbox(scattered, west, south, east, north, columns=("u",))
    assert set(result) & {"vertices", "offsets", *COLUMNS} == {"u"}


def test_iter_bbox_tiles_accepts_columns(scattered):
    tiles = iter_bbox_tiles(scattered, -180, -90, 180, 90, columns=("v",))
    assert all(set(tile) == {"v"} for tile in tiles)


@pytest.mark.parametrize("reader", [read_bbox, iter_bbox_tiles])
def test_unknown_column_is_rejected(scattered, reader):
    with pytest.raises(ValueError, match="nonexistent"):
        reader(scattered, -180, -90, 180, 90, columns=("nonexistent",))


def test_iter_bbox_tiles_rejects_bad_bbox_before_iteration(scattered):
    with pytest.raises(ValueError, match="Invalid WGS84 bbox"):
        iter_bbox_tiles(scattered, 0, 91, 1, 92)


def test_repeated_reads_share_opened_dictionaries(scattered):
    first = read_bbox(scattered, -180, -90, 180, 90)
    second = read_bbox(scattered, 0, 0, 1, 1)
    assert first["dictionaries"]["name"] is second["dictionaries"]["name"]


def test_callers_cannot_corrupt_shared_metadata(scattered):
    first = read_bbox(scattered, -180, -90, 180, 90)
    first["dictionaries"].clear()
    first["highway_enum"].clear()
    first["flag_bits"].clear()
    second = read_bbox(scattered, -180, -90, 180, 90)
    assert second["dictionaries"] and second["highway_enum"] and second["flag_bits"]


def test_replaced_dataset_is_not_served_from_cache(pbf, tmp_path):
    out = tmp_path / "roads"
    preprocess(pbf({1: (0, 0), 2: (0.1, 0)}, [(1, [1, 2], {"highway": "road"})]), out)
    assert set(read_bbox(out, -180, -90, 180, 90)["osmid"]) == {1}
    shutil.rmtree(out)
    preprocess(pbf({1: (0, 0), 2: (0.1, 0)}, [(7, [1, 2], {"highway": "road"})]), out)
    assert set(read_bbox(out, -180, -90, 180, 90)["osmid"]) == {7}


def test_file_count_does_not_grow_with_tiles(pbf, tmp_path):
    def grid(size):
        nodes = {i + 1: (i * 0.3, 0.1) for i in range(size)}
        ways = [(i + 1, [i + 1, i + 2], {"highway": "road"}) for i in range(size - 1)]
        return pbf(nodes, ways)

    preprocess(grid(4), tmp_path / "few")
    preprocess(grid(40), tmp_path / "many")
    assert len(read_bbox(tmp_path / "many", -180, -90, 180, 90)["tile_ids"]) > 30
    assert sum(1 for _ in (tmp_path / "few").rglob("*.npy")) == 26
    assert sum(1 for _ in (tmp_path / "many").rglob("*.npy")) == 26


def test_columns_are_directly_loadable_with_global_offsets(scattered):
    full = read_bbox(scattered, -180, -90, 180, 90)
    offsets = np.load(scattered / "columns" / "offsets.npy", mmap_mode="r", allow_pickle=False)
    vertices = np.load(scattered / "columns" / "vertices.npy", mmap_mode="r", allow_pickle=False)
    np.testing.assert_array_equal(offsets, full["offsets"])
    np.testing.assert_array_equal(vertices, full["vertices"])
    assert offsets[0] == 0 and offsets[-1] == len(vertices)


def test_manifest_locates_tiles_as_ranges(scattered):
    manifest = json.loads((scattered / "manifest.json").read_text())
    full = read_bbox(scattered, -180, -90, 180, 90)
    assert manifest["version"] == 2
    edge_start = vertex_start = 0
    for tile in manifest["tiles"]:
        assert "path" not in tile
        assert (tile["edge_start"], tile["vertex_start"]) == (edge_start, vertex_start)
        edge_start += tile["edges"]
        vertex_start += tile["vertices"]
    assert (edge_start, vertex_start) == (len(full["u"]), len(full["vertices"]))


def test_adjacent_tiles_are_read_without_copying(scattered):
    result = read_bbox(scattered, -180, -90, 180, 90)
    assert len(result["tile_ids"]) > 1
    for name in ("vertices", *COLUMNS):
        assert isinstance(result[name], np.memmap)
        assert not result[name].flags.writeable


def test_separated_tiles_concatenate_with_rebased_offsets(scattered):
    full = read_bbox(scattered, -180, -90, 180, 90)
    result = read_bbox(scattered, 170, -5, -170, 5)  # Both sides of the antimeridian only.
    assert 0 < len(result["u"]) < len(full["u"])
    assert result["offsets"][0] == 0 and result["offsets"][-1] == len(result["vertices"])
    for i in range(len(result["u"])):
        geometry = result["vertices"][result["offsets"][i] : result["offsets"][i + 1]]
        match = np.flatnonzero((full["u"] == result["u"][i]) & (full["v"] == result["v"][i]))
        expected = full["vertices"][full["offsets"][match[0]] : full["offsets"][match[0] + 1]]
        np.testing.assert_array_equal(geometry, expected)


def test_tile_offsets_start_at_zero_and_stay_read_only(scattered):
    for tile in iter_bbox_tiles(scattered, -180, -90, 180, 90):
        assert tile["offsets"][0] == 0 and tile["offsets"][-1] == len(tile["vertices"])
        assert not tile["offsets"].flags.writeable


def test_keeping_every_tile_needs_no_extra_file_descriptors(scattered):
    script = f"""
import resource
from osm_road_prep import iter_bbox_tiles
resource.setrlimit(resource.RLIMIT_NOFILE, (64, resource.getrlimit(resource.RLIMIT_NOFILE)[1]))
print(len(list(iter_bbox_tiles({str(scattered)!r}, -180, -90, 180, 90))))
"""
    run = subprocess.run([sys.executable, "-c", script], capture_output=True, text=True)
    assert run.returncode == 0, run.stderr
    assert int(run.stdout) > 10


def test_previous_format_version_is_rejected(scattered):
    path = scattered / "manifest.json"
    manifest = json.loads(path.read_text())
    manifest["version"] = 1
    path.write_text(json.dumps(manifest))
    with pytest.raises(ValueError, match="version"):
        read_bbox(scattered, -180, -90, 180, 90)
