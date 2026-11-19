# -*- coding: utf-8 -*-
"""Генерирует assets/icon.png (256x256) и assets/icon.ico (16..256).

Дизайн: скруглённый квадрат индиго-градиента с белой монетой и символом "$".
Запуск: python scripts/gen_icon.py
"""
import os
from PIL import Image, ImageDraw, ImageFont

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ASSETS = os.path.join(ROOT, "assets")
S = 1024  # супер-сэмплинг, затем уменьшение для сглаживания
SIZES = [16, 24, 32, 48, 64, 128, 256]

TOP = (58, 66, 190)      # indigo
BOTTOM = (17, 22, 74)    # deep navy
WHITE = (255, 255, 255)
FONT_PATH = r"C:\Windows\Fonts\segoeuib.ttf"


def rounded_mask(size, radius, supersample=4):
    m = Image.new("L", (size * supersample, size * supersample), 0)
    d = ImageDraw.Draw(m)
    d.rounded_rectangle(
        (0, 0, size * supersample - 1, size * supersample - 1),
        radius=radius * supersample,
        fill=255,
    )
    return m.resize((size, size), Image.LANCZOS)


def vertical_gradient(size, top, bottom):
    g = Image.new("RGB", (1, size))
    px = g.load()
    for y in range(size):
        t = y / max(size - 1, 1)
        px[0, y] = tuple(round(top[i] + (bottom[i] - top[i]) * t) for i in range(3))
    return g.resize((size, size), Image.BILINEAR)


def dollar_glyph(size, draw_radius_hint):
    """Возвращает RGBA-слой с белым символом '$'."""
    layer = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    if os.path.exists(FONT_PATH):
        # Подбираем размер шрифта так, чтобы глиф занимал ~62% высоты.
        target = int(size * 0.66)
        font = None
        for delta in range(0, target):
            for cand in (target - delta, target + delta):
                if cand <= 0:
                    continue
                f = ImageFont.truetype(FONT_PATH, cand)
                bbox = d.textbbox((0, 0), "$", font=f)
                if bbox[3] - bbox[1] >= size * 0.60:
                    font = f
                    break
            if font:
                break
        if font is None:
            font = ImageFont.truetype(FONT_PATH, target)
        bbox = d.textbbox((0, 0), "$", font=font)
        w = bbox[2] - bbox[0]
        h = bbox[3] - bbox[1]
        d.text(
            ((size - w) / 2 - bbox[0], (size - h) / 2 - bbox[1]),
            "$",
            font=font,
            fill=WHITE + (255,),
        )
    else:
        # Fallback: рисуем "$" примитивами.
        cx, r = size / 2, size * 0.22
        stroke = max(int(size * 0.075), 2)
        d.arc(
            (cx - r, size * 0.26, cx + r, size * 0.26 + 2 * r),
            start=0, end=300, fill=WHITE + (255,), width=stroke,
        )
        d.arc(
            (cx - r, size * 0.50, cx + r, size * 0.50 + 2 * r),
            start=240, end=180, fill=WHITE + (255,), width=stroke,
        )
        d.line(
            (cx, size * 0.20, cx, size * 0.80),
            fill=WHITE + (255,), width=stroke,
        )
    return layer


def make_master():
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))

    bg = vertical_gradient(S, TOP, BOTTOM).convert("RGBA")
    mask = rounded_mask(S, radius=int(S * 0.22), supersample=2)
    img.paste(bg, (0, 0), mask)

    # Тонкая светлая окантовка — помогает читаемости на тёмном фоне панели задач.
    edge = ImageDraw.Draw(img)
    edge.rounded_rectangle(
        (2, 2, S - 3, S - 3),
        radius=int(S * 0.22),
        outline=(255, 255, 255, 46),
        width=max(int(S * 0.012), 2),
    )

    # Монета: белое кольцо.
    d = ImageDraw.Draw(img)
    ring_w = int(S * 0.058)
    pad = int(S * 0.185)
    d.ellipse((pad, pad, S - pad, S - pad), outline=WHITE + (255,), width=ring_w)

    glyph = dollar_glyph(S, ring_w)
    img.alpha_composite(glyph)

    return img


def main():
    os.makedirs(ASSETS, exist_ok=True)
    master = make_master()

    png_path = os.path.join(ASSETS, "icon.png")
    master.resize((256, 256), Image.LANCZOS).save(png_path, "PNG")
    print("written:", png_path)

    frames = [master.resize((s, s), Image.LANCZOS) for s in SIZES]
    ico_path = os.path.join(ASSETS, "icon.ico")
    frames[-1].save(
        ico_path,
        format="ICO",
        sizes=[(s, s) for s in SIZES],
        append_images=frames[:-1],
    )
    print("written:", ico_path, "sizes:", SIZES)


if __name__ == "__main__":
    main()
