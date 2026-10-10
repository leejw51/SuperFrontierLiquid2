"""Turn the magenta screen behind every sprite transparent, trim, and
downscale to a crisp sprite size."""
from pathlib import Path
from PIL import Image

RAW = Path("art-src")
OUT = Path("assets")
KEEP_BG = {"hero_city", "title_logo", "panel_frame"}

def knock(img: Image.Image) -> Image.Image:
    img = img.convert("RGBA")
    px = img.load()
    w, h = img.size
    for y in range(h):
        for x in range(w):
            r, g, b, a = px[x, y]
            # Magenta-ish: red high, green low, blue anywhere from hot pink
            # to violet. Grok does not always hit #FF00FF exactly.
            if r > 150 and g < 110 and b > 90 and (r - g) > 100 and (b - g) > 40:
                px[x, y] = (0, 0, 0, 0)
    return img

for src in sorted(RAW.glob("*.png")):
    name = src.stem
    img = Image.open(src)
    if name in KEEP_BG:
        if name == "hero_city":
            img = img.convert("RGB").resize((960, 540), Image.NEAREST)
        elif name == "title_logo":
            img = img.convert("RGBA").resize((384, 256), Image.NEAREST)
        else:
            img = img.convert("RGBA").resize((192, 192), Image.NEAREST)
        img.save(OUT / f"{name}.png", optimize=True)
        continue
    img = knock(img)
    bbox = img.getbbox()
    if bbox:
        img = img.crop(bbox)
    side = max(img.size)
    square = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    square.paste(img, ((side - img.size[0]) // 2, (side - img.size[1]) // 2))
    square = square.resize((96, 96), Image.LANCZOS)
    # Requantise so the downscaled sprite still reads as pixel art.
    square = square.resize((48, 48), Image.NEAREST).resize((96, 96), Image.NEAREST)
    square.save(OUT / f"{name}.png", optimize=True)
    print("wrote", OUT / f"{name}.png")
