import argparse
import json
import sys

from .preprocess import preprocess


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(
        description="Build a tiled directed road graph from an OSM PBF"
    )
    parser.add_argument("input", help="Input .osm.pbf snapshot")
    parser.add_argument("outdir", help="New or empty output directory")
    parser.add_argument("--tile-degrees", type=float, default=0.25)
    parser.add_argument("--workdir", help="Directory for disk-backed scratch indexes and spools")
    parser.add_argument(
        "--cache-mb", type=int, default=32, help="SQLite cache size (default: 32 MiB)"
    )
    parser.add_argument(
        "--threads", type=int, default=2, help="PBF decompression threads (default: 2)"
    )
    parser.add_argument("--quiet", action="store_true", help="Suppress progress on stderr")
    args = parser.parse_args(argv)
    try:
        report = preprocess(
            args.input,
            args.outdir,
            tile_degrees=args.tile_degrees,
            workdir=args.workdir,
            cache_mb=args.cache_mb,
            threads=args.threads,
            progress=None if args.quiet else lambda message: print(message, file=sys.stderr),
        )
    except (OSError, ValueError, RuntimeError) as error:
        parser.exit(1, f"prep: {error}\n")
    print(json.dumps(report, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
