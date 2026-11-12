"""Hero art for the landing page, in the style of a dithered painting.

The whole picture is drawn as ordered-dither pixel art (the 4x4 Bayer pattern of the dither
effect tern's wallpaper had), then dissolves into the page colour:
towards the bottom the dots get sparser and fade toward the background on a fine lattice.

    python3 site/tools/make_hero.py <picture.jpg> <out.jpg> <width> <height> [focus_y]
"""
import sys
from PIL import Image, ImageEnhance

BAYER = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]]


def bayer8():
    """The 8x8 Bayer matrix: 64 steps, so the dissolve thins out without visible bands."""
    m = [[0]]
    for _ in range(3):
        n = len(m)
        m = [[4 * m[y % n][x % n] + [[0, 2], [3, 1]][y // n][x // n] for x in range(2 * n)] for y in range(2 * n)]
    return m


B8 = bayer8()
BG = (255, 255, 255)      # --bg in site/style.css (light)
PX = 2                    # one art pixel = 2x2 screen pixels at 1x
LEVELS = 6                # colour steps per channel: few enough to read as dithered art
FADE_FROM, FADE_TO = 0.42, 1.0   # fraction of the height where the dissolve starts and ends

src, out, W, H = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
focus = float(sys.argv[5]) if len(sys.argv) > 5 else 0.5

im = Image.open(src).convert("RGB")
s = max(W / im.width, H / im.height)
im = im.resize((round(im.width * s), round(im.height * s)), Image.LANCZOS)
left, top = (im.width - W) // 2, int((im.height - H) * focus)
im = im.crop((left, top, left + W, top + H))
# Pastel, like a watercolour on a light page: softer colour, lifted toward white.
im = ImageEnhance.Color(im).enhance(0.8)
im = Image.blend(im, Image.new("RGB", im.size, (255, 255, 255)), 0.28)

# Work at art-pixel resolution, then scale up with hard edges.
w, h = W // PX, H // PX
art = im.resize((w, h), Image.LANCZOS)
p = art.load()
step = 255 / (LEVELS - 1)

def smooth(t):
    return t * t * (3 - 2 * t)

for y in range(h):
    t = y / h
    fade = 0.0 if t <= FADE_FROM else min(1.0, (t - FADE_FROM) / (FADE_TO - FADE_FROM))
    fade = smooth(fade)
    for x in range(w):
        thr = (BAYER[y % 4][x % 4] + 0.5) / 16
        r, g, b = p[x, y]
        # Ordered dither each channel to LEVELS steps: the painting becomes pixel art.
        c = []
        for v in (r, g, b):
            q = v / step
            lo = int(q)
            c.append(min(255, round((lo + (1 if q - lo > thr else 0)) * step)))
        if fade > 0:
            # Lower down the dots thin out one Bayer step at a time and pale toward the page.
            keep = (B8[y % 8][x % 8] + 0.5) / 64 > fade
            mix = fade * 0.75
            c = [round(cv * (1 - mix) + bv * mix) for cv, bv in zip(c, BG)] if keep else list(BG)
        p[x, y] = tuple(c)

# Saved at art-pixel size; the page scales it up with `image-rendering: pixelated`, which
# keeps every dot sharp at a fraction of the file size of a full-size JPEG.
if out.endswith(".png"):
    art.save(out, "PNG", optimize=True)
else:
    art.save(out, "WEBP", quality=90, method=6)
print(out, art.size)
