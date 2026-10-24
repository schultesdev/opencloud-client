#!/usr/bin/env python3
"""Generate the packaging icons from the embedded application icon.

The single source of truth for the app icon is the Rust module
`crates/oc-app/assets/icon.rs` (64x64 RGBA), which the window and the tray
use at runtime. Packaging needs the same icon as PNG (Linux), ICO (Windows),
and ICNS (macOS); this script derives them so they cannot drift apart.

Usage:
    python3 packaging/icons/generate.py

Writes into the repository:
    packaging/icons/opencloud-sync-64.png      64x64   (hicolor 64x64)
    packaging/icons/opencloud-sync-128.png     128x128 (hicolor 128x128)
    packaging/icons/opencloud-sync-256.png     256x256 (hicolor 256x256)
    packaging/icons/opencloud-sync.ico         16..256 (Windows exe/installer)
    packaging/icons/OpenCloudSync.icns        32..512 (macOS app bundle)

Only the Python standard library is required.
"""

from __future__ import annotations

import re
import struct
import sys
import zlib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
ICON_RS = REPO_ROOT / "crates" / "oc-app" / "assets" / "icon.rs"
OUT_DIR = Path(__file__).resolve().parent


def load_rgba() -> tuple[int, list[tuple[int, int, int, int]]]:
    """Parse `ICON_WIDTH`, `ICON_HEIGHT`, and `ICON_RGBA` from icon.rs."""
    text = ICON_RS.read_text(encoding="utf-8")
    width = int(re.search(r"pub const ICON_WIDTH: u32 = (\d+);", text).group(1))
    height = int(re.search(r"pub const ICON_HEIGHT: u32 = (\d+);", text).group(1))
    match = re.search(
        r"pub const ICON_RGBA: &\[u8; [^\]]*\] = &\[(.*?)\];",
        text,
        re.DOTALL,
    )
    values = [int(v) for v in re.findall(r"\d+", match.group(1))]
    if len(values) != width * height * 4:
        raise SystemExit(
            f"icon.rs: expected {width * height * 4} bytes, found {len(values)}"
        )
    pixels = [
        (values[i], values[i + 1], values[i + 2], values[i + 3])
        for i in range(0, len(values), 4)
    ]
    return width, pixels


def bilinear_scale(
    pixels: list[tuple[int, int, int, int]], size: int, target: int
) -> list[tuple[int, int, int, int]]:
    """Bilinear rescale of a `size`x`size` image to `target`x`target`."""
    if size == target:
        return pixels
    ratio = size / target
    out: list[tuple[int, int, int, int]] = []
    for y in range(target):
        src_y = min((y + 0.5) * ratio - 0.5, size - 1)
        y0 = int(src_y)
        y1 = min(y0 + 1, size - 1)
        fy = src_y - y0
        for x in range(target):
            src_x = min((x + 0.5) * ratio - 0.5, size - 1)
            x0 = int(src_x)
            x1 = min(x0 + 1, size - 1)
            fx = src_x - x0
            px = []
            for c in range(4):
                top = pixels[y0 * size + x0][c] * (1 - fx) + pixels[y0 * size + x1][c] * fx
                bot = pixels[y1 * size + x0][c] * (1 - fx) + pixels[y1 * size + x1][c] * fx
                px.append(int(round(top * (1 - fy) + bot * fy)))
            out.append((px[0], px[1], px[2], px[3]))
    return out


def premultiply(pixels: list[tuple[int, int, int, int]]) -> list[tuple[int, int, int, int]]:
    return [(r * a // 255, g * a // 255, b * a // 255, a) for (r, g, b, a) in pixels]


def write_png(path: Path, size: int, pixels: list[tuple[int, int, int, int]]) -> None:
    """Minimal PNG writer (color type 6, RGBA, non-interlaced)."""
    raw = bytearray()
    for i in range(size):
        row = pixels[i * size : (i + 1) * size]
        raw.append(0)  # no filter
        for (r, g, b, a) in row:
            raw.extend((r, g, b, a))

    def chunk(kind: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + kind
            + data
            + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
        )

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )
    path.write_bytes(png)


def write_ico(path: Path, images: list[tuple[int, list[tuple[int, int, int, int]]]]) -> None:
    """ICO container embedding each size as a PNG (Vista+ format)."""
    entries = bytearray()
    blobs = bytearray()
    for index, (size, pixels) in enumerate(images):
        blob_path = OUT_DIR / f".tmp-ico-{index}.png"
        write_png(blob_path, size, pixels)
        blob = blob_path.read_bytes()
        blob_path.unlink()
        width_byte = 0 if size >= 256 else size
        entries.extend(
            struct.pack(
                "<BBBBHHII",
                width_byte,
                width_byte,
                0,  # color palette
                0,  # reserved
                1,  # color planes
                32,  # bits per pixel
                len(blob),
                6 + 16 * len(images) + len(blobs),
            )
        )
        blobs.extend(blob)
    path.write_bytes(
        struct.pack("<HHH", 0, 1, len(images)) + entries + blobs
    )


def write_icns(
    path: Path,
    width: int,
    pixels: list[tuple[int, int, int, int]],
    images: dict[str, int],
) -> None:
    """ICNS container; the listed OSTypes all hold PNG data."""
    body = bytearray()
    for ostype, size in images.items():
        blob_path = OUT_DIR / f".tmp-icns-{ostype}.png"
        write_png(blob_path, size, bilinear_scale(pixels, width, size))
        blob = blob_path.read_bytes()
        blob_path.unlink()
        body.extend(ostype.encode("ascii") + struct.pack(">I", len(blob) + 8) + blob)
    path.write_bytes(b"icns" + struct.pack(">I", len(body) + 8) + body)


def main() -> None:
    width, pixels = load_rgba()
    height = len(pixels) // width
    if width != height:
        raise SystemExit("icon.rs: image is not square as expected")

    write_png(OUT_DIR / "opencloud-sync-64.png", 64, pixels)
    for size in (128, 256):
        write_png(
            OUT_DIR / f"opencloud-sync-{size}.png",
            size,
            bilinear_scale(pixels, width, size),
        )
    write_ico(
        OUT_DIR / "opencloud-sync.ico",
        [(size, bilinear_scale(pixels, width, size)) for size in (16, 24, 32, 48, 64, 128, 256)],
    )
    write_icns(
        OUT_DIR / "OpenCloudSync.icns",
        width,
        pixels,
        # ic11=32, ic07=128, ic08=256, ic09=512, ic10=1024
        {"ic11": 32, "ic07": 128, "ic08": 256, "ic09": 512},
    )
    print("icons written to", OUT_DIR)


if __name__ == "__main__":
    sys.exit(main())
