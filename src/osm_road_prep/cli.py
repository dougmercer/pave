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
    parser.add_argument("--tile-degrees", type=float, default=0.1, help="Tile size (default: 0.1)")
    parser.add_argument(
        "--workdir", help="Scratch directory for indexes and spools (default: beside outdir)"
    )
    parser.add_argument(
        "--sort-mb", type=int, default=64, help="Node reference sort run size (default: 64 MiB)"
    )
    parser.add_argument(
        "--spool-mb", type=int, default=64, help="Buffered tile output (default: 64 MiB)"
    )
    parser.add_argument("--threads", type=int, default=8, help="PBF decode workers (default: 8)")
    parser.add_argument("--quiet", action="store_true", help="Suppress progress on stderr")
    args = parser.parse_args(argv)
    try:
        report = preprocess(
            args.input,
            args.outdir,
            tile_degrees=args.tile_degrees,
            workdir=args.workdir,
            sort_mb=args.sort_mb,
            spool_mb=args.spool_mb,
            threads=args.threads,
            progress=None if args.quiet else lambda message: print(message, file=sys.stderr),
        )
    except (OSError, ValueError, RuntimeError) as error:
        parser.exit(1, f"prep: {error}\n")
    print(json.dumps(report, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
