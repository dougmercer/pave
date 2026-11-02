# Real-extract benchmark

Measured 2026-11-02 with the Maturin **release** build on an **Apple M1 Max**
(10 cores), macOS 26.2 arm64, Python 3.15.0, NumPy 2.5.4, and rustc 1.93.0.
Settings are the defaults: 0.25° tiles, 8 threads, 32 MiB SQLite cache, 64 MiB
sort runs, and 64 MiB tile buffers. Both extracts carry the PBF replication
timestamp `2026-11-01T21:21:36Z`.

| Extract | Input | Edges | Vertices | Tiles | Median time | Median edges/s | Peak RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| [Luxembourg](https://download.geofabrik.de/europe/luxembourg.html) | 45.6 MiB | 266,808 | 1,625,556 | 14 | **0.419 s** | **637,291** | **232.5 MiB** |
| [Belgium](https://download.geofabrik.de/europe/belgium.html) | 666.0 MiB | 2,943,069 | 14,763,799 | 94 | **5.212 s** | **564,630** | **598.2 MiB** |

Including Python startup and imports, median CLI throughput was
541,518 edges/s on Luxembourg and 556,517 edges/s on Belgium.

## Individual runs

Each run is a fresh process. Time includes decoding, both disk-backed indexes,
graph construction, dictionary encoding, all column writes, manifest creation,
and scratch cleanup. Downloads and compilation are excluded. Filesystem caches
were not flushed; writes were flushed to the OS without `fsync`. The median does
not claim cold-cache performance or planet-scale throughput. RSS includes resident
memory-mapped index pages.

Luxembourg:

| Run | Preprocess seconds | Edges/s | CLI edges/s | Peak RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| 1 | 0.407 | 655,453 | 559,082 | 232.5 |
| 2 | 0.419 | 637,291 | 541,518 | 227.4 |
| 3 | 0.420 | 635,227 | 539,258 | 224.3 |

Belgium:

| Run | Preprocess seconds | Edges/s | CLI edges/s | Peak RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| 1 | 5.106 | 576,443 | 568,056 | 594.7 |
| 2 | 5.212 | 564,630 | 556,517 | 598.2 |
| 3 | 5.268 | 558,670 | 550,498 | 576.6 |

## Read latency

Median `read_bbox` latency on the Belgian dataset at 0.25° tiles, with a warm page
cache and identical query boxes centred on the 12 busiest tiles. Routing columns
are `u`, `v`, `vertices`, `offsets`, `width_m`, and `highway`. Results contain
whole intersecting tiles, so edge counts reflect tile size and not the query box.

| Query | Tiles | Edges | All columns | Routing columns |
| --- | ---: | ---: | ---: | ---: |
| point 200 m | 1 | 71,671 | 2.00 ms | 0.69 ms |
| city 5 km | 2 | 97,242 | 5.23 ms | 2.09 ms |
| region 25 km | 4 | 224,844 | 12.15 ms | 5.56 ms |
| metro 100 km | 21 | 927,868 | 69.35 ms | 27.12 ms |

Latency is dominated by opening one file per column per tile, so it rises with the
number of tiles selected. Requesting fewer columns is the most effective change a
caller can make. These figures were measured on macOS, where creating a memory map
is also much slower while other disk writes are in flight.

## Sources

Luxembourg SHA-256: `5fc9e68a664f359268f5f3a2bed33c09b24361150fed0860e150ba30e3e8e3b6`.
Belgium SHA-256: `4a139068b5131d018a11f85a4fae498d01eadb8ecf199c795970891bee4f8372`.
Full reports and settings: [luxembourg.json](luxembourg.json), [belgium.json](belgium.json).

Reproduce after installing the package:

```sh
curl -L -o belgium.osm.pbf https://download.geofabrik.de/europe/belgium-latest.osm.pbf
uv run python benchmarks/run.py belgium.osm.pbf \
    --source-url https://download.geofabrik.de/europe/belgium-latest.osm.pbf \
    --report benchmarks/result.json
uv run prep belgium.osm.pbf belgium-roads/
uv run python benchmarks/read.py belgium-roads/
```

The latest download changes over time; use the checksum and source timestamp
above when comparing runs.
