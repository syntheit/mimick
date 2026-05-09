#!/usr/bin/env python3
"""Configurable test-asset generator for Mimick startup-scan / dedup benchmarks.

Generates synthetic media files in formats Mimick recognises (jpg, png, webp,
gif, bmp, tiff, svg, mp4) with deterministic, parameterisable content
distributions. Designed to make startup-scan and SyncIndex dedup paths
reproducible across A/B benchmark runs.

Usage examples:

    # 500 unique files split across the default image formats, written as fast
    # as the disk allows.
    python scripts/gen_test_assets.py --out /tmp/mimick-bench --count 500

    # 1000 files with 20% exact duplicates (same content, same hash). Useful
    # for exercising the SyncIndex.local_path_for_checksum path.
    python scripts/gen_test_assets.py --out /tmp/mimick-bench --count 1000 \\
        --duplicate-ratio 0.2

    # 2000 files at 50ms apart (mimics a slow dump rather than a burst), capped
    # at 1GiB total disk usage.
    python scripts/gen_test_assets.py --out /tmp/mimick-bench --count 2000 \\
        --delay-ms 50 --cap-bytes 1073741824

    # Restrict to specific formats (subset of --formats list).
    python scripts/gen_test_assets.py --out /tmp/mimick-bench --count 200 \\
        --formats jpg,png,webp,mp4

Image generation requires Pillow (`pip install Pillow`). Video generation
requires `ffmpeg` on PATH; if unavailable, mp4 is skipped with a warning.
SVG and BMP have pure-stdlib fallbacks.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import os
import random
import shutil
import struct
import subprocess
import sys
import time
import zlib
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Optional

# ----------------------------------------------------------------------------
# Format generators
# ----------------------------------------------------------------------------

# Each generator takes (rng, size_hint_px) and returns the file bytes. The
# size_hint_px controls roughly how large the resulting file is — bumping it
# makes hashing slower and disk usage higher, both useful knobs for benching.

GenFn = Callable[[random.Random, int], bytes]


def _need_pillow():
    try:
        import PIL  # noqa: F401

        return True
    except ImportError:
        return False


def _gen_pillow(fmt: str, save_kwargs: dict | None = None):
    def inner(rng: random.Random, size_hint_px: int) -> bytes:
        from PIL import Image

        w = h = max(8, size_hint_px)
        # Generate w*h*3 bytes of random RGB. os.urandom is fast and
        # incompressible, so the resulting file's size scales with size_hint_px.
        seed = rng.getrandbits(64)
        local = random.Random(seed)
        raw = bytes(local.getrandbits(8) for _ in range(w * h * 3))
        img = Image.frombytes("RGB", (w, h), raw)
        buf = io.BytesIO()
        img.save(buf, fmt, **(save_kwargs or {}))
        return buf.getvalue()

    return inner


def gen_svg(rng: random.Random, size_hint_px: int) -> bytes:
    # Pure-stdlib path: a minimal but valid SVG with a unique nonce so its
    # bytes hash differently every time. Cheap to generate; small file.
    nonce = rng.getrandbits(128)
    body = (
        f'<?xml version="1.0" encoding="UTF-8"?>\n'
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">'
        f'<!-- mimick-test-{nonce:032x} -->'
        f'<rect width="16" height="16" fill="#{rng.randrange(0, 0xFFFFFF):06x}"/>'
        f"</svg>\n"
    )
    return body.encode("utf-8")


def gen_bmp(rng: random.Random, size_hint_px: int) -> bytes:
    # Pure-stdlib BMP writer. 24-bit uncompressed, padded to 4-byte rows.
    w = h = max(8, size_hint_px)
    row_padded = (w * 3 + 3) & ~3
    pixel_data_size = row_padded * h
    file_size = 14 + 40 + pixel_data_size
    header = b"BM" + struct.pack("<IHHI", file_size, 0, 0, 14 + 40)
    dib = struct.pack("<IIIHHIIIIII", 40, w, h, 1, 24, 0, pixel_data_size, 2835, 2835, 0, 0)
    seed = rng.getrandbits(64)
    local = random.Random(seed)
    rows = []
    for _ in range(h):
        row = bytes(local.getrandbits(8) for _ in range(w * 3))
        rows.append(row + b"\x00" * (row_padded - len(row)))
    return header + dib + b"".join(rows)


def gen_mp4(rng: random.Random, size_hint_px: int) -> bytes:
    # Use ffmpeg to encode a 1-second monochrome noise video at the requested
    # resolution. Returns None as a sentinel if ffmpeg isn't on PATH; the
    # caller will skip the format.
    if shutil.which("ffmpeg") is None:
        return b""
    w = h = max(16, size_hint_px)
    seed = rng.getrandbits(31)
    cmd = [
        "ffmpeg",
        "-loglevel", "error",
        "-f", "lavfi",
        "-i", f"nullsrc=size={w}x{h}:duration=1:rate=30",
        "-vf", f"geq=random({seed}/255):128:128",
        "-pix_fmt", "yuv420p",
        "-c:v", "libx264",
        "-preset", "ultrafast",
        "-f", "mp4",
        "-movflags", "+frag_keyframe+empty_moov",
        "pipe:1",
    ]
    proc = subprocess.run(cmd, capture_output=True, check=False)
    if proc.returncode != 0:
        return b""
    return proc.stdout


def gen_dummy(rng: random.Random, size_hint_px: int) -> bytes:
    # Generates reproducible random noise for formats where we don't have
    # a native builder. Mimick primarily relies on the extension and file size
    # for startup indexing, so dummy bytes work for scale benchmarking.
    size = max(1024, size_hint_px * size_hint_px * 3)
    return rng.randbytes(size)


@dataclass(frozen=True)
class FormatSpec:
    ext: str
    gen: GenFn
    needs_pillow: bool = False
    needs_ffmpeg: bool = False


def build_format_registry() -> dict[str, FormatSpec]:
    return {
        # Standard images
        "avif": FormatSpec("avif", gen_dummy),
        "bmp":  FormatSpec("bmp",  gen_bmp),
        "gif":  FormatSpec("gif",  _gen_pillow("GIF"),                   needs_pillow=True),
        "heic": FormatSpec("heic", gen_dummy),
        "heif": FormatSpec("heif", gen_dummy),
        "hif":  FormatSpec("hif",  gen_dummy),
        "insp": FormatSpec("insp", gen_dummy),
        "jpe":  FormatSpec("jpe",  _gen_pillow("JPEG", {"quality": 85}), needs_pillow=True),
        "jpeg": FormatSpec("jpeg", _gen_pillow("JPEG", {"quality": 85}), needs_pillow=True),
        "jpg":  FormatSpec("jpg",  _gen_pillow("JPEG", {"quality": 85}), needs_pillow=True),
        "jp2":  FormatSpec("jp2",  gen_dummy),
        "jxl":  FormatSpec("jxl",  gen_dummy),
        "png":  FormatSpec("png",  _gen_pillow("PNG"),                   needs_pillow=True),
        "mpo":  FormatSpec("mpo",  _gen_pillow("JPEG", {"quality": 85}), needs_pillow=True),
        "psd":  FormatSpec("psd",  gen_dummy),
        "svg":  FormatSpec("svg",  gen_svg),
        "tif":  FormatSpec("tif",  _gen_pillow("TIFF"),                  needs_pillow=True),
        "tiff": FormatSpec("tiff", _gen_pillow("TIFF"),                  needs_pillow=True),
        "webp": FormatSpec("webp", _gen_pillow("WebP", {"quality": 80}), needs_pillow=True),
        
        # RAW Formats
        "3fr": FormatSpec("3fr", gen_dummy),
        "ari": FormatSpec("ari", gen_dummy),
        "arw": FormatSpec("arw", gen_dummy),
        "cap": FormatSpec("cap", gen_dummy),
        "cin": FormatSpec("cin", gen_dummy),
        "cr2": FormatSpec("cr2", gen_dummy),
        "cr3": FormatSpec("cr3", gen_dummy),
        "crw": FormatSpec("crw", gen_dummy),
        "dcr": FormatSpec("dcr", gen_dummy),
        "dng": FormatSpec("dng", gen_dummy),
        "erf": FormatSpec("erf", gen_dummy),
        "fff": FormatSpec("fff", gen_dummy),
        "iiq": FormatSpec("iiq", gen_dummy),
        "k25": FormatSpec("k25", gen_dummy),
        "kdc": FormatSpec("kdc", gen_dummy),
        "mrw": FormatSpec("mrw", gen_dummy),
        "nef": FormatSpec("nef", gen_dummy),
        "nrw": FormatSpec("nrw", gen_dummy),
        "orf": FormatSpec("orf", gen_dummy),
        "ori": FormatSpec("ori", gen_dummy),
        "pef": FormatSpec("pef", gen_dummy),
        "raf": FormatSpec("raf", gen_dummy),
        "raw": FormatSpec("raw", gen_dummy),
        "rw2": FormatSpec("rw2", gen_dummy),
        "rwl": FormatSpec("rwl", gen_dummy),
        "sr2": FormatSpec("sr2", gen_dummy),
        "srf": FormatSpec("srf", gen_dummy),
        "srw": FormatSpec("srw", gen_dummy),
        "x3f": FormatSpec("x3f", gen_dummy),

        # Video formats
        "3gp":  FormatSpec("3gp",  gen_dummy),
        "3gpp": FormatSpec("3gpp", gen_dummy),
        "avi":  FormatSpec("avi",  gen_dummy),
        "flv":  FormatSpec("flv",  gen_dummy),
        "insv": FormatSpec("insv", gen_dummy),
        "mp4":  FormatSpec("mp4",  gen_mp4, needs_ffmpeg=True),
        "m2t":  FormatSpec("m2t",  gen_dummy),
        "m2ts": FormatSpec("m2ts", gen_dummy),
        "mts":  FormatSpec("mts",  gen_dummy),
        "ts":   FormatSpec("ts",   gen_dummy),
        "m4v":  FormatSpec("m4v",  gen_dummy),
        "mkv":  FormatSpec("mkv",  gen_dummy),
        "mpe":  FormatSpec("mpe",  gen_dummy),
        "mpeg": FormatSpec("mpeg", gen_dummy),
        "mpg":  FormatSpec("mpg",  gen_dummy),
        "mov":  FormatSpec("mov",  gen_dummy),
        "mxf":  FormatSpec("mxf",  gen_dummy),
        "vob":  FormatSpec("vob",  gen_dummy),
        "webm": FormatSpec("webm", gen_dummy),
        "wmv":  FormatSpec("wmv",  gen_dummy),
    }


# ----------------------------------------------------------------------------
# Plan + write
# ----------------------------------------------------------------------------


@dataclass
class PlanEntry:
    index: int
    format: str
    duplicate_of: Optional[int]  # index of an earlier entry with the same content


def plan_distribution(
    count: int,
    formats: list[str],
    duplicate_ratio: float,
    rng: random.Random,
) -> list[PlanEntry]:
    """Decide what each file looks like before any IO happens.

    Splits `count` files round-robin across the chosen formats, then marks
    `duplicate_ratio` fraction of them as content-duplicates of an earlier
    entry. A duplicate inherits its source's exact bytes (and therefore hash).
    """
    plan: list[PlanEntry] = []
    for i in range(count):
        plan.append(PlanEntry(index=i, format=formats[i % len(formats)], duplicate_of=None))

    n_dupes = int(count * duplicate_ratio)
    if n_dupes == 0 or count <= 1:
        return plan

    candidates = list(range(count))
    rng.shuffle(candidates)
    dupe_targets = candidates[:n_dupes]
    for target_idx in dupe_targets:
        # Pick any earlier entry with the same format (so the extension stays
        # honest; otherwise a .png would contain .jpg bytes which Mimick rejects
        # at fingerprint time).
        same_fmt_earlier = [
            j for j in range(target_idx) if plan[j].format == plan[target_idx].format
        ]
        if not same_fmt_earlier:
            continue
        plan[target_idx].duplicate_of = rng.choice(same_fmt_earlier)
    return plan


def execute_plan(
    plan: list[PlanEntry],
    out: Path,
    formats: dict[str, FormatSpec],
    size_hint_px: int,
    delay_ms: int,
    cap_bytes: Optional[int],
    rng: random.Random,
) -> dict:
    out.mkdir(parents=True, exist_ok=True)
    cache: dict[int, bytes] = {}
    total_bytes = 0
    total_written = 0
    skipped_cap = 0
    per_format_counts: dict[str, int] = {}
    unique_hashes: set[str] = set()

    for entry in plan:
        if entry.duplicate_of is not None and entry.duplicate_of in cache:
            data = cache[entry.duplicate_of]
        else:
            spec = formats[entry.format]
            data = spec.gen(rng, size_hint_px)
            if not data:
                # Generator opted out (e.g. ffmpeg missing). Skip silently;
                # caller already warned at format-resolution time.
                continue
            cache[entry.index] = data

        if cap_bytes is not None and total_bytes + len(data) > cap_bytes:
            skipped_cap += 1
            continue

        path = out / f"asset_{entry.index:06d}.{entry.format}"
        path.write_bytes(data)
        total_bytes += len(data)
        total_written += 1
        per_format_counts[entry.format] = per_format_counts.get(entry.format, 0) + 1
        unique_hashes.add(hashlib.sha1(data).hexdigest())

        if delay_ms > 0:
            time.sleep(delay_ms / 1000.0)

    return {
        "written": total_written,
        "bytes": total_bytes,
        "unique_hashes": len(unique_hashes),
        "per_format": per_format_counts,
        "skipped_cap": skipped_cap,
    }


# ----------------------------------------------------------------------------
# CLI
# ----------------------------------------------------------------------------


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="Generate test assets for Mimick startup-scan / dedup benchmarks.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    p.add_argument("--out", required=True, type=Path, help="Output directory.")
    p.add_argument("--count", type=int, default=200, help="Total files to generate (default: 200).")
    p.add_argument(
        "--formats",
        type=str,
        default="jpg,png,webp,bmp",
        help="Comma-separated subset of formats, or 'all'. Supports all formats from api_client.rs (e.g. jpg, png, heic, arw, mp4, mkv). Default: jpg,png,webp,bmp.",
    )
    p.add_argument(
        "--duplicate-ratio",
        type=float,
        default=0.0,
        help="Fraction of files (0.0–1.0) that should share content with an earlier file (default: 0.0).",
    )
    p.add_argument(
        "--delay-ms",
        type=int,
        default=0,
        help="Sleep this many milliseconds between writes (default: 0).",
    )
    p.add_argument(
        "--cap-bytes",
        type=int,
        default=None,
        help="Stop appending to total disk usage past this many bytes (default: no cap).",
    )
    p.add_argument(
        "--size-hint-px",
        type=int,
        default=64,
        help="Image edge in pixels; controls per-file size (default: 64).",
    )
    p.add_argument("--seed", type=int, default=42, help="RNG seed for reproducibility (default: 42).")
    return p.parse_args()


def resolve_formats(requested: list[str], registry: dict[str, FormatSpec]) -> list[str]:
    pillow_ok = _need_pillow()
    ffmpeg_ok = shutil.which("ffmpeg") is not None
    resolved: list[str] = []
    for fmt in requested:
        spec = registry.get(fmt)
        if spec is None:
            print(f"warning: unknown format '{fmt}' (skipped)", file=sys.stderr)
            continue
        if spec.needs_pillow and not pillow_ok:
            print(f"warning: format '{fmt}' needs Pillow (pip install Pillow); skipped", file=sys.stderr)
            continue
        if spec.needs_ffmpeg and not ffmpeg_ok:
            print(f"warning: format '{fmt}' needs ffmpeg on PATH; skipped", file=sys.stderr)
            continue
        resolved.append(fmt)
    return resolved


def main() -> int:
    args = parse_args()
    if not (0.0 <= args.duplicate_ratio <= 1.0):
        print("error: --duplicate-ratio must be between 0.0 and 1.0", file=sys.stderr)
        return 2
    if args.count <= 0:
        print("error: --count must be > 0", file=sys.stderr)
        return 2

    registry = build_format_registry()
    if args.formats.strip().lower() == "all":
        requested = list(registry.keys())
    else:
        requested = [f.strip().lower() for f in args.formats.split(",") if f.strip()]
    formats = resolve_formats(requested, registry)
    if not formats:
        print("error: no usable formats after resolving dependencies", file=sys.stderr)
        return 2

    rng = random.Random(args.seed)
    plan = plan_distribution(args.count, formats, args.duplicate_ratio, rng)

    print(
        f"plan: {args.count} files across {formats}, "
        f"{int(args.count * args.duplicate_ratio)} duplicates, seed={args.seed}",
    )
    started = time.monotonic()
    result = execute_plan(
        plan,
        args.out,
        registry,
        args.size_hint_px,
        args.delay_ms,
        args.cap_bytes,
        rng,
    )
    elapsed = time.monotonic() - started

    print()
    print(f"wrote {result['written']} files ({result['bytes'] / 1_048_576:.1f} MiB) in {elapsed:.1f}s")
    print(f"unique content hashes: {result['unique_hashes']}")
    print(f"per-format: {result['per_format']}")
    if result["skipped_cap"]:
        print(f"skipped {result['skipped_cap']} files (cap-bytes reached)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
