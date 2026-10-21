#!/usr/bin/env python3
"""Render Entangled's own procedural identity and Retina DMG artwork."""
import argparse
import math
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont, ImageFilter

# Deep Field, as named in apps/manager/src/theme.rs: the DMG window is the
# first screen of the product, so it wears the manager's colours, not its own.
BG_DEEP = (0x0a, 0x0d, 0x12)
CARD = (0x11, 0x16, 0x1e)
STROKE = (0x1f, 0x27, 0x33)
STROKE_STRONG = (0x2c, 0x36, 0x45)
DIVIDER = (0x1a, 0x21, 0x2b)
TEXT = (0xe4, 0xe8, 0xee)
TEXT_DIM = (0x98, 0xa2, 0xb3)
TEXT_FAINT = (0x7d, 0x86, 0x95)
ACCENT = (0x8c, 0xc8, 0xe0)
PRIMARY = (0xdc, 0xe6, 0xef)
STAR = (0x4f, 0x56, 0x5c)

# The Finder window's content area in points, and its Retina scale. The window
# bounds and both icon positions live in make-macos-dmg.py (800x548 window,
# icons centred at (220, 306) and (580, 306)); the cards and label plates
# below are drawn around those two points.
WIDTH, HEIGHT, RETINA = 800, 520, 2
# Drawn at this many pixels per point and filtered down to RETINA, because
# Pillow's shapes are not anti-aliased.
SUPERSAMPLE = 4

FONT = "/System/Library/Fonts/SFNS.ttf"
MONO = "/System/Library/Fonts/SFNSMono.ttf"


# The mark, as apps/manager/src/logo.rs draws it: two rings that pass over and
# under each other, designed on a 48-unit square. Keep the numbers in step.
UNITS = 48.0
RING_RADIUS = 12.0
RING_OFFSET = 5.0
RING_GAP = 1.8
ARC_HALF = math.degrees(.35)
ARC_OVERLAP = math.degrees(.07)


def ring_stroke(size):
    """Stroke in units for a mark `size` pixels wide: heavier as it shrinks."""
    return 3.4 if size >= 96 else 3.8 if size >= 40 else 4.4 if size >= 24 else 5.0


def linked_mark(draw, cx, cy, size, background, seen=None):
    """Draws the mark `size` pixels wide centred on (cx, cy). The gap where one
    ring passes over the other is painted in `background`. `seen` is the width
    it ends up at after supersampling, which is what picks the stroke."""
    unit = size / UNITS
    r, width = RING_RADIUS * unit, ring_stroke(seen or size) * unit
    gap = RING_GAP * width / 3.4
    left, right = (cx - RING_OFFSET * unit, cy), (cx + RING_OFFSET * unit, cy)

    def ring(c, color):
        h = r + width / 2
        draw.ellipse((c[0] - h, c[1] - h, c[0] + h, c[1] + h), outline=color,
                     width=round(width))

    def arc(c, around, half, w, color):
        h = r + w / 2
        draw.arc((c[0] - h, c[1] - h, c[0] + h, c[1] + h), around - half,
                 around + half, fill=color, width=round(w))

    ring(right, ACCENT)
    ring(left, TEXT)
    # The left ring is on top at the upper crossing, the right one at the lower.
    a = math.degrees(math.atan2(math.sqrt(RING_RADIUS**2 - RING_OFFSET**2), RING_OFFSET))
    for c, around, color in ((left, -a, TEXT), (right, 180 - a, ACCENT)):
        arc(c, around, ARC_HALF, width + 2 * gap, background)
        arc(c, around, ARC_HALF + ARC_OVERLAP, width, color)


def app_icon(size):
    """The macOS icon: the mark on a Deep Field tile, on Apple's grid (an 824
    tile on a 1024 canvas, with the shadow the template draws under it)."""
    k = SUPERSAMPLE
    n = size * k
    image = Image.new("RGBA", (n, n))
    tile = (n * 100 / 1024, n * 100 / 1024, n * 924 / 1024, n * 924 / 1024)
    radius = n * 185 / 1024
    shadow = Image.new("RGBA", (n, n))
    ImageDraw.Draw(shadow).rounded_rectangle(
        (tile[0], tile[1] + n * 12 / 1024, tile[2], tile[3] + n * 12 / 1024),
        radius=radius, fill=(0, 0, 0, 120))
    image = Image.alpha_composite(image, shadow.filter(ImageFilter.GaussianBlur(n * 14 / 1024)))
    d = ImageDraw.Draw(image)
    d.rounded_rectangle(tile, radius=radius, fill=CARD,
                        outline=STROKE_STRONG, width=max(k, round(n * 5 / 1024)))
    mark_size = (tile[2] - tile[0]) * .72
    linked_mark(d, n / 2, n / 2, mark_size, CARD, seen=mark_size / k)
    return image.resize((size, size), Image.Resampling.LANCZOS)


class Canvas:
    """Draws in points; every coordinate is scaled to supersampled pixels."""

    def __init__(self, k):
        self.k = k
        self.image = Image.new("RGB", (WIDTH * k, HEIGHT * k), BG_DEEP)
        self.d = ImageDraw.Draw(self.image)

    def px(self, v):
        return round(v * self.k)

    def box(self, x0, y0, x1, y1):
        return (self.px(x0), self.px(y0), self.px(x1) - 1, self.px(y1) - 1)

    def font(self, size, path=FONT, weight=None):
        font = ImageFont.truetype(path, self.px(size))
        if weight:
            try:
                font.set_variation_by_name(weight)
            except (OSError, ValueError):
                pass
        return font

    def dot(self, x, y, r, color):
        self.d.ellipse(self.box(x - r, y - r, x + r, y + r), fill=color)

    def card(self, x0, y0, x1, y1, radius, fill, outline=None):
        self.d.rounded_rectangle(self.box(x0, y0, x1, y1), radius=self.px(radius),
                                 fill=fill, outline=outline,
                                 width=self.px(1) if outline else 0)

    def line(self, points, width, color):
        self.d.line([(self.px(x), self.px(y)) for x, y in points], fill=color,
                    width=self.px(width), joint="curve")
        for x, y in (points[0], points[-1]):
            self.dot(x, y, width / 2, color)

    def width(self, value, font, tracking=0.0):
        if not tracking:
            return self.d.textlength(value, font=font) / self.k
        return sum(self.d.textlength(c, font=font) for c in value) / self.k \
            + tracking * (len(value) - 1)

    def text(self, x, y, value, font, color, tracking=0.0, align="left"):
        """`y` is the top of the line's ascender; tracking is in points."""
        w = self.width(value, font, tracking)
        if align == "right":
            x -= w
        elif align == "center":
            x -= w / 2
        if not tracking:
            self.d.text((self.px(x), self.px(y)), value, font=font, fill=color, anchor="la")
            return x + w
        for c in value:
            self.d.text((self.px(x), self.px(y)), c, font=font, fill=color, anchor="la")
            x += self.d.textlength(c, font=font) / self.k + tracking
        return x


def background(version=None):
    c = Canvas(SUPERSAMPLE)
    # A handful of static stars, as in the manager's backdrop: sparse, and
    # nowhere near the icons.
    for x, y, r in ((64, 128, 1), (232, 52, .75), (544, 70, .75), (696, 118, 1),
                    (752, 236, .75), (40, 322, .75), (404, 168, .75)):
        c.dot(x, y, r, STAR)

    # The linked-pair mark and the product name.
    linked_mark(c.d, c.px(65), c.px(57), c.px(40), BG_DEEP, seen=40 * RETINA)
    c.text(94, 38, "Entangled Desktop", c.font(19, weight=b"Semibold"), TEXT)
    c.text(94, 63, "Linux machines on your Mac, with 3D graphics.", c.font(13), TEXT_DIM)
    mono = c.font(11, MONO, b"Medium")
    c.text(752, 50, "APPLE SILICON · macOS 26+", mono, TEXT_FAINT, tracking=1.2, align="right")

    # A quiet card under each icon. Finder draws icon labels in black over a
    # background image, so each label gets a light plate of its own.
    for cx in (220, 580):
        c.card(cx - 100, 206, cx + 100, 416, 16, CARD, STROKE)
        c.card(cx - 80, 375, cx + 80, 397, 11, PRIMARY)

    # The gesture: a dashed path from the app to Applications, ending in the
    # accent arrowhead.
    c.text(400, 270, "DRAG TO INSTALL", c.font(10.5, weight=b"Semibold"), TEXT_FAINT,
           tracking=1.4, align="center")
    x = 344.0
    while x < 448:
        c.line([(x, 306), (min(x + 4, 448), 306)], 1.5, STROKE_STRONG)
        x += 9
    c.line([(442, 299), (451, 306), (442, 313)], 2, ACCENT)

    c.d.rectangle(c.box(48, 452, 752, 453), fill=DIVIDER)
    body = c.font(13)
    x = c.text(48, 472, "Then open ", body, TEXT_DIM)
    x = c.text(x, 472, "Entangled Desktop", body, TEXT)
    c.text(x, 472, " from Applications.", body, TEXT_DIM)
    if version:
        c.text(752, 474, f"VERSION {version}", mono, TEXT_FAINT, tracking=1, align="right")

    return c.image.resize((WIDTH * RETINA, HEIGHT * RETINA), Image.Resampling.LANCZOS)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--version", help="release version printed on the DMG window")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    background(args.version).save(args.output/"background.png", dpi=(72*RETINA, 72*RETINA))
    iconset = args.output/"Entangled.iconset"
    iconset.mkdir(exist_ok=True)
    # Each size is drawn for itself, so the ring stroke can thicken as it shrinks.
    for size in (16, 32, 128, 256, 512):
        for scale in (1, 2):
            name = f"icon_{size}x{size}" + ("@2x" if scale == 2 else "") + ".png"
            app_icon(size * scale).save(iconset/name)
    app_icon(1024).save(args.output/"icon-preview.png")


if __name__ == "__main__":
    main()
