"""Build application icon assets from the transparent master artwork."""

from pathlib import Path

from PIL import Image, ImageDraw


ROOT = Path(__file__).resolve().parents[1]
ICON_DIR = ROOT / "assets" / "icons"
MASTER = ICON_DIR / "auto-voice-master.png"
OUTPUT = ICON_DIR / "auto-voice.png"
PNG_DIR = ICON_DIR / "png"
SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256, 512)


def normalized_master() -> Image.Image:
    image = Image.open(MASTER).convert("RGBA")
    alpha_bbox = image.getchannel("A").getbbox()
    if alpha_bbox is None:
        raise ValueError(f"{MASTER} has no visible pixels")

    artwork = image.crop(alpha_bbox)
    side = max(artwork.size)
    padding = round(side * 0.055)
    canvas_side = side + padding * 2
    canvas = Image.new("RGBA", (canvas_side, canvas_side), (0, 0, 0, 0))
    offset = ((canvas_side - artwork.width) // 2, (canvas_side - artwork.height) // 2)
    canvas.alpha_composite(artwork, offset)
    return canvas.resize((1024, 1024), Image.Resampling.LANCZOS)


def make_preview(icons: dict[int, Image.Image]) -> None:
    scale = 4
    cells = []
    for size in (16, 20, 24, 32, 48):
        checker = Image.new("RGB", (size, size), "#d8dee9")
        draw = ImageDraw.Draw(checker)
        tile = max(2, size // 4)
        for y in range(0, size, tile):
            for x in range(0, size, tile):
                if (x // tile + y // tile) % 2:
                    draw.rectangle((x, y, x + tile - 1, y + tile - 1), fill="#aeb8c7")
        checker.paste(icons[size], mask=icons[size].getchannel("A"))
        cells.append(checker.resize((size * scale, size * scale), Image.Resampling.NEAREST))

    gap = 16
    width = sum(cell.width for cell in cells) + gap * (len(cells) + 1)
    height = max(cell.height for cell in cells) + gap * 2
    preview = Image.new("RGB", (width, height), "#f6f8fb")
    x = gap
    for cell in cells:
        preview.paste(cell, (x, (height - cell.height) // 2))
        x += cell.width + gap
    preview.save(ICON_DIR / "auto-voice-preview.png", optimize=True)


def main() -> None:
    PNG_DIR.mkdir(parents=True, exist_ok=True)
    master = normalized_master()
    master.save(OUTPUT, optimize=True)

    icons: dict[int, Image.Image] = {}
    for size in SIZES:
        icon = master.resize((size, size), Image.Resampling.LANCZOS)
        icon.save(PNG_DIR / f"auto-voice-{size}.png", optimize=True)
        icons[size] = icon

    master.save(
        ICON_DIR / "auto-voice.ico",
        format="ICO",
        sizes=[(size, size) for size in (16, 20, 24, 32, 40, 48, 64, 128, 256)],
    )
    (ICON_DIR / "auto-voice-32.rgba").write_bytes(icons[32].tobytes())
    make_preview(icons)


if __name__ == "__main__":
    main()
