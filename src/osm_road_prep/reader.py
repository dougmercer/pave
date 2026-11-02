"""Read only selected tiles. No per-edge Python objects or pickle columns."""

import json
import math
from collections.abc import Iterable, Iterator, Sequence
from dataclasses import dataclass
from functools import lru_cache
from pathlib import Path

import numpy as np

from .schema import COLUMNS, FORMAT, VERSION

DTYPES = {"vertices": "<f8", "offsets": "<i8", **COLUMNS}
ALL_COLUMNS = tuple(DTYPES)


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


@dataclass(frozen=True, eq=False)
class _Dataset:
    """An immutable published dataset: its manifest, tile envelopes, and dictionaries."""

    root: Path
    manifest: dict
    tiles: list[dict]
    bounds: np.ndarray
    dictionaries: dict[str, StringDictionary]
    highway_enum: dict[int, str]


def _load(path: Path) -> np.ndarray:
    return np.load(path, mmap_mode="r", allow_pickle=False)


@lru_cache(maxsize=8)
def _open_published(root: Path, _stamp: tuple[int, int]) -> _Dataset:
    with (root / "manifest.json").open(encoding="utf-8") as file:
        manifest = json.load(file)
    if manifest.get("format") != FORMAT or manifest.get("version") != VERSION:
        raise ValueError("Unsupported road graph format or version")
    tiles = manifest["tiles"]
    return _Dataset(
        root,
        manifest,
        tiles,
        np.array([tile["bbox"] for tile in tiles], dtype=float).reshape(-1, 4),
        {
            name: StringDictionary(_load(root / table["data"]), _load(root / table["offsets"]))
            for name, table in manifest["string_dictionaries"].items()
        },
        {int(code): value for code, value in manifest["highway_enum"].items()},
    )


def _open(outdir: str | Path) -> _Dataset:
    # Datasets are published by rename, so a replacement has a new inode and mtime.
    root = Path(outdir)
    status = (root / "manifest.json").stat()
    return _open_published(root, (status.st_ino, status.st_mtime_ns))


def _select(dataset: _Dataset, west, south, east, north) -> list[dict]:
    values = (west, south, east, north)
    if (
        not all(math.isfinite(value) for value in values)
        or not -90 <= south <= north <= 90
        or not (-180 <= west <= 180 and -180 <= east <= 180)
    ):
        raise ValueError(
            "Invalid WGS84 bbox: require longitudes [-180,180] and -90 <= south <= north <= 90"
        )
    tile_west, tile_south, tile_east, tile_north = dataset.bounds.T
    hits = (tile_south <= north) & (tile_north >= south)
    if west <= east:
        hits &= (tile_west <= east) & (tile_east >= west)
    else:
        hits &= (tile_east >= west) | (tile_west <= east)
    return [dataset.tiles[index] for index in np.flatnonzero(hits)]


def _names(columns: Iterable[str] | None) -> tuple[str, ...]:
    if columns is None:
        return ALL_COLUMNS
    names = set(columns)
    if unknown := names.difference(ALL_COLUMNS):
        raise ValueError(f"Unknown columns: {sorted(unknown)}; choose from {ALL_COLUMNS}")
    if "vertices" in names:
        names.add("offsets")  # Vertices cannot be split into edges without their offsets.
    return tuple(name for name in ALL_COLUMNS if name in names)


def _tile(root: Path, tile: dict, names: tuple[str, ...]) -> dict[str, np.ndarray]:
    directory = root / tile["path"]
    return {name: _load(directory / f"{name}.npy") for name in names}


def _iter_tiles(root, tiles, names) -> Iterator[dict[str, np.ndarray]]:
    for tile in tiles:
        yield _tile(root, tile, names)


def iter_bbox_tiles(
    outdir: str | Path,
    west: float,
    south: float,
    east: float,
    north: float,
    columns: Iterable[str] | None = None,
) -> Iterator[dict[str, np.ndarray]]:
    """Yield zero-copy, read-only memmaps per intersecting tile; strings are global codes.

    Bboxes are inclusive. west > east crosses the antimeridian. Tile envelopes
    include all edge vertices; complete tiles are returned, without clipping.
    `columns` limits which columns are opened, as in `read_bbox`. Each mapped
    column holds a file descriptor until released, so process tiles one at a time.
    """
    dataset = _open(outdir)
    tiles = _select(dataset, west, south, east, north)
    return _iter_tiles(dataset.root, tiles, _names(columns))


def read_bbox(
    outdir: str | Path,
    west: float,
    south: float,
    east: float,
    north: float,
    columns: Iterable[str] | None = None,
) -> dict:
    """Return concatenated columns for intersecting tiles, with rebased offsets.

    String columns contain global int32 codes. Decode via result['dictionaries'][name][code].
    Highway codes and boolean bits are defined in result['highway_enum'] and ['flag_bits'].
    A one-tile result keeps its read-only memmaps. A multi-tile result allocates
    contiguous arrays exactly once and maps/copies one input column at a time.
    `columns` names the columns to return, avoiding work on any others; requesting
    `vertices` also returns `offsets`. By default every column is returned.
    The dataset's metadata is cached between calls and refreshed if it is replaced.
    """
    dataset = _open(outdir)
    tiles = _select(dataset, west, south, east, north)
    names = _names(columns)
    if len(tiles) == 1:
        result = _tile(dataset.root, tiles[0], names)
    else:
        edge_count = sum(tile["edges"] for tile in tiles)
        vertex_count = sum(tile["vertices"] for tile in tiles)
        shapes = {"vertices": (vertex_count, 2), "offsets": edge_count + 1}
        result = {name: np.empty(shapes.get(name, edge_count), DTYPES[name]) for name in names}
        if "offsets" in result:
            result["offsets"][0] = 0
        edge_start = vertex_start = 0
        for tile in tiles:
            directory = dataset.root / tile["path"]
            edge_stop, vertex_stop = edge_start + tile["edges"], vertex_start + tile["vertices"]
            for name in names:
                source = _load(directory / f"{name}.npy")
                if name == "vertices":
                    result[name][vertex_start:vertex_stop] = source
                elif name == "offsets":
                    result[name][edge_start + 1 : edge_stop + 1] = source[1:] + vertex_start
                else:
                    result[name][edge_start:edge_stop] = source
            edge_start, vertex_start = edge_stop, vertex_stop
    result.update(
        {
            "dictionaries": dict(dataset.dictionaries),
            "highway_enum": dict(dataset.highway_enum),
            "flag_bits": dict(dataset.manifest["flag_bits"]),
            "tile_ids": [tile["id"] for tile in tiles],
        }
    )
    return result
