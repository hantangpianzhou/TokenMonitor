#!/usr/bin/env python3
"""把 render_screen.py 生成的整帧 RGB565 发给图灵智显 3.5 寸（Revision A）。

协议严格依据 mathoudebine/turing-smart-screen-python 官方实现：
  library/lcd/lcd_comm_rev_a.py  +  library/lcd/serialize.py

Revision A 3.5"：320×480，串口 115200，RGB565 小端，命令码见下。
Revision B（玄方，背面 Q002_V2.0）协议完全不同（10 字节帧 + 大端），本脚本不支持。

用法：
    python send_to_screen.py --list                    # 列出串口
    python send_to_screen.py --port COM3 --frame frame_landscape.rgb565
    python send_to_screen.py --port COM3 --frame frame_landscape.rgb565 --orient landscape
    python send_to_screen.py --self-test               # 无硬件：验证命令字节流
"""
from __future__ import annotations

import argparse
import time
from enum import IntEnum

try:
    import serial
    from serial.tools import list_ports
except ImportError:
    serial = None
    list_ports = None


class Command(IntEnum):
    RESET = 101
    CLEAR = 102
    SCREEN_OFF = 108
    SCREEN_ON = 109
    SET_BRIGHTNESS = 110
    SET_ORIENTATION = 121
    DISPLAY_BITMAP = 197

    SET_MIRROR = 122
    DISPLAY_PIXELS = 195
    # 官方 UsbMonitor.exe 反编译 rid=482 发图时压入 198；权威开源库用 197。
    # 只差 1，疑为不同固件子版本。留作 --bitmap-cmd 切换验证。
    DISPLAY_BITMAP_ALT = 198


class Orientation(IntEnum):
    PORTRAIT = 0
    LANDSCAPE = 1
    REVERSE_PORTRAIT = 2
    REVERSE_LANDSCAPE = 3


class Turing35RevA:
    """仅负责协议编码；写入端口交给调用方，便于环回测试。"""

    def __init__(self, width: int = 320, height: int = 480):
        self.width = width
        self.height = height

    @staticmethod
    def command_packet(cmd: Command, x: int, y: int, ex: int, ey: int) -> bytes:
        """官方 SendCommand：6 字节，坐标按位拆分。"""
        return bytes([
            (x >> 2) & 0xFF,
            (((x & 3) << 6) + (y >> 4)) & 0xFF,
            (((y & 15) << 4) + (ex >> 6)) & 0xFF,
            (((ex & 63) << 2) + (ey >> 8)) & 0xFF,
            ey & 0xFF,
            cmd & 0xFF,
        ])

    def orientation_packet(self, orientation: Orientation) -> bytes:
        """官方 SetOrientation：16 字节（前 6 字节同 command_packet）。"""
        w, h = self.width, self.height
        buf = bytearray(16)
        buf[0:6] = self.command_packet(Command.SET_ORIENTATION, 0, 0, 0, 0)
        buf[6] = orientation + 100
        buf[7] = (w >> 8) & 0xFF
        buf[8] = w & 0xFF
        buf[9] = (h >> 8) & 0xFF
        buf[10] = h & 0xFF
        return bytes(buf)

    def brightness_packet(self, level: int = 50) -> bytes:
        """官方：值越小越亮，0=最亮，255=最暗。"""
        assert 0 <= level <= 100
        absolute = int(255 - (level / 100) * 255)
        return self.command_packet(Command.SET_BRIGHTNESS, absolute, 0, 0, 0)

    def bitmap_header(self, x0: int, y0: int, x1: int, y1: int,
                      cmd: int = Command.DISPLAY_BITMAP) -> bytes:
        return self.command_packet(cmd, x0, y0, x1, y1)

    def chunks(self, rgb565: bytes):
        """官方 chunked(data, width*8)：320*8=2560 字节/块（=4 行），共 120 块。"""
        size = self.width * 8
        for i in range(0, len(rgb565), size):
            yield rgb565[i:i + size]

    @staticmethod
    def rotate_cw(landscape: bytes, w: int = 480, h: int = 320) -> bytes:
        """横屏 w列×h行 → 物理 h列×w行（顺时针 90°）。
        屏的物理面板是 320 宽，故发送的像素必须按 320 列重排。
        源 H[y*w+x] → 目标 P[x*h+(h-1-y)]。
        若实机方向反了，改用 rotate_ccw。"""
        out = bytearray(len(landscape))
        src = memoryview(landscape).cast("H")
        dst = memoryview(out).cast("H")
        for y in range(h):
            for x in range(w):
                dst[x * h + (h - 1 - y)] = src[y * w + x]
        return bytes(out)

    @staticmethod
    def rotate_ccw(landscape: bytes, w: int = 480, h: int = 320) -> bytes:
        """横屏 w列×h行 → 物理 h列×w行（逆时针 90°），方向备选。"""
        out = bytearray(len(landscape))
        src = memoryview(landscape).cast("H")
        dst = memoryview(out).cast("H")
        for y in range(h):
            for x in range(w):
                dst[(w - 1 - x) * h + y] = src[y * w + x]
        return bytes(out)

    @staticmethod
    def from_physical(physical: bytes, w: int = 480, h: int = 320) -> bytes:
        """rotate_cw 的逆：物理 h列×w行 → 横屏 w列×h行，仅供自检。"""
        out = bytearray(len(physical))
        src = memoryview(physical).cast("H")
        dst = memoryview(out).cast("H")
        for a in range(w):
            for b in range(h):
                dst[(h - 1 - b) * w + a] = src[a * h + b]
        return bytes(out)

    def build_stream(self, frame: bytes, orientation: Orientation, brightness: int,
                     bitmap_cmd: int = Command.DISPLAY_BITMAP) -> bytes:
        """产出完整字节流：方向 → 亮度 → 位图头 → 分块像素。供写入或测试。"""
        out = bytearray()
        out += self.orientation_packet(orientation)
        out += self.brightness_packet(brightness)
        out += self.bitmap_header(0, 0, self.width - 1, self.height - 1, cmd=bitmap_cmd)
        for chunk in self.chunks(frame):
            out += chunk
        return bytes(out)


def send(port: str, frame_path: str, orientation: Orientation, brightness: int, baud: int) -> None:
    if serial is None:
        raise SystemExit("需要 pyserial：pip install pyserial")

    with open(frame_path, "rb") as f:
        frame = f.read()
    expect = 320 * 480 * 2
    if len(frame) != expect:
        raise SystemExit(f"帧长度不对：{len(frame)}，应为 {expect}（320×480 RGB565）")

    dev = Turing35RevA()
    # 物理面板是 320 宽。横屏预览是 480×320 排列，须先重排成 320×480，
    # 再靠 SET_ORIENTATION(LANDSCAPE) 让屏横显；方向错了换 rotate_ccw。
    if orientation == Orientation.LANDSCAPE:
        frame = dev.rotate_cw(frame)
    stream = dev.build_stream(frame, orientation, brightness)

    with serial.Serial(port, baudrate=baud, timeout=2) as ser:
        ser.write(stream)
        ser.flush()
    print(f"sent {len(stream)} bytes to {port} @ {baud}")


def self_test() -> None:
    """无硬件验证：命令字节与官方源码逐字节比对。"""
    dev = Turing35RevA()

    # 官方 SendCommand(cmd,x,y,ex,ey)：坐标按位拆分，亮度值在 x 里
    # 亮度 0% → level_absolute=255 → x=255：b0=255>>2=63, b1=(255&3)<<6=192
    pkt = dev.brightness_packet(0)
    assert pkt == bytes([63, 192, 0, 0, 0, Command.SET_BRIGHTNESS]), pkt

    # 官方 SetOrientation：16 字节，方向值 = orientation+100，宽高随后
    op = dev.orientation_packet(Orientation.LANDSCAPE)
    assert len(op) == 16
    assert op[5] == Command.SET_ORIENTATION and op[6] == 101, op
    assert op[7:11] == bytes([0x01, 0x40, 0x01, 0xE0]), op  # 320,480 大端

    # 官方 6 字节坐标拆分：全屏 x0=0,y0=0,x1=319,y1=479
    # b0=0>>2=0; b1=0; b2=(319>>6)=4; b3=((319&63)<<2)+(479>>8)=252+1=253; b4=479&255=223
    bh = dev.bitmap_header(0, 0, 319, 479)
    assert bh == bytes([0, 0, 4, 253, 223, Command.DISPLAY_BITMAP]), bh

    # 分块：width*8=2560 字节/块，307200/2560=120 块
    frame = bytes(320 * 480 * 2)
    chunks = list(dev.chunks(frame))
    assert len(chunks) == 120 and all(len(c) == 2560 for c in chunks), len(chunks)

    stream = dev.build_stream(frame, Orientation.LANDSCAPE, 25)
    assert len(stream) == 16 + 6 + 6 + 320 * 480 * 2, len(stream)

    # 旋转重排可逆性：cw → from_physical 必须回到原横屏帧
    import array
    landscape = array.array("H", (i % 65536 for i in range(480 * 320))).tobytes()
    physical = dev.rotate_cw(landscape)
    assert len(physical) == 480 * 320 * 2
    assert dev.from_physical(physical) == landscape, "旋转重排不可逆"
    print("self-test OK: 命令帧/坐标拆分/方向宽高/分块/旋转重排 全部通过")


def list_ports_cmd() -> None:
    if list_ports is None:
        raise SystemExit("需要 pyserial：pip install pyserial")
    for p in list_ports.comports():
        mark = "  <== 图灵 3.5寸候选" if p.vid == 0x1A86 and p.pid == 0x5722 else ""
        print(f"{p.device}  VID:PID={p.vid:04x}:{p.pid:04x}  SN={p.serial_number}{mark}")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", help="COM 口，如 COM3")
    ap.add_argument("--frame", help="render_screen.py 导出的 .rgb565")
    ap.add_argument("--orient", choices=["portrait", "landscape"], default="portrait")
    ap.add_argument("--brightness", type=int, default=50,
                    help="亮度百分比 0-100：100 最亮、0 最暗（默认 50）")
    ap.add_argument("--baud", type=int, default=115200)
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args()

    if args.self_test:
        self_test()
    elif args.list:
        list_ports_cmd()
    elif args.port and args.frame:
        # 横屏预览是 480×320 画在物理 320×480 上：按官方做法发整帧，
        # 由 SET_ORIENTATION 决定物理映射；画面旋转方向需屏到手后实测微调。
        send(args.port, args.frame,
             Orientation.LANDSCAPE if args.orient == "landscape" else Orientation.PORTRAIT,
             args.brightness, args.baud)
    else:
        ap.print_help()


if __name__ == "__main__":
    main()
