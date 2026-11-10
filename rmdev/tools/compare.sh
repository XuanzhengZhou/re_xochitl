#!/bin/sh
# 把官方截图和我们的一张拼成左右对照，缩放一半省 token。
#   sh tools/compare.sh official/10-library.png 22-status.png out.png
set -eu
cd "$(dirname "$0")/.."
A=${1:?官方图}; B=${2:?我们的图}; O=${3:-shots/compare.png}
"$HOME/myenv/bin/python3" - "$A" "$B" "$O" <<'PY'
import sys
from PIL import Image, ImageDraw
a, b, o = sys.argv[1], sys.argv[2], sys.argv[3]
ia = Image.open(a).convert("L").resize((702, 936), Image.LANCZOS)
ib = Image.open(b).convert("L").resize((702, 936), Image.LANCZOS)
c = Image.new("L", (1412, 966), 255)
c.paste(ia, (0, 30)); c.paste(ib, (710, 30))
d = ImageDraw.Draw(c)
d.text((4, 8), "OFFICIAL: " + a, fill=0)
d.text((714, 8), "OURS: " + b, fill=0)
d.line([(706, 0), (706, 966)], fill=0, width=2)
c.save(o)
print(o)
PY
