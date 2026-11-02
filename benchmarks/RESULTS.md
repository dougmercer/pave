# Real-extract benchmark

Measured 2026-11-02 with the Maturin **release** build on an **Apple M1 Max**
(10 cores, 64 GiB), macOS 26.2 arm64, Python 3.15.0, NumPy 2.5.4, and rustc 1.93.0.
Settings are the defaults: 0.1° tiles, 8 threads, and 64 MiB tile buffers. Both extracts carry the PBF replication timestamp
`2026-11-01T21:21:36Z`.

| Extract | Input | Edges | Vertices | Tiles | Median time | Median edges/s | Peak RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| [Luxembourg](https://download.geofabrik.de/europe/luxembourg.html) | 45.6 MiB | 269,803 | 1,635,960 | 52 | **0.21 s** | **1,271,834** | **369 MiB** |
| [South America](https://download.geofabrik.de/south-america.html) | 3,963 MiB | 53,680,755 | 398,566,495 | 103,167 | **21.6 s** | **2,488,641** | **5,052 MiB** |

Including Python startup and imports, median CLI throughput was
958,374 edges/s on Luxembourg and 2,474,370 edges/s on South America.

Peak RSS includes resident pages of the memory-mapped location index, which the OS
can reclaim. South America's heap footprint was about 2.5 GiB of its 4.9 GiB, most of
it the bitmap of used nodes.

## Individual runs

Each run is a fresh process. Time includes decoding, the disk-backed indexes,
graph construction, dictionary encoding, all column writes, manifest creation, and
scratch cleanup. Downloads and compilation are excluded. Filesystem caches were not
flushed; writes were flushed to the OS without `fsync`. The median does not claim
cold-cache performance or planet-scale throughput.

Luxembourg:

| Run | Seconds | Topology | Edges | Columns | Edges/s | CLI edges/s | Peak RSS MiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 0.27 | 0.15 | 0.09 | 0.03 | 998,742 | 749,016 | 361.7 |
| 2 | 0.21 | 0.09 | 0.09 | 0.03 | 1,271,834 | 958,374 | 359.3 |
| 3 | 0.21 | 0.09 | 0.09 | 0.02 | 1,308,032 | 976,855 | 369.0 |

South America:

| Run | Seconds | Topology | Edges | Columns | Edges/s | CLI edges/s | Peak RSS MiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 21.57 | 6.10 | 8.61 | 6.77 | 2,488,641 | 2,474,370 | 5036.8 |
| 2 | 20.93 | 6.03 | 8.08 | 6.74 | 2,564,950 | 2,551,221 | 5035.3 |
| 3 | 21.74 | 6.05 | 8.29 | 7.31 | 2,469,633 | 2,455,376 | 5051.8 |

Writing columns reads the spool log back in tile order after writing the output has
evicted much of it from the page cache, so those reads run concurrently and each
spill is laid out in tile order to let neighbouring chunks share a read.

## Read latency and tile size

Median `read_bbox` latency on South America with a warm page cache, over 60 identical
query boxes centred on the 12 busiest 0.25° tiles. Each figure includes touching
every returned byte of the routing columns (`u`, `v`, `vertices`, `offsets`,
`width_m`, and `highway`), because memory maps return before their pages load.
Results contain whole intersecting tiles, so edge counts reflect tile size and not
the query box, and every returned edge is work for the caller.

| Query | 0.25° edges | 0.25° | 0.1° edges | 0.1° | 0.05° edges | 0.05° |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| point 200 m | 187,015 | 0.69 ms | 33,361 | 0.38 ms | 9,999 | 0.84 ms |
| city 5 km | 199,039 | 0.83 ms | 50,088 | 0.52 ms | 32,447 | 1.12 ms |
| region 25 km | 304,103 | 1.71 ms | 242,368 | 1.57 ms | 191,268 | 1.98 ms |
| metro 100 km | 719,643 | 4.17 ms | 677,061 | 4.24 ms | 660,922 | 4.77 ms |

Reads at 0.05° are slower than at 0.1° for small queries because each one scans
about 319,000 tile envelopes. Tile size also affects the build and the first read:

| Tile size | Tiles | Build | Manifest | First open in a process |
| --- | ---: | ---: | ---: | ---: |
| 0.25° | 20,221 | 20.7 s | 3.6 MiB | 60 ms |
| 0.1° | 103,167 | 21.6 s | 21.8 MiB | 256 ms |
| 0.05° | 318,942 | 22.6 s | 67.6 MiB | 806 ms |

## Sources

Luxembourg SHA-256: `5fc9e68a664f359268f5f3a2bed33c09b24361150fed0860e150ba30e3e8e3b6`.
South America SHA-256: `8bb9be492fc0f0dd3b15cddcb1257ededb5655a566b9efbcbfc7548c87d90ffd`.
Full reports and settings: [luxembourg.json](luxembourg.json),
[south-america.json](south-america.json).

Reproduce after installing the package:

```sh
curl -L -o south-america.osm.pbf https://download.geofabrik.de/south-america-261101.osm.pbf
uv run python benchmarks/run.py south-america.osm.pbf \
    --source-url https://download.geofabrik.de/south-america-261101.osm.pbf \
    --report benchmarks/result.json
uv run pave south-america.osm.pbf south-america-roads/
uv run python benchmarks/read.py south-america-roads/
```
