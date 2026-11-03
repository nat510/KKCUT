"""Draws the KKCUT logo: KKBOX's K turned into scissors on a cyan tile.

The scissors open exactly like the K: the blades leave the pivot at +-45
degrees (the K's arms), and the finger rings stand where the K's stem is.

    python -I tools/logo.py assets

writes assets/logo.svg, assets/icon.png (256 px), assets/logo-1024.png and
assets/app.ico.
"""

import math
import sys
from pathlib import Path

from PIL import Image, ImageDraw

# Everything is laid out on a 220 x 220 canvas.
SIZE = 220
RADIUS = 48  # corner radius of the tile
TOP, BOTTOM = (26, 222, 248), (9, 205, 246)  # tile gradient
WHITE = (255, 255, 255)

# The scissors are drawn at unit size around the pivot, then scaled by SCALE.
SCALE = 1.6
ANGLE = 45.0  # each blade's angle from horizontal, as the K's arms
RING_AT, RING_R, RING_W = 32.0, 12.0, 8.0  # ring centre distance, radius, stroke
SHANK_W = 10.0
# A disk under the pivot screw joins the blades and shanks cleanly.
HUB_R, SCREW_R = 11.0, 4.5
# One blade pointing right from the pivot (+y faces the other blade): a
# curved back and a straight cutting edge meeting at the tip.
BLADE_BACK = ((-4, -13), (26, -14), (52, -9), (66, 0))
BLADE_EDGE_END = (-4, 8)
# The scissors reach 38.6 left of the pivot (rings) and 48 right (tips);
# the pivot sits left of the tile's centre so they are centred.
PIVOT = (SIZE / 2 - (48 - 38.6) / 2 * SCALE, SIZE / 2)


def rotate(points, degrees):
    """`points` (unit size, around the pivot) turned, scaled and placed."""
    t = math.radians(degrees)
    c, s = math.cos(t), math.sin(t)
    ox, oy = PIVOT
    return [(ox + (x * c - y * s) * SCALE, oy + (x * s + y * c) * SCALE) for x, y in points]


def cubic(p0, p1, p2, p3, n=24):
    out = []
    for i in range(n + 1):
        t = i / n
        u = 1 - t
        out.append(
            (
                u**3 * p0[0] + 3 * u * u * t * p1[0] + 3 * u * t * t * p2[0] + t**3 * p3[0],
                u**3 * p0[1] + 3 * u * u * t * p1[1] + 3 * u * t * t * p2[1] + t**3 * p3[1],
            )
        )
    return out


def blades():
    """(blade polygon, shank start, shank end, ring centre) for both blades."""
    out = []
    for sign in (-1, 1):  # upper blade, lower blade
        deg = sign * ANGLE
        poly = rotate(cubic(*BLADE_BACK) + [BLADE_EDGE_END], deg)
        shank = rotate([(0, 0), (-(RING_AT - RING_R), 0)], deg)
        ring = rotate([(-RING_AT, 0)], deg)[0]
        out.append((poly, shank[0], shank[1], ring))
    return out


# ------------------------------------------------------------------ SVG


def svg():
    f = lambda p: f"{p[0]:.2f},{p[1]:.2f}"
    k = SCALE
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {SIZE} {SIZE}">',
        '<defs><linearGradient id="bg" x1="0" y1="0" x2="0" y2="1">'
        f'<stop offset="0" stop-color="rgb{TOP}"/><stop offset="1" stop-color="rgb{BOTTOM}"/>'
        "</linearGradient></defs>",
        f'<rect width="{SIZE}" height="{SIZE}" rx="{RADIUS}" fill="url(#bg)"/>',
        '<g fill="#fff">',
    ]
    for poly, s0, s1, ring in blades():
        parts.append(f'<polygon points="{" ".join(f(p) for p in poly)}"/>')
        parts.append(
            f'<line x1="{s0[0]:.2f}" y1="{s0[1]:.2f}" x2="{s1[0]:.2f}" y2="{s1[1]:.2f}" '
            f'stroke="#fff" stroke-width="{SHANK_W * k:.2f}" stroke-linecap="round"/>'
        )
        parts.append(
            f'<circle cx="{ring[0]:.2f}" cy="{ring[1]:.2f}" r="{RING_R * k:.2f}" fill="none" '
            f'stroke="#fff" stroke-width="{RING_W * k:.2f}"/>'
        )
    parts.append(f'<circle cx="{PIVOT[0]:.2f}" cy="{PIVOT[1]:.2f}" r="{HUB_R * k:.2f}"/>')
    parts.append("</g>")
    parts.append(f'<circle cx="{PIVOT[0]:.2f}" cy="{PIVOT[1]:.2f}" r="{SCREW_R * k:.2f}" fill="rgb{BOTTOM}"/>')
    parts.append("</svg>")
    return "\n".join(parts) + "\n"


# ------------------------------------------------------------------ raster


def render(px):
    """The logo at `px` x `px`, drawn 8x larger and scaled down for smooth edges."""
    ss = 8
    big = px * ss
    k = big / SIZE
    s = lambda p: (p[0] * k, p[1] * k)

    gradient = Image.new("RGBA", (1, big))
    for y in range(big):
        t = y / (big - 1)
        gradient.putpixel((0, y), tuple(round(a + (b - a) * t) for a, b in zip(TOP, BOTTOM)) + (255,))
    tile = gradient.resize((big, big))
    mask = Image.new("L", (big, big), 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, big - 1, big - 1), RADIUS * k, fill=255)
    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    img.paste(tile, (0, 0), mask)
    d = ImageDraw.Draw(img)

    def disk(centre, r, fill):
        x, y = s(centre)
        r *= k
        d.ellipse((x - r, y - r, x + r, y + r), fill=fill)

    for poly, s0, s1, ring in blades():
        d.polygon([s(p) for p in poly], fill=WHITE)
        w = SHANK_W * SCALE
        d.line([s(s0), s(s1)], fill=WHITE, width=round(w * k))
        for end in (s0, s1):
            disk(end, w / 2, WHITE)
        disk(ring, (RING_R + RING_W / 2) * SCALE, WHITE)
        # The ring's hole shows the tile behind it.
        hole = Image.new("L", (big, big), 0)
        x, y = s(ring)
        r = (RING_R - RING_W / 2) * SCALE * k
        ImageDraw.Draw(hole).ellipse((x - r, y - r, x + r, y + r), fill=255)
        img.paste(tile, (0, 0), hole)
    disk(PIVOT, HUB_R * SCALE, WHITE)
    disk(PIVOT, SCREW_R * SCALE, BOTTOM + (255,))
    return img.resize((px, px), Image.LANCZOS)


def main():
    out = Path(sys.argv[1] if len(sys.argv) > 1 else "assets")
    out.mkdir(parents=True, exist_ok=True)
    (out / "logo.svg").write_text(svg(), encoding="utf-8")
    render(256).save(out / "icon.png")
    sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    frames = [render(n) for n in sizes]
    frames[-1].save(out / "app.ico", sizes=[(n, n) for n in sizes], append_images=frames[:-1])
    render(1024).save(out / "logo-1024.png")


if __name__ == "__main__":
    main()
