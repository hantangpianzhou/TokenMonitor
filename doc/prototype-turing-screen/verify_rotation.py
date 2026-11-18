"""验证横屏帧经 rotate_cw 重排后的画面方向是否正确。"""
import os
from PIL import Image
from send_to_screen import Turing35RevA

p = os.path.dirname(os.path.abspath(__file__))
raw = open(os.path.join(p, "frame_landscape.rgb565"), "rb").read()
dev = Turing35RevA()
phys = dev.rotate_cw(raw)

# 物理 320 列 × 480 行解码
px = []
for i in range(0, len(phys), 2):
    w = phys[i + 1] << 8 | phys[i]
    px.append((((w >> 11) & 0x1F) << 3, ((w >> 5) & 0x3F) << 2, (w & 0x1F) << 3))
img = Image.new("RGB", (320, 480))
img.putdata(px)
img.save(os.path.join(p, "verify_rotated_physical.png"))
print("saved verify_rotated_physical.png (320x480 物理排列)")
