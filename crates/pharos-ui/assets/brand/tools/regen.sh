#!/usr/bin/env bash
# Regenerates every file in assets/brand from the drawing in build_brand.py.
# Needs python3 with fontTools + Pillow, node with playwright, and Inter
# Display SemiBold (pass its path, or set INTER_DISPLAY_SEMIBOLD).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
out=$(dirname "$here")
font=${1:-${INTER_DISPLAY_SEMIBOLD:?pass the InterDisplay-SemiBold.otf path}}
python3 "$here/build_brand.py" "$out" "$font"
node "$here/raster.js" "$out"
python3 - "$out" <<'PY'
import sys
from PIL import Image
d = sys.argv[1] + "/"
ims = [Image.open(d + f"_favicon-{s}.png") for s in (16, 32, 48)]
ims[2].save(d + "favicon.ico", sizes=[(16, 16), (32, 32), (48, 48)], append_images=ims[:2])
PY
rm -f "$out"/_tile_*.svg "$out"/_favicon-*.png
