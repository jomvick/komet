#!/usr/bin/env python3
"""Regenerate multi-platform icons from the single master artwork dist/logo.png.

Outputs:
- dist/komet.png: 1024x1024 RGBA squircle with transparent outer corners (Linux, AppImage, UI)
- dist/macos/icon-1024.png: 1024x1024 RGBA squircle following Apple HIG margins & subtle drop shadow
- dist/windows/komet.ico: multi-resolution ICO (16, 24, 32, 48, 64, 128, 256) with alpha channel
- dist/icons/hicolor/<size>x<size>/apps/komet.png: standard FreeDesktop icon hierarchy

Usage:
  python3 scripts/gen-icons.py
"""

import os
from collections import deque
from PIL import Image, ImageFilter

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC_LOGO = os.path.join(ROOT, "dist", "logo.png")

HICOLOR_SIZES = [16, 24, 32, 48, 64, 128, 256, 512, 1024]
ICO_SIZES = [(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)]


def extract_squircle(master_path: str) -> Image.Image:
    """Extract squircle with anti-aliased alpha transparency in the outer corners."""
    img = Image.open(master_path).convert("RGBA")
    w, h = img.size
    pixels = img.load()

    # Mask: 255 inside squircle, 0 outside
    mask = Image.new("L", (w, h), 255)
    mask_px = mask.load()

    visited = [[False] * h for _ in range(w)]
    q = deque([(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)])
    for x, y in q:
        visited[x][y] = True
        mask_px[x, y] = 0

    # Flood fill connected black corner pixels
    while q:
        cx, cy = q.popleft()
        for dx, dy in [(-1, 0), (1, 0), (0, -1), (0, 1)]:
            nx, ny = cx + dx, cy + dy
            if 0 <= nx < w and 0 <= ny < h and not visited[nx][ny]:
                # Pixels in the outer corners are black (<= 2)
                if max(pixels[nx, ny][:3]) <= 2:
                    visited[nx][ny] = True
                    mask_px[nx, ny] = 0
                    q.append((nx, ny))

    # Anti-alias mask edge
    smooth_mask = mask.filter(ImageFilter.GaussianBlur(radius=1.5))
    img.putalpha(smooth_mask)
    return img


def generate_macos_icon(squircle: Image.Image) -> Image.Image:
    """Generate 1024x1024 macOS app icon according to Apple HIG grid & drop shadow."""
    canvas = Image.new("RGBA", (1024, 1024), (0, 0, 0, 0))
    # Standard macOS squircle body size (~860px)
    body_size = 860
    resized = squircle.resize((body_size, body_size), Image.LANCZOS)

    # Subtle drop shadow
    shadow_mask = resized.split()[-1].filter(ImageFilter.GaussianBlur(radius=16))
    shadow = Image.new("RGBA", (body_size, body_size), (0, 0, 0, 110))
    shadow.putalpha(shadow_mask)

    offset_x = (1024 - body_size) // 2
    offset_y = (1024 - body_size) // 2

    # Paste shadow slightly shifted down (y + 12)
    canvas.paste(shadow, (offset_x, offset_y + 12), shadow)
    # Paste body
    canvas.paste(resized, (offset_x, offset_y), resized)
    return canvas


def main():
    assert os.path.isfile(SRC_LOGO), f"Master logo not found at {SRC_LOGO}"
    print(f"Loading master: {SRC_LOGO}")
    squircle = extract_squircle(SRC_LOGO)

    # 1. dist/komet.png (1024x1024 RGBA)
    komet_1024 = squircle.resize((1024, 1024), Image.LANCZOS)
    komet_png_path = os.path.join(ROOT, "dist", "komet.png")
    komet_1024.save(komet_png_path, "PNG")
    print(f"Generated: {os.path.relpath(komet_png_path, ROOT)}")

    # 2. dist/macos/icon-1024.png
    macos_icon = generate_macos_icon(squircle)
    macos_icon_path = os.path.join(ROOT, "dist", "macos", "icon-1024.png")
    macos_icon.save(macos_icon_path, "PNG")
    print(f"Generated: {os.path.relpath(macos_icon_path, ROOT)}")

    # 3. dist/windows/komet.ico (multi-resolution)
    ico_path = os.path.join(ROOT, "dist", "windows", "komet.ico")
    komet_1024.save(ico_path, format="ICO", sizes=ICO_SIZES)
    print(f"Generated: {os.path.relpath(ico_path, ROOT)} (sizes: {ICO_SIZES})")

    # 4. Standard XDG hicolor icon hierarchy
    for size in HICOLOR_SIZES:
        hicolor_dir = os.path.join(ROOT, "dist", "icons", "hicolor", f"{size}x{size}", "apps")
        os.makedirs(hicolor_dir, exist_ok=True)
        out_path = os.path.join(hicolor_dir, "komet.png")
        resized = squircle.resize((size, size), Image.LANCZOS)
        resized.save(out_path, "PNG")
    print(f"Generated XDG hicolor icons: {HICOLOR_SIZES}")


if __name__ == "__main__":
    main()
