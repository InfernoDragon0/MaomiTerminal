"""Renders icon.svg into icon.ico (Windows), icon.png (Linux) and icon.icns (macOS).

Needs the resvg CLI (`cargo install resvg`) and Pillow. Each size is rendered from the SVG
instead of downscaled, so the small sizes stay sharp.

    python packaging/icon/build.py [path/to/resvg]
"""

import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image

HERE = Path(__file__).resolve().parent
SVG = HERE / "icon.svg"
ICO_SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]
ICNS_SIZES = [16, 32, 64, 128, 256, 512, 1024]


def render(resvg: str, size: int, out_dir: Path) -> Image.Image:
    png = out_dir / f"{size}.png"
    subprocess.run([resvg, "-w", str(size), "-h", str(size), str(SVG), str(png)], check=True)
    return Image.open(png).convert("RGBA")


def main() -> None:
    resvg = sys.argv[1] if len(sys.argv) > 1 else "resvg"
    with tempfile.TemporaryDirectory() as tmp:
        out = Path(tmp)
        sizes = sorted(set(ICO_SIZES + ICNS_SIZES))
        images = {s: render(resvg, s, out) for s in sizes}

        ico = [images[s] for s in reversed(ICO_SIZES)]
        ico[0].save(HERE / "icon.ico", sizes=[(s, s) for s in ICO_SIZES], append_images=ico[1:])

        images[512].save(HERE / "icon.png")

        icns = [images[s] for s in reversed(ICNS_SIZES)]
        icns[0].save(HERE / "icon.icns", append_images=icns[1:])


if __name__ == "__main__":
    main()
