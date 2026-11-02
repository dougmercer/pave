"""Read only selected tiles. No per-edge Python objects or pickle columns."""

import json
import math
from collections.abc import Iterator, Sequence
from dataclasses import dataclass
from pathlib import Path

import numpy as np

from .schema import COLUMNS, FORMAT, VERSION


@dataclass(frozen=True)
class StringDictionary(Sequence):
    """A memory-mapped UTF-8 dictionary; -1 is null, not Python's last-item index."""

    data: np.ndarray
    offsets: np.ndarray

    def __len__(self) -> int:
        return len(self.offsets) - 1

    def __getitem__(self, code: int | slice):
        if isinstance(code, slice):
            return [self[i] for i in range(*code.indices(len(self)))]
        if code == -1:
            return None
        if code < 0 or code >= len(self):
            raise IndexError(code)
        return self.data[self.offsets[code] : self.offsets[code + 1]].tobytes().decode("utf-8")


def _select(outdir, west, south, east, north):
    values = (west, south, east, north)
    if (
        not all(math.isfinite(value) for value in values)
        or not -90 <= south <= north <= 90
        or not (-180 <= west <= 180 and -180 <= east <= 180)
    ):
        raise ValueError(
            "Invalid WGS84 bbox: require longitudes [-180,180] and -90 <= south <= north <= 90"
        )
    root = Path(outdir)
    with (root / "manifest.json").open(encoding="utf-8") as file:
        manifest = json.load(file)
    if manifest.get("format") != FORMAT or manifest.get("version") != VERSION:
        raise ValueError("Unsupported road graph format or version")
    longitude_ranges = [(west, east)] if west <= east else [(west, 180), (-180, east)]
    selected = []
    for tile in manifest["tiles"]:
        tw, ts, te, tn = tile["bbox"]
        if ts <= north and tn >= south and any(tw <= e and te >= w for w, e in longitude_ranges):
            selected.append(tile)
    return root, manifest, selected


def _load(path: Path) -> np.ndarray:
    return np.load(path, mmap_mode="r", allow_pickle=False)


def _tile(root: Path, tile: dict) -> dict[str, np.ndarray]:
    directory = root / tile["path"]
    return {name: _load(directory / f"{name}.npy") for name in ("vertices", "offsets", *COLUMNS)}


def _dictionaries(root: Path, manifest: dict) -> dict[str, StringDictionary]:
    return {
        name: StringDictionary(_load(root / table["data"]), _load(root / table["offsets"]))
        for name, table in manifest["string_dictionaries"].items()
    }


def iter_bbox_tiles(
    outdir: str | Path, west: float, south: float, east: float, north: float
) -> Iterator[dict[str, np.ndarray]]:
    """Yield zero-copy, read-only memmaps per intersecting tile; strings are global codes.

    Bboxes are inclusive. west > east crosses the antimeridian. Tile envelopes
    include all edge vertices; complete tiles are returned, without clipping.
    """
    root, _, tiles = _select(outdir, west, south, east, north)
    for tile in tiles:
        yield _tile(root, tile)


def read_bbox(outdir: str | Path, west: float, south: float, east: float, north: float) -> dict:
    """Return concatenated columns for intersecting tiles, with rebased offsets.

    String columns contain global int32 codes. Decode via result['dictionaries'][name][code].
    Highway codes and boolean bits are defined in result['highway_enum'] and ['flag_bits'].
    A one-tile result keeps its read-only memmaps. A multi-tile result allocates
    contiguous arrays exactly once and maps/copies one input column at a time.
    """
    root, manifest, tiles = _select(outdir, west, south, east, north)
    if len(tiles) == 1:
        result = _tile(root, tiles[0])
    else:
        edge_count = sum(tile["edges"] for tile in tiles)
        vertex_count = sum(tile["vertices"] for tile in tiles)
        result = {name: np.empty(edge_count, dtype=dtype) for name, dtype in COLUMNS.items()}
        result["vertices"] = np.empty((vertex_count, 2), dtype="<f8")
        result["offsets"] = np.empty(edge_count + 1, dtype="<i8")
        result["offsets"][0] = 0
        edge_start = vertex_start = 0
        for tile in tiles:
            directory = root / tile["path"]
            edge_stop, vertex_stop = edge_start + tile["edges"], vertex_start + tile["vertices"]
            for name in COLUMNS:
                result[name][edge_start:edge_stop] = _load(directory / f"{name}.npy")
            result["vertices"][vertex_start:vertex_stop] = _load(directory / "vertices.npy")
            result["offsets"][edge_start + 1 : edge_stop + 1] = (
                _load(directory / "offsets.npy")[1:] + vertex_start
            )
            edge_start, vertex_start = edge_stop, vertex_stop
    result.update(
        {
            "dictionaries": _dictionaries(root, manifest),
            "highway_enum": {int(code): value for code, value in manifest["highway_enum"].items()},
            "flag_bits": manifest["flag_bits"],
            "tile_ids": [tile["id"] for tile in tiles],
        }
    )
    return result
