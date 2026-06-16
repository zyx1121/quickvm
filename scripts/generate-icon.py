#!/usr/bin/env python3
"""QuicKVM 圖示 — 仿 zyx 品牌處理（深色 squircle + 白 mark + glow），但 mark 是閃電。

從一個閃電多邊形（直接 PIL 畫，免 SVG/qlmanage）產三樣：
  - macos-app/Resources/AppIcon.icns   mac app 圖示（Finder / 設定 / 輔助使用清單）
  - macos-app/Resources/BoltMark.png   mac menubar template（白+alpha，SwiftUI/NSImage tint）
  - crates/app/assets/quickvm.ico      windows tray + exe 圖示（多尺寸，原生 render 每個尺寸保清晰）

用法：python3 scripts/generate-icon.py   （需 Pillow；.icns 需 macOS 的 iconutil）
"""
import os
import subprocess
import tempfile
from PIL import Image, ImageDraw, ImageFilter

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.join(HERE, "..")
ICNS = os.path.join(ROOT, "macos-app", "Resources", "AppIcon.icns")
MARK = os.path.join(ROOT, "macos-app", "Resources", "BoltMark.png")
ICO = os.path.join(ROOT, "crates", "app", "assets", "quickvm.ico")

# 閃電多邊形（feather "zap"，24 單位座標）→ 正規化 0..1。
_PTS = [(13, 2), (3, 14), (12, 14), (11, 22), (21, 10), (12, 10)]
_MINX, _MAXX = 3, 21
_MINY, _MAXY = 2, 22


def bolt_mask(size, coverage=0.72, ss=4):
    """回傳閃電的 L mask（255=閃電），置中、佔畫布 coverage、supersample 抗鋸齒。"""
    big = size * ss
    bw, bh = (_MAXX - _MINX), (_MAXY - _MINY)
    scale = (big * coverage) / max(bw, bh)
    ow = (big - bw * scale) / 2 - _MINX * scale
    oh = (big - bh * scale) / 2 - _MINY * scale
    pts = [(x * scale + ow, y * scale + oh) for (x, y) in _PTS]
    m = Image.new("L", (big, big), 0)
    ImageDraw.Draw(m).polygon(pts, fill=255)
    return m.resize((size, size), Image.LANCZOS)


def white_mark(size):
    """白色閃電 + 透明底（menubar template 用）。"""
    a = bolt_mask(size)
    img = Image.new("RGBA", (size, size), (255, 255, 255, 0))
    img.putalpha(a)
    return img.crop(img.getbbox())


def app_icon(size):
    """深色圓角底（垂直漸層）+ 白閃電 + 柔光，原生 render 指定尺寸。"""
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    bg = Image.new("RGBA", (size, size))
    for y in range(size):
        t = y / size
        r = int(0x20 * (1 - t) + 0x0C * t)
        g = int(0x23 * (1 - t) + 0x0D * t)
        b = int(0x2A * (1 - t) + 0x10 * t)
        for x in range(size):
            bg.putpixel((x, y), (r, g, b, 255))
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        [0, 0, size - 1, size - 1], radius=int(size * 0.225), fill=255
    )
    img.paste(bg, (0, 0), mask)

    bolt_a = bolt_mask(size, coverage=0.56)
    bolt = Image.new("RGBA", (size, size), (255, 255, 255, 0))
    bolt.putalpha(bolt_a)
    glow = bolt.filter(ImageFilter.GaussianBlur(max(1, size // 50)))
    img = Image.alpha_composite(img, glow)
    img = Image.alpha_composite(img, bolt)
    return img


# --- BoltMark.png（512 高清 template）---
os.makedirs(os.path.dirname(MARK), exist_ok=True)
white_mark(512).save(MARK)
print("saved", MARK)

# --- quickvm.ico（每尺寸原生 render，小圖才清晰）---
os.makedirs(os.path.dirname(ICO), exist_ok=True)
ico_sizes = [16, 24, 32, 48, 64, 128, 256]
imgs = [app_icon(s) for s in ico_sizes]
imgs[-1].save(ICO, format="ICO", sizes=[(s, s) for s in ico_sizes], append_images=imgs[:-1])
print("saved", ICO)

# --- AppIcon.icns（iconset → iconutil）---
base = app_icon(1024)
with tempfile.TemporaryDirectory() as tmp:
    iconset = os.path.join(tmp, "AppIcon.iconset")
    os.makedirs(iconset)
    for px, name in [
        (16, "icon_16x16.png"), (32, "icon_16x16@2x.png"),
        (32, "icon_32x32.png"), (64, "icon_32x32@2x.png"),
        (128, "icon_128x128.png"), (256, "icon_128x128@2x.png"),
        (256, "icon_256x256.png"), (512, "icon_256x256@2x.png"),
        (512, "icon_512x512.png"), (1024, "icon_512x512@2x.png"),
    ]:
        base.resize((px, px), Image.LANCZOS).save(os.path.join(iconset, name))
    subprocess.run(["iconutil", "-c", "icns", "-o", ICNS, iconset], check=True)
print("saved", ICNS)
