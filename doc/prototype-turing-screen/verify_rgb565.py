"""RGB565 往返验证：把导出的 .bin 解码回 PNG，确认字节序与渲染一致。"""
from PIL import Image
import os

p = os.path.dirname(os.path.abspath(__file__))
raw = open(os.path.join(p, "frame_landscape.rgb565"), "rb").read()
print("bytes", len(raw), "expect", 480 * 320 * 2)

px = []
for i in range(0, len(raw), 2):
    w = raw[i + 1] << 8 | raw[i]          # 小端还原
    r = ((w >> 11) & 0x1F) << 3
    g = ((w >> 5) & 0x3F) << 2
    b = (w & 0x1F) << 3
    px.append((r, g, b))

img = Image.new("RGB", (480, 320))
img.putdata(px)
img.save(os.path.join(p, "verify_roundtrip.png"))
print("roundtrip saved")
