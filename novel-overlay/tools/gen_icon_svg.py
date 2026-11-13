#!/usr/bin/env python3
"""用 SVG + 无头 Chrome 生成应用图标。

比 `tools/gen_icon.py`（纯标准库、只有纯色圆角矩形）多出渐变、投影、发光，
而且曲线是连续的，缩到 32px 不会发毛。

用法：
    python3 tools/gen_icon_svg.py candidates        # 渲染 3 个方案 + 对比图
    python3 tools/gen_icon_svg.py install <a|b|c>   # 把选中方案装进 app-icon.png 与 src-tauri/icons/
"""

from __future__ import annotations

import math
import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WORK = ROOT / "tools" / "icon"
CANDIDATES = WORK / "candidates"

CHROME = os.environ.get(
    "CHROME",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
)

# ---- 与 src/styles/main.css 的设计令牌保持一致 ------------------------------
BG_TOP = "#2b3038"
BG_MID = "#1d2128"
BG_BOT = "#12141a"
ACCENT = "#6ea8fe"
ACCENT_DEEP = "#4f8bea"
PAPER_TOP = "#e9edf4"
PAPER_BOT = "#b9c2cf"

CANVAS = 1024
# macOS 图标网格：824x824 的主体居中放在 1024x1024 画布上。
BODY = 824
BODY_OFF = (CANVAS - BODY) // 2


def squircle_path(x: float, y: float, w: float, h: float, n: float = 5.0, steps: int = 960) -> str:
    """超椭圆 |X/a|^n + |Y/b|^n = 1，n≈5 是 Apple 连续圆角很好的近似。"""
    cx, cy = x + w / 2, y + h / 2
    a, b = w / 2, h / 2
    pts = []
    for i in range(steps):
        t = 2 * math.pi * i / steps
        ct, st = math.cos(t), math.sin(t)
        px = cx + a * math.copysign(abs(ct) ** (2.0 / n), ct)
        py = cy + b * math.copysign(abs(st) ** (2.0 / n), st)
        pts.append(f"{px:.2f},{py:.2f}")
    return "M " + " L ".join(pts) + " Z"


BODY_PATH = squircle_path(BODY_OFF, BODY_OFF, BODY, BODY)


# ---- 共用片段 ---------------------------------------------------------------


def defs(extra: str = "") -> str:
    return f"""<defs>
  <linearGradient id="body" x1="0" y1="0" x2="0" y2="1">
    <stop offset="0%" stop-color="{BG_TOP}"/>
    <stop offset="52%" stop-color="{BG_MID}"/>
    <stop offset="100%" stop-color="{BG_BOT}"/>
  </linearGradient>
  <radialGradient id="ambient" cx="0.5" cy="0.40" r="0.62">
    <stop offset="0%" stop-color="{ACCENT}" stop-opacity="0.22"/>
    <stop offset="100%" stop-color="{ACCENT}" stop-opacity="0"/>
  </radialGradient>
  <linearGradient id="paper" x1="0.1" y1="0" x2="0.6" y2="1">
    <stop offset="0%" stop-color="{PAPER_TOP}"/>
    <stop offset="100%" stop-color="{PAPER_BOT}"/>
  </linearGradient>
  <linearGradient id="accent" x1="0" y1="0" x2="0" y2="1">
    <stop offset="0%" stop-color="#9cc4ff"/>
    <stop offset="100%" stop-color="{ACCENT_DEEP}"/>
  </linearGradient>
  <linearGradient id="card" x1="0" y1="0" x2="0.25" y2="1">
    <stop offset="0%" stop-color="#b3d3ff" stop-opacity="0.40"/>
    <stop offset="100%" stop-color="{ACCENT_DEEP}" stop-opacity="0.17"/>
  </linearGradient>
  <linearGradient id="gloss" x1="0" y1="0" x2="0" y2="1">
    <stop offset="0%" stop-color="#ffffff" stop-opacity="0.18"/>
    <stop offset="100%" stop-color="#ffffff" stop-opacity="0"/>
  </linearGradient>
  <filter id="soft" x="-40%" y="-40%" width="180%" height="180%">
    <feDropShadow dx="0" dy="16" stdDeviation="20" flood-color="#000000" flood-opacity="0.45"/>
  </filter>
  <filter id="lift" x="-50%" y="-50%" width="200%" height="200%">
    <feDropShadow dx="-14" dy="22" stdDeviation="26" flood-color="#04101f" flood-opacity="0.62"/>
  </filter>
  <filter id="blur" x="-60%" y="-60%" width="220%" height="220%">
    <feGaussianBlur stdDeviation="32"/>
  </filter>
  <clipPath id="bodyClip"><path d="{BODY_PATH}"/></clipPath>
{extra}
</defs>"""


def shell(art: str, extra_defs: str = "") -> str:
    """把画稿包进「主体 + 环境光 + 顶部高光」的壳里。"""
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{CANVAS}" height="{CANVAS}" viewBox="0 0 {CANVAS} {CANVAS}">
{defs(extra_defs)}
  <g filter="url(#soft)"><path d="{BODY_PATH}" fill="url(#body)"/></g>
  <g clip-path="url(#bodyClip)">
    <rect x="{BODY_OFF}" y="{BODY_OFF}" width="{BODY}" height="{BODY}" fill="url(#ambient)"/>
    <rect x="{BODY_OFF}" y="{BODY_OFF}" width="{BODY}" height="330" fill="url(#gloss)"/>
  </g>
  <path d="{BODY_PATH}" fill="none" stroke="#ffffff" stroke-opacity="0.12" stroke-width="2.5"/>
  <g clip-path="url(#bodyClip)">
{art}
  </g>
</svg>"""


# ---- 三个方案 ---------------------------------------------------------------


def concept_a() -> str:
    """A. 悬浮阅读卡 —— 一页纸，右下压着一张半透明浮窗卡。"""
    return shell(
        """  <g filter="url(#soft)">
    <rect x="188" y="196" width="452" height="628" rx="40" fill="url(#paper)"/>
  </g>
  <g fill="#7f8896">
    <rect x="244" y="282" width="340" height="34" rx="17" opacity="0.92"/>
    <rect x="244" y="352" width="282" height="34" rx="17" opacity="0.78"/>
    <rect x="244" y="422" width="324" height="34" rx="17" opacity="0.66"/>
    <rect x="244" y="492" width="238" height="34" rx="17" opacity="0.54"/>
    <rect x="244" y="562" width="290" height="34" rx="17" opacity="0.42"/>
    <rect x="244" y="632" width="200" height="34" rx="17" opacity="0.30"/>
  </g>
  <g filter="url(#blur)" opacity="0.42">
    <rect x="476" y="492" width="400" height="332" rx="52" fill="#6ea8fe"/>
  </g>
  <g filter="url(#lift)">
    <rect x="476" y="492" width="400" height="332" rx="52" fill="url(#card)"
          stroke="#8dbcff" stroke-opacity="0.92" stroke-width="4"/>
  </g>
  <rect x="489" y="505" width="374" height="306" rx="42" fill="none"
        stroke="#ffffff" stroke-opacity="0.14" stroke-width="2"/>
  <g fill="#bcd9ff">
    <rect x="530" y="572" width="294" height="32" rx="16" opacity="0.95"/>
    <rect x="530" y="644" width="232" height="32" rx="16" opacity="0.72"/>
    <rect x="530" y="716" width="170" height="32" rx="16" opacity="0.50"/>
  </g>"""
    )


def concept_b() -> str:
    """B. 脱离的文字行 —— 竖书脊 + 文字行，其中一行「浮」起来变成玻璃条。"""
    return shell(
        """  <rect x="230" y="228" width="72" height="568" rx="36" fill="url(#accent)"/>
  <g fill="#e8eaee">
    <rect x="350" y="268" width="392" height="48" rx="24" opacity="0.95"/>
    <rect x="350" y="368" width="312" height="48" rx="24" opacity="0.68"/>
    <rect x="350" y="588" width="352" height="48" rx="24" opacity="0.40"/>
    <rect x="350" y="688" width="256" height="48" rx="24" opacity="0.26"/>
  </g>
  <g transform="rotate(-8 592 500)" filter="url(#blur)" opacity="0.5">
    <rect x="424" y="434" width="336" height="132" rx="66" fill="#6ea8fe"/>
  </g>
  <g transform="rotate(-8 592 500)" filter="url(#lift)">
    <rect x="424" y="434" width="336" height="132" rx="66" fill="url(#card)"
          stroke="#8dbcff" stroke-opacity="0.92" stroke-width="4"/>
  </g>
  <g transform="rotate(-8 592 500)" fill="#c3ddff">
    <rect x="486" y="470" width="212" height="26" rx="13" opacity="0.95"/>
    <rect x="486" y="510" width="148" height="20" rx="10" opacity="0.62"/>
  </g>"""
    )


def concept_c() -> str:
    """C. 书封与浮窗 —— 一本带蓝色书脊的封面，右上角压一个蓝色描边的浮窗。"""
    return shell(
        """  <g filter="url(#soft)">
    <rect x="226" y="300" width="410" height="490" rx="44" fill="url(#paper)"/>
  </g>
  <g clip-path="url(#coverClip)">
    <rect x="226" y="300" width="40" height="490" fill="url(#accent)"/>
    <rect x="266" y="300" width="5" height="490" fill="#000000" opacity="0.14"/>
    <rect x="624" y="316" width="10" height="458" rx="5" fill="#ffffff" opacity="0.55"/>
    <rect x="636" y="326" width="8" height="438" rx="4" fill="#8b94a2" opacity="0.35"/>
  </g>
  <g fill="#8b94a2" opacity="0.5">
    <rect x="330" y="420" width="212" height="26" rx="13"/>
    <rect x="330" y="478" width="166" height="26" rx="13"/>
    <rect x="330" y="536" width="190" height="26" rx="13"/>
    <rect x="330" y="594" width="140" height="26" rx="13"/>
  </g>
  <g filter="url(#blur)" opacity="0.40">
    <rect x="448" y="188" width="392" height="280" rx="50" fill="#6ea8fe"/>
  </g>
  <g filter="url(#lift)">
    <rect x="448" y="188" width="392" height="280" rx="50" fill="url(#card)"
          stroke="#8dbcff" stroke-opacity="0.92" stroke-width="4"/>
  </g>
  <rect x="461" y="201" width="366" height="254" rx="40" fill="none"
        stroke="#ffffff" stroke-opacity="0.14" stroke-width="2"/>
  <g fill="#bcd9ff">
    <rect x="500" y="246" width="280" height="30" rx="15" opacity="0.95"/>
    <rect x="500" y="304" width="212" height="30" rx="15" opacity="0.72"/>
    <rect x="500" y="362" width="150" height="30" rx="15" opacity="0.50"/>
  </g>""",
        extra_defs="""  <clipPath id="coverClip"><rect x="226" y="300" width="410" height="490" rx="44"/></clipPath>""",
    )


CONCEPTS = {
    "a": ("悬浮阅读卡", concept_a),
    "b": ("脱离的文字行", concept_b),
    "c": ("书封与浮窗", concept_c),
}


# ---- 渲染 -------------------------------------------------------------------


def render_svg(svg: str, out_png: Path) -> None:
    out_png.parent.mkdir(parents=True, exist_ok=True)
    html = WORK / "build" / f"{out_png.stem}.html"
    html.parent.mkdir(parents=True, exist_ok=True)
    html.write_text(
        "<!doctype html><meta charset='utf-8'>"
        "<style>html,body{margin:0;padding:0;width:%dpx;height:%dpx;"
        "background:transparent;overflow:hidden}svg{display:block}</style>%s"
        % (CANVAS, CANVAS, svg),
        encoding="utf-8",
    )
    subprocess.run(
        [
            CHROME,
            "--headless",
            "--disable-gpu",
            "--hide-scrollbars",
            "--no-first-run",
            "--no-default-browser-check",
            f"--user-data-dir={WORK / 'build' / 'chrome-profile'}",
            "--default-background-color=00000000",
            f"--window-size={CANVAS},{CANVAS}",
            f"--screenshot={out_png}",
            html.as_uri(),
        ],
        check=True,
        capture_output=True,
    )


def load_font(size: int):
    from PIL import ImageFont

    for path in (
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/Supplemental/Songti.ttc",
        "/System/Library/Fonts/Helvetica.ttc",
    ):
        if Path(path).exists():
            try:
                return ImageFont.truetype(path, size)
            except Exception:
                continue
    return ImageFont.load_default()


def contact_sheet(pngs: dict[str, Path]) -> Path:
    """把每个方案按 256 / 96 / 48 / 32 四档排出来，看小尺寸还认不认得出。"""
    from PIL import Image, ImageDraw

    pad, label_w, header_h, art_h = 40, 250, 132, 256
    sizes = [256, 96, 48, 32]
    row_h = art_h + pad
    width = label_w + sum(s + pad for s in sizes) + pad
    height = header_h + row_h * len(pngs) + pad

    sheet = Image.new("RGBA", (width, height), (14, 16, 20, 255))
    draw = ImageDraw.Draw(sheet)
    font = load_font(28)
    font_small = load_font(20)

    draw.text((pad, 30), "应用图标方案对比", font=font, fill=(232, 234, 238, 255))
    x = label_w
    for s in sizes:
        draw.text((x, 92), f"{s}px", font=font_small, fill=(154, 163, 175, 255))
        x += s + pad

    for i, (key, path) in enumerate(pngs.items()):
        name = CONCEPTS[key][0]
        top = header_h + i * row_h
        draw.text(
            (pad, top + 96),
            f"{key.upper()}. {name}",
            font=font,
            fill=(110, 168, 254, 255),
        )
        icon = Image.open(path).convert("RGBA")
        x = label_w
        for s in sizes:
            thumb = icon.resize((s, s), Image.LANCZOS)
            sheet.alpha_composite(thumb, (x, top + art_h - s))
            x += s + pad

    out = CANDIDATES / "preview.png"
    sheet.save(out)
    return out


# ---- 安装 -------------------------------------------------------------------


def install(key: str) -> None:
    svg = CONCEPTS[key][1]()
    src = CANDIDATES / f"{key}.png"
    render_svg(svg, src)

    (ROOT / "app-icon.png").write_bytes(src.read_bytes())

    iconset = WORK / "build" / "app.iconset"
    if iconset.exists():
        shutil.rmtree(iconset)
    iconset.mkdir(parents=True)

    pairs = [
        (16, "icon_16x16.png"),
        (32, "icon_16x16@2x.png"),
        (32, "icon_32x32.png"),
        (64, "icon_32x32@2x.png"),
        (128, "icon_128x128.png"),
        (256, "icon_128x128@2x.png"),
        (256, "icon_256x256.png"),
        (512, "icon_256x256@2x.png"),
        (512, "icon_512x512.png"),
        (1024, "icon_512x512@2x.png"),
    ]
    for size, name in pairs:
        subprocess.run(
            ["sips", "-z", str(size), str(size), str(src), "--out", str(iconset / name)],
            check=True,
            capture_output=True,
        )

    icons_dir = ROOT / "src-tauri" / "icons"
    icons_dir.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["iconutil", "-c", "icns", str(iconset), "-o", str(icons_dir / "icon.icns")],
        check=True,
        capture_output=True,
    )
    for size, name in ((32, "32x32.png"), (128, "128x128.png"), (256, "128x128@2x.png")):
        subprocess.run(
            ["sips", "-z", str(size), str(size), str(src), "--out", str(icons_dir / name)],
            check=True,
            capture_output=True,
        )

    print(f"installed concept {key} ({CONCEPTS[key][0]})")
    for p in (ROOT / "app-icon.png", icons_dir / "icon.icns"):
        print(f"  {p.relative_to(ROOT)}  {p.stat().st_size} B")


def main() -> None:
    action = sys.argv[1] if len(sys.argv) > 1 else "candidates"

    if action == "candidates":
        CANDIDATES.mkdir(parents=True, exist_ok=True)
        pngs = {}
        for key, (_, build) in CONCEPTS.items():
            out = CANDIDATES / f"{key}.png"
            render_svg(build(), out)
            pngs[key] = out
            print(f"rendered {out.relative_to(ROOT)}")
        print(f"preview -> {contact_sheet(pngs).relative_to(ROOT)}")
    elif action == "install":
        key = sys.argv[2] if len(sys.argv) > 2 else "a"
        if key not in CONCEPTS:
            sys.exit(f"unknown concept {key!r}; pick one of {sorted(CONCEPTS)}")
        install(key)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
