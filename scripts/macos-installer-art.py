#!/usr/bin/env python3
"""Render Entangled's own procedural identity and Retina DMG artwork."""
import argparse
import math
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont, ImageFilter

CYAN = (53, 226, 240)
VIOLET = (168, 107, 255)


def orbit(draw, center, radius, tilt, color, width):
    points = []
    for i in range(181):
        t = i * 2 * math.pi / 180
        x, y = radius * math.cos(t), radius * .40 * math.sin(t)
        points.append((center[0] + x * math.cos(tilt) - y * math.sin(tilt),
                       center[1] + x * math.sin(tilt) + y * math.cos(tilt)))
    draw.line(points, fill=color, width=width, joint="curve")


def mark(size):
    image = Image.new("RGBA", (size, size))
    d = ImageDraw.Draw(image)
    m = size * .06
    d.rounded_rectangle((m, m, size-m, size-m), radius=size*.20,
                        fill=(7, 11, 24), outline=(38, 50, 78), width=max(1, size//100))
    glow = Image.new("RGBA", image.size)
    g = ImageDraw.Draw(glow)
    for tilt, color in ((.52, CYAN), (-.52, VIOLET)):
        orbit(g, (size/2, size/2), size*.35, tilt, color+(130,), max(2, size//24))
    image = Image.alpha_composite(image, glow.filter(ImageFilter.GaussianBlur(size*.026)))
    d = ImageDraw.Draw(image)
    for tilt, color, phase in ((.52, CYAN, .62), (-.52, VIOLET, 3.76)):
        orbit(d, (size/2, size/2), size*.35, tilt, color, max(2, size//40))
        x, y = size*.35*math.cos(phase), size*.14*math.sin(phase)
        cx = size/2 + x*math.cos(tilt)-y*math.sin(tilt)
        cy = size/2 + x*math.sin(tilt)+y*math.cos(tilt)
        r = size*.036
        d.ellipse((cx-r, cy-r, cx+r, cy+r), fill=color)
    c, r = size/2, size*.036
    d.polygon(((c,c-r),(c+r,c),(c,c+r),(c-r,c)), fill=(219,237,255))
    return image


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    image = Image.new("RGB", (1600, 1040))
    pixels = image.load()
    for y in range(1040):
        for x in range(1600):
            cyan = math.exp(-(((x-350)/620)**2 + ((y-660)/410)**2))
            violet = math.exp(-(((x-1330)/620)**2 + ((y-190)/400)**2))
            pixels[x,y] = (int(7+13*violet+2*cyan), int(11+18*cyan+2*violet),
                           int(24+25*violet+21*cyan))
    d = ImageDraw.Draw(image)
    regular = "/System/Library/Fonts/SFNS.ttf"
    mono = "/System/Library/Fonts/SFNSMono.ttf"
    def text(x, y, value, size, color=(230,238,255), font=regular):
        d.text((x,y), value, fill=color, font=ImageFont.truetype(font,size))
    # Subtle coordinate grid and orbital paths, leaving native icons legible.
    for x in range(0,1600,64):
        d.line((x,0,x,1040), fill=(17,25,43), width=1)
    for y in range(0,1040,64):
        d.line((0,y,1600,y), fill=(17,25,43), width=1)
    orbit(d, (1320,100), 510, .52, (42,47,79), 2)
    orbit(d, (1320,100), 510, -.52, (29,63,76), 2)
    image.paste(mark(112), (98,72), mark(112))
    text(236,86,"ENTANGLED",46)
    text(239,144,"D E S K T O P",20,(128,149,177))
    text(100,240,"Two worlds. One desktop.",78)
    text(105,341,"Ubuntu on your Mac, with hardware 3D.",29,(150,170,197))
    for x, color in ((440,CYAN),(1160,VIOLET)):
        d.rounded_rectangle((x-170,475,x+170,785), radius=42,
                            fill=(14,23,41), outline=(39,54,78), width=2)
        # Finder draws native filename labels in black over image backgrounds.
        # Give those labels their own light plate rather than duplicating text.
        d.rounded_rectangle((x-164,752,x+164,797), radius=12, fill=(222,232,247))
        d.line((x-74,805,x+74,805),fill=color,width=3)
    # Native app and Applications icons occupy (220, 306) and (580, 306) points.
    d.line((692,616,906,616), fill=(116,143,175), width=3)
    d.line((884,597,906,616,884,635), fill=CYAN, width=4)
    text(680,541,"DRAG TO INSTALL",18,(129,155,184),mono)
    d.line((104,877,1496,877),fill=(38,51,73),width=2)
    text(104,918,"Move the app to Applications. Then open Entangled Desktop.",27,(171,192,216))
    text(105,977,"APPLE SILICON  /  macOS 26+",18,(105,127,155),mono)
    image.save(args.output/"background.png",dpi=(144,144))
    iconset = args.output/"Entangled.iconset"
    iconset.mkdir(exist_ok=True)
    master = mark(1024)
    for size in (16,32,128,256,512):
        for scale in (1,2):
            name = f"icon_{size}x{size}" + ("@2x" if scale==2 else "") + ".png"
            master.resize((size*scale,size*scale),Image.Resampling.LANCZOS).save(iconset/name)
    master.save(args.output/"icon-preview.png")


if __name__ == "__main__":
    main()
