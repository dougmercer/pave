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
    --threads 8 --cache-mb 32 --sort-mb 64 --spool-mb 64
```

Scratch files go beside the output unless `--workdir` names another disk.
`--threads` sets the workers that decode and transform PBF blocks, and bounds the
blocks in flight; one more thread reads the file and the calling thread writes
output. `--cache-mb` sizes the SQLite cache for string dictionaries. `--sort-mb`
limits each in-memory run when sorting node references, and `--spool-mb` limits
tile output buffered in RAM. Exceeding either spills to scratch and never changes
the output. The report's `buffers` section shows `sort_runs` and `spool_flushes`,
which reveal whether a larger setting would help. `--quiet` suppresses stderr
progress. Stdout is a JSON performance report, also saved as `outdir/stats.json`.

Input must be a snapshot with **unique, ascending node IDs and way IDs**, as in
normal Geofabrik extracts. Files advertising `Sort.Type_then_ID` skip node blocks
when scanning ways, and are rejected if they are not actually sorted; other files
work but scan every block twice. History files are rejected. Incomplete references on
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

Each column is one memory-mapped file for the whole dataset, opened once per
process, so a read touches only the pages of the tiles it selects. Tiles adjacent
on disk are returned as read-only memory maps without copying; separated tiles
are concatenated into new arrays. `offsets` is always computed, and rebased to the
returned vertices. For bounded-memory or zero-copy consumption, use
`iter_bbox_tiles(outdir, west, south, east, north)`, which yields each tile's
columns separately. Direct access also works, using the ranges in `manifest.json`:

```python
import numpy as np
u = np.load("outdir/columns/u.npy", mmap_mode="r", allow_pickle=False)
tile_u = u[tile["edge_start"] : tile["edge_start"] + tile["edges"]]
```

Both readers accept `columns` to skip mapping and copying what you do not use,
which is usually the cheapest way to speed up a read. Requesting `vertices` also
returns `offsets`:

```python
roads = read_bbox("outdir", 6.0, 49.5, 6.3, 49.8, columns=("u", "v", "vertices", "width_m"))
```

Columns, tile envelopes, and dictionaries stay open between calls for up to four
datasets, and are refreshed when a dataset is replaced. Each open dataset holds 26
file descriptors, however many tiles it has or you keep.

Extra tiles cost almost nothing to read, and smaller tiles return fewer unwanted
edges, so `--tile-degrees 0.05` suits country-sized extracts. Avoid it at planet
scale, where the JSON tile list in `manifest.json` would become very large.

Empty results have `vertices.shape == (0, 2)`, `offsets == [0]`, and empty edge
columns with the normal dtypes. Bounding boxes use inclusive intersection;
`west > east` represents an antimeridian-crossing query. Results contain **whole
intersecting tiles**, without clipping or exact per-edge filtering.

## Version 2 storage contract

All arrays are little-endian, C-contiguous, uncompressed `.npy` files. No object
arrays and no pickle are used. An edge's `lonlat` is the slice of `vertices`
between consecutive offsets, always ordered from `u` to `v`. Tiles are contiguous
ranges of each column, ordered by grid `x` and then `y`.

| File within `columns/` | Dtype and shape | Meaning |
| --- | --- | --- |
| `vertices.npy` | float64 `(V, 2)` | WGS84 `[lon, lat]`, all original shape vertices |
| `offsets.npy` | int64 `(E + 1,)` | Global across tiles; starts at 0, ends at V; each edge has at least two vertices |
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

The manifest lists each tile's ID, `edge_start`, `vertex_start`, edge/vertex
counts, `grid_bbox`, and `bbox`, all using `[west, south, east, north]`. **`bbox` encloses every vertex
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

An explicit `oneway` tag always beats an implied rule. `yes/true/1` emits forward
edges and `-1` emits reversed edges. `no/false/0` permits both directions, as do
`reversible` and `alternating`, whose traffic runs both ways at different times.
Otherwise `junction=roundabout` or `circular`, `highway=motorway`, and
`motorway_link` imply forward-only travel. Other roads emit both directions with
reversed vertex order for the second.
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

Preprocessing makes three passes. The first scans way blocks and externally sorts
every node reference from a drivable way, producing a sorted list of the nodes
those ways use, each marked as a junction or a shape point. The second scans node
blocks and keeps only those nodes in a sorted location file, with the junction bit
in the same record. Drivable ways reference about 8-16% of an extract's nodes, so
this index stays far smaller than one covering every node. The third scans way
blocks again, resolves locations through a memory map, and spools directed edges
by tile. Decoding and per-block work happen on the worker threads, in order.

The native process keeps bounded PBF batches, one sort run (`--sort-mb`), buffered
tile output (`--spool-mb`), a SQLite cache (`--cache-mb`), an 8,192-entry string
cache, and 32,768 rows per column-conversion chunk. Global dictionaries live in
SQLite. Tile metadata stays in RAM, which scales with the tile count and not the
extract size. No complete extract, topology hash map, or tile's complete geometry
is accumulated in application RAM. Memory-mapped index pages are demand-paged by
the OS, so observed RSS includes resident file-backed pages.

Budget scratch disk for the sorted runs (8 bytes per reference), used-node list
(8 bytes per used node), location index (24 bytes per used node), dictionary
database, edge spools, and output. Geometry spools use 16 bytes per directed vertex
and row spools use 74 bytes per edge. Final edge columns use 74 bytes per edge plus
16 bytes per directed vertex, dictionaries, and small `.npy` headers. Runs, the
used-node list, and the location index are deleted as soon as each pass finishes,
and tile spools as their columns are finalized.

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

Measure read latency on a preprocessed dataset. Queries are placed on its busiest
tiles; to compare datasets on identical boxes, place them with `--boxes-from`:

```sh
uv run python benchmarks/read.py outdir/ --boxes-from other-dataset/
```

Measured results are in [benchmarks/RESULTS.md](benchmarks/RESULTS.md), with raw
records alongside. These are regional-extract measurements, not a claim of
planet-scale benchmark performance.
