"""Clean + align avatar frames.

source/cat-*.png  ->  assets/web/img/*.webp  (embedded into murlyka.exe; rebuild after running)

- removes stray specks around the cat (keeps only the main shape)
- removes the coloured halo on the outline (edge colours are replaced by
  the nearest solid fur colour, alpha is choked by ~1px)
- aligns every frame to cat-idle by the body/paws, so frames do not jump
- downsizes to OUT px (smaller = less memory in OBS)

Run:  python tools/clean.py
"""
from pathlib import Path

import numpy as np
from PIL import Image, ImageFilter
from scipy import ndimage

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "source"
DST = ROOT / "assets" / "web" / "img"
OUT = 768
NAMES = ["idle", "blink", "talk", "shout", "sad", "sad-talk", "laugh", "surprise", "sing", "eat-1", "eat-2"]
# frames aligned to another frame instead of idle (plate changes the bottom silhouette)
ALIGN_TO = {"eat-1": None, "eat-2": "eat-1"}


def clean(img: Image.Image) -> Image.Image:
    a = np.array(img.convert("RGBA")).astype(np.float32)
    alpha = a[..., 3]

    # 1. drop tiny specks (keeps the cat and real details like music notes)
    lab, n = ndimage.label(alpha > 10)
    if n > 1:
        sizes = ndimage.sum(np.ones_like(alpha), lab, range(1, n + 1))
        keep = np.isin(lab, np.where(sizes >= 400)[0] + 1)
        keep = ndimage.binary_fill_holes(keep)
        alpha[~keep] = 0

    # 2. choke the edge: 1px min-filter then soft blur -> no halo ring
    al = Image.fromarray(alpha.astype(np.uint8))
    al = al.filter(ImageFilter.MinFilter(3)).filter(ImageFilter.GaussianBlur(0.6))
    alpha = np.minimum(alpha, np.array(al, dtype=np.float32))
    alpha[alpha < 12] = 0

    # 3. de-fringe: edge pixels take colour of nearest solid inner pixel
    solid = ndimage.binary_erosion(a[..., 3] >= 250, iterations=2)
    _, (iy, ix) = ndimage.distance_transform_edt(~solid, return_indices=True)
    edge = alpha < 250
    for c in range(3):
        ch = a[..., c]
        ch[edge] = ch[iy[edge], ix[edge]]

    a[..., 3] = alpha
    return Image.fromarray(np.clip(a, 0, 255).astype(np.uint8), "RGBA")


def body_box(img: Image.Image):
    """Width/centre of the paws area (bottom 20% rows) — stable between frames."""
    al = np.array(img)[..., 3] > 200
    h = al.shape[0]
    rows = al[int(h * 0.8):]
    cols = np.where(rows.any(axis=0))[0]
    return cols[0], cols[-1]


def save(im: Image.Image, n: str) -> None:
    im.save(DST / f"{n}.webp", quality=90, method=6)


def main():
    DST.mkdir(parents=True, exist_ok=True)
    frames = {n: clean(Image.open(SRC / f"cat-{n}.png")) for n in NAMES if (SRC / f"cat-{n}.png").exists()}
    sized = {n: im.resize((OUT, OUT), Image.Resampling.LANCZOS) for n, im in frames.items()}

    for n, im in sized.items():
        ref_name = ALIGN_TO.get(n, "idle")
        if ref_name is None or ref_name == n:
            save(im, n)
            print(f"{n:9s} (as is)")
            continue
        rx0, rx1 = body_box(sized[ref_name])
        x0, x1 = body_box(im)
        s = (rx1 - rx0) / (x1 - x0)
        if abs(s - 1) > 0.002:
            size = round(OUT * s)
            im = im.resize((size, size), Image.Resampling.LANCZOS)
            x0, x1 = x0 * s, x1 * s
        dx = round((rx0 + rx1) / 2 - (x0 + x1) / 2)
        dy = OUT - im.height  # bottom edge stays glued to the bottom
        canvas = Image.new("RGBA", (OUT, OUT), (0, 0, 0, 0))
        canvas.paste(im, (dx, dy))  # canvas is empty -> plain paste keeps alpha
        save(canvas, n)
        print(f"{n:9s} scale={s:.3f} dx={dx:+d} dy={dy:+d}")


if __name__ == "__main__":
    main()
