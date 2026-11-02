# Real-extract benchmark

Measured 2026-11-02 with the Maturin **release** build on an **Apple M1 Max**
(10 cores), macOS 26.2 arm64, Python 3.15.0, NumPy 2.5.4, and rustc 1.93.0.
Settings are the defaults: 0.25° tiles, 8 threads, 32 MiB SQLite cache, 64 MiB
sort runs, and 64 MiB tile buffers. Both extracts carry the PBF replication
timestamp `2026-11-01T21:21:36Z`.

| Extract | Input | Edges | Vertices | Tiles | Median time | Median edges/s | Peak RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| [Luxembourg](https://download.geofabrik.de/europe/luxembourg.html) | 45.6 MiB | 266,808 | 1,625,556 | 14 | **0.453 s** | **588,522** | **229.2 MiB** |
| [Belgium](https://download.geofabrik.de/europe/belgium.html) | 666.0 MiB | 2,943,069 | 14,763,799 | 94 | **5.327 s** | **552,442** | **603.0 MiB** |

Including Python startup and imports, median CLI throughput was
508,541 edges/s on Luxembourg and 544,548 edges/s on Belgium.

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
| 1 | 0.491 | 543,300 | 474,912 | 229.2 |
| 2 | 0.453 | 588,522 | 508,541 | 227.1 |
| 3 | 0.421 | 633,413 | 542,071 | 221.8 |

Belgium:

| Run | Preprocess seconds | Edges/s | CLI edges/s | Peak RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| 1 | 5.602 | 525,353 | 518,186 | 589.1 |
| 2 | 5.327 | 552,442 | 544,548 | 585.6 |
| 3 | 5.222 | 563,561 | 554,610 | 603.0 |

## Read latency

Median `read_bbox` latency on the Belgian dataset with a warm page cache, over 60
identical query boxes centred on the 12 busiest 0.25° tiles. Each figure includes
touching every returned byte of the routing columns (`u`, `v`, `vertices`,
`offsets`, `width_m`, and `highway`), because memory maps return before their pages
load. Results contain whole intersecting tiles, so edge counts reflect tile size
and not the query box. Version 1 stored 16 files per tile; version 2 stores one
file per column.

| Query | Edges at 0.25° | Version 1, quiet disk | Version 1, during backup | Version 2, 0.25° | Edges at 0.05° | Version 2, 0.05° |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| point 200 m | 79,452 | 1.73 ms | 36.00 ms | 0.33 ms | 4,138 | 0.07 ms |
| city 5 km | 104,623 | 3.38 ms | 52.63 ms | 0.50 ms | 12,048 | 0.12 ms |
| region 25 km | 226,027 | 6.68 ms | 138.00 ms | 1.44 ms | 103,426 | 0.71 ms |
| metro 100 km | 927,868 | 37.39 ms | 753.87 ms | 5.48 ms | 688,730 | 4.40 ms |

The backup column was measured during a Time Machine backup. On macOS, creating a
memory map slows sharply while other disk writes are in flight: it went from 23 µs
to about 3 ms, tracking the cost of `F_FULLFSYNC`. Version 1 creates maps on every
read, so it is exposed to this. Version 2 creates them once, and its figures were
the same with and without the backup. Linux was not measured.

Opening a dataset costs about 35 ms once per process.

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
