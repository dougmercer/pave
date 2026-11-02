# osm-road-prep

A Maturin/PyO3 package that converts an OpenStreetMap `.osm.pbf` snapshot into a
tiled directed road graph. **The entire preprocessing hot path runs in Rust**:
PBF decoding, disk indexes, junction detection, tag conversion, edge construction,
dictionary encoding, and `.npy` writing. Python supplies the CLI, atomic output
publication, and the NumPy reader. The native call releases the GIL.

## Install and run

Requires Python 3.11+, a Rust toolchain, and macOS or Linux. SQLite is compiled
into the extension. Pyosmium is only a development dependency for generating
fixtures and independently checking real PBF data.

```sh
uv sync
uv run prep input.osm.pbf outdir/
```

The default build profile is **release**, including when `uv sync` builds the
editable Maturin package. Alternatively, `pip install .` builds the package and
installs the `prep` command. After editing Rust code, rebuild with:

```sh
uv run maturin develop --release
```

Options:

```sh
prep input.osm.pbf outdir/ --tile-degrees 0.25 --workdir /fast-disk/scratch \
    --cache-mb 32 --threads 2
```

`--threads` controls the number of parallel PBF decoders and bounds the number
of decoded blocks in flight. `--cache-mb` controls the SQLite page cache.
`--quiet` suppresses stderr progress. Stdout is a JSON performance report, also
saved as `outdir/stats.json`.

Input must be a snapshot with **unique, ascending node IDs and way IDs**, as in
normal Geofabrik extracts. History files are rejected. Incomplete references on
drivable ways cause a descriptive failure: no partial graph is published.
Missing references on excluded ways are irrelevant. Output must be absent or
empty; existing datasets are never overwritten. Scratch files are removed on
success and ordinary exceptions, and the complete output is published by rename
within its destination filesystem.

## Read

```python
from osm_road_prep import read_bbox

roads = read_bbox("outdir", west=6.0, south=49.5, east=6.3, north=49.8)
i = 0
lonlat = roads["vertices"][roads["offsets"][i]:roads["offsets"][i + 1]]
u, v = roads["u"][i], roads["v"][i]
highway = roads["highway_enum"][int(roads["highway"][i])]
name = roads["dictionaries"]["name"][int(roads["name"][i])]  # str or None
```

`read_bbox` returns a dictionary of concatenated NumPy columns plus
`dictionaries`, `highway_enum`, `flag_bits`, and `tile_ids`. Every string column
contains global integer codes, so concatenation never needs to remap codes or
create Python objects for each edge. Dictionaries expose memory-mapped UTF-8
bytes and offsets; decoding a specific code is optional.

Only selected tile files are opened. A single-tile result consists of read-only
memory maps. Multiple tiles require one allocation per concatenated column;
offsets are rebased to the concatenated vertices. For bounded-memory or zero-copy
consumption, use `iter_bbox_tiles(outdir, west, south, east, north)`, which yields
each tile's read-only column memory maps separately. Direct access also works:

```python
import numpy as np
u = np.load("outdir/tiles/744_558/u.npy", mmap_mode="r", allow_pickle=False)
```

The example tile ID is illustrative; actual paths are in `manifest.json`.
Empty results have `vertices.shape == (0, 2)`, `offsets == [0]`, and empty edge
columns with the normal dtypes. Bounding boxes use inclusive intersection;
`west > east` represents an antimeridian-crossing query. Results contain **whole
intersecting tiles**, without clipping or exact per-edge filtering.

## Version 1 storage contract

All arrays are little-endian, C-contiguous, uncompressed `.npy` files. No object
arrays and no pickle are used. An edge's `lonlat` is the slice of `vertices`
between consecutive offsets, always ordered from `u` to `v`.

| File within each tile | Dtype and shape | Meaning |
| --- | --- | --- |
| `vertices.npy` | float64 `(V, 2)` | WGS84 `[lon, lat]`, all original shape vertices |
| `offsets.npy` | int64 `(E + 1,)` | Starts at 0, ends at V; each edge has at least two vertices |
| `u.npy`, `v.npy` | int64 `(E,)` | Global OSM junction node IDs |
| `osmid.npy` | int64 `(E,)` | Original OSM way ID |
| `width_m.npy` | float64 `(E,)` | Positive full carriageway width, same in both directions |
| `highway.npy` | uint8 `(E,)` | Code in manifest `highway_enum` |
| `maxspeed_kmh.npy`, `maxweight_t.npy`, `maxheight_m.npy` | float32 `(E,)` | Explicit converted limits; NaN when missing or not parseable |
| `flags.npy` | uint8 `(E,)` | Boolean values and presence bits |
| `access.npy`, `motor_vehicle.npy`, `hgv.npy`, `surface.npy`, `name.npy` | int32 `(E,)` | Global dictionary codes; `-1` means null |

`flags` bits are `bridge=1`, `tunnel=2`, `bridge_present=4`, and
`tunnel_present=8`. Presence bits distinguish an absent tag from explicit `no`.
For a present tag, only the exact value `no` is false; all other values are true.
For example, bridge absent = 0, bridge `no` = 4, bridge `yes` = 5.

Each string dictionary has `dictionaries/<field>.data.npy` (uint8 UTF-8 bytes)
and `dictionaries/<field>.offsets.npy` (int64, dictionary length + 1).
Code `c >= 0` selects the bytes between offsets `c` and `c + 1`.
Empty strings are real values, distinct from null. No strings are normalized.
The manifest provides dictionary paths, encoding, counts, and the null code.

The highway enum is stable in version 1:

```text
0 motorway          5 unclassified       10 motorway_link
1 trunk             6 residential        11 trunk_link
2 primary           7 service            12 primary_link
3 secondary         8 track              13 secondary_link
4 tertiary          9 road               14 tertiary_link
```

The grid origin is `[-180, -90]`. Cell coordinates are
`floor((lon + 180) / tile_degrees)` and `floor((lat + 90) / tile_degrees)`, with
the positive world limits assigned to the last cell. Tile IDs are `x_y`.
Each directed edge is stored exactly once, in the cell containing its **first
vertex**. A reverse edge can therefore be stored in a different tile.
Junction IDs are never made local to a tile.

The manifest lists each tile's ID, path, edge/vertex counts, `grid_bbox`, and
`bbox`, all using `[west, south, east, north]`. **`bbox` encloses every vertex
stored in the tile**, including edges reaching outside its grid cell. The reader
uses this envelope so crossing roads are discoverable from their destination or
interior. Envelopes can overlap, but edges are not duplicated. Dateline-crossing
edges conservatively widen the tile's longitude envelope to `[-180, 180]`.

Other manifest entries include the format/version, dtypes, CRS, enum tables,
flag bits, and `source_pbf_timestamp`. The timestamp comes from the PBF's
replication header, not file modification time. It is JSON null if the source
does not supply a timestamp. `source` records the filename, size, and generator.

## Graph and tag semantics

Only the fifteen highway classes in the enum are included. Access tags do not
filter roads. A junction is any endpoint of a drivable way or any node referenced
by at least two distinct drivable ways. Repeated references within a single way
count once. Shape nodes remain in the polyline. Closed roads retain their closing
vertex and can produce self-loops. Nothing is projected, simplified, interpolated,
or split merely because it crosses a tile boundary.

Explicit `oneway=yes/true/1` emits forward edges; `-1` emits reversed edges.
`no/false/0` explicitly permits both directions. Otherwise `junction=roundabout`
or `circular`, `highway=motorway`, and `motorway_link` imply forward-only travel.
Other roads emit both directions with reversed vertex order for the second.
Turn restrictions and all other relations are ignored.

Width precedence is a positive `width`, then positive `lanes × 3.5`, then service
subtype (`driveway=3`, `parking_aisle=5.5`, `alley=3.5`), then class defaults:
motorway 11, trunk 9, primary 7.5, secondary 7, tertiary 6.5, unclassified /
residential / road 5.5, service 4, track 3.5, otherwise 4 metres. Link class
defaults are their base default × 0.6. Width is not divided by direction.

Explicit numeric tags accept unambiguous decimal/scientific quantities. Speed
accepts bare km/h, `km/h`, `kmh`, `kph`, or `mph` (× 1.609344). Length accepts
bare metres, `m`, metre/meter plurals, `cm`, `ft`, `feet`, `in`, and feet/inches
such as `12'6"`. Weight accepts bare tonnes, `t`, `tonne(s)`, `kg`, `lb(s)`,
`st` (short tons), and `lt` (long tons). Negative, nonfinite, symbolic,
conditional, and compound values such as `signals`, `none`, `30;50`, and
`50 @ (wet)` become NaN in numeric columns. Values overflowing float32 also
become NaN. The package never computes a speed or a vehicle permission.

## Memory and temporary disk

The first pass writes a sorted sparse node-location file (24 bytes per input
node) and counts drivable-way incidence in SQLite. It exports only junction IDs
to another sorted disk file (8 bytes per junction). The second pass memory-maps
these immutable indexes and reads only PBF blocks containing ways, whose file
offsets were recorded in pass one. Sparse indexes scale with actual node count,
not the maximum OSM ID.

The native process keeps bounded PBF batches, one way at a time, a 32 MiB SQLite
cache, at most 32 tile spool pairs, an 8,192-entry string cache, and 32,768 rows
per column-conversion chunk. Tile metadata and global dictionaries remain in
SQLite; even the manifest tile list is streamed. No complete extract, topology
hash map, or tile's complete geometry is accumulated in application RAM.
Memory-mapped index pages are demand-paged by the OS, so observed RSS includes
resident file-backed pages and is not a strict `--cache-mb` cap.

Budget scratch disk for the location index, topology DB, junction IDs, edge
spools, and output. Geometry spools use 16 bytes per directed vertex and row
spools use 74 bytes per edge. Final edge columns use 74 bytes per edge plus
16 bytes per directed vertex, dictionaries, and small `.npy` headers. Indexes
and spools are temporary; tile spools are removed as columns are finalized.

## Tests and benchmarks

```sh
uv run pytest -q
uv run ruff check src tests benchmarks
PYO3_PYTHON=.venv/bin/python cargo clippy --all-targets -- -D warnings
uv run maturin build --release --out dist
```

Tests construct binary PBF fixtures, exercise cache eviction and column chunk
boundaries, compare randomized graphs with an independent oracle, and check
every directed edge and vertex in a vendored **real Monaco extract** using an
independent pyosmium parser. Provenance and OSM attribution are recorded in
[tests/data/SOURCE.md](tests/data/SOURCE.md).

Run a reproducible benchmark on any local real extract:

```sh
uv run python benchmarks/run.py input.osm.pbf --report benchmarks/result.json
```

The script launches three fresh CLI processes, reports directed edges per
second and peak RSS, and records the source checksum, timestamp, machine,
compiler, settings, and each run. It measures full preprocessing and separately
records CLI wall time including Python imports. Download and compilation time
are excluded. OS file caches are not flushed; writes are flushed to the OS,
not forced to physical media with `fsync`.

Measured results are in [benchmarks/RESULTS.md](benchmarks/RESULTS.md), with raw
records in [benchmarks/luxembourg.json](benchmarks/luxembourg.json). These are
regional-extract measurements, not a claim of planet-scale benchmark performance.
