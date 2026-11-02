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
    columns: dict[str, np.ndarray]
    dictionaries: dict[str, StringDictionary]
    highway_enum: dict[int, str]


def _load(path: Path) -> np.ndarray:
    return np.load(path, mmap_mode="r", allow_pickle=False)


@lru_cache(maxsize=4)
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
        {name: _load(root / "columns" / f"{name}.npy") for name in ALL_COLUMNS},
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


def _runs(tiles: list[dict]) -> list[tuple[int, int, int, int]]:
    """Merge selected tiles into contiguous (edge, edge stop, vertex, vertex stop) ranges."""
    runs = []
    for tile in tiles:
        edge, vertex = tile["edge_start"], tile["vertex_start"]
        edge_stop, vertex_stop = edge + tile["edges"], vertex + tile["vertices"]
        if runs and runs[-1][1] == edge:
            runs[-1] = (runs[-1][0], edge_stop, runs[-1][2], vertex_stop)
        else:
            runs.append((edge, edge_stop, vertex, vertex_stop))
    return runs


def _read(dataset: _Dataset, runs: list[tuple[int, int, int, int]], name: str) -> np.ndarray:
    column = dataset.columns[name]
    if name == "offsets":
        parts, base = [np.zeros(1, column.dtype)], 0
        for edge, edge_stop, vertex, vertex_stop in runs:
            parts.append(column[edge + 1 : edge_stop + 1] - vertex + base)
            base += vertex_stop - vertex
        offsets = np.concatenate(parts)
        offsets.flags.writeable = False
        return offsets
    parts = [
        column[vertex:vertex_stop] if name == "vertices" else column[edge:edge_stop]
        for edge, edge_stop, vertex, vertex_stop in runs
    ]
    if len(parts) == 1:
        return parts[0]  # Still a read-only view of the mapped file.
    return np.concatenate(parts) if parts else column[:0]


def _iter_tiles(dataset, tiles, names) -> Iterator[dict[str, np.ndarray]]:
    for tile in tiles:
        runs = _runs([tile])
        yield {name: _read(dataset, runs, name) for name in names}


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
    `columns` limits which columns are yielded, as in `read_bbox`.
    """
    dataset = _open(outdir)
    tiles = _select(dataset, west, south, east, north)
    return _iter_tiles(dataset, tiles, _names(columns))


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
    Tiles adjacent on disk are returned as read-only memmaps, without copying;
    separated tiles are concatenated into new arrays. `offsets` is always computed.
    `columns` names the columns to return, avoiding work on any others; requesting
    `vertices` also returns `offsets`. By default every column is returned.
    The dataset's metadata is cached between calls and refreshed if it is replaced.
    """
    dataset = _open(outdir)
    tiles = _select(dataset, west, south, east, north)
    names = _names(columns)
    runs = _runs(tiles)
    result = {name: _read(dataset, runs, name) for name in names}
    result.update(
        {
            "dictionaries": dict(dataset.dictionaries),
            "highway_enum": dict(dataset.highway_enum),
            "flag_bits": dict(dataset.manifest["flag_bits"]),
            "tile_ids": [tile["id"] for tile in tiles],
        }
    )
    return result
