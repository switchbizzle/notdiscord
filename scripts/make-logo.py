"""Draw the NotDiscord mark, from the mockup switchb picked.

    python scripts/make-logo.py            # writes assets/logo.png at 1024px

A headset ring around a gradient N whose leg carries a speech-bubble tail,
with a typing indicator inside the counter. Everything is drawn at 4x and
downsampled, which is cheaper than antialiasing each shape by hand and gives
cleaner curves than any of the alternatives.

It's a script rather than an exported PNG so the thing stays editable: nudge a
colour or a radius here, re-run, and every icon in the repo follows from
scripts/make-icons.py.
"""

import math
import os

from PIL import Image, ImageDraw

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "assets", "logo.png")

SIZE = 1024
SS = 4  # supersample factor
S = SIZE * SS

TILE = (18, 20, 28, 255)      # the dark square the mark sits on
HEADSET = (86, 92, 104, 255)  # muted steel, so it frames without competing
BLUE = (59, 130, 246)         # the N's left edge
PURPLE = (168, 85, 247)      # and its right
DOTS = (150, 120, 245, 255)


def rounded(draw, box, radius, fill):
    draw.rounded_rectangle(box, radius=radius, fill=fill)


def gradient(size, start, end):
    """A left-to-right ramp, used through the N's mask."""
    ramp = Image.new("RGBA", (size, size))
    px = ramp.load()
    for x in range(size):
        t = x / max(1, size - 1)
        px[x, 0] = (
            round(start[0] + (end[0] - start[0]) * t),
            round(start[1] + (end[1] - start[1]) * t),
            round(start[2] + (end[2] - start[2]) * t),
            255,
        )
    return ramp.resize((size, size), Image.NEAREST) if False else ramp.crop((0, 0, size, 1)).resize((size, size))


def draw_headset(draw):
    cx, cy = S // 2, int(S * 0.45)
    outer = int(S * 0.30)
    band = int(S * 0.034)
    # The band. A full ring reads as a headset once the cups are on it.
    draw.ellipse(
        (cx - outer, cy - outer, cx + outer, cy + outer),
        outline=HEADSET,
        width=band,
    )
    # Earcups, centred on the band at three and nine o'clock.
    cup_w, cup_h = int(S * 0.058), int(S * 0.135)
    for side in (-1, 1):
        x = cx + side * outer - cup_w // 2
        rounded(draw, (x, cy - cup_h // 2, x + cup_w, cy + cup_h // 2), cup_w // 2, HEADSET)

    # The boom: concentric with the band so it looks hinged to the right cup,
    # sweeping under the chin. PIL angles run clockwise from three o'clock.
    # Short and set out from the band. Concentric and long, it just reads as
    # a second ring — the first attempt looked like headphones inside
    # headphones.
    boom_r = outer + int(S * 0.075)
    draw.arc(
        (cx - boom_r, cy - boom_r, cx + boom_r, cy + boom_r),
        start=32,
        end=112,
        fill=HEADSET,
        width=int(S * 0.027),
    )
    # The capsule, sitting at the boom's far end rather than adrift near it.
    end = math.radians(112)
    mx, my = cx + boom_r * math.cos(end), cy + boom_r * math.sin(end)
    tip_w, tip_h = int(S * 0.070), int(S * 0.038)
    rounded(
        draw,
        (int(mx - tip_w / 2), int(my - tip_h / 2), int(mx + tip_w / 2), int(my + tip_h / 2)),
        tip_h // 2,
        HEADSET,
    )


def n_geometry():
    """Where the N sits, shared by the mask and the dots that tuck inside it."""
    cx, cy = S // 2, int(S * 0.45)
    half_w = int(S * 0.132)
    half_h = int(S * 0.140)
    stroke = int(S * 0.058)
    return cx, cy, half_w, half_h, stroke


def n_mask():
    """The N as a white-on-black mask: two legs, a diagonal, and a tail."""
    mask = Image.new("L", (S, S), 0)
    d = ImageDraw.Draw(mask)
    cx, cy, half_w, half_h, stroke = n_geometry()
    r = stroke // 2

    left = cx - half_w
    right = cx + half_w - stroke
    top = cy - half_h
    bottom = cy + half_h

    rounded(d, (left, top, left + stroke, bottom), r, 255)
    rounded(d, (right, top, right + stroke, bottom), r, 255)
    d.line((left + r, top + r, right + r, bottom - r), fill=255, width=stroke, joint="curve")
    d.ellipse((left, top, left + stroke, top + stroke), fill=255)
    d.ellipse((right, bottom - stroke, right + stroke, bottom), fill=255)

    # A speech-bubble tail: a short spur off the bottom of the left leg,
    # pointing away from the letter. Small enough to read as a tail rather
    # than as part of the N.
    spur = int(S * 0.042)
    d.polygon(
        [
            (left, bottom - stroke // 2),
            (left + stroke, bottom - stroke // 2),
            (left + stroke // 2, bottom + spur),
        ],
        fill=255,
    )
    return mask


def draw_dots(img):
    """The typing indicator, in the open counter below the diagonal."""
    d = ImageDraw.Draw(img)
    cx, cy, half_w, half_h, stroke = n_geometry()
    r = int(S * 0.0135)
    y = cy + half_h - stroke // 2 - int(S * 0.012)
    gap = int(S * 0.042)
    for i in (-1, 0, 1):
        x = cx + int(S * 0.020) + i * gap
        d.ellipse((x - r, y - r, x + r, y + r), fill=DOTS)


def main():
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    # The tile. Rounded, because every platform that shows this will round it
    # anyway, and doing it here keeps the corners consistent.
    rounded(d, (0, 0, S - 1, S - 1), int(S * 0.22), TILE)
    draw_headset(d)

    # Span the ramp across the N itself: stretched over the whole canvas, the
    # letter only ever samples the middle of it and comes out one flat violet.
    cx, _, half_w, _, _ = n_geometry()
    ramp = Image.new("RGBA", (S, S))
    band = gradient(2 * half_w, BLUE, PURPLE)
    ramp.paste(band.crop((0, 0, 2 * half_w, S)) if band.height >= S else band.resize((2 * half_w, S)),
               (cx - half_w, 0))
    img.paste(ramp, (0, 0), n_mask())
    draw_dots(img)

    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    img.resize((SIZE, SIZE), Image.LANCZOS).save(OUT)
    print(f"wrote {os.path.relpath(OUT, ROOT)} at {SIZE}px")
    print("now: python scripts/make-icons.py")


if __name__ == "__main__":
    main()
