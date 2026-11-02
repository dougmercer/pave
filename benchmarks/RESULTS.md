# Real-extract benchmark

Measured 2026-11-02 with the Maturin **release** build on an **Apple M1 Max**,
macOS 26.2 arm64, Python 3.15.0, NumPy 2.5.4, and rustc 1.93.0.

Input: [Geofabrik Luxembourg](https://download.geofabrik.de/europe/luxembourg.html),
47,857,936 bytes (45.6 MiB), PBF replication timestamp `2026-11-01T21:21:36Z`.

Output: **266,808 directed edges**, 1,625,556 directed vertices, and 14 tiles from
92,711 drivable ways. Grid 0.25°, SQLite cache 32 MiB, 2 PBF decode threads.

**Median preprocessing throughput: 164,264 edges/s.**
Median time: **1.624 s**.
Maximum peak RSS across runs: **177.8 MiB**.
Including Python startup/imports, median CLI throughput was
**158,093 edges/s**.

| Run | Preprocess seconds | Edges/s | CLI edges/s | Peak RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| 1 | 2.637 | 101,170 | 98,198 | 177.8 |
| 2 | 1.620 | 164,675 | 158,302 | 174.4 |
| 3 | 1.624 | 164,264 | 158,093 | 176.2 |

Each run is a fresh process. Time includes decoding, both disk-backed indexes,
graph construction, dictionary encoding, all column writes, manifest creation,
and scratch cleanup. Downloads and compilation are excluded. Filesystem caches
were not flushed; writes were flushed to the OS without `fsync`. The slower
first run is retained. The median does not claim cold-cache performance or
planet-scale throughput. RSS includes resident memory-mapped index pages.

Input SHA-256: `5fc9e68a664f359268f5f3a2bed33c09b24361150fed0860e150ba30e3e8e3b6`.
Full individual reports and settings: [luxembourg.json](luxembourg.json).

Reproduce after installing the package:

```sh
curl -L -o luxembourg.osm.pbf https://download.geofabrik.de/europe/luxembourg-latest.osm.pbf
uv run python benchmarks/run.py luxembourg.osm.pbf \
    --source-url https://download.geofabrik.de/europe/luxembourg-latest.osm.pbf \
    --report benchmarks/result.json
```

The latest download changes over time; use the checksum and source timestamp
above when comparing runs.
