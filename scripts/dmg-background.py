#!/usr/bin/env python3
"""Render the dmg background from the landing-page backdrop artwork.

The source is dist/macos/background.png — the same dark noise field used
on the landing page — cover-cropped to the 660x400 pt drag-to-Applications
window so nothing is distorted.

Outputs dist/macos/dmg-background.png (1x) and dmg-background@2x.png;
scripts/package-macos.sh pairs them into a hidpi tiff with tiffutil.
Both outputs are committed so CI never needs Pillow — rerun this only
when changing the artwork.

Usage: python3 scripts/dmg-background.py
"""

import os

from PIL import Image

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(ROOT, "dist/macos/background.png")
OUT = os.path.join(ROOT, "dist/macos")

# Window geometry in points; must match the dmgbuild settings in
# scripts/package-macos.sh.
W, H = 660, 400


def render(s, path):
    """Cover-crop the source artwork to the window at pixel scale s."""
    img = Image.open(SRC).convert("RGB")
    tw, th = W * s, H * s
    scale = max(tw / img.width, th / img.height)
    img = img.resize((round(img.width * scale), round(img.height * scale)), Image.LANCZOS)
    x = (img.width - tw) // 2
    y = (img.height - th) // 2
    img.crop((x, y, x + tw, y + th)).save(path)
    print(f"wrote {os.path.relpath(path, ROOT)} ({tw}x{th})")


render(1, os.path.join(OUT, "dmg-background.png"))
render(2, os.path.join(OUT, "dmg-background@2x.png"))