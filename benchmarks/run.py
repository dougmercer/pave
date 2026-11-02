"""Run fresh CLI processes and save reproducible real-extract measurements."""

import argparse
import hashlib
import importlib.metadata
import json
import platform
import statistics
import subprocess
import sys
import tempfile
import time
from datetime import UTC, datetime
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--source-url")
    parser.add_argument("--repeat", type=int, default=3)
    args = parser.parse_args()
    if args.repeat < 1:
        parser.error("--repeat must be positive")
    runs = []
    for i in range(args.repeat):
        with tempfile.TemporaryDirectory(prefix="osm-road-benchmark-") as temporary:
            started = time.perf_counter()
            result = subprocess.run(
                [
                    sys.executable,
                    "-m",
                    "osm_road_prep.cli",
                    str(args.input.resolve()),
                    str(Path(temporary) / "roads"),
                    "--quiet",
                ],
                check=True,
                text=True,
                capture_output=True,
            )
            wall = time.perf_counter() - started
            report = json.loads(result.stdout)
            report["cli_wall_seconds"] = wall
            report["cli_edges_per_second"] = report["edges"] / wall
            runs.append(report)
            print(
                f"Run {i + 1}: {report['edges_per_second']:,.0f} edges/s, "
                f"{report['peak_rss_bytes'] / 2**20:.1f} MiB peak RSS",
                flush=True,
            )
    processor = platform.processor()
    if sys.platform == "darwin":
        processor = subprocess.check_output(
            ["sysctl", "-n", "machdep.cpu.brand_string"], text=True
        ).strip()
    with args.input.open("rb") as file:
        digest = hashlib.file_digest(file, "sha256").hexdigest()
    document = {
        "recorded_at": datetime.now(UTC).isoformat(),
        "source_url": args.source_url,
        "input_sha256": digest,
        "input_bytes": args.input.stat().st_size,
        "platform": platform.platform(),
        "processor": processor,
        "python": sys.version,
        "numpy": importlib.metadata.version("numpy"),
        "package": importlib.metadata.version("osm-road-prep"),
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "build": "Maturin release, thin LTO, codegen-units=1",
        "method": "Fresh subprocess per run with default CLI settings, recorded per run; "
        "local temporary disk; OS file cache not flushed. "
        "Preprocessing time includes indexes, graph, columns and scratch cleanup; "
        "CLI wall also includes Python startup/imports. Downloads/builds excluded. "
        "Writes are flushed to the OS, not fsynced to physical media.",
        "median_edges_per_second": statistics.median(r["edges_per_second"] for r in runs),
        "median_elapsed_seconds": statistics.median(r["elapsed_seconds"] for r in runs),
        "median_cli_edges_per_second": statistics.median(r["cli_edges_per_second"] for r in runs),
        "peak_rss_bytes": max(r["peak_rss_bytes"] for r in runs),
        "runs": runs,
    }
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
