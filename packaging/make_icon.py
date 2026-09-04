#!/usr/bin/env python3
"""Draw the app icon and build an .icns from it.

The mark is the bridge itself: console faders at the top, a DAW timeline with a
marker at the bottom, and a two-way arrow linking them.
"""
import math, os, subprocess, sys
from PIL import Image, ImageDraw

S = 1024
BG_TOP, BG_BOTTOM = (38, 43, 50), (23, 26, 30)
ACCENT = (72, 168, 190)
LIGHT = (226, 230, 236)
DIM = (120, 129, 141)

def rounded_mask(size, radius):
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, size - 1, size - 1], radius, fill=255)
    return mask

def background():
    img = Image.new("RGB", (S, S))
    draw = ImageDraw.Draw(img)
    for y in range(S):
        t = y / (S - 1)
        draw.line([(0, y), (S, y)], fill=tuple(
            round(a + (b - a) * t) for a, b in zip(BG_TOP, BG_BOTTOM)))
    return img

def draw_mark(img):
    d = ImageDraw.Draw(img, "RGBA")

    # Top half - the console: four faders with caps at different positions.
    top, height, width = 232, 300, 40
    xs = [286, 418, 550, 682]
    caps = [0.30, 0.66, 0.16, 0.50]
    for x, cap in zip(xs, caps):
        d.rounded_rectangle([x, top, x + width, top + height], width // 2,
                            fill=(255, 255, 255, 40))
        cy = top + int(height * cap)
        d.rounded_rectangle([x - 20, cy - 26, x + width + 20, cy + 26], 24, fill=LIGHT)

    # Bottom half - the session: a timeline rule with a marker on it.
    ty = 728
    d.rounded_rectangle([246, ty - 8, 778, ty + 8], 8, fill=DIM)
    for x in range(286, 779, 82):
        d.rounded_rectangle([x - 5, ty + 26, x + 5, ty + 52], 5, fill=(255, 255, 255, 55))
    mx = 362
    d.rounded_rectangle([mx - 8, ty - 150, mx + 8, ty + 18], 8, fill=LIGHT)
    d.polygon([(mx + 8, ty - 150), (mx + 150, ty - 110), (mx + 8, ty - 70)], fill=ACCENT)
    return img

def main():
    out_dir = os.path.dirname(os.path.abspath(__file__))
    img = draw_mark(background())
    img.putalpha(rounded_mask(S, 228))
    png = os.path.join(out_dir, "icon.png")
    img.save(png)

    iconset = os.path.join(out_dir, "icon.iconset")
    os.makedirs(iconset, exist_ok=True)
    for size in (16, 32, 64, 128, 256, 512):
        for scale in (1, 2):
            px = size * scale
            name = f"icon_{size}x{size}{'@2x' if scale == 2 else ''}.png"
            img.resize((px, px), Image.LANCZOS).save(os.path.join(iconset, name))
    subprocess.run(["iconutil", "-c", "icns", iconset, "-o",
                    os.path.join(out_dir, "AppIcon.icns")], check=True)
    print("wrote", png, "and AppIcon.icns")

if __name__ == "__main__":
    sys.exit(main())
