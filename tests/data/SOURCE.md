# Real-extract fixture provenance

`monaco.osm.pbf` is the unmodified Monaco OpenStreetMap extract downloaded from
[Geofabrik](https://download.geofabrik.de/europe/monaco.html) on 2026-11-02.

- Download: https://download.geofabrik.de/europe/monaco-latest.osm.pbf
- Source PBF replication timestamp: `2026-11-01T21:21:36Z`
- Size: 706,895 bytes
- SHA-256: `5c947eae875b35cc2268bff30c0b2662ad81b838a47ddc7d963afdbf7c896ff3`

© OpenStreetMap contributors. OSM data is available under the
[Open Database License (ODbL)](https://www.openstreetmap.org/copyright).
This data fixture is not covered by any source-code license.

The independent test oracle uses pyosmium only in tests. It compares every
expected directed edge, global endpoint ID, highway class, and original vertex
against the Rust output. The fixed extract produces 2,354 directed edges.
