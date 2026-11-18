#!/usr/bin/env python3
"""生成应用图标源图（1024x1024 RGBA PNG），纯标准库，不依赖 Pillow。

用法：python3 tools/gen_icon.py app-icon.png
之后用 `npx tauri icon app-icon.png` 生成全套图标。
"""

import struct
import sys
import zlib

SIZE = 1024

BG = (28, 32, 39)
ACCENT = (110, 168, 254)
LINE = (232, 234, 238)


def coverage(x, y, x0, y0, x1, y1, radius):
    """圆角矩形覆盖度，半径处做 1px 抗锯齿。"""
    if x < x0 - 1 or x > x1 + 1 or y < y0 - 1 or y > y1 + 1:
        return 0.0
    cx = min(max(x, x0 + radius), x1 - radius)
    cy = min(max(y, y0 + radius), y1 - radius)
    dx = x - cx
    dy = y - cy
    distance = (dx * dx + dy * dy) ** 0.5
    return max(0.0, min(1.0, radius + 0.5 - distance))


# (x0, y0, x1, y1, radius, color, opacity)
SHAPES = [
    (0, 0, SIZE - 1, SIZE - 1, 224, BG, 1.0),
    (176, 268, 216, 756, 20, ACCENT, 1.0),
    (280, 300, 848, 344, 22, LINE, 0.94),
    (280, 420, 760, 464, 22, LINE, 0.80),
    (280, 540, 812, 584, 22, LINE, 0.80),
    (280, 660, 640, 704, 22, LINE, 0.62),
]


def blend(dst, src, alpha):
    return tuple(round(s * alpha + d * (1 - alpha)) for s, d in zip(src, dst))


def render():
    rows = []
    for py in range(SIZE):
        row = bytearray()
        for px in range(SIZE):
            color = (0, 0, 0)
            alpha = 0.0
            for x0, y0, x1, y1, radius, shape_color, opacity in SHAPES:
                hit = coverage(px + 0.5, py + 0.5, x0, y0, x1, y1, radius)
                if hit <= 0.0:
                    continue
                a = hit * opacity
                color = blend(color, shape_color, a)
                alpha = a + alpha * (1 - a)
            if alpha <= 0.0:
                row += bytes([0, 0, 0, 0])
            else:
                row += bytes([color[0], color[1], color[2], round(alpha * 255)])
        rows.append(bytes(row))
    return rows


def write_png(path, rows):
    raw = b"".join(b"\x00" + row for row in rows)

    def chunk(tag, data):
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    header = struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0)
    payload = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )
    with open(path, "wb") as handle:
        handle.write(payload)


def main():
    target = sys.argv[1] if len(sys.argv) > 1 else "app-icon.png"
    write_png(target, render())
    print(f"wrote {target} ({SIZE}x{SIZE})")


if __name__ == "__main__":
    main()
