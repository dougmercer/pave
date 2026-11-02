"""Atomic publication and reporting around the single native preprocessing call."""

import json
import math
import resource
import sys
import tempfile
import time
from collections.abc import Callable
from pathlib import Path

from . import _native


def peak_rss_bytes() -> int:
    value = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return int(value if sys.platform == "darwin" else value * 1024)


def preprocess(
    input_path: str | Path,
    outdir: str | Path,
    *,
    tile_degrees: float = 0.1,
    workdir: str | Path | None = None,
    cache_mb: int = 32,
    sort_mb: int = 64,
    spool_mb: int = 64,
    threads: int = 8,
    progress: Callable[[str], None] | None = None,
) -> dict:
    """Run the Rust preprocessor, then atomically publish a complete dataset.

    Input must be a PBF snapshot with unique ascending node and way IDs and
    complete node references for drivable ways. The native call releases the GIL.
    Scratch indexes/spools go beside `outdir` unless `workdir` names another disk. `cache_mb`,
    `sort_mb`, and `spool_mb` bound the SQLite dictionary cache, the sort run size,
    and buffered tile output; the report's `buffers` shows how often the latter two
    spilled. `threads` sets the decode and spool read workers. Output must be absent or empty.
    `progress` gets start/end messages; native stages go to stderr when progress is
    enabled. Peak RSS is the lifetime high-water mark of this process; benchmark in
    a fresh subprocess for a per-run measurement.
    """
    started = time.perf_counter()
    if not math.isfinite(tile_degrees) or not 0 < tile_degrees <= 180:
        raise ValueError("tile_degrees must be finite and in (0, 180]")
    if min(cache_mb, sort_mb, spool_mb, threads) < 1:
        raise ValueError("cache_mb, sort_mb, spool_mb and threads must be positive integers")
    input_path, outdir = Path(input_path).resolve(), Path(outdir).absolute()
    if not input_path.is_file():
        raise FileNotFoundError(input_path)
    if outdir.is_symlink() or (outdir.exists() and (not outdir.is_dir() or any(outdir.iterdir()))):
        raise FileExistsError(f"Destination must be absent or empty: {outdir}")
    outdir.parent.mkdir(parents=True, exist_ok=True)
    if workdir is not None:
        Path(workdir).mkdir(parents=True, exist_ok=True)
    if progress:
        progress("Starting Rust preprocessor")
    with tempfile.TemporaryDirectory(prefix=".osm-road-output-", dir=outdir.parent) as staging:
        output = Path(staging) / "dataset"
        output.mkdir()
        # The system temp directory may be RAM-backed, so stay on the output's disk by default.
        scratch = outdir.parent if workdir is None else workdir
        with tempfile.TemporaryDirectory(prefix="osm-road-scratch-", dir=scratch) as temporary:
            report = json.loads(
                _native.build(
                    input_path,
                    output,
                    Path(temporary),
                    tile_degrees,
                    cache_mb,
                    sort_mb,
                    spool_mb,
                    threads,
                    progress is None,
                )
            )
        elapsed = time.perf_counter() - started
        report.update(
            elapsed_seconds=elapsed,
            edges_per_second=report["edges"] / elapsed,
            peak_rss_bytes=peak_rss_bytes(),
        )
        (output / "stats.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        output.rename(outdir)
    if progress:
        progress(f"Wrote {report['edges']:,} directed edges")
    return report
