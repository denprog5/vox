"""Иконки Vox из исходной картинки (`assets/brand/vox-icon.png`, генерация Codex).

Запуск: python tools/brand.py
Результат: assets/icon.ico, assets/tray-{paused,busy,error}.ico, assets/logo.png,
assets/web/logo.png, assets/wizard-small*.png.

Крупные размеры (40 px и больше) — уменьшенная картинка. Для 16–32 px «V» из столбиков
сливается в кашу, поэтому там рисуется та же плитка со сплошной «V»: в трее она читается.
"""
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "assets" / "brand" / "vox-icon.png"
SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]
SMALL = 32
RADIUS = 0.2  # скругление плитки от стороны
C1, C2 = (42, 47, 214), (139, 61, 255)  # индиго → фиолетовый


def rounded_mask(size, inset=0):
    k = 4
    m = Image.new("L", (size * k, size * k), 0)
    ImageDraw.Draw(m).rounded_rectangle(
        (inset * k, inset * k, (size - inset) * k - 1, (size - inset) * k - 1),
        radius=int(size * RADIUS * k),
        fill=255,
    )
    return m.resize((size, size), Image.LANCZOS)


def art():
    """Плитка из сгенерированной картинки: обрезка по краю плитки, прозрачные углы."""
    im = Image.open(SRC).convert("RGB")
    gray = im.convert("L").point(lambda v: 255 if v < 235 else 0)
    box = gray.getbbox()
    im = im.crop(box)
    side = max(im.size)
    im = im.resize((side, side), Image.LANCZOS).resize((1024, 1024), Image.LANCZOS)
    out = im.convert("RGBA")
    out.putalpha(rounded_mask(1024, inset=6))
    return out


def gradient(size):
    g = Image.new("RGB", (size, size))
    px = g.load()
    for y in range(size):
        for x in range(size):
            t = (x + (size - 1 - y)) / (2 * (size - 1) or 1)
            px[x, y] = tuple(round(a + (b - a) * t) for a, b in zip(C1, C2))
    return g


def drawn(size):
    """Простая плитка для мелких размеров: градиент + толстая белая «V»."""
    k = 8
    s = size * k
    tile = gradient(s).convert("RGBA")
    tile.putalpha(rounded_mask(s))
    d = ImageDraw.Draw(tile)
    w = round(s * 0.17)
    pts = [(s * 0.25, s * 0.27), (s * 0.5, s * 0.75), (s * 0.75, s * 0.27)]
    d.line(pts, fill="white", width=w, joint="curve")
    for x, y in (pts[0], pts[2]):
        d.ellipse((x - w / 2, y - w / 2, x + w / 2, y + w / 2), fill="white")
    return tile.resize((size, size), Image.LANCZOS)


def frames(big):
    return {n: (drawn(n) if n <= SMALL else big.resize((n, n), Image.LANCZOS)) for n in SIZES}


def badge(im, color, mark=None):
    """Кружок-статус в правом нижнем углу (жёлтый — загрузка, красный «!» — ошибка)."""
    n = im.size[0]
    k = 8
    s = n * k
    layer = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    r = s * 0.27
    cx = cy = s - r - s * 0.01
    ring = max(s * 0.06, k)
    d.ellipse((cx - r - ring, cy - r - ring, cx + r + ring, cy + r + ring), fill=(20, 18, 40, 255))
    d.ellipse((cx - r, cy - r, cx + r, cy + r), fill=color)
    if mark == "!":
        w = r * 0.32
        d.rounded_rectangle((cx - w / 2, cy - r * 0.62, cx + w / 2, cy + r * 0.18), radius=w / 2, fill="white")
        d.ellipse((cx - w / 2, cy + r * 0.32, cx + w / 2, cy + r * 0.32 + w), fill="white")
    out = im.copy()
    out.alpha_composite(layer.resize((n, n), Image.LANCZOS))
    return out


def paused(im):
    a = im.getchannel("A")
    g = im.convert("L").point(lambda v: int(70 + v * 0.55))
    out = Image.merge("RGBA", (g, g, g, a.point(lambda v: int(v * 0.85))))
    return out


def save_ico(path, fr):
    base = fr[256]
    base.save(path, format="ICO", sizes=[(n, n) for n in SIZES], append_images=[fr[n] for n in SIZES[:-1]])


def main():
    big = art()
    fr = frames(big)
    a = ROOT / "assets"
    save_ico(a / "icon.ico", fr)
    save_ico(a / "tray-paused.ico", {n: paused(im) for n, im in fr.items()})
    save_ico(a / "tray-busy.ico", {n: badge(im, (255, 196, 61, 255)) for n, im in fr.items()})
    save_ico(a / "tray-error.ico", {n: badge(im, (235, 64, 64, 255), "!") for n, im in fr.items()})
    logo = big.resize((256, 256), Image.LANCZOS)
    logo.save(a / "logo.png")
    logo.save(a / "web" / "logo.png")
    for name, (w, h) in {"wizard-small.png": (55, 58), "wizard-small-2x.png": (110, 116)}.items():
        bg = Image.new("RGBA", (w, h), "white")
        side = min(w, h) - 4
        ic = big.resize((side, side), Image.LANCZOS)
        bg.alpha_composite(ic, ((w - side) // 2, (h - side) // 2))
        bg.convert("RGB").save(a / name)
    # превью всех размеров для проверки глазами
    prev = Image.new("RGBA", (sum(SIZES) + 10 * len(SIZES), 256 * 4 + 30), (40, 42, 50, 255))
    for row, set_ in enumerate([fr, {n: paused(i) for n, i in fr.items()},
                                {n: badge(i, (255, 196, 61, 255)) for n, i in fr.items()},
                                {n: badge(i, (235, 64, 64, 255), "!") for n, i in fr.items()}]):
        x = 0
        for n in SIZES:
            prev.alpha_composite(set_[n], (x, row * 262))
            x += n + 10
    prev.save(ROOT / "assets" / "brand" / "preview.png")
    print("ok")


if __name__ == "__main__":
    main()
