"""Measure read_bbox latency, and the same read with every returned byte touched."""

import argparse
import json
import random
import statistics
import time
from pathlib import Path

import numpy as np

from osm_road_prep import read_bbox

QUERIES = {"point 200 m": 0.001, "city 5 km": 0.025, "region 25 km": 0.125, "metro 100 km": 0.5}
ROUTING_COLUMNS = ("u", "v", "vertices", "width_m", "highway")


def touch(result):
    """Memory maps return before their pages load, so include reading the data."""
    return sum(value.sum() for value in result.values() if isinstance(value, np.ndarray))


def measure(root, half, tiles, columns, repeat, rng):
    seconds, touched, edges, selected = [], [], [], []
    for _ in range(repeat):
        west, south, east, north = rng.choice(tiles)["grid_bbox"]
        lon, lat = rng.uniform(west, east), rng.uniform(south, north)
        started = time.perf_counter()
        result = read_bbox(root, lon - half, lat - half, lon + half, lat + half, columns=columns)
        seconds.append(time.perf_counter() - started)
        touch(result)
        touched.append(time.perf_counter() - started)
        edges.append(len(result["u"]))
        selected.append(len(result["tile_ids"]))
    return {
        "median_ms": statistics.median(seconds) * 1e3,
        "median_touched_ms": statistics.median(touched) * 1e3,
        "median_edges": statistics.median(edges),
        "median_tiles": statistics.median(selected),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("dataset", type=Path)
    parser.add_argument(
        "--boxes-from",
        type=Path,
        help="Dataset whose busiest tiles place the queries, to compare other datasets fairly",
    )
    parser.add_argument("--repeat", type=int, default=200)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    placement = args.boxes_from or args.dataset
    manifest = json.loads((placement / "manifest.json").read_text(encoding="utf-8"))
    busiest = sorted(manifest["tiles"], key=lambda tile: -tile["edges"])[:12]
    read_bbox(args.dataset, -180, -90, 180, 90, columns=ROUTING_COLUMNS)  # Warm the page cache.
    results = {}
    for name, half in QUERIES.items():
        for label, columns in (("all columns", None), ("routing columns", ROUTING_COLUMNS)):
            # Reseed so every column set answers the same boxes.
            rng = random.Random(1)
            row = measure(args.dataset, half, busiest, columns, args.repeat, rng)
            results[f"{name}, {label}"] = row
            print(
                f"{name:13s} {label:16s} {row['median_tiles']:5.0f} tiles "
                f"{row['median_edges']:10,.0f} edges {row['median_ms']:9.2f} ms "
                f"{row['median_touched_ms']:9.2f} ms touched",
                flush=True,
            )
    if args.report:
        args.report.write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
